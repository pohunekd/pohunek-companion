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
- opening a session attaches to it in the owner's terminal;
- a standalone dialog creates a session in a chosen project (see Creating a
  session).

Non-goals: an embedded terminal, managing issues or PRs, editing herds or work
links, a replacement for the Pohunek web control center.

Targets: Linux (Wayland) and macOS. Small binary, fast compile, near-zero idle
CPU.

## Stack

- Rust, toolchain pinned in `rust-toolchain.toml`.
- Slint (winit backend, software renderer, no `accessibility` feature) for UI,
  drag and drop (`DragArea`/`DropArea`), and the tray (`SystemTrayIcon`:
  StatusNotifierItem on Linux, `NSStatusItem` on macOS). The Slint
  Royalty-free License requires attribution: the app shows an "About Slint"
  entry. Release profile: fat LTO, `opt-level = "s"`, `panic = "abort"`. See
  "Renderer and release profile" for the measurements behind the choice.
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
- Until the daemon signals a lagging subscriber (Pohunek #1314), the companion
  re-seeds every `daemon.reseed_interval_secs` while connected, buffering
  events during the fetch and applying them after the seed, as on reconnect. A
  re-seed that corrects anything is logged at `warn`. The lag signal (#19)
  replaces or complements this fallback once it ships.
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

Owner decisions of 2026-10-19 (epic #3):

- **Client-side.** Groups and manual order live in
  `$XDG_STATE_HOME/pohunek-companion/layout.toml` (`~/.local/state/...` when
  unset; `~/Library/Application Support/pohunek-companion/` on macOS unless an
  absolute `XDG_STATE_HOME` is set), keyed by `(host, session_id)`. Nothing is
  written to the daemon and a drag never changes session metadata: that would
  broadcast every drag to all clients, fails for read-only external sessions,
  and puts one client's view into shared state.
- **Metadata drives placement, not storage.** Owner-defined rules map session
  metadata (project, `work.link.*`, `work.role`, agent, later herd) to a group
  so a new session lands there without a drag. A rule only decides the first
  placement; the result is stored like a manual one, and a manual drag
  overrides the rule for that session. Sessions that match no rule land in
  Inbox, or under their `spawned_by` parent when it is known.
- **Four views** over the same sessions: My groups (default, the only view
  that accepts drag and drop), By state, By project, and By Work item / herd.
  The derived views are read-only; the selected view survives a restart.
- **Needs you** is a separate section above all groups in every view. A session
  that needs the owner is lifted out temporarily and returns to its place when
  it no longer does.

Storage and reconciliation:

- The file has a format version. Groups carry a stable id, a name, and a
  fractional sort key; items carry the session key, a group id, and a sort
  key, so a move rewrites exactly one item. Sort keys are short lowercase
  strings that order lexicographically, so repeated inserts between the same
  two neighbours never run out of precision.
- Inbox always exists (created on first start) and is first by default.
  Sessions the layout does not know are appended to the end of Inbox in list
  order. Every seeded `LiveUpdate::Sessions` list is the daemon's full current
  set, so a session missing from it is pruned, which also covers sessions
  removed while the companion was disconnected. Nothing is pruned while
  disconnected. Resume keeps the session id and therefore the position; a fork
  is a new session.
- Writes are atomic (temporary file in the same directory, then rename), the
  directory is `0700` and the file `0600`, and a write happens only when the
  content changed.
- An unreadable or invalid file is shown as an error in the window and is
  never overwritten; the app runs with an unsaved Inbox-only layout until the
  next start.
- An item is a session now; a whole herd block that moves as a unit and keeps
  its own tree order (manager, then members by role, nested herds inside)
  comes with herds (M3).
- A drag in My groups moves one session: it rewrites that item's group and
  sort key only and sets `manual = true` on it, which placement rules must
  respect. The drop is addressed by session key, so a daemon update during a
  drag cannot lose it.
- Group operations rewrite one group entry: create appends after the last
  group, rename and collapse change that group's fields, a header drag
  rewrites its sort key, and delete appends the group's sessions to the end of
  Inbox in their display order with their `manual` flag kept. Names are
  trimmed, non-empty, and unique ignoring case (placement rules select a group
  by name); Inbox cannot be renamed or deleted. A collapsed group keeps its
  sessions that need the owner in the Needs-you section.
- Drag never changes daemon state: no herd add/remove, no work relinking.

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

## Creating a session

Creating a session is the companion's only mutation of daemon state. It runs
from `pohunek-companion new-session [--project <id>]`, a subcommand of the
same binary that runs as its own process: `slint::set_xdg_app_id` is per
process, so the dialog sets the app id `pohunek-companion-new-session` and a
compositor rule can float it apart from the sidebar. It creates no tray icon
and does not follow the session stream.

- Choices come from `project.list` (projects) and `host.inspect` (agents). An
  agent is offered when its runtime is `available` and `supported` is not
  `false`.
- The form becomes one `session.new` (`Client::session_new`): the chosen
  project and agent, the trimmed name and prompt (empty means omitted), and
  `cols`/`rows` from `[new_session]` in `companion.toml`. The daemon picks the
  working directory from the project.
- `session.new` has no idempotency key, so the request is sent once and never
  resent. A typed daemon error is shown with the daemon's message and the form
  stays editable. A failure before the request left (socket unavailable, no
  connection) is shown as is. Any other failure, a timeout included, leaves
  the outcome unknown: the dialog says so, and Create stays disabled until the
  window is reopened, after the owner checked the sidebar.
- Log lines carry the session id and the error code, never the name or prompt.

## Window placement

A Wayland client cannot position itself or stay on top. The sidebar position
comes from compositor window rules (documented per compositor in
`docs/`); layer-shell is out of scope for v1. On macOS the app positions its
window itself and runs as an accessory app without a Dock icon. A Linux tray
needs a StatusNotifierItem host (a bar with a tray module; on GNOME the
AppIndicator extension).

The new-session dialog is a separate process (`pohunek-companion new-session`)
with its own `app_id`, started by the sidebar's "+ New session" control, the
tray's "New session..." entry, or a desktop keybinding; it outlives the sidebar.

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

### Renderer and release profile (#8)

Decision: **software renderer, no `accessibility`, fat LTO,
`opt-level = "s"`, `panic = "abort"`** (`Cargo.toml` carries the reason for
each). The M1 UI is flat lists of text and badges; femtovg's GL driver stack
is what costs RAM on Linux, and the software renderer drew everything the
current UI uses. Dropping `accessibility` means no screen-reader support; the
companion is an owner-only tool and this is a deliberate omission. With
`panic = "abort"` a panic on any thread ends the process instead of only that
thread.

Measured on the real M1 UI (release builds of `main` at 03be559 with only the
profile and feature changes). Linux ran in a nested headless sway against the
owner's busy daemon (79 sessions) and against an isolated daemon with no
sessions; RSS and CPU were sampled 300 s after start, CPU over the next 60 s.

| Variant (Linux) | Binary | RSS, busy daemon | CPU, busy | RSS, quiet daemon | CPU, quiet |
| --- | --- | --- | --- | --- | --- |
| femtovg, no `accessibility`, thin LTO, `s`, abort | 15.3 MB | 94.0 MB | 0.32 % | 83.9 MB | 0.00 % |
| **software, no `accessibility`, fat LTO, `s`, abort** | **13.6 MB** | **29.6 MB** | **0.20 %** | **21.6 MB** | **0.00 %** |

The same software build with thin LTO is 15.5 MB, which misses the 15 MB
target; `opt-level = "z"` with fat LTO gives 12.7 MB but trades speed for 0.8
MB the target does not need. Clean `cargo build --release` takes 63 s on
Linux (24 cores).

| Variant (macOS arm64, host zajcaair) | Binary | RSS | Physical footprint | CPU |
| --- | --- | --- | --- | --- |
| femtovg, fat LTO, `s`, abort | 9.0 MB | 94 MB | 44 MB | about 0.05 % |
| **software, fat LTO, `s`, abort** | **9.0 MB** | **112 MB** | **40 MB** | **about 0.06 %** |

Both targets hold on Linux. On macOS the binary target holds, but RSS cannot
meet 60 MB with either renderer: it counts the shared AppKit and system
frameworks the process maps (the spike's thin-LTO femtovg build read 99 MB).
The software renderer reads 18 MB higher RSS than femtovg there (112 against 94
MB) but a 4 MB lower footprint, which is the figure that reflects the
process's own memory.
The target is amended there to the physical footprint (`footprint <pid>`),
which is 40 MB. The clean release build took 94.5 s on that 8 GB host, 4.5 s
over the 90 s target (the spike's thin LTO build took 80.2 s); fat LTO is the
price of the size target. The macOS window and tray were not observable from
the remote session (see D2), so the macOS figures describe a running process
whose UI nobody saw; they are not a claim that it renders.

Linux, software renderer, checked in a nested headless sway:

- The sidebar draws its rows, status dots, and text correctly
  (`docs/renderer-profile/sidebar-software-renderer.png`).
- Tray: the StatusNotifierItem registers, its `ToolTip` followed the daemon
  (`0 need you, 0 idle, 3 working` after three sessions appeared) and the
  DBusMenu layout has the labels "No sessions need you", "Show window",
  "About Slint", and "Quit".
- Drag and drop: the sidebar has none yet (M2), so the M0 spike
  (`6d83a1a`) was built with the same renderer and driven by a wlroots
  virtual pointer in the nested sway. The drop indicator shows during the drag
  (`dnd-software-renderer-mid-drag.png`) and a drop moves the row to the
  indicated place across groups (`dnd-software-renderer-after-drop.png`).
  Behaviour is identical with femtovg. In both, a release that follows the
  last motion event without one more motion event over the drop target did not
  drop in 4 of 4 and 3 of 3 attempts; with a final motion event before the
  release 6 of 6 dropped. A real mouse always sends motion, so this is a note
  for #9 and for any scripted check, not a renderer problem.

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
- A second `Window` component (the "About Slint" window) that is created at
  startup and shown later never mapped under sway; creating it on first use
  maps it. The tray menu is a `Menu` with a `for` repeater over a model, which
  DBusMenu `GetLayout` reflects at runtime.
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
