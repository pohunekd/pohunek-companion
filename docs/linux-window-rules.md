# Linux window rules

A Wayland client cannot position itself or stay on top, so the compositor
places the sidebar. The app sets its Wayland `app_id` to `pohunek-companion`
(`slint::set_xdg_app_id`), which window rules match on.

## sway

Add to `~/.config/sway/config` (or an included file):

```
for_window [app_id="pohunek-companion"] floating enable, sticky enable, border none, resize set width 320 px height 100 ppt, move position 0 px 0 px
```

- `floating enable` and `resize set width 320 px height 100 ppt` make the
  window a 320 px wide column spanning the full height of the workspace area
  (below a top bar when one reserves space).
- `move position 0 px 0 px` pins it to the left edge of that area.
- `sticky enable` keeps it on screen when the workspace changes.
- `border none` removes the frame.

The window floats above tiled windows and does not reserve space for itself.
To keep tiled windows clear of the sidebar, reserve the space with
`gaps left 320` (global) or the equivalent per-workspace gap; a sticky floating
window does not do this on its own.

The same rule works as a one-off command on an already running window, which
is how it was verified on sway 1.12 with a waybar top bar:

```
swaymsg '[app_id="pohunek-companion"] floating enable, sticky enable, border none, resize set width 320 px height 100 ppt, move position 0 px 0 px'
```

Measured result on a 3440x1440 output with a 20 px bar: window rectangle
`x=0 y=20 width=320 height=1420` in output coordinates.

## Status tray

The tray icon is a StatusNotifierItem. It needs a host: waybar with the `tray`
module enabled (the owner's setup) or the AppIndicator extension on GNOME.
