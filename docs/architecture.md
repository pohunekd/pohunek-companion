# Architecture

Authoritative design for Pohunek Companion. Change it deliberately, in the same
PR as the code that departs from it, and say so in the PR.

## Product

A narrow, always-available sidebar for the owner of a Pohunek daemon:

- every session with its live state (working, needs the owner, idle, ended);
- a layout the owner arranges by drag and drop: own groups and order;
- Pohunek Work context (linked issue/PR, role) and herd structure (manager,
  members, nesting) shown on the sessions;
- a tray icon that summarises which sessions need the owner and which sit idle,
  with a menu to jump to them;
- opening a session attaches to it in the owner's terminal.

Non-goals: an embedded terminal, managing issues or PRs, editing herds or work
links, a replacement for the Pohunek web control center.

Targets: Linux (Wayland) and macOS. Small binary, fast compile, near-zero idle
CPU.

## Stack

- Rust, toolchain pinned in `rust-toolchain.toml`.
- Slint (winit backend, femtovg renderer) for UI, drag and drop
  (`DragArea`/`DropArea`), and the tray (`SystemTrayIcon`: StatusNotifierItem
  on Linux, `NSStatusItem` on macOS). The Slint Royalty-free License requires
  attribution: the app shows an "About Slint" entry.
- `pohunek-client`, pinned to a Pohunek git tag, as the only protocol
  dependency.

Alternatives considered: Qt via cxx-qt (most mature tray and DnD, heavy
deployment and build), GPUI with gpui-component (no tray, slow compile,
snapshot-pinned API), egui (fast, non-native look), Iced (no DnD or tray).
Qt + cxx-qt is the fallback if Slint fails the M0 spike on maturity, egui if
it fails on size or speed.

## Layout

```
crates/
  core/   no UI dependency: daemon connection and reconcile, session model,
          layout store, tray state derivation, config, attach launcher
  app/    Slint UI and tray; thin layer that maps core state to models
```

`core` is introduced with the first real logic; until then `app` is the only
crate.

## Data from Pohunek

All facts come from the public protocol (Pohunek `docs/public-api.md`):

- Seed with `session.list` and `notification.list`; keep current with one
  `subscribe` connection. On connect, reconnect, or a lagging stream, re-seed
  and reconcile, as the protocol requires.
- `agent_state`, `subagent_state`, and `background_state` events are applied
  only when their runtime identity matches the session snapshot and their
  revision is newer.
- Sessions are keyed by `(host, session_id)` from the start. v1 connects to the
  local daemon only; more hosts come through `host.discover`.
- Pohunek Work metadata: `work.link.provider|kind|id|url|branch`, `work.role`,
  `work.rev`, and the legacy `link.*` family.
- Herds (Pohunek #1300-#1307, not shipped yet): member metadata `herd`,
  `herd_role`, the session's `spawned_by`, and the daemon's herd record. Names
  may change before Pohunek ships them; the companion follows the shipped
  contract.

## Layout model

- Client-side state in `$XDG_STATE_HOME/pohunek-companion/layout.toml` (or the
  macOS equivalent), keyed by `(host, session_id)`. Fractional sort keys, so a
  drag writes one entry. Never stored in session metadata: that would broadcast
  every drag to all clients, fails for read-only external sessions, and puts one
  client's view into shared state.
- Owner groups contain items. An item is a session or a whole herd block; a
  herd block moves as a unit and keeps its own tree order (manager, then
  members by role, nested herds inside).
- New sessions land in an Inbox group, or right under their `spawned_by`
  parent when it is known. `session_removed` prunes the entry. Resume keeps
  the session id and therefore the position; a fork is a new session.
- Drag never changes daemon state: no herd add/remove, no work relinking.
- Optional read-only views group by work item, project, or herd next to the
  manual layout.

## Tray states

| State | Rule |
| --- | --- |
| Needs owner | open attention notification (`agent_blocked`, `approval_required`), or `activity = blocked` as a fallback |
| Working | `activity = working`, or `idle` while subagents or background tasks still run |
| Idle | `idle` with no subagent or background work for longer than the configured threshold |
| Ended / lost | `stopped`, `done`, `failed`, or `session_runtime_lost` |

The icon shows the most urgent state and the count of sessions needing the
owner; the menu lists them and attaches on click. The tray is a passive summary:
desktop notifications stay with Pohunek's own notification command.

## Window placement

A Wayland client cannot position itself or stay on top. The sidebar position
comes from compositor window rules (documented per compositor in
`docs/`); layer-shell is out of scope for v1. On macOS the app positions its
window itself and runs as an accessory app without a Dock icon. A Linux tray
needs a StatusNotifierItem host (a bar with a tray module; on GNOME the
AppIndicator extension).

## Attach

Opening a session launches the owner's terminal through a configured command
template (`attach_command`, argv mode, no shell interpolation of session data)
or, on macOS, a terminal app. The binary resolution and macOS edge cases follow
the existing pohunek-work native GUI (`native/crates/gui/src/attach.rs`,
`bin_resolver.rs`).

## M0 spike result

Decision: **go with Slint.** Tray and drag and drop work on Linux (sway,
Wayland), exercised with a virtual pointer rather than a physical mouse, and one
drag-reliability problem has a workaround whose cause is unknown (Findings). As
committed, the Linux build misses two targets (binary 17.5 MB against 15 MB, RSS
93 MB against 60 MB); scratch builds with other features and profile settings
reach 12.7 MB and, with the software renderer, 20 MB RSS at 20 s after start
(not yet measured at 5 min). Closing that gap is a configuration choice for M1.
The macOS half is unverified (see D2): the go/no-go for macOS stays open until
the owner runs the app on the Mac desktop. Qt + cxx-qt and egui stay the
fallbacks; nothing measured triggers either.

### Verified on Linux (sway 1.12, waybar tray module)

- Tray: an icon whose colour and bars change every 2 s, a tooltip and menu
  labels that follow, and a menu entry that runs a Rust callback. Observed in
  waybar (`docs/m0-spike/tray-states.png`) and over D-Bus: the
  StatusNotifierItem `ToolTip` and the DBusMenu `GetLayout` labels change at
  runtime, and `Event(clicked)` on the "Bump counter" entry advanced the count
  between two timer ticks.
- Drag and drop: rows reorder within a group, move between two groups, and
  append to a group by dropping in the empty space below it, with a drop
  indicator between rows (`docs/m0-spike/dnd-*.png`; left half mid-drag, right
  half after the drop). Payload travels as `DataTransfer::set_user_data`
  (an in-process Rust value), the model change is applied as `remove` plus
  `insert` after the drop callback returned.
- Input was a wlroots virtual pointer in a nested headless sway; a physical
  mouse was not used. Real-mouse behaviour on the owner's desktop and on macOS
  is for the owner's manual check.

### Measurements (release profile as committed: thin LTO, strip, femtovg)

| Metric | Target | Linux | macOS (arm64) |
| --- | --- | --- | --- |
| Clean `cargo build --release` | < 90 s | 37.6 s (24 cores) | 80.2 s (8 GB host) |
| Incremental `cargo build` after `touch main.rs` | < 5 s | 0.4 to 0.5 s | 1.0 s |
| Release binary | < 15 MB | 17.5 MB (17,538,648 B) | 9.8 MB |
| Idle CPU, window open, 60 s after 5 min | about 0 % | 0.033 % | about 0.5 % (CPU time 2.49 s to 2.78 s over about 60 s, at 6 min) |
| RSS after 5 min | < 60 MB | 93 MB | 99 MB (at 6 min) |

Idle numbers include the 2 s tray timer of the spike. Linux ran on the
owner's GPU desktop with the sidebar window rule applied.

Linux build variants, measured in a scratch copy with the same sources:

| Variant | Binary | RSS (20 s after start) |
| --- | --- | --- |
| as committed (femtovg, accessibility on) | 17.5 MB | 92 MB |
| femtovg, without the `accessibility` feature | 15.8 MB | not measured |
| femtovg, no `accessibility`, `opt-level = "s"`, `panic = "abort"` | 12.7 MB | 88 MB |
| software renderer, no `accessibility`, `opt-level = "s"`, `panic = "abort"` | 12.9 MB | 20 MB |

So the binary size target is reachable with release-profile and feature
choices; the 60 MB RAM target is not reachable with femtovg (the GPU driver
stack dominates RSS) but a 20 MB RSS at 20 s after start was seen with the software renderer
(5-minute idle not measured), which drew the spike UI correctly in the nested
compositor. M1 picks the renderer and the release
profile; this is a configuration decision, not a reason to leave Slint.

### D2: macOS (host zajcaair, run from a remote shell session)

The release binary builds and starts, stays running, and is registered with
LaunchServices (`lsappinfo list` shows `pohunek-companion`). The window and the
menu bar item could not be observed from the remote session:

- `osascript -e 'tell application "System Events" to get name of every process whose name contains "pohunek"'`:
  `execution error: System Events got an error: AppleEvent timed out. (-1712)`
- `screencapture -x shot.png`: `could not create image from display`
- the app wrote nothing to stderr.

No claim is made that the tray or drag and drop work on macOS.

### Findings that shape M1 and M2

- `SystemTrayIcon` must be the root of its own exported component, so it is
  separate from the window component and cannot read window properties
  directly; its icon and tooltip are set through properties of that component.
  The root element's own `icon` and `tooltip` have no generated Rust setters.
- A `DragArea` that has no mouse-handling child stopped starting drags after
  the first two or three drops (most later presses did not start one), in
  Slint 1.18.1. With a `TouchArea` child, every one of the roughly 20 drags
  attempted started and finished. The cause was not found; keep a child on
  every `DragArea`.
- The dragged row is destroyed by the model change that the drop causes, so
  the model is changed after the `dropped` callback has returned.
- `slint::set_xdg_app_id` works only after the first component is created and
  before `show()`; window rules match on `pohunek-companion`
  (`docs/linux-window-rules.md`).
- The spike hardcodes its few tunables as named constants, reports errors with
  `eprintln!`, and keeps a few layout literals in the Slint components; real
  code reads tunables from `companion.toml`, logs with `tracing`, and shows
  errors in the UI per `.agents/instructions/coding.md`.

## Delivery phases (milestones)

- **M0 Spike:** Slint tray and drag and drop on the owner's compositor and on
  macOS; measure clean and incremental build time, binary size, idle CPU, and
  RAM. Targets: clean build under 90 s, incremental under 5 s, release binary
  under 15 MB, idle CPU about 0 %, RAM under 60 MB. Decide go or fallback.
- **M1 Sessions and tray:** live session list with reconcile, state display,
  attach, Pohunek Work badges, tray with counts and menu.
- **M2 Layout:** owner groups, drag and drop, Inbox, `spawned_by` placement,
  persistence.
- **M3 Herds:** herd blocks once Pohunek ships herds.
- **M4 Multi-host and packaging:** `host.discover`, macOS app bundle, Linux
  package.
