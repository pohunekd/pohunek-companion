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
