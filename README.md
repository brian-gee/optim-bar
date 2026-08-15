# optim-bar

Minimal, fast, native status bar for Windows — companion to
[optim](https://github.com/brian-gee/optim). No runtime, no webview, no GPU
context — a single small exe rendering with software Direct2D.

- **komorebi workspaces** — live via pipe subscription, click to switch
- **Window taskbar** — this monitor's windows as icons, click to focus/minimize
- **Clock** — right-click flips to the full date
- **Volume** — scroll adjusts, right-click mutes
- **CPU % / RAM %** — straight from Win32
- **GPU temp** — native NVML from your NVIDIA driver, no helper apps
- **Any sensor from LibreHardwareMonitor** — optional; the widget hides itself
  when LHM isn't running (CPU die temp needs LHM's kernel driver)
- **Script widgets** — run any command line on an interval, show its stdout,
  bind commands to left/middle/right click. YASB-style `<span>` color output
  is understood, so YASB custom-widget scripts work unchanged.
- **Alt+Tab switcher** — replaces Windows' thumbnail switcher with a vertical
  list of window *titles*. Keyboard only, deliberately: the overlay opens under
  wherever the cursor happens to be resting, and hover selection kept dragging
  the highlight back to that row mid-switch, so mouse messages are ignored.
  Windows reserves Alt+Tab, so this is a
  `WH_KEYBOARD_LL` hook on its own thread; it changes no registry keys, so
  quitting the bar hands Alt+Tab straight back to Windows. `[switcher]`
  section, `enabled = false` to turn it off. It always lists every open
  window — nothing is ever filtered out of it. Rows are numbered: press
  `1`-`9` (or `0` for the tenth) while it's open to jump straight to that
  window. Past the tenth row there's no number, because no single keystroke
  addresses it and a two-digit entry would need a timeout on Alt+Tab.
- **System tray host** — the bar *is* the tray: it takes over the
  Shell_NotifyIcon protocol, hides explorer's taskbar, and renders every
  tray icon with click/right-click forwarding. Icons live behind a chevron
  in a grid flyout by default (`flyout_cols` / `flyout_icon` / `flyout_cell`
  / `flyout_pad` under `[widget.systray]`, or `collapsed = false` to render
  them inline). Icons that register with `NIS_HIDDEN` — Windows 11's
  "overflow", which is how Proton VPN and other H.NotifyIcon apps announce
  themselves — appear in the flyout but never inline, since the flyout *is*
  the overflow. `--restore-tray` hands everything back to explorer instantly.
- **Multi-monitor** — one bar per monitor (`monitors = all|primary`),
  rebuilt automatically when displays attach/detach, per-monitor DPI aware,
  per-monitor widget overrides via `[left.1]`-style sections
- Registers as an AppBar: maximized windows stop at the bar's edge. Explorer
  applies appbar reservations to the **primary** monitor only — a secondary
  bar's `ABM_NEW`/`QUERYPOS`/`SETPOS` all return success and the work area
  never moves — so every other monitor is reserved with `SPI_SETWORKAREA`
  instead, re-applied by the 250 ms self-heal whenever the shell erases it.
  Create `%LOCALAPPDATA%\optim-bar\appbar-probe.log` (empty) to log the
  negotiation if a monitor ever refuses; delete it to switch the log off.
- Hot-reloading TOML config; dark Catppuccin Mocha defaults; no light mode

## Config

`%APPDATA%\optim-bar\config.toml` — created with commented defaults on first
run. Tables: `[bar]` (height, position top/bottom, colors, font, reserve),
`[left]`/`[center]`/`[right]` (widget lists), `[widget.<name>]` (per-widget
options; `type =` selects the implementation so several widgets can share
one, e.g. multiple `exec` script widgets), `[switcher]` (Alt+Tab list).

Colors are quoted hex (`surface = "313244"`) — unquoted, an all-digit color
is a perfectly valid decimal integer and would render the wrong shade with
no error anywhere. Command lines with Windows paths need TOML's single-quoted
literal strings, since `C:\Users` inside `"…"` is an invalid `\U` escape.

```
optim-bar.exe --check-config
```

prints the values the bar will actually use, and reports a syntax error with
its line and column. Worth running after an edit: a value that parses but
isn't understood (`position = "topp"`) silently falls back to its default.

Upgrading from a pre-0.5.4 `config.ini`? The first run converts it to
`config.toml` automatically, comments and ordering intact, and keeps the
original as `config.ini.bak`.

Any widget can set `min_width` to give each of its segments a minimum
clickable cell, with the content centered inside. Single-glyph segments like
workspace numbers measure ~8 px otherwise, which is a poor mouse target;
widening the cell fixes that without touching the font size.

## Build

```
cargo build --release
```

Requires the MSVC toolchain; the only dependency is the official
[`windows`](https://crates.io/crates/windows) crate.

## Autostart

```
optim-bar.exe --install-autostart
optim-bar.exe --uninstall-autostart
```

## Not included (yet)

Calendar and per-app mixer popups (right-click the clock for the date;
volume click opens sndvol), tray icon overflow/pinning (all icons show flat).

## If the taskbar ever seems gone

optim-bar hides explorer's taskbar while hosting the tray. If the bar dies
ungracefully, run `optim-bar.exe --restore-tray` (or just start the bar
again) — it un-hides explorer's taskbar and broadcasts TaskbarCreated.

## If one tray icon is missing

```
optim-bar.exe --list-tray     # what the running host actually holds
optim-bar.exe --refresh-tray  # ask every app to register again
```

`--list-tray` messages the *running* bar and prints its icon table — owning
exe, window class, whether an icon bitmap came through, and whether the app
marked it hidden. The table lives in that process's memory, so this is the
only way to tell "registered with the wrong tray" apart from "this app never
had an icon", which otherwise look identical.

Only one Shell_TrayWnd receives a given `Shell_NotifyIcon` call, and there
are brief windows where that isn't ours: right after explorer restarts, and
while an appbar message is being relayed (which requires handing explorer
the top slot on purpose). An icon registered during one of those goes to
explorer's hidden tray and nothing ever asks for it again. The 2 s timer
now notices the top slot was lost and re-broadcasts, throttled to once per
15 s, so this should self-heal; the flag is the manual version.

One case it cannot fix: optim-bar runs unelevated, and a broadcast from a
medium-integrity process never reaches an elevated app's windows. Elevated
apps register fine — high-to-medium sends are allowed — but if one lands on
the wrong tray, only restarting optim-bar recovers it.
