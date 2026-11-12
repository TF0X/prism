# Prism

> A tiny, glass, tabbed web browser for Windows — built in Rust on `tao` + `wry`.

[![CI](https://github.com/TF0X/prism/actions/workflows/ci.yml/badge.svg)](https://github.com/TF0X/prism/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/TF0X/prism?include_prereleases&sort=semver)](https://github.com/TF0X/prism/releases)
![Platform](https://img.shields.io/badge/platform-Windows%2011-0a7bff)
![Rust](https://img.shields.io/badge/rust-stable-orange)
![Status](https://img.shields.io/badge/status-beta-yellow)

Prism is a frameless browser whose pages — and its own chrome — frost the desktop
behind them, so the whole window reads as glass. It ships uBlock Origin Lite baked
in, suspends idle tabs to claw back memory, and weighs about **3.9 MB** on disk
because it borrows the system WebView2 runtime instead of bundling a browser.

> **Beta.** Windows 11 only, and rough edges remain (see [Known limitations](#known-limitations)).

## Features

- **Glass everywhere** — a real `ACCENT_ENABLE_ACRYLICBLURBEHIND` backdrop with a
  light, tunable tint (not Windows 11's forced-dark acrylic), plus an injected
  engine that makes page chrome — headers, toolbars, nav rails — translucent so
  the blur shows through. Toggle per page with the ◐ button.
- **Live tabs** — each tab is its own borderless window owned by the main one, the
  one arrangement where a transparent webview composites with the acrylic *and*
  tabs stay live instead of reloading on switch.
- **uBlock Origin Lite, bundled** — downloaded from GitHub releases on first run,
  auto-updated on later launches, registered through WebView2's native extension
  support. (MV3; popup UI is unreachable but default blocking works.)
- **Idle-tab suspend** — background tabs freeze after a minute via WebView2
  `TrySuspend`, which keeps scroll position and typed text.
- **Keyboard** — `Ctrl+T` new tab, `Ctrl+W` close, `Ctrl+L` focus address bar,
  `Ctrl+R` reload, intercepted at the WebView2 controller so they work even with
  focus inside a page.
- **Tiny** — ~3.9 MB exe; the shell process costs single-digit MB of private memory.

## Install

Grab the latest `prism.exe` from the [Releases](https://github.com/TF0X/prism/releases)
page and run it. No installer, no admin. It needs the **WebView2 runtime**, which
ships with Windows 11 and current Windows 10.

Per-user install with a desktop shortcut:

```powershell
$dir = "$env:LOCALAPPDATA\Programs\Prism"
New-Item -ItemType Directory -Force $dir | Out-Null
Copy-Item .\prism.exe $dir
$s = (New-Object -ComObject WScript.Shell).CreateShortcut("$([Environment]::GetFolderPath('Desktop'))\Prism.lnk")
$s.TargetPath = "$dir\prism.exe"; $s.Save()
```

## Build from source

Requires the Rust toolchain (MSVC) and the Windows SDK / VS Build Tools.

```powershell
git clone https://github.com/TF0X/prism
cd prism
cargo build --release
# -> target/release/prism.exe
```

## Usage

- Type a URL or a search into the address bar and press Enter.
- `+` or `Ctrl+T` opens a tab; the left rail lists them.
- The **◐** button cycles page glass: Off → Frosted → Clear.
- Set `PRISM_NOBLUR=1` to drop the blur-behind entirely, or `PRISM_DIAG=1` to log
  the glass engine's decisions to `%LOCALAPPDATA%\prism\diag.log`.

## How it works

Prism is one frameless `tao` window. The content webview attaches directly to it —
the only way a transparent WebView2 composites with the DWM acrylic — and the
address bar lives in a second, owned, transparent window pinned to the top. Each
tab is another owned window layered over the content area, shown only when active.
A script injected at document-start walks the page, clears opaque "chrome" surfaces
(full-width bands, short bars, edge-hugging nav rails) inline so the backdrop reads
through, and leaves genuine content alone.

## CI / CD

GitHub Actions on `windows-latest`:

- **CI** (`.github/workflows/ci.yml`) — runs on every push and PR: `cargo fmt
  --check`, `cargo clippy`, and `cargo build --release`.
- **Release** (`.github/workflows/release.yml`) — runs on any `v*` tag: builds
  the release binary and attaches `prism.exe` to the GitHub Release.

Cut a release by tagging:

```bash
git tag v0.1.0-beta
git push origin v0.1.0-beta
```

## Known limitations

- **Windows 11 only.** The blur backdrop and transparency paths are Windows-specific.
- The acrylic-blur accent can smear slightly while *resizing* — inherent to that mode.
- Page glass is heuristic; an unusually structured site may leave a stray opaque
  strip. `PRISM_DIAG=1` surfaces what was missed.
- uBlock Origin Lite's per-site popup UI is unreachable (WebView2 has no toolbar to
  host it); default network/cosmetic blocking still applies.

## License

MIT
