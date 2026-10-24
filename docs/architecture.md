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
- `pohunek-client`, pinned to a Pohunek git tag, as the protocol dependency,
  and `pohunek-assistant` at the same tag for starting assistant sessions.

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
- Sessions are keyed by `(host, session_id)`. `host` is `local` for the local
  daemon and, for a remote one, the `target` of its `[[hosts]]` entry in
  `companion.toml` verbatim (a Pohunek host route as `pohunek --host` accepts
  it); it never comes from a display value. The local daemon is always
  followed; every configured remote host gets its own connection loop (seed,
  stream, re-seed timer, reconnect backoff) through
  `Client::connect_with_options`, and the overlay route is resolved again on
  every connection, never cached. A failure of one host never interrupts
  another. A target that names this machine, is empty, is `local`, or repeats
  another entry is refused at startup with a message naming the entry. The
  sidebar shows the union of every host's latest list; a remote row carries its
  host label (the configured `label`, else the `target`) on its detail line and
  offers no Open folder, because its path exists on the other machine.
- Hosts are also learned from the local daemon's `host.discover`
  (`HostDiscoverParams { force: false }`), asked at startup and every
  `daemon.discover_interval_secs`; the owner chose to connect every reachable
  one without configuration (#139 Q1: A). The target rule is the Pohunek CLI's
  host fan-out rule, rebuilt from `pohunek_client` items: only
  `reachable_daemon` records with an IP address and a peer id (else fqdn)
  qualify. The host key is `<overlay>:<identity selector>`, without the port;
  the route that is dialled (and handed to the attach command) is the key with
  the record's port, kept in a process-wide key-to-route table so layout
  positions survive a port change. The label is the record's name, else its
  fqdn, else the key, sanitized, and never enters a key or an argv. A
  configured `[[hosts]]` entry that names the same peer (its target, without
  `@port`, equals the key or the peer id, fqdn or name, bare or prefixed with
  the overlay) keeps its configured key and is not followed twice; without an
  explicit `@port` it dials the record's port. A
  `version_mismatch` peer is listed in the status line as incompatible, and
  `unreachable` and `candidate` peers are ignored. A peer that leaves the answer
  is treated like a lost host: its loop stops, its rows stay marked
  unreachable and its placements are kept. A failed discovery (no overlay
  configured, no local daemon) is logged once at `warn` per distinct cause and
  leaves local and configured hosts working.
- Pohunek Work metadata: `work.link.provider|kind|id|url|branch`, `work.role`,
  `work.rev`, and the legacy `link.*` family.
- Herds (Pohunek #1300-#1307, not shipped yet): member metadata `herd`,
  `herd_role`, the session's `spawned_by`, and the daemon's herd record. Names
  may change before Pohunek ships them; the companion follows the shipped
  contract.

## Layout model

Owner decisions of 2026-10-19 (epic #3):

- **Client-side state.** Group order, session order, collapse, views,
  filters and rules live in
  `$XDG_STATE_HOME/pohunek-companion/layout.toml` (`~/.local/state/...` when
  unset; `~/Library/Application Support/pohunek-companion/` on macOS unless an
  absolute `XDG_STATE_HOME` is set), keyed by `(host, session_id)`. Reordering
  and collapsing never touch session metadata: that would broadcast every drag
  to all clients and put one client's view into shared state.
  *Revised 2026-10-24 (decision on #216, option A):* the one fact that is
  shared is which group a session is in, held in its `companion.group`
  metadata (see "Shared groups"). Issues #218 and #219 add the writes.
- **Metadata drives placement.** A valid `companion.group` key places the
  session in the group of that name. Without the key, a local manual placement
  in the layout file applies (sessions that cannot carry the key), then the
  owner-defined rules, which map session metadata (project, `work.link.*`,
  `work.role`, agent, later herd) to a group so a new session lands there
  without a drag. A rule applies only while the key is absent and never writes
  it. Sessions that match no rule land in Inbox, or under their `spawned_by`
  parent when it is known.
- **Four views** over the same sessions: My groups (default, the only view
  that accepts drag and drop), By state, By project, and By Work item / herd.
  The derived views are read-only as views (no drag and drop, no group
  editing), but a session's context menu offers "Move to group", "New
  group..." and "Return to rule placement" in every view; they change the My
  groups layout and persist like a drag. The selected view survives a restart.
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
  order. Every seeded `LiveUpdate::Sessions` list is one host's full current
  set, so a session of that host missing from it is pruned, which also covers
  sessions removed while the companion was disconnected. Pruning is per host:
  a list seeded from host H reconciles only H's keys, and a host that is
  disconnected or has never been seeded prunes nothing, so a lost host keeps
  its placements. Resume keeps the session id and therefore the position; a fork
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
- *Operations below (a drag, the menu moves, group edits) are changed by #218:
  they write `companion.group`.* A drag in My groups moves one session: it rewrites that item's group and
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
- Drag never changes herd or Work state: no herd add/remove, no work relinking.
- *Changed by #219: a session created from a row's menu gets
  `companion.group` at creation, and a fork copies it.* **A session created
  from a row's menu** lands in that row's group. The
  dialog is a separate process, so the sidebar passes the group id as
  `new-session --group <id>`. After `session.new` returns the id, the dialog
  appends `(host, session id, group id, time)` to `pending-placement.toml` in
  the state directory (atomic write, read-modify-write under an exclusive
  `flock` on `pending-placement.lock`). The sidebar checks the file on every
  session list: for a session that is present it moves the session to the
  group's end as a manual placement (the layout stays the sidebar's alone) and
  deletes the entry; an entry older than
  `new_session.pending_placement_max_age_secs` whose session never appeared is
  deleted and logged. A group deleted meanwhile refuses the move and the
  session keeps its rule or Inbox placement. Without `--group` (tray, sidebar
  button, standalone command) nothing is recorded.

### Shared groups

Decided on #216 (option A) and implemented in #217 to #219; the Android
companion implements the same semantics. This subsection states the read side
(#217) and the known limitation; the writes, the migration and the not-shared
marker are described by #218 and #219 when they land.

- **Key and value.** Group membership is the session metadata key
  `companion.group`; its value is the group's display name. No other key is
  added. The value is trimmed; a value that is empty, longer than
  `layout.group_name_max_bytes` UTF-8 bytes (default 64, at most the daemon's
  4096) or that contains a control or bidirectional-format character is
  treated as absent and logged once at `warn` with the host, session id and
  reason, never the value. A valid value is shown through `badge::sanitize`.
  The rules live in `crates/core/src/group_key.rs`.
- **Merging.** Names merge ignoring case across all sessions of all hosts.
  The value `Inbox` (ignoring case) means the owner put the session in Inbox;
  a missing key means the session is unplaced.
- **Precedence.** A valid key places the session. With no key, a local
  `manual` placement places it. With neither, the first matching rule places
  it, else Inbox. When a session carries a valid key its local `manual` flag
  is cleared, so removing the key later returns it to the rules or Inbox
  instead of a stale local group. Rules never write the key, so two desktops
  with different rules never fight over it.
- **Learned groups.** Each group has an origin, `created` (by the owner on this
  client or by a rule) or `learned`, and only a learned one is written to the
  file. A valid key naming a group this client does not have
  appends a learned group after the last group. A learned group with no member
  is dropped once every host is seeded; a created group stays until the owner
  deletes it, even when empty. When another client renames a group, this
  client's created group with the old name stays, empty, until deleted.
- **Order.** Session order inside a group is per client. A session that
  arrives in a group without a local position (a new session, a session moved
  by another client, one that got its key from the CLI) goes to that group's
  end.
- **Concurrency.** The daemon applies each metadata patch under its sessions
  lock, so the last write wins per key. A client re-derives membership from
  the next `session_updated` event, and the periodic re-seed corrects a missed
  event; there is no membership state of its own to reconcile.
- **Known limitation.** At v0.35.2, `session.set_metadata` is persisted only
  for sessions with a native reference (zajca/pohunek#1556). A group written
  to a plain shell, or to an agent session before it reports its native id,
  may be lost on a daemon restart; the session then shows by the precedence
  above. Nothing in the companion works around it.

## Setup screen

While the local daemon has never answered in this process and no host lists
sessions, the window shows a setup screen over the list (`SetupPanel`,
`crates/app/src/setup.rs`, derivation in `crates/core/src/setup.rs`). It is
driven by the local connection loop: every `Reconnecting` update triggers one
probe on a worker thread, so probes follow the reconnect backoff and there is
no timer of their own. The probe finds the `pohunek` CLI (`pohunek_bin`, then
`~/.local/bin`, `/opt/homebrew/bin`, `/usr/local/bin`) and reads
`pohunek service status --json`. `derive` maps the facts to one state:

| State | Screen |
| --- | --- |
| CLI not found | "Install Pohunek" on Linux x86_64 (release archive) and on macOS 14+ with Homebrew (`brew install`); manual steps on macOS without Homebrew; "platform not supported" with the install guide link elsewhere |
| service not installed | "Install service" |
| installed, daemon not running | "Start service" when `pohunek service --help` lists `start`, else the job name to start by hand |
| job running, daemon silent | the connection error |
| interrupted install or upgrade | pointer to `pohunek service status` |
| `version_mismatch`, `daemon_protocol_too_old` | the daemon's message and the companion's `pohunek-client` version and protocol range |

An action is two clicks. The first runs the read-only `pohunek service check
--json` and shows the exact command and what it touches; a refused check shows
its error and leaves Run disabled. Run executes one fixed argv
(`service install` or `service start`, with `--json`), bounded by
`setup.action_timeout_secs`, and shows the CLI's error code and message, cleaned
and clipped to `setup.message_max_chars`, or "finished". `integration install`,
`--accept-runtime-loss`, `--stop-sessions` and `--purge` have no representation
in `SetupAction`. When the daemon answers, `LiveUpdate::Connected` closes the
screen. After the first connection a later outage keeps the list and the
status line, as before. A remote host that lists sessions keeps the list
visible. Remote hosts never use this screen.

### Installing Pohunek

`SetupAction::InstallRelease` (Linux x86_64) and `BrewInstall` (macOS) are
offered from the CLI-missing state; the machine's `InstallPlan` comes from
`plan_for_host` (`crates/core/src/install.rs`). The version is the git tag of
`pohunek-client` in `Cargo.lock`, read by `crates/core/build.rs`, never
"latest". The first click of the Linux install runs everything that does not
touch the machine, in a fresh 0700 directory below the state directory (it is
on disk and not `noexec`, unlike `/tmp` on hardened systems), removed on every
path by a drop guard:

1. download `pohunek-daemon-<version>-x86_64-unknown-linux-gnu.tar.gz` and its
   `.sha256` from `<setup.release_base_url>/<tag>/` with `ureq` (rustls and the
   bundled webpki roots); `https` only, redirects capped, bodies capped by
   `setup.download_max_mib`, `http` accepted only for a loopback base URL;
2. parse the checksum file (exactly `<64 hex>  <archive name>`) and compare it
   with the archive's sha256; any difference refuses the install;
3. `gh attestation verify <archive> --repo zajca/pohunek` when `gh` is on the
   machine and signed in (a failure refuses; otherwise the screen says "not
   checked" and why);
4. unpack with `flate2` and `tar`, entry by entry: only regular files and
   directories below the archive's single root directory with plain path
   components, exclusive file creation, modes reduced to 0755 or 0644, and caps
   on entries and unpacked size;
5. run the unpacked `pohunek service check --json` and show its plan.

Run then executes the archive's `packaging/install-daemon.sh` with no
argument (never `--accept-runtime-loss`), which installs under `~/.local` and
starts the login service; the screen then follows the normal detection to the
list. `pohunek assistant setup` and `pohunek integration install` are named in
text and never run. The macOS action runs `brew install
zajca/pohunek/pohunek`; the formula's current release can be newer than the
pinned tag, and the screen says so. The service step is the existing "Install
service" action.

## Tray states

| State | Rule |
| --- | --- |
| Needs owner | open attention notification (`agent_blocked`, `approval_required`), or `activity = blocked` as a fallback |
| Working | `activity = working`, or `idle` while subagents or background tasks still run |
| Idle | `idle` with no subagent or background work for longer than the configured threshold |
| Runtime problem | runtime state `conflict` or `incompatible` (not running as far as the daemon can tell, not lost); wins over activity and attention. `reconnecting` stays with its activity |
| Ended / lost | `stopped`, `done`, `failed`, or `session_runtime_lost` |

The icon is the Pohunek herder figure in the colour of the most urgent state,
with a badge digit for the count of sessions needing the owner; the menu lists
them and attaches on click. Icon files derive from `assets/pohunek-icon-master.svg`
(`scripts/build-icons`, origin and licence in `assets/README.md`). Runtime problem, Ended and
no-agent-state sessions never count in the tray. The state filter shows a
"Problem" chip while some session is in that state, and the row's detail line
adds the runtime state and its `loss_reason`. The tray is a passive summary:
desktop notifications stay with Pohunek's own notification command.

Each followed host has its own connection state: connected, reconnecting
(attempt and reason), or refused with a typed protocol error such as
`version_mismatch` or `daemon_protocol_too_old` (shown with the daemon's
message). The window status text names every remote host that is not connected
with its label and reason; the local host keeps its own wording. A remote host
that is not connected keeps its rows with their last-known facts: the rows are
dimmed, say "host unreachable" on the detail line, offer no attach or row
action, and are not updated until the host's next seed. An outage does not
imply the sessions stopped, so they are never shown as ended. Unreachable-host
rows never count in the tray and its menu; the tooltip adds "N hosts
unreachable". A tray entry of a remote session carries its host label. The
local daemon's outage keeps the offline tray and unmarked rows.

A small "Hosts" button at the end of the view chip row opens a dropdown: "All
hosts", then every followed host with its connection state (connected or
unreachable). Picking a host narrows every view to that host's sessions. The
selection is part of the per-view filter stored in `layout.toml` (`host`, the
host's target), like the project and states. The button is absent while only the
local host is followed and no host is selected.

## Creating a session

Creating, renaming, stopping and removing a session are the companion's only
mutations of daemon state (see "Renaming a session" and "Stopping or removing
a session"). Creation runs
from `pohunek-companion new-session [--host <target>] [--project <id>]
[--group <id>]`, a subcommand of the
same binary that runs as its own process: `slint::set_xdg_app_id` is per
process, so the dialog sets the app id `pohunek-companion-new-session` and a
compositor rule can float it apart from the sidebar. It creates no tray icon
and does not follow the session stream.

- The Host select at the top offers Local, then every `[[hosts]]` entry, then
  every peer the local daemon discovers (the sidebar's labels). The window and
  the Local choices never wait for discovery. The sidebar passes the hosts it
  already follows (targets, labels, routes, and why a peer is unpickable) as
  `--known-hosts <json>`; the dialog lists them at once and registers their
  routes. Without it (a standalone launch, or a sidebar whose first discovery
  has not answered) the dialog asks `host.discover` itself in the background
  with the same derivation as the sidebar (`discovery::plan`, shared core
  code), shows "Looking for other hosts..." under the select, and fills it when
  the answer arrives; a peer that is this machine is left out. A remote host's
  choices load only after its route is registered, so picking a remote host
  early waits for discovery, never Local. A `version_mismatch` peer is listed
  but cannot be picked, with a note naming the protocol version, like an
  unlaunchable agent. A failed discovery is logged and leaves Local and the
  configured hosts. `--host <target>` preselects `local`, a configured target
  or a discovered key; a target not listed yet is shown as a pending entry and
  selected once discovery lists it, unless the owner picked another host
  meanwhile. An unknown target or an incompatible peer shows its message in
  the window, loads Local, and keeps Create disabled until a host is chosen.
  Choosing a host reloads the choices from that host's daemon and keeps
  the project and agent when the host has them; a host that cannot be reached
  shows its error and leaves Create disabled. `session.new` goes to the chosen
  host, and "New session in ..." on a remote row passes the row's host key
  with its group, so the pending placement carries that host in the session
  key.
- Choices come from `project.list` (projects) and `host.inspect` (agents) of
  the chosen host. The projects are shown as soon as `project.list` answers;
  the agents follow when `host.inspect` does, which can take seconds on a
  daemon with many agent runtimes. The dialog logs `window shown`, `hosts
  listed`, `projects loaded` and `choices loaded` with `elapsed_ms` since the
  process entered the dialog. An
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

## Renaming a session

The session row's context menu has "Rename...". It opens a name editor in the
sidebar prefilled with the current name (empty when the session shows its id).
Accepting sends one `session.rename`: the trimmed text is the name, blank
clears it so the row falls back to the session id. The editor keeps no local
copy of the result: the row changes when the daemon's session event arrives. A
daemon refusal (for example an external read-only session or a control
character) or a failure to reach the daemon is shown in the window as
"Cannot rename <id>: <message>" and nothing changes. Log lines carry the
session id and the error code, never the name. For a remote session the call
goes to the daemon of the session's host and the message reads "Cannot rename
<id> on <host>: <message>", with the host's label (else its target).

## Stopping or removing a session

The session row's context menu has "Stop..." and "Remove...". An entry is
shown only when the daemon's own rule allows it for the session
(`SessionInfo::can_stop`, `SessionInfo::can_remove`): an external read-only
session or an ended session (no Stop) gets no entry. A `conflict` runtime
offers Stop and Remove and an `incompatible` one offers Remove, although the
client rule refuses them: the daemon is authoritative and refuses the records
it cannot retire. For such a session the modal says the daemon may refuse and
shows its message and `recover` hint. Choosing one opens a modal inside the sidebar
window: a dimmed backdrop over the whole window that swallows pointer and
scroll input (the list behind stays inert) and a centred card. It is an
overlay component rather than a `PopupWindow`, which gives full control of
the backdrop, focus and keys. A click on the backdrop cancels. It names the
session and the consequence: stop ends the agent and keeps the session in the
list as ended; remove deletes it from the daemon (stopped first if live)
together with any worktree checkout Pohunek created for it. The modal starts
on Cancel: Enter and Escape cancel, Tab or the
Right arrow moves the selection to the destructive button (Left returns to
Cancel), and only then Enter (or a click) sends one `session.stop` or
`session.remove`. The sidebar performs the call itself and closes the modal
when it succeeds without anything to report.

Removal first sends `session.remove`. When the daemon refuses it with
`runtime_supervision_ambiguous` (it cannot confirm that processes of the
session's runtime ended, for example because they have an unreadable
environment) the same modal shows the refusal as "Cannot remove <id>:
<message>" and turns into the "Remove anyway" step, again starting on Cancel,
that says in plain words that the daemon cannot confirm the processes ended,
that they are not signalled and may keep running unsupervised after the
session, its logs and its worktree are deleted. Only confirming it sends
`session.remove_accepting_unconfirmed` for that one session; the consent is
never remembered and the safe call is always tried first. After that removal
the modal stays open with the accepted processes (pid and command name) from
the result and a Close button. Any other refusal is shown in the modal,
which stays open (Cancel closes it, the destructive button retries). Both
calls run on a connection whose request timeout is
`daemon.session_action_timeout_ms`, because they wait for the runtime to end
and be swept. The row changes, and the layout store drops a removed session,
when the daemon's session events arrive. A request that exceeded the timeout
has an unknown outcome and may still complete in the daemon. A removal that
could not delete worktree checkouts is reported with their count in the
modal. Log lines carry the session id and the error code.

Stop and remove work on a remote session as on a local one, against the daemon
of the session's host (`Client::connect_with_options` with the session-action
timeout). The modal names the host ("Remove <name> on <host>?"), the remove
consequence adds that the worktree checkout is deleted on that host, and errors
read "Cannot remove <id> on <host>: <message>".

## Row action icons

While the pointer is on a row, small monochrome icons appear at its right
edge, each with a tooltip: Open folder, Open link, Stop and Remove. They are
shown on hover only (the sidebar has no keyboard-selected row, so the context
menu is the keyboard route); each also has a context-menu entry. Stop and
Remove use the confirmation flow above and appear under the same
`can_stop`/`can_remove` rules.

- **Open folder:** the session's `worktree_path` when it has one, else its
  `cwd`; offered only when the path is absolute and an existing directory,
  and checked again when clicked.
- **Open link:** `work.link.url`, else the legacy `link.url`; offered only for
  an `http` or `https` URL with a host and no whitespace or control
  characters. Session metadata is untrusted, so any other value is never
  offered or opened.
- **Opener:** `open_command` in `companion.toml`, an argv template whose only
  placeholder is `{target}` (default `xdg-open` on Linux, `open` on macOS).
  The target is one argv element; no shell is involved. A failure to start
  the opener, or an early failed exit, is shown in the window.

## Window placement

A Wayland client cannot position itself or stay on top. The sidebar position
comes from compositor window rules (documented per compositor in
`docs/`); layer-shell is out of scope for v1. On macOS the app positions its
window itself. The sidebar process runs as an accessory app without a Dock
icon: it sets winit's `ActivationPolicy::Accessory` through Slint's
`BackendSelector::with_winit_event_loop_builder` (`select_accessory_backend`
in `crates/app/src/main.rs`, macOS only). `LSUIElement` is not used because
it would also apply to the dialog, which is the same executable. The bundle is
built by `packaging/macos/build-app-bundle` and ad-hoc signed by
`packaging/macos/sign`. A Linux tray
needs a StatusNotifierItem host (a bar with a tray module; on GNOME the
AppIndicator extension).

The new-session dialog is a separate process (`pohunek-companion new-session`)
with its own `app_id`, started by the sidebar's "+ New session" control, the
tray's "New session..." entry, or a desktop keybinding; it outlives the sidebar.
The assistant dialog (`pohunek-companion assistant`) is started the same way.

## Starting an assistant session

`pohunek-companion assistant [--host <target>] [--project <id>] [--intent
<intent>] [--known-hosts <json>]` is a second dialog, its own process with the
app id `pohunek-companion-assistant` and no tray icon, started by the
sidebar's "+ Assistant" control, the tray's "Assistant..." entry, the desktop
entry's `assistant` action, or a keybinding. It is a separate window rather
than a mode of New session because its fields differ (intent, a multi-line
request, no name, template or branch) and a mode switch would hide half of
New session's form. The Slint controls both dialogs share live in
`ui/dialog_fields.slint`, and the assistant dialog reuses New session's host
handling (known hosts, discovery, `--host` resolution, per-host choices).

- The launch is `pohunek-assistant`'s: it picks the agent (the host's
  `pohunek-assistant` profile, then `codex`, `claude`, `hermes`), materializes
  the knowledge bundle and a redacted live snapshot (on the host itself for a
  remote launch), composes the prompt, and calls `session.new`. The companion
  runs `prepare_with_options` and `start_prepared_with_options` so that a
  failure before `session.new` (nothing exists) is told apart from one after
  it (unconfirmed, never resent), as in New session.
- Fields: Host (as in New session), Project (a remote host needs one, which
  the library requires), Intent chips (help, setup, project, update, debug),
  Agent (Automatic, or an assistant-capable runtime of the chosen host:
  launchable and not shell-backed), and a multi-line Request. Enter starts
  everywhere except in the request, where Ctrl+Enter does. The request field
  has the focus on open.
- A refusal (`no_capable_agent`, `bundle_unavailable`, an unreachable host)
  shows the daemon's message and recovery hint in the window. A host with no
  capable agent does not disable Start: the launch reports it with the hint.
  A remote launch asks for no confirmation; the dialog is the confirmation.
- After `session.new` the session reaches the sidebar through the event
  stream, and the dialog attaches to it with `attach_command` when "Attach
  after starting" is checked (initially `new_session.attach_after_create`), the
  same way New session does. `new_session.cols`, `rows` and `create_timeout_ms`
  apply.

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
  every `DragArea`. Investigation (#9): the failure did not reproduce without
  the child, so no upstream issue was filed. 12 of 12 drags dropped through
  `scripts/ui-agent` `drag_element` on the real rows with the `TouchArea`
  removed, and 60 of 60 in a standalone Slint program (list of `Rectangle`
  rows with a `DropArea` and an empty `DragArea`, pointer hover moves before
  each press) with and without the child, using in-process events under
  `SLINT_BACKEND=headless-software`. Slint's `DragArea`
  (`i-slint-core-1.18.1/items/drag_n_drop.rs`) needs no child: its
  press/threshold filter and `StartDrag` do not depend on one. What stays
  untested is real Wayland pointer input through winit, which the original
  observation used; if it recurs, capture that input path first. A release
  that no `DragMove` over a `DropArea` preceded never drops: the window turns
  it into `Exit` (`i-slint-core-1.18.1/window.rs`, "No DropArea accepted the
  most recent DragMove"), so a synthetic drag needs a final motion event over
  the target.
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
