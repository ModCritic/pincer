# Embedded default cursor theme

`pincer.default.lottie` is the source archive for Pincer's built-in cursor,
and `pincer.default.pincer-theme` is the runtime artifact compiled from it.
`build_default_theme.py` writes the source archive; edit the script, not the
archive.

The overlay embeds only the `.pincer-theme` artifact and never parses
dotLottie ZIP files or Lottie JSON at runtime. The artifact holds bounded
vector geometry, paints, transforms and animation frames sampled at 30 fps,
not a pixel atlas, and tiny-skia rasterizes it at the display's backing scale.

Regenerate both files from the Rust workspace (`rust/`):

```bash
python3 crates/cursor-overlay/assets/build_default_theme.py
cargo run -p cursor-theme-cli -- build \
  crates/cursor-overlay/assets/pincer.default.lottie \
  --output crates/cursor-overlay/assets/pincer.default.pincer-theme
cargo run -p cursor-theme-cli -- inspect \
  crates/cursor-overlay/assets/pincer.default.pincer-theme
```

The generator uses only Python's standard library and writes ZIP entries in a
fixed order with fixed metadata, so it always produces the same bytes. A Rust
test checks that the source hash inside the compiled artifact matches the
checked-in `.lottie`.

## Artwork

The cursor is a dart on a 128-unit canvas, drawn 42 px across, with its
hotspot at the outline's mitered tip. The dart is filled with `#C95E48`,
outlined 5 units wide in `#2B0000`, and casts a soft black drop shadow offset
down and right. Each of the 12 actions adds marks stroked in `#E28150`, with
soft shadows of their own, painted over the dart.

`#C95E48` is the tint key. At runtime the default theme's fill is replaced
with the session's color: the first cursor on the desktop keeps `#C95E48`,
and later ones get redder or yellower variants (`slot_fill_rgba` in
`src/theme.rs`). The outline and the marks are never tinted, and the shared
four-second float is applied to the default theme only. Installed custom
themes keep their authored colors. Delivery and target context is painted by
the host's session badge, not by themes.

On GNOME Wayland the cursor is drawn by the Shell helper,
`wayland-helper/winrects@pincer/extension.js`, which ports this artwork by
hand. Change the helper together with `build_default_theme.py`;
platform-linux's tests compare the helper with the compiled theme.

`Inter.ttf` is the session badge's font, licensed under `Inter-OFL.txt`.
