# Assets

The icon artwork is derived from the Pohunek assets in
<https://github.com/zajca/pohunek/tree/main/assets> (MIT, same owner as this
repository).

| File | Origin |
| --- | --- |
| `source/pohunek_icon_mark.png` | Unmodified copy of `assets/pohunek_icon_mark.png` from zajca/pohunek (265x255 raster, no alpha). |
| `pohunek-icon-master.svg` | Vector trace of the file above (potrace, two layers: the beige rounded square and the dark herder and prompt shapes). The master for every icon. |
| `icons/` | Generated from the master by `scripts/build-icons`; do not edit by hand. |

Regenerate the icons with `scripts/build-icons` (needs `rsvg-convert`,
ImageMagick and `python3`). The outputs are committed, so a normal build needs
none of these tools.

- `icons/pohunek-app-256.png`: window icon of the sidebar and dialogs, and the
  logo in the About window.
- `icons/pohunek-companion.icns`: macOS app bundle icon, installed by
  `packaging/macos/build-app-bundle`.
- `icons/pohunek-tray-mask-128.gray`: 128x128 8-bit coverage mask of the herder
  figure (without prompt and field lines, thickened) that `crates/app/src/tray.rs`
  tints with the state colour.
