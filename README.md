# Winri

Winri is an opinionated experimental window tiling manager for Windows that arranges your existing normal Win32 GUI application windows in a horizontally scrollable strip. It aims to give you keyboard-centric window management including positioning and sizing.

All winri actions are based on the Windows key, leaving other modifier keys free for application use. All native Windows shortcuts based on the Windows key are disabled. (Other modifiers can be used for winri actions but only when the Windows key is also used. For example, Ctrl+Win+Left is valid).

Winri is inspired by [niri](https://github.com/YaLTeR/niri), a scrollable-tiling wayland compositor on linux.

Winri name is derived from "Win" (Windows) + "ri" (from "niri").

Only Windows 11 x64 is supported and tested right now. I do not plan to support Windows 10. I'm open to PR to support arm64 machines. PR to fix Windows 10 compatibility are welcome but must be tested on Windows 11 to prevent regression.

If winri prevents you from doing what you want in any way, open an issue to discuss about a solution to implement.

> Status: Early prototype. Expect breaking changes until 1.0.0

> This is the `harveymoon/winri` fork. Releases live at https://github.com/harveymoon/winri/releases. Upstream is [`sub07/winri`](https://github.com/sub07/winri); this fork accumulates fixes and features not yet upstreamed (see "Fork changes" below).

![winri-demo](https://github.com/user-attachments/assets/db90ad36-6ed0-4278-acad-ec3d833b5fe9)

## Features

- Horizontal scroll tiler with dynamic window widths
- Keyboard-centric navigation
- One‑key fullscreen / half‑screen sizing
- Overview mode with live thumbnails + right-click menu (close / force-redraw)
- Multi-monitor: dedicated tiling monitor, drag-out to float on other monitors, secondary-monitor overflow clipping
- System tray icon + per-app exemptions
- Local HTTP / Server-Sent-Events control API for Stream Deck, AHK, web dashboards (see `INTEGRATION.md`)
- Smooth animated scroll and resize (API-driven and keyboard)
- Settings modal (Win+,) with side-tabs
- Safe recovery: windows moved to visible viewport, clips cleared on exit, panic, and at startup

## Installation

Via Scoop (recommended):

```sh
scoop bucket add winri https://github.com/sub07/winri-bucket
scoop install winri
```

With cargo install (Rust toolchain required):

```sh
cargo install --git https://github.com/sub07/winri
```

Then run `winri` from command line or create a shortcut to `winri.exe`.

### Update

Via Scoop:

```sh
scoop update winri
```

## Bug Reporting

Please use the repo issue tracker to report bugs.

When reporting bugs, please include:

- Winri version (see `winri --version`)
- Windows version (see `winver` command)
- Steps to reproduce the issue
- Expected behavior
- Actual behavior
- Application logs located at `C:\Users\<user>\AppData\Roaming\winri\logs\winri.log`. This is not required as it may contains sensitive data, but it will dramatically help debugging.
- Screenshots or screen recordings if applicable
- Any other information you think may be relevant

## Default keybindings

Keybindings are not customizable yet.

In Tiler mode:

| Shortcut                   | Action                                        |
|----------------------------|-----------------------------------------------|
| Win + Left / Right         | Move focus to previous / next window          |
| Win + Ctrl + Left / Right  | Swap focused window with neighbor             |
| Win + Q                    | Close focused window                          |
| Win + F                    | Resize focused window to fullscreen width     |
| Win + C                    | Resize focused window to half of screen width |
| Win + Shift + Left / Right | Resize by increment (20 px by default)        |
| Win + R                    | Force tiler refresh                           |
| Win + Up                   | Enter Overview mode                           |
| Win + Escape               | Exit winri and restore windows                |


In Overview mode:
| Input        | Action         |
|--------------|----------------|
| Win + Down   | Close overview |

## Configuration

Configuration is not implemented yet. It will include at least the following options:

- Keybindings
- Padding between windows
- Border color and thickness for thumbnails and focused window
- Per-process modifiers (e.g. exclude certain apps from tiling)

You can check the [config issue](https://github.com/sub07/winri/issues/3) for more fields to come.

## Roadmap

You can check the [milestones](https://github.com/sub07/winri/milestones) to see planned features for upcoming releases.

## Contributing

Winri is open to contributions!

Before tackling an issue or submitting a PR, please check the issue tracker for existing discussions. Before submitting a PR, consider opening an issue first to discuss your ideas.

For your PR to be accepted, please ensure the following:

- Format code with `cargo fmt --all -- --check`
- Run this clippy command `cargo clippy -- -D warnings`
- Run tests with `cargo test --all-features`
- Run cargo-machete to ensure dependency hygiene: `cargo machete --with-metadata` (install with `cargo install cargo-machete`)

Branch naming is flexible. I use `feat/#issue_number` but you can use whatever you prefer. Just give a meaningful name.

Winri aims to be built with stable Rust.

- Rust edition: 2024
- CI:
  - check: fmt, clippy (check Contributing section), tests, cargo-machete
  - build: Build winri.exe
  - deploy: Create github release + Scoop bucket manifest update
- Dependencies:
  - `windows` crate for Win32 API
  - `rdev` for global input capture
  - `iced` for overlay and other custom windows

Releases are automated via GitHub Actions on pushes to `main`.

Tagging is derived from GitHub releases automatically; manual tagging is not required for the standard flow.

## Security

For Winri to manipulate windows of elevated processes (like task manager), it must run with administrative privileges. One can choose to run winri without admin rights, but then windows of elevated processes will be ignored.

Winri will never collect or transmit any user data. Bugs will be reported by users voluntarily.

Nonetheless, keep in mind that manipulating windows of other applications can have security implications, especially if those applications handle sensitive data.

## Known Limitations

- Hard-coded keybindings (configurable from settings is partial; full rebinding is still TODO)
- Some applications may not behave correctly (e.g. some UWP like windows calculator or settings)
- A clipped window whose geometric center crosses onto a second monitor will briefly flicker at the new monitor's DPI as it scrolls past the boundary (Windows reassigns per-monitor DPI by window center; cannot be suppressed without moving the window)
- Apps that quietly clamp coordinates to `i16::MAX` (Chromium-based, including Chrome and Electron) need the tile strip kept inside `±25_000` px — handled internally, but extremely wide strips may not behave well

See issue tracker for more.

## FAQ

**Q: Will Winri work with all applications?**
A: Winri works with standard Win32 GUI applications. Some applications like UWP may not behave as expected.

**Q: Can I customize keybindings?**
A: Not yet, but this feature is planned for future releases.

**Q: Is there multi-monitor support?**
A: Yes. Pick one monitor as the tiling monitor; other monitors hold floating windows. Drag a tile onto a secondary monitor to untile it; drag a window back onto the tiling monitor to tile it. Tiles that scroll past the tiling monitor's edge are clipped, not spilled.

## Fork changes (since upstream `v0.3.2`)

Bugs, perf work, and features that landed on this fork's `dev`:

- **Multi-monitor**: configurable tiling monitor, drag-out / drag-back to (un)tile, secondary-monitor overflow hidden via `SetWindowRgn` clip (with full clip-restore on crash + clean exit)
- **Virtual desktop survival**: cloaked + iconic windows no longer get pruned; window-strip respects desktop changes
- **Chrome / Chromium / Electron compat**: park coordinates kept inside `i16::MAX`, defensive divergence detection, Electron `WS_EX_TOOLWINDOW` + owned-popup filtering, `/wake` recovery endpoint for blank windows
- **Win11 Widgets**: `WidgetBoard.exe` and friends filtered so the panel doesn't ghost-tile as "MSN"
- **Window-hook crash fix**: `EVENT_OBJECT_LOCATIONCHANGE` handling moved off per-event thread spawns onto a single persistent coalescer (was hitting `ERROR_COMMITMENT_LIMIT` after long sessions)
- **Process-handle leak fix**: `OpenProcess` is now `CloseHandle`'d via RAII (was accumulating ~300k+ handles per session)
- **Performance**: per-snapshot Win32 dedup, virtual-desktop GUID cache, snapshot info-log gated to debug, `publish_state` short-circuits on no-diff
- **Overview**: right-click context menu (close, force-redraw); jump-to-thumbnail focus reliability
- **Settings**: modal popup with side-tabs (General / Apps / Excludes), Win+, re-open after X-close
- **Control API** (`INTEGRATION.md`): `/state`, `/windows`, `/scroll` (now with `animate_ms`), `/windows/{id}/resize` (with `animate_ms` + `center`), `/windows/{id}/wake`, `GET /events` SSE push channel
- **Animated drag-resize**: dragging a tile's left edge animates the scroll instead of jumping the right edge
- **Width-cache trust**: `update_widths` rejects large bounds-vs-cache deltas at rest (Chrome / Electron / TouchDesigner no longer get stomped to natural width on first snapshot)
- **DWM restart survival**: `E_HANDLE` floods from `DwmGetWindowAttribute` (graphics driver reset, dwm.exe crash) are detected, calls back off for 250 ms, and the cached `IVirtualDesktopManager` proxy is dropped so the next caller rebuilds against fresh dwm.exe instead of holding a stale COM proxy that blows up at shutdown
- **Hung-app guard**: `SetWindowRgn` / clip-clear skip windows that don't pong a 50 ms `WM_NULL` probe, so one frozen Chrome window can't stall the whole tiler (29 s freeze observed before the guard); failed clears now leave `WindowItem.last_clip` set so the next layout pass retries instead of leaving the window visually truncated
- **Settings/overview symmetric guard**: synchronous `overview_opening` flag covers the async gap between queueing the overview window-creation tasks and committing `Mode::Overview`, closing the May 2026 freeze-cascade in both hotkey orderings

## Disclaimer

Experimental software manipulating arbitrary third-party windows. Use at your own risk; keep unsaved work backed up.

Microsoft Windows API can be inconsistent and buggy. If you encounter issues, please report them via the issue tracker, but understand that some issues may not be fixable due to limitations in the Windows API or specific application behaviors.
