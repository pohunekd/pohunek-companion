# Linux window rules

A Wayland client cannot position itself or stay on top, so the compositor
places the sidebar. The app sets its Wayland `app_id` to `pohunek-companion`
(`slint::set_xdg_app_id`), which window rules match on.

## sway

Add to `~/.config/sway/config` (or an included file):

```
for_window [app_id="^pohunek-companion$"] floating enable, sticky enable, border none, resize set width 320 px height 100 ppt, move position 0 px 0 px
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

The sidebar asks for 320x800. Its chip rows set an explicit maximum width so
the window does not advertise a maximum width of 176 px to the compositor
(`xdg_toplevel.set_max_size`); with that cap its first committed geometry was
176x800, the size seen at startup before the rule's resize took effect. The
startup size of 176x800 itself did not reproduce in a nested headless sway
(the rule produced `320x1417` with and without the cap), so the cap is the
only cause found, not a confirmed one.

The same rule works as a one-off command on an already running window, which
is how it was verified on sway 1.12 with a waybar top bar:

```
swaymsg '[app_id="^pohunek-companion$"] floating enable, sticky enable, border none, resize set width 320 px height 100 ppt, move position 0 px 0 px'
```

Measured result on a 3440x1440 output with a 20 px bar: window rectangle
`x=0 y=20 width=320 height=1420` in output coordinates.

## New-session dialog

The dialog (`pohunek-companion new-session`) is its own window with the
Wayland `app_id` `pohunek-companion-new-session`. Sway criteria are regular
expressions, so an unanchored `app_id="pohunek-companion"` also matches the
dialog; the sidebar rule above is anchored with `^...$` for that reason. Float
and centre the dialog with:

```
for_window [app_id="^pohunek-companion-new-session$"] floating enable, resize set width 480 px height 520 px, move position center
```

To open it from a key, run the binary directly (no sidebar needed):

```
bindsym $mod+Shift+n exec pohunek-companion new-session
```

The sidebar's "+ New session" control and the tray's "New session..." entry
start the same command as a separate process, so the dialog stays open if the
sidebar is closed.

The assistant dialog (`pohunek-companion assistant`) has the `app_id`
`pohunek-companion-assistant`; float it the same way and bind it to a key:

```
for_window [app_id="^pohunek-companion-assistant$"] floating enable, resize set width 480 px height 560 px, move position center
bindsym $mod+Shift+a exec pohunek-companion assistant
```

On macOS the dialog is a regular (not accessory) app process; winit activates
it on launch, which brings it to the front with keyboard focus.

## Status tray

The tray icon is a StatusNotifierItem. It needs a host: waybar with the `tray`
module enabled (the owner's setup) or the AppIndicator extension on GNOME.
