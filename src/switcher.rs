//! Alt+Tab replacement: a vertical list of window titles.
//!
//! Windows reserves Alt+Tab, so `RegisterHotKey` can't claim it — the only way
//! in is a `WH_KEYBOARD_LL` hook. That hook runs on the thread that installed
//! it and has to return inside `LowLevelHooksTimeout` (300 ms by default) or
//! Windows silently unhooks us, so it gets its own thread with nothing else on
//! it. The hook proc decides swallow-or-pass from atomics alone and posts the
//! real work to the overlay's queue, which runs after the hook has returned.
//!
//! The overlay never takes focus (`WS_EX_NOACTIVATE`, `SW_SHOWNOACTIVATE`).
//! It doesn't need to — the hook is global — and taking focus would disturb
//! the very Z-order we're reading the window list from.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT, D2D_RECT_F, D2D_SIZE_U,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1Bitmap, ID2D1Factory, ID2D1HwndRenderTarget, ID2D1SolidColorBrush,
    D2D1_BITMAP_INTERPOLATION_MODE_LINEAR, D2D1_BITMAP_PROPERTIES,
    D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_HWND_RENDER_TARGET_PROPERTIES,
    D2D1_PRESENT_OPTIONS_NONE, D2D1_RENDER_TARGET_PROPERTIES, D2D1_RENDER_TARGET_TYPE_SOFTWARE,
    D2D1_ROUNDED_RECT,
};
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, IDWriteFactory, IDWriteInlineObject, IDWriteTextFormat,
    DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL,
    DWRITE_FONT_WEIGHT_NORMAL, DWRITE_MEASURING_MODE_NATURAL, DWRITE_PARAGRAPH_ALIGNMENT_CENTER,
    DWRITE_TEXT_ALIGNMENT_TRAILING, DWRITE_TRIMMING, DWRITE_TRIMMING_GRANULARITY_CHARACTER,
    DWRITE_WORD_WRAPPING_NO_WRAP,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::{
    BeginPaint, EndPaint, GetMonitorInfoW, MonitorFromPoint, MonitorFromWindow, MONITORINFO,
    MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY, PAINTSTRUCT,
};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{
    AttachThreadInput, GetCurrentThreadId, OpenProcess, QueryFullProcessImageNameW,
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, SetActiveWindow, SetFocus, VK_ESCAPE, VK_LMENU, VK_MENU, VK_RMENU,
    VK_SHIFT, VK_TAB,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DispatchMessageW, GetCursorPos,
    GetForegroundWindow, GetMessageW, GetWindowLongPtrW, GetWindowTextLengthW, GetWindowTextW,
    GetWindowThreadProcessId, IsIconic, LoadCursorW, PostMessageW, RegisterClassW,
    SetForegroundWindow, SetWindowLongPtrW, SetWindowPos, SetWindowsHookExW, ShowWindow,
    UnhookWindowsHookEx, CREATESTRUCTW, GWLP_USERDATA, HC_ACTION, HWND_TOP, IDC_ARROW,
    KBDLLHOOKSTRUCT, MA_NOACTIVATE, MSG, SWP_NOMOVE, SWP_NOSIZE, SW_HIDE, SW_RESTORE,
    SW_SHOWNOACTIVATE, WH_KEYBOARD_LL, WM_APP, WM_ERASEBKGND, WM_KEYDOWN, WM_KEYUP,
    WM_LBUTTONUP, WM_MOUSEACTIVATE, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_NCCREATE, WM_NCDESTROY,
    WM_PAINT, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_TIMER, WNDCLASSW, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

use crate::bar::ICON_SRC;
use crate::config::BarConfig;
use crate::widgets::komorebi;
use crate::widgets::tasks::{enumerate_ex, window_icon};

const CLASS: PCWSTR = w!("optim_bar_switcher");

/// Posted by the hook proc; handled after the hook has already returned.
const WM_SWITCH_OPEN: u32 = WM_APP + 1;
const WM_SWITCH_STEP: u32 = WM_APP + 2;
const WM_SWITCH_COMMIT: u32 = WM_APP + 3;
const WM_SWITCH_CANCEL: u32 = WM_APP + 4;
const WM_SWITCH_PICK: u32 = WM_APP + 5;

/// Timer id for the 250 ms foreground sample that feeds the MRU.
const MRU_TIMER: usize = 1;

const ROW_H: f32 = 34.0;
const ICON: f32 = 20.0;
const PAD: f32 = 8.0;
const GAP: f32 = 10.0; // icon-to-text gutter
const TAG_W: f32 = 64.0; // reserved for the workspace tag
const NUM_W: f32 = 18.0; // reserved for the row number, left of the icon

/// The digit shown beside a visible slot, and the key that picks it: 1-9,
/// then 0 for the tenth row. Rows past the tenth get no number, because no
/// single keystroke can address them — a two-digit entry would need a
/// timeout, and a laggy Alt+Tab is worse than an unnumbered eleventh row.
fn slot_digit(slot: usize) -> Option<char> {
    match slot {
        0..=8 => char::from_digit(slot as u32 + 1, 10),
        9 => Some('0'),
        _ => None,
    }
}

/// Inverse of [`slot_digit`] for a virtual-key code, main row and numpad.
fn vk_slot(vk: u32) -> Option<usize> {
    match vk {
        0x31..=0x39 => Some((vk - 0x31) as usize), // '1'..'9'
        0x30 => Some(9),                           // '0' -> tenth row
        0x61..=0x69 => Some((vk - 0x61) as usize), // numpad 1..9
        0x60 => Some(9),                           // numpad 0
        _ => None,
    }
}

/// Live only while the user holds Alt after a swallowed Tab. Read by the hook
/// proc on every keystroke, so it stays an atomic rather than a lock.
static ACTIVE: AtomicBool = AtomicBool::new(false);
static OVERLAY: AtomicIsize = AtomicIsize::new(0);
/// Steps queued by the hook but not yet applied, so held-Tab autorepeat can't
/// outrun the message loop. Low 16 bits: forward count. High: backward.
static PENDING: AtomicUsize = AtomicUsize::new(0);
static ENABLED: AtomicBool = AtomicBool::new(false);

struct Style {
    bg: (u32, f32),
    fg: u32,
    dim: u32,
    accent: u32,
    surface: u32,
    font: String,
    font_size: f32,
    width: f32,
    max_rows: usize,
    /// Foreground exes that keep the stock Alt+Tab (fullscreen games).
    bypass: Vec<String>,
}

struct Gfx {
    rt: ID2D1HwndRenderTarget,
    brush: ID2D1SolidColorBrush,
    fmt: IDWriteTextFormat,
    /// Right-aligned, for the workspace tag on off-workspace rows.
    tag_fmt: IDWriteTextFormat,
    /// Aligned with `rows`; None = no extractable icon.
    bitmaps: Vec<Option<ID2D1Bitmap>>,
    px_w: u32,
    px_h: u32,
}

pub struct Row {
    pub hwnd: isize,
    pub title: String,
    /// Set when the window sits on a komorebi workspace that isn't showing:
    /// the workspace's name (drawn as a tag) and the (monitor, workspace)
    /// indices needed to bring it up on commit.
    pub offscreen: Option<(String, usize, usize)>,
}

/// Windows in the order they were last focused, most recent first.
///
/// Z-order cannot stand in for this under a tiling WM. komorebi parks the
/// windows you aren't looking at on hidden workspaces, so the window you just
/// came from is usually *cloaked* a moment later, while the window on your
/// other monitor stays visible forever. Ordering by anything derived from
/// visibility therefore pins that other monitor at row 1 permanently, and
/// single-Alt+Tab stops meaning "back to the last one".
fn mru() -> std::sync::MutexGuard<'static, Vec<isize>> {
    static MRU: OnceLock<std::sync::Mutex<Vec<isize>>> = OnceLock::new();
    MRU.get_or_init(|| std::sync::Mutex::new(Vec::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Record that `hwnd` has the user's attention. Called for our own switches
/// the instant we commit one, and from a 250 ms sample of the foreground so
/// that clicks, komorebi keybinds and anything else are picked up too.
pub fn note_focus(hwnd: isize) {
    if hwnd == 0 {
        return;
    }
    let mut mru = mru();
    if mru.first() == Some(&hwnd) {
        return; // already current; the common case on a timer tick
    }
    mru.retain(|&h| h != hwnd);
    mru.insert(0, hwnd);
    mru.truncate(64);
}

/// Every switchable window on the machine, in the order the overlay lists them.
///
/// Windows komorebi has parked on other workspaces are included — they're open,
/// so they belong in a switcher even though the taskbar (which mirrors the
/// current workspace) leaves them out.
pub fn window_list() -> Vec<Row> {
    let mut rows: Vec<Row> = enumerate_ex(None, true)
        .into_iter()
        .map(|h| Row {
            hwnd: h,
            title: title_of(HWND(h as *mut _)),
            offscreen: komorebi::managed(h)
                .filter(|m| !m.visible)
                .map(|m| (m.ws_name, m.monitor, m.workspace)),
        })
        .filter(|r| !r.title.is_empty())
        .collect();
    let order = mru().clone();
    // Sort is stable, so windows we have never seen focused — everything on a
    // cold start — keep their old behaviour: on-screen first, Z-order within.
    rows.sort_by_key(|r| {
        (
            order.iter().position(|&h| h == r.hwnd).unwrap_or(usize::MAX),
            r.offscreen.is_some(),
        )
    });
    rows
}

struct Switcher {
    style: Style,
    rows: Vec<Row>,
    index: usize,
    scroll: usize,
    scale: f32,
    /// Survives across opens — icon extraction is the expensive part.
    icons: HashMap<isize, Option<std::sync::Arc<Vec<u8>>>>,
    dwrite: IDWriteFactory,
    gfx: Option<Gfx>,
}

fn col(v: u32, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: ((v >> 16) & 0xFF) as f32 / 255.0,
        g: ((v >> 8) & 0xFF) as f32 / 255.0,
        b: (v & 0xFF) as f32 / 255.0,
        a,
    }
}

/// Selection after `delta` Tab steps through `n` rows, wrapping both ways.
///
/// Kept free of `Switcher` (and therefore of D2D) so the arithmetic can be
/// tested directly: the wrap *is* the Alt+Tab feel — one Tab means "the window
/// I was just in", and Shift+Tab off the top belongs at the bottom.
fn stepped(index: usize, delta: i64, n: usize) -> usize {
    if n == 0 {
        return 0;
    }
    (index as i64 + delta).rem_euclid(n as i64) as usize
}

/// Scroll offset that keeps `index` on screen in a list `visible` rows tall.
fn scrolled_to(index: usize, scroll: usize, visible: usize) -> usize {
    if visible == 0 {
        return 0;
    }
    if index < scroll {
        index // stepped off the top
    } else if index >= scroll + visible {
        index + 1 - visible // stepped off the bottom
    } else {
        scroll
    }
}

fn title_of(hwnd: HWND) -> String {
    unsafe {
        let len = GetWindowTextLengthW(hwnd);
        if len <= 0 {
            return String::new();
        }
        let mut buf = vec![0u16; len as usize + 1];
        let n = GetWindowTextW(hwnd, &mut buf);
        String::from_utf16_lossy(&buf[..n.max(0) as usize])
    }
}

/// Lowercased exe file name of the window's process, for the bypass list
/// and for naming tray-icon owners in `--list-tray`.
pub(crate) fn exe_of(hwnd: HWND) -> String {
    unsafe {
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 {
            return String::new();
        }
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return String::new();
        };
        let mut buf = [0u16; 260];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(
            h,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
        .is_ok();
        let _ = windows::Win32::Foundation::CloseHandle(h);
        if !ok {
            return String::new();
        }
        String::from_utf16_lossy(&buf[..len as usize])
            .rsplit('\\')
            .next()
            .unwrap_or_default()
            .to_lowercase()
    }
}

/// Blocks until `hwnd` is no longer cloaked, or `timeout` runs out.
///
/// komorebi handles the workspace switch asynchronously over its own pipe, and
/// a still-cloaked window ignores `SetForegroundWindow` — activating too early
/// leaves the old window focused and looks like the switcher did nothing.
fn wait_uncloaked(hwnd: HWND, timeout: Duration) {
    let start = Instant::now();
    loop {
        let mut cloaked = 0u32;
        unsafe {
            let _ = DwmGetWindowAttribute(
                hwnd,
                DWMWA_CLOAKED,
                &mut cloaked as *mut _ as _,
                std::mem::size_of::<u32>() as u32,
            );
        }
        if cloaked == 0 || start.elapsed() >= timeout {
            return;
        }
        std::thread::sleep(Duration::from_millis(15));
    }
}

/// Raises `hwnd` and gives it focus.
///
/// Two separate things fight us here.
///
/// The foreground *lock* only lets a process call `SetForegroundWindow` if it
/// already owns the foreground or supplied the last input event. We're neither
/// — we swallowed the keystroke and the overlay never activates — so we borrow
/// the outgoing foreground thread's input queue to get past it.
///
/// And foreground is not focus. Focus is per input queue, so making a window
/// foreground does not move the keyboard focus inside the thread that owns it;
/// attaching only to the *outgoing* thread raises the window and leaves the
/// caret where it was. Borrow the target's queue too, and set focus in it.
fn activate(hwnd: HWND) {
    unsafe {
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        let cur = GetCurrentThreadId();
        let fg = GetForegroundWindow();
        let from = if fg.is_invalid() {
            0
        } else {
            GetWindowThreadProcessId(fg, None)
        };
        let to = GetWindowThreadProcessId(hwnd, None);

        let a_from = from != 0 && from != cur && AttachThreadInput(cur, from, true).as_bool();
        let a_to =
            to != 0 && to != cur && to != from && AttachThreadInput(cur, to, true).as_bool();

        let _ = SetForegroundWindow(hwnd);
        let _ = SetWindowPos(hwnd, Some(HWND_TOP), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE);
        // Only meaningful while attached to the target's queue — which is
        // exactly what the old version was missing.
        let _ = SetActiveWindow(hwnd);
        let _ = SetFocus(Some(hwnd));

        if a_to {
            let _ = AttachThreadInput(cur, to, false);
        }
        if a_from {
            let _ = AttachThreadInput(cur, from, false);
        }
    }
}

/// `activate`, re-asserted until the window actually holds the foreground.
///
/// komorebi focuses the incoming workspace's own last-focused window after a
/// workspace switch, on its own schedule. A single activate races that and
/// often loses, which reads as the switcher taking you to the right workspace
/// and then focusing the wrong window. Returns as soon as it has won, so the
/// common case costs one call.
fn activate_until(hwnd: HWND, budget: Duration) {
    let start = Instant::now();
    loop {
        activate(hwnd);
        if unsafe { GetForegroundWindow() } == hwnd || start.elapsed() >= budget {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Installs the hook and overlay on a dedicated thread. Returns immediately.
pub fn install(cfg: &BarConfig) {
    let values = &cfg.values;
    if values.get_or("switcher", "enabled", "true") == "false" {
        return;
    }
    let style = Style {
        bg: cfg.bg,
        fg: cfg.fg,
        dim: cfg.dim,
        accent: cfg.accent,
        surface: cfg.surface,
        font: values.get_or("switcher", "font", &cfg.font),
        font_size: values
            .get_f32("switcher", "font_size", cfg.font_size + 1.0)
            .clamp(8.0, 28.0),
        width: values.get_f32("switcher", "width", 620.0).clamp(240.0, 1400.0),
        max_rows: values.get_u64("switcher", "max_rows", 14).clamp(3, 40) as usize,
        bypass: values
            .get_list("switcher", "bypass")
            .iter()
            .map(|s| s.to_lowercase())
            .collect(),
    };
    ENABLED.store(true, Ordering::Relaxed);
    std::thread::spawn(move || run(style));
}

fn run(style: Style) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let Ok(hinstance) = GetModuleHandleW(None) else { return };
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: CLASS,
            ..Default::default()
        };
        RegisterClassW(&wc);

        let Ok(dwrite) = DWriteCreateFactory::<IDWriteFactory>(DWRITE_FACTORY_TYPE_SHARED) else {
            return;
        };
        let state = Box::new(Switcher {
            style,
            rows: Vec::new(),
            index: 0,
            scroll: 0,
            scale: 1.0,
            icons: HashMap::new(),
            dwrite,
            gfx: None,
        });
        // Created once and reused: building a window per Alt+Tab would put
        // window creation on the critical path of every switch.
        let Ok(hwnd) = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
            CLASS,
            w!("optim-bar switcher"),
            WS_POPUP,
            0,
            0,
            10,
            10,
            None,
            None,
            Some(hinstance.into()),
            Some(Box::into_raw(state) as *const c_void),
        ) else {
            return;
        };
        OVERLAY.store(hwnd.0 as isize, Ordering::Relaxed);
        // Sample the foreground so focus changes we didn't cause — clicks,
        // komorebi keybinds, anything — still reach the MRU. Deliberately on
        // the switcher's own window rather than the tasks widget's tick: the
        // MRU must not go stale just because someone drops `tasks` from their
        // widget list.
        windows::Win32::UI::WindowsAndMessaging::SetTimer(Some(hwnd), MRU_TIMER, 250, None);

        let Ok(hook) = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), None, 0) else {
            return;
        };

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            DispatchMessageW(&msg);
        }
        let _ = UnhookWindowsHookEx(hook);
    }
}

/// Runs on every keystroke system-wide. Everything here is atomics and a
/// `PostMessage` — no allocation, no enumeration, no blocking calls.
unsafe extern "system" fn keyboard_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code != HC_ACTION as i32 || !ENABLED.load(Ordering::Relaxed) {
        return CallNextHookEx(None, code, wparam, lparam);
    }
    let kb = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
    let vk = kb.vkCode;
    let msg = wparam.0 as u32;
    let down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
    let up = msg == WM_KEYUP || msg == WM_SYSKEYUP;
    let overlay = HWND(OVERLAY.load(Ordering::Relaxed) as *mut _);
    let active = ACTIVE.load(Ordering::Relaxed);

    // LLKHF_ALTDOWN (0x20) beats GetAsyncKeyState here: it's already in the
    // event we were handed, no extra call.
    let alt = kb.flags.0 & 0x20 != 0;

    if down && vk == VK_TAB.0 as u32 && alt {
        let back = GetAsyncKeyState(VK_SHIFT.0 as i32) as u16 & 0x8000 != 0;
        if !active {
            if !ACTIVE.swap(true, Ordering::Relaxed) {
                PENDING.store(0, Ordering::Relaxed);
                let _ = PostMessageW(
                    Some(overlay),
                    WM_SWITCH_OPEN,
                    WPARAM(back as usize),
                    LPARAM(0),
                );
            }
        } else {
            // Fold repeats into a counter: the loop may be a frame behind.
            PENDING.fetch_add(if back { 1 << 16 } else { 1 }, Ordering::Relaxed);
            let _ = PostMessageW(Some(overlay), WM_SWITCH_STEP, WPARAM(0), LPARAM(0));
        }
        return LRESULT(1); // swallow: Windows must not also switch
    }

    if active {
        if down && vk == VK_ESCAPE.0 as u32 {
            ACTIVE.store(false, Ordering::Relaxed);
            let _ = PostMessageW(Some(overlay), WM_SWITCH_CANCEL, WPARAM(0), LPARAM(0));
            return LRESULT(1);
        }
        // A digit jumps straight to the row carrying that number. ACTIVE is
        // left alone here and cleared by the handler only if the digit
        // actually addresses a row, so an out-of-range press is a no-op
        // rather than a cycle that ends on the wrong window. Swallowed
        // either way, so the digit never leaks to the app behind us.
        if down {
            if let Some(slot) = vk_slot(vk) {
                let _ = PostMessageW(Some(overlay), WM_SWITCH_PICK, WPARAM(slot), LPARAM(0));
                return LRESULT(1);
            }
        }
        // Alt released: commit. Deliberately *not* swallowed — the foreground
        // app saw Alt go down and would be left with a stuck modifier.
        if up && (vk == VK_MENU.0 as u32 || vk == VK_LMENU.0 as u32 || vk == VK_RMENU.0 as u32) {
            ACTIVE.store(false, Ordering::Relaxed);
            let _ = PostMessageW(Some(overlay), WM_SWITCH_COMMIT, WPARAM(0), LPARAM(0));
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

impl Switcher {
    /// Snapshots the window list. Z-order at open time is the MRU order that
    /// matters; taking it once means held-Tab navigation can't be reshuffled
    /// underfoot by windows raising themselves mid-cycle.
    fn open(&mut self, hwnd: HWND, back: bool) -> bool {
        let fg = unsafe { GetForegroundWindow() };
        if !fg.is_invalid() {
            let exe = exe_of(fg);
            if !exe.is_empty() && self.style.bypass.iter().any(|b| *b == exe) {
                return false;
            }
        }
        self.rows = window_list();
        if self.rows.len() < 2 {
            self.rows.clear();
            return false;
        }
        for r in &self.rows {
            self.icons
                .entry(r.hwnd)
                .or_insert_with(|| window_icon(HWND(r.hwnd as *mut _)).map(std::sync::Arc::new));
        }
        if self.icons.len() > 256 {
            self.icons.retain(|&k, _| unsafe {
                windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(HWND(k as *mut _))).as_bool()
            });
        }
        // Index 0 is the current window, so a single Alt+Tab lands on the
        // previous one — the behaviour everyone's fingers already know.
        self.index = if back { self.rows.len() - 1 } else { 1 };
        self.scroll = 0;
        self.gfx = None; // row count changed; render target must be rebuilt
        self.layout(hwnd);
        true
    }

    fn step(&mut self) {
        let packed = PENDING.swap(0, Ordering::Relaxed);
        let fwd = (packed & 0xFFFF) as i64;
        let back = (packed >> 16) as i64;
        self.index = stepped(self.index, fwd - back, self.rows.len());
        self.ensure_visible();
    }

    fn visible_rows(&self) -> usize {
        self.rows.len().min(self.style.max_rows)
    }

    fn ensure_visible(&mut self) {
        self.scroll = scrolled_to(self.index, self.scroll, self.visible_rows());
    }

    /// Moves the selection to the row labelled `slot + 1`. Numbering follows
    /// the visible window rather than the full list, so the digit drawn next
    /// to a row is always the digit that picks it, even once it has scrolled.
    /// False when no row is showing there.
    fn select_slot(&mut self, slot: usize) -> bool {
        let i = self.scroll + slot;
        if slot >= self.visible_rows() || i >= self.rows.len() {
            return false;
        }
        self.index = i;
        true
    }

    /// Centres the overlay on the monitor holding the cursor.
    fn layout(&mut self, hwnd: HWND) {
        unsafe {
            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            let mon = if pt.x == 0 && pt.y == 0 {
                MonitorFromWindow(GetForegroundWindow(), MONITOR_DEFAULTTOPRIMARY)
            } else {
                MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST)
            };
            let mut mi = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            let _ = GetMonitorInfoW(mon, &mut mi);
            let m = mi.rcMonitor;

            self.scale = GetDpiForWindow(hwnd).max(96) as f32 / 96.0;
            let (w, h) = self.px_size();
            let x = m.left + ((m.right - m.left) - w as i32) / 2;
            let y = m.top + ((m.bottom - m.top) - h as i32) / 2;
            let _ = SetWindowPos(hwnd, None, x, y, w as i32, h as i32, SWP_NOZORDER_KEEP);
        }
    }

    fn px_size(&self) -> (u32, u32) {
        let w = self.style.width * self.scale;
        let h = (2.0 * PAD + self.visible_rows() as f32 * ROW_H) * self.scale;
        (w as u32, h as u32)
    }

    fn build_gfx(&self, hwnd: HWND) -> Option<Gfx> {
        unsafe {
            let factory: ID2D1Factory =
                D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None).ok()?;
            let (px_w, px_h) = self.px_size();
            let rt = factory
                .CreateHwndRenderTarget(
                    &D2D1_RENDER_TARGET_PROPERTIES {
                        // Software target: same call the bar makes. A hardware
                        // target here would spin up a D3D device per switch.
                        r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
                        dpiX: 96.0,
                        dpiY: 96.0,
                        ..Default::default()
                    },
                    &D2D1_HWND_RENDER_TARGET_PROPERTIES {
                        hwnd,
                        pixelSize: D2D_SIZE_U {
                            width: px_w,
                            height: px_h,
                        },
                        presentOptions: D2D1_PRESENT_OPTIONS_NONE,
                    },
                )
                .ok()?;
            let brush = rt.CreateSolidColorBrush(&col(self.style.fg, 1.0), None).ok()?;

            let font: Vec<u16> = self
                .style
                .font
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            let fmt = self
                .dwrite
                .CreateTextFormat(
                    PCWSTR(font.as_ptr()),
                    None,
                    DWRITE_FONT_WEIGHT_NORMAL,
                    DWRITE_FONT_STYLE_NORMAL,
                    DWRITE_FONT_STRETCH_NORMAL,
                    self.style.font_size * self.scale,
                    w!(""),
                )
                .ok()?;
            let _ = fmt.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER);
            let _ = fmt.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP);
            // Window titles are long and the interesting part is the front,
            // so clip the tail with an ellipsis rather than letting it run.
            if let Ok(sign) = self.dwrite.CreateEllipsisTrimmingSign(&fmt) {
                let trim = DWRITE_TRIMMING {
                    granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER,
                    ..Default::default()
                };
                let _ = fmt.SetTrimming(&trim, Some(&sign as &IDWriteInlineObject));
            }

            let tag_fmt = self
                .dwrite
                .CreateTextFormat(
                    PCWSTR(font.as_ptr()),
                    None,
                    DWRITE_FONT_WEIGHT_NORMAL,
                    DWRITE_FONT_STYLE_NORMAL,
                    DWRITE_FONT_STRETCH_NORMAL,
                    (self.style.font_size - 1.0).max(8.0) * self.scale,
                    w!(""),
                )
                .ok()?;
            let _ = tag_fmt.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER);
            let _ = tag_fmt.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP);
            let _ = tag_fmt.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_TRAILING);

            let bitmaps = self
                .rows
                .iter()
                .map(|r| {
                    let px = self.icons.get(&r.hwnd)?.as_ref()?;
                    rt.CreateBitmap(
                        D2D_SIZE_U {
                            width: ICON_SRC as u32,
                            height: ICON_SRC as u32,
                        },
                        Some(px.as_ptr() as _),
                        (ICON_SRC * 4) as u32,
                        &D2D1_BITMAP_PROPERTIES {
                            pixelFormat: D2D1_PIXEL_FORMAT {
                                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                            },
                            dpiX: 96.0,
                            dpiY: 96.0,
                        },
                    )
                    .ok()
                })
                .collect();
            Some(Gfx {
                rt,
                brush,
                fmt,
                tag_fmt,
                bitmaps,
                px_w,
                px_h,
            })
        }
    }

    fn render(&mut self, hwnd: HWND) {
        if self.rows.is_empty() {
            return;
        }
        let (want_w, want_h) = self.px_size();
        if self
            .gfx
            .as_ref()
            .is_some_and(|g| g.px_w != want_w || g.px_h != want_h)
        {
            self.gfx = None;
        }
        if self.gfx.is_none() {
            self.gfx = self.build_gfx(hwnd);
        }
        let Some(gfx) = &self.gfx else { return };
        unsafe {
            let s = |v: f32| v * self.scale;
            let w = want_w as f32;
            let h = want_h as f32;
            gfx.rt.BeginDraw();
            gfx.rt.Clear(Some(&col(self.style.bg.0, 1.0)));

            gfx.brush.SetColor(&col(self.style.surface, 1.0));
            gfx.rt.DrawRoundedRectangle(
                &D2D1_ROUNDED_RECT {
                    rect: D2D_RECT_F {
                        left: 0.5,
                        top: 0.5,
                        right: w - 0.5,
                        bottom: h - 0.5,
                    },
                    radiusX: 6.0,
                    radiusY: 6.0,
                },
                &gfx.brush,
                1.0,
                None,
            );

            let vis = self.visible_rows();
            for slot in 0..vis {
                let i = self.scroll + slot;
                let Some(row) = self.rows.get(i) else { break };
                let top = s(PAD) + slot as f32 * s(ROW_H);
                let bottom = top + s(ROW_H);
                let selected = i == self.index;

                if selected {
                    gfx.brush.SetColor(&col(self.style.surface, 1.0));
                    gfx.rt.FillRoundedRectangle(
                        &D2D1_ROUNDED_RECT {
                            rect: D2D_RECT_F {
                                left: s(PAD / 2.0),
                                top,
                                right: w - s(PAD / 2.0),
                                bottom,
                            },
                            radiusX: 4.0,
                            radiusY: 4.0,
                        },
                        &gfx.brush,
                    );
                    // Accent rail on the left edge of the selection.
                    gfx.brush.SetColor(&col(self.style.accent, 1.0));
                    gfx.rt.FillRoundedRectangle(
                        &D2D1_ROUNDED_RECT {
                            rect: D2D_RECT_F {
                                left: s(PAD / 2.0),
                                top: top + s(6.0),
                                right: s(PAD / 2.0) + s(3.0),
                                bottom: bottom - s(6.0),
                            },
                            radiusX: 1.5,
                            radiusY: 1.5,
                        },
                        &gfx.brush,
                    );
                }

                let num_l = s(PAD) + s(GAP / 2.0);
                let icon_l = num_l + s(NUM_W);
                let cy = (top + bottom) / 2.0;

                // Right-aligned against the icon so the digits form a column
                // regardless of width. tag_fmt is already the right-aligned,
                // slightly-smaller format used by the workspace tag.
                if let Some(d) = slot_digit(slot) {
                    gfx.brush.SetColor(&col(
                        if selected {
                            self.style.accent
                        } else {
                            self.style.dim
                        },
                        if selected { 1.0 } else { 0.7 },
                    ));
                    let d16 = [d as u16];
                    gfx.rt.DrawText(
                        &d16,
                        &gfx.tag_fmt,
                        &D2D_RECT_F {
                            left: num_l,
                            top,
                            right: icon_l - s(GAP / 2.0),
                            bottom,
                        },
                        &gfx.brush,
                        Default::default(),
                        DWRITE_MEASURING_MODE_NATURAL,
                    );
                }

                if let Some(Some(b)) = gfx.bitmaps.get(i) {
                    gfx.rt.DrawBitmap(
                        b,
                        Some(&D2D_RECT_F {
                            left: icon_l,
                            top: cy - s(ICON) / 2.0,
                            right: icon_l + s(ICON),
                            bottom: cy + s(ICON) / 2.0,
                        }),
                        1.0,
                        D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                        None,
                    );
                }

                gfx.brush.SetColor(&col(
                    if selected {
                        self.style.fg
                    } else {
                        self.style.dim
                    },
                    1.0,
                ));
                // Rows on another workspace get a tag, so a title that isn't
                // where you expect it explains itself instead of looking stale.
                let tag_w = if row.offscreen.is_some() { s(TAG_W) } else { 0.0 };
                let text_r = w - s(PAD) - s(GAP / 2.0);
                let t16: Vec<u16> = row.title.encode_utf16().collect();
                gfx.rt.DrawText(
                    &t16,
                    &gfx.fmt,
                    &D2D_RECT_F {
                        left: icon_l + s(ICON) + s(GAP),
                        top,
                        right: text_r - tag_w,
                        bottom,
                    },
                    &gfx.brush,
                    Default::default(),
                    DWRITE_MEASURING_MODE_NATURAL,
                );
                if let Some((name, _, _)) = &row.offscreen {
                    gfx.brush.SetColor(&col(self.style.dim, if selected { 0.9 } else { 0.6 }));
                    let g16: Vec<u16> = name.encode_utf16().collect();
                    gfx.rt.DrawText(
                        &g16,
                        &gfx.tag_fmt,
                        &D2D_RECT_F {
                            left: text_r - tag_w,
                            top,
                            right: text_r,
                            bottom,
                        },
                        &gfx.brush,
                        Default::default(),
                        DWRITE_MEASURING_MODE_NATURAL,
                    );
                }
            }
            let _ = gfx.rt.EndDraw(None, None);
        }
    }

    fn commit(&mut self) {
        if let Some(row) = self.rows.get(self.index) {
            let hwnd = HWND(row.hwnd as *mut _);
            if let Some((_, mon, ws)) = row.offscreen.as_ref().map(|(_, m, w)| ((), *m, *w)) {
                // Cloaked windows can't take the foreground, so bring the
                // workspace up first and wait for komorebi to uncloak it.
                // This runs on the switcher thread, never the hook thread.
                komorebi::focus_workspace(mon, ws);
                wait_uncloaked(hwnd, Duration::from_millis(700));
                // komorebi will focus this workspace's own last-focused window
                // once the switch settles, so hold the foreground against it.
                activate_until(hwnd, Duration::from_millis(400));
            } else {
                activate(hwnd);
            }
            // Authoritative, and immediate: our own switch is the one case
            // where waiting for the next foreground sample could let a fast
            // second Alt+Tab read a stale order.
            note_focus(row.hwnd);
        }
        self.rows.clear();
    }
}

/// `SetWindowPos` flags for a move+resize that leaves the Z-order alone.
const SWP_NOZORDER_KEEP: windows::Win32::UI::WindowsAndMessaging::SET_WINDOW_POS_FLAGS =
    windows::Win32::UI::WindowsAndMessaging::SWP_NOZORDER;

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        if msg == WM_NCCREATE {
            let cs = lparam.0 as *const CREATESTRUCTW;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, (*cs).lpCreateParams as isize);
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Switcher;
        if ptr.is_null() {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        match msg {
            WM_SWITCH_OPEN => {
                if (*ptr).open(hwnd, wparam.0 != 0) {
                    let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                    (*ptr).render(hwnd);
                } else {
                    // Nothing to switch between, or a bypassed fullscreen app.
                    ACTIVE.store(false, Ordering::Relaxed);
                }
                LRESULT(0)
            }
            WM_SWITCH_STEP => {
                (*ptr).step();
                (*ptr).render(hwnd);
                LRESULT(0)
            }
            WM_TIMER if wparam.0 == MRU_TIMER => {
                // Skip while the overlay is up: the list is a snapshot taken at
                // open time, and re-ordering underneath a held Tab would move
                // rows out from under the selection mid-cycle.
                if !ACTIVE.load(Ordering::Relaxed) {
                    note_focus(GetForegroundWindow().0 as isize);
                }
                LRESULT(0)
            }
            WM_SWITCH_COMMIT => {
                let _ = ShowWindow(hwnd, SW_HIDE);
                (*ptr).commit();
                LRESULT(0)
            }
            WM_SWITCH_PICK => {
                if (*ptr).select_slot(wparam.0) {
                    ACTIVE.store(false, Ordering::Relaxed);
                    let _ = ShowWindow(hwnd, SW_HIDE);
                    (*ptr).commit();
                }
                LRESULT(0)
            }
            // The switcher is keyboard-only, on purpose. Hover selection used to
            // fight the keyboard: the overlay pops up under wherever the cursor
            // happens to be resting, and the WM_MOUSEMOVE that arrives when the
            // window appears (and on every re-render under a still cursor) kept
            // yanking the selection back to the hovered row, so Tab looked dead.
            // Mouse messages are swallowed here rather than handled.
            WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
            WM_MOUSEMOVE | WM_LBUTTONUP | WM_MOUSEWHEEL => LRESULT(0),
            WM_SWITCH_CANCEL => {
                let _ = ShowWindow(hwnd, SW_HIDE);
                (*ptr).rows.clear();
                LRESULT(0)
            }
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                let _ = BeginPaint(hwnd, &mut ps);
                (*ptr).render(hwnd);
                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_NCDESTROY => {
                OVERLAY.store(0, Ordering::Relaxed);
                drop(Box::from_raw(ptr));
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{scrolled_to, slot_digit, stepped, vk_slot};

    /// The label drawn on a row and the key that picks it must never drift
    /// apart — ASCII digits double as their own main-row virtual-key codes.
    #[test]
    fn every_drawn_digit_picks_the_row_it_labels() {
        for slot in 0..10 {
            let d = slot_digit(slot).expect("the first ten slots are numbered");
            assert_eq!(vk_slot(d as u32), Some(slot), "digit {d} -> slot {slot}");
        }
        assert_eq!(slot_digit(9), Some('0'), "the tenth row is 0, not 10");
        assert_eq!(slot_digit(10), None, "past ten there is no single keystroke");
    }

    #[test]
    fn numpad_picks_the_same_rows() {
        assert_eq!(vk_slot(0x61), Some(0)); // numpad 1 -> first row
        assert_eq!(vk_slot(0x60), Some(9)); // numpad 0 -> tenth row
        assert_eq!(vk_slot(0x41), None); // 'A' addresses nothing
    }

    #[test]
    fn one_tab_lands_on_the_previous_window() {
        assert_eq!(stepped(0, 1, 5), 1);
    }

    #[test]
    fn stepping_wraps_both_ways() {
        assert_eq!(stepped(4, 1, 5), 0); // off the bottom
        assert_eq!(stepped(0, -1, 5), 4); // Shift+Tab off the top
        assert_eq!(stepped(0, 7, 5), 2); // several steps queued at once
        assert_eq!(stepped(3, -7, 5), 1);
    }

    /// The hook can queue steps before the list exists.
    #[test]
    fn an_empty_list_never_indexes_anything() {
        assert_eq!(stepped(0, 3, 0), 0);
        assert_eq!(scrolled_to(0, 0, 0), 0);
    }

    #[test]
    fn scrolling_follows_the_selection_off_either_edge() {
        // Showing rows 0..=3 of many: stepping to 4 pulls the window down one.
        assert_eq!(scrolled_to(4, 0, 4), 1);
        // Back up to row 0 from a scrolled list.
        assert_eq!(scrolled_to(0, 4, 4), 0);
        // Wrapping from the last row to the first shows the top of the list.
        assert_eq!(scrolled_to(0, 8, 4), 0);
    }

    #[test]
    fn a_visible_selection_doesnt_move_the_list() {
        assert_eq!(scrolled_to(5, 4, 4), 4);
        assert_eq!(scrolled_to(4, 4, 4), 4);
        assert_eq!(scrolled_to(7, 4, 4), 4);
    }
}
