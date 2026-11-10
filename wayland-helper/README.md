# pincer WinRects: GNOME Shell helper extension (Wayland)

A small GNOME Shell extension that lets pincer get **pixel coordinates**,
activate an exact target window, capture the compositor stage, and draw the
**agent cursor** on GNOME Mutter Wayland. A normal Wayland client cannot do
these things globally.

It exposes `org.pincer.WinRects` on the session bus:

- `GetVersion() -> uint`: a browser-sensitive API version. pincer only
  accepts it after resolving the immutable D-Bus owner and proving the owner is
  the current user's system-installed `gnome-shell` process.
- `GetRects() -> json`: every window's frame geometry and surface-buffer
  origin. pincer combines the buffer origin with AT-SPI
  `CoordType::Window` per-widget coords: `screen = origin + window_xy`. This is
  the GNOME analogue of the X11 `_GTK_FRAME_EXTENTS` reconstruction (AT-SPI's
  `CoordType::Screen` is `(0,0)` for every widget on Mutter). Keeping the frame
  and buffer origins separate accounts for GTK client-side shadows.
- `Activate(id) -> bool`: activate one Shell stable-sequence window and report
  whether the request was accepted. pincer verifies focus through a second
  `GetRects` snapshot before sending focus-bound portal/libei input, preventing
  input from leaking into whichever application happened to be focused.
- `Capture() -> png_base64`: capture the compositor stage through Shell's
  screenshot API. pincer crops it with the same authoritative geometry.
- `MoveCursor(x,y)` / `ClickPulse(x,y)` / `HideCursor()`: position and hide
  the agent cursor as a Clutter actor on the compositor stage.
- `SetCursorState(action,delivery,target,active)`: render the same 12 semantic
  action states as the cross-platform `pincer.default` cursor theme. Delivery and
  target context appears as host-owned chips in the session badge rather than
  pointer-relative theme artwork. This contract requires helper v8.
- `SetCursorColor(fill_color)`: apply the stable per-session fill selected by
  pincer. The helper validates the `#RRGGBB` value and paints the pointer's
  body in it, as the Rust overlay recolors `pincer.default`. The outline, the
  shadow and the action marks keep the theme's own colors.
- `CaptureWindow(id,pid) -> png_base64` (API 9): snapshot one window's own
  compositor actor (`paint_to_content`), not the stage, so a covering window
  never shows and the target is not activated. `pid` 0 skips the owner check.
  A 1x1 transparent clone keeps Mutter painting a fully covered window and
  expires 4 s after the last capture. Refused while the screen is locked, for
  minimized windows, and above 32 MiB.
- `CaptureWindowPreview(id,pid,max_dimension) -> (png_base64, width, height)`
  (API 10): the same capture, downscaled on the GPU so its long edge is at
  most `max_dimension` (1 to 4096). `width`/`height` are the window's
  full-size pixels, which pincer reports as the original size. pincer
  removes GNOME's per-capture `Creation Time` PNG chunk, so identical frames
  are byte-identical.
- `ShowMinimalChip(keys,mac,foreground)` (API 11, signature `asbb`): the
  minimal badge's brief chip, as the Rust overlay draws it. `keys` are the
  combo's keys in display order, one string per key (so a `+` key stays one
  key), drawn as light keycaps over a bevel in the session color in a dark
  tray, with a `+` between caps unless `mac`. `foreground` adds the
  foreground chip; with no keys the chip is a pill that reads "foreground".
  A combo too wide for the 188 px badge shows as its text in the pill. A
  call while a chip is pending joins it (keys replace, `foreground` adds).
  The chip waits for `SetCursorState(..., active=false)`, or 10 s, then
  plays while the cursor is at rest: 0.12 s delay, 0.08 s fade in, 0.5 s
  hold, 0.3 s fade out. It takes the session badge's place while it shows,
  and the next `SetCursorState(..., active=true)` drops it. Ignored while the
  cursor is hidden. pincer sends it only under the minimal badge
  (`cursor.badges`), and only to a helper that reports API 11 or newer; with
  an older helper the minimal badge keeps the foreground chip from
  `SetCursorState` and shows no key combo.

The agent cursor is `pincer.default` as the Rust overlay draws it on X11,
ported to Cairo by hand from
`rust/crates/cursor-overlay/assets/build_default_theme.py`, so the two change
together: the dart, 42 px across with its tip on the given point, in the
session color with a dark red (#2B0000) outline and a soft drop shadow down
and to the right, and the orange (#E28150) action marks, played from the
theme's own keyframes at 30 fps. `MoveCursor` glides the way the Rust overlay
moves the cursor with Pincer's default motion (`signature_arc` with `steady`
timing, to a point with no target rect): along a curve bowed to one side, in
0.6 to 1.1 s by Fitts' law, with the dart turning to lead along its path and
then, for up to 0.3 s more, settling back to rest. The helper plans each glide
from where the cursor is, the way it points and the destination, as
`pincer-cursor-motion`'s `plan_move` does, and each frame only interpolates
the plan. A motion style or timing chosen in Pincer does not reach the helper,
which always glides this way. The effects of the default motion are drawn in
the session color lifted toward white: a speed glow that follows the glide,
and a ripple and a squish after `ClickPulse`. The glide follows Shell's
animation settings, so with animations off the cursor jumps, without a turn
or a glow.

The session badge is a pill in the session color with white text, a 1 px
rim of that color darkened 35%, and a soft shadow, at most 188 px wide
(longer labels ellipsize). The frame timer that animates the cursor and the
badge runs only while the cursor is shown. The cursor moves as a plain actor
around its drawing, since St redraws a drawing area that is moved itself; a
glide redraws it at most every 12 ms, and at rest the timer does every 33 ms.

It runs in the shell's privileged context, so **no xdg-desktop-portal grant** is
needed (unlike libei/RemoteDesktop).

## Install

```
~/.pincer/packages/current/wayland-helper/install.sh
# From a source checkout, use ./install.sh in this directory.
# then log out/in once (GNOME loads extensions only at session startup)
gnome-extensions info winrects@pincer   # -> State: ACTIVE
```

pincer auto-detects it at runtime (`wayland::shell_helper`). AX operations
still work when it is absent, but pixel geometry, the Shell cursor, and safe
foreground portal input are unavailable. pincer refuses focus-bound input
instead of injecting into an unverified target.

With helper API 9 or newer, pincer captures GNOME windows only through
`CaptureWindow`; if the helper refuses a window, the screenshot fails rather
than falling back to a stage crop that could show another app. With an older
helper it still crops the stage to the window frame, which shows whatever
covers the window. Re-run the installer to get per-window capture.

The semantic cursor requires helper v8. When an older helper is still loaded,
pincer does not draw its legacy cursor. Re-run the helper installer, then
reload the GNOME session so the new compositor-owned artwork becomes active.

Browser setup and consent are held to a stricter boundary: helper API v4 or
newer must be served by the verified GNOME Shell owner. The driver addresses
that owner's unique D-Bus name, so another same-session process cannot replace
the public name between verification and an activation request. One exact
target is activated only for the bounded operation, then the previously
focused Shell window is restored and verified.

## Versions and other apps that bundle the helper

The extension is identified by its UUID, `winrects@pincer`, and serves the
session-bus name `org.pincer.WinRects`. Other apps ship the same helper: T3 Code
bundles it with its Linux build of Pincer. Keeping one UUID is deliberate.
Two copies under different UUIDs would both be enabled and race for the same
bus name and object path, and which one answered would be undefined. With one
UUID there is exactly one installed copy per user, and the rules below let
any copy serve every client:

- `GetVersion()` returns an integer API version, and `metadata.json`
  `"version"` carries the same number.
- API versions only add methods. A method never changes or disappears in a
  later version, so a newer helper serves every older client. Clients gate
  each feature on `GetVersion()`.
- A version number names one method set everywhere. Versions 9 and 10 were
  first defined in T3 Code's Linux patch to Pincer 0.34.0
  ([pingdotgg/t3code#16975](https://github.com/pingdotgg/t3code/pull/16975))
  and are adopted here unchanged, so T3 Code's v10 and this v10 are
  interchangeable. Version 11 was first defined here. Coordinate any new
  number with apps that bundle the helper.
- Installers never downgrade. `install.sh` and the pincer installers keep
  an installed helper whose version is newer than the bundled one, and say so.
  `install.sh --force` replaces it anyway.

| Version | Adds                                                               |
| ------- | ------------------------------------------------------------------ |
| 8       | The semantic `SetCursorState` contract; shipped by Pincer 0.34 |
| 9       | `CaptureWindow(id,pid)`                                            |
| 10      | `CaptureWindowPreview(id,pid,max_dimension)`                       |
| 11      | `ShowMinimalChip(keys,mac,foreground)`; lists GNOME Shell 51       |

Pincer 0.34 and earlier bundle v8 or older, and their installers overwrite
an installed helper unconditionally. Running one of those installers after an app
installed v10 or newer downgrades the helper until the newer app reinstalls it.

## Other compositors

wlroots compositors such as Sway and labwc do not need it: pincer uses
foreign-toplevel activation, virtual-pointer input, and layer-shell there.

KDE Plasma Wayland needs an equivalent target-addressable KWin activation
adapter; it is not yet provided. Portal reachability alone is insufficient
because RemoteDesktop/libei input is global to the compositor focus.
