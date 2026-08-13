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
- Registers as an AppBar: maximized windows stop at the bar's edge
- Hot-reloading INI config; dark Catppuccin Mocha defaults; no light mode

## Config

`%APPDATA%\optim-bar\config.ini` — created with commented defaults on first
run. Sections: `[bar]` (height, position top/bottom, colors, font, reserve),
`[left]`/`[center]`/`[right]` (widget lists), `[widget.<name>]` (per-widget
options; `type =` selects the implementation so several widgets can share
one, e.g. multiple `exec` script widgets).

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

System-tray hosting (the Win11 taskbar keeps that job for now), calendar and
per-app mixer popups (sndvol opens instead), multi-monitor bars.
