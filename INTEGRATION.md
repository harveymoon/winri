# Winri Integration Guide

Winri exposes a small **local HTTP control API** that other tools can use to
read tiler state, drive the same actions as the keyboard, and pull live
thumbnails of tiled windows. Designed for Stream Deck plugins, Python macro
scripts, voice control, AutoHotkey, web dashboards, etc.

The API is loopback-only by default and disabled out of the box. You must
opt in via `%APPDATA%\winri\config.toml`.

---

## Enabling the API

Open settings with **Win+,** (or edit `%APPDATA%\winri\config.toml` directly)
and add / edit:

```toml
[api]
enabled = true
bind = "127.0.0.1"
port = 47812
```

Restart winri (Win+Esc, then re-launch). On startup the log will say:

```
INFO  winri::api::server  Starting control API on http://127.0.0.1:47812
```

Verify with:

```sh
curl http://127.0.0.1:47812/state
```

Don't change `bind` to `0.0.0.0` unless you fully trust your network — the
API has no auth.

---

## Endpoints

All bodies are JSON. Errors come back as `{ "error": "..." }` with the
matching HTTP status code.

### `GET /state`

Returns the full current state in one shot.

```json
{
  "windows": [
    {
      "id": 7405134,
      "title": "winri – README.md",
      "process": "Code.exe",
      "class": "Chrome_WidgetWin_1",
      "width": 1280.0,
      "x": 10.0,
      "focused": true
    },
    ...
  ],
  "focused_id": 7405134,
  "scroll_offset": 0.0,
  "screen_width": 5120.0,
  "screen_height": 1392.0
}
```

`id` is the Win32 HWND of the window — stable within a session, gone after
the window closes.

`x` is the window's position in **tile-strip coordinates** (not screen
coordinates). Subtract `scroll_offset` to get on-screen X.

### `GET /windows`

Shortcut for just the `windows` array from `/state`.

### `GET /windows/{id}/thumbnail`

Returns a PNG capture of the window's client area. Uses
`PrintWindow(PW_RENDERFULLCONTENT)` so it works even when the window is
occluded or offscreen (winri parks windows offscreen during overview mode).

By default the capture is at the window's **native resolution** — a
maximized window on a 4K monitor is a multi-MB PNG. For Stream Deck /
dashboard use, pass `?w=NNN` to downsample to that width (aspect ratio
preserved):

```sh
curl http://127.0.0.1:47812/windows/7405134/thumbnail -o full.png
curl 'http://127.0.0.1:47812/windows/7405134/thumbnail?w=256' -o small.png
```

`width` is accepted as an alias for `w`. Malformed or zero values are
ignored (falls back to native res).

A few apps refuse to be captured (some UWP apps without permissions); you'll
get a 500 with a descriptive error.

### `POST /focus/{id}`

Focuses the given window. Goes through winri's normal focus path, so the
viewport will glide to bring it on-screen if needed.

```sh
curl -X POST http://127.0.0.1:47812/focus/7405134
```

Response: `202 Accepted` on success.

### `POST /scroll`

Set the strip's horizontal scroll offset. JSON body:

| Field    | Type    | Meaning                                              |
| -------- | ------- | ---------------------------------------------------- |
| `offset` | number  | Absolute scroll position in pixels                   |
| `delta`  | number  | Relative scroll in pixels (positive = strip → right) |

Provide either `offset` *or* `delta`. If both, `offset` wins.

```sh
curl -X POST http://127.0.0.1:47812/scroll \
     -H 'Content-Type: application/json' \
     -d '{"offset": 1200}'

curl -X POST http://127.0.0.1:47812/scroll -d '{"delta": -200}'
```

### `POST /windows/{id}/resize`

Smoothly (or instantly) resize the tile width for a tiled window. The same
16ms animation tick that drives smooth scroll drives this — multiple
resizes can be in flight at once.

JSON body:

| Field        | Type    | Meaning                                                              |
| ------------ | ------- | -------------------------------------------------------------------- |
| `width`      | number  | Target tile width in pixels (clamped to `[50, max_screen_width]`)    |
| `animate_ms` | number  | Animation duration in ms; `0` (default) snaps instantly              |
| `center`     | boolean | If true, also smooth-scroll so the window is centered at final width |

```sh
# Snap to 1600 px wide
curl -X POST http://127.0.0.1:47812/windows/7405134/resize \
     -H 'Content-Type: application/json' \
     -d '{"width": 1600}'

# Ease over 250 ms
curl -X POST http://127.0.0.1:47812/windows/7405134/resize \
     -H 'Content-Type: application/json' \
     -d '{"width": 800, "animate_ms": 250}'

# "Fullscreen this app": grow and center in one motion
curl -X POST http://127.0.0.1:47812/windows/7405134/resize \
     -H 'Content-Type: application/json' \
     -d '{"width": 5100, "animate_ms": 250, "center": true}'
```

Easing is ease-out-cubic (snappy start, soft landing). Sending another
resize while one is in flight replaces it — useful for a knob/encoder that
streams a moving target. With `center: true`, the scroll target is computed
against the *final* width (not the live interpolated one), so both
animations finish at the same composed position.

Returns 202 on accept; 400 if `width` isn't a positive finite number; 503
if the API channel is closed. The command is a no-op (logged) if winri is
in overview mode or the HWND isn't tracked by the tiler.

### `POST /action/{name}`

Trigger any of the keyboard-equivalent actions. The action name is
lowercase-kebab.

| Action name          | Equivalent keyboard shortcut |
| -------------------- | ---------------------------- |
| `focus-prev`         | Win + ←                      |
| `focus-next`         | Win + →                      |
| `swap-prev`          | Win + Ctrl + ←               |
| `swap-next`          | Win + Ctrl + →               |
| `resize-fullscreen`  | Win + F                      |
| `resize-halfscreen`  | Win + C                      |
| `width-increment`    | Win + Shift + →              |
| `width-decrement`    | Win + Shift + ←              |
| `refresh`            | Win + R                      |
| `center-focused`     | Win + H                      |
| `open-overview`      | Win + ↑                      |
| `close-overview`     | Win + ↓                      |
| `open-settings`      | Win + ,                      |
| `exit`               | Win + Esc                    |

```sh
curl -X POST http://127.0.0.1:47812/action/focus-next
```

---

## Python quick start

```python
import requests, time

WINRI = "http://127.0.0.1:47812"

# List windows
windows = requests.get(f"{WINRI}/windows").json()
for w in windows:
    marker = "*" if w["focused"] else " "
    print(f"{marker} {w['id']:>10}  {w['process']:<24}  {w['title']}")

# Find the first Chrome window and jump to it
chrome = next((w for w in windows if w["process"] == "chrome.exe"), None)
if chrome:
    requests.post(f"{WINRI}/focus/{chrome['id']}")

# Smooth scroll to the right over ~250 ms
for _ in range(10):
    requests.post(f"{WINRI}/scroll", json={"delta": 50})
    time.sleep(0.025)

# Or jump to an absolute scroll position
requests.post(f"{WINRI}/scroll", json={"offset": 0})

# Save thumbnails of every tiled window
for w in requests.get(f"{WINRI}/windows").json():
    png = requests.get(f"{WINRI}/windows/{w['id']}/thumbnail").content
    with open(f"thumb_{w['id']}.png", "wb") as fh:
        fh.write(png)
```

---

## Stream Deck idea sketch

A typical pattern:

1. A small companion Python script polls `/windows` every ~1 second,
   downloads `/windows/{id}/thumbnail` for each, and writes them to a folder
   the Stream Deck plugin watches.
2. The plugin shows each thumbnail as a button.
3. Pressing a button POSTs `/focus/{id}` back to winri.

A coarser version that doesn't need a companion: configure each Stream Deck
key to call `curl` (via the "Open" action with a `cmd.exe /c` wrapper) for
a specific HWND. The HWND will change between sessions, so the polling
approach scales better.

---

## Notes & limitations

- **No auth.** The API binds to `127.0.0.1` so only local processes can
  reach it. Be deliberate before relaxing that.
- **HWND-based ids.** A window's `id` is its Win32 HWND, which is reused
  after the window closes. Don't persist ids across restarts of the source
  app.
- **No WebSocket / push events yet.** Poll `/state` or `/windows` for
  changes. A push channel is a candidate for a future addition.
- **Thumbnails come from `PrintWindow`.** Apps that explicitly refuse
  capture (some UWP, anti-cheat-protected games, etc.) return a 500.
- **Cross-mode behavior.** Scroll endpoints are no-ops while overview is
  open — exit overview first (e.g. `POST /action/close-overview`).
- **Wire format stability.** This is v0; expect breaking changes until
  winri reaches 1.0.
