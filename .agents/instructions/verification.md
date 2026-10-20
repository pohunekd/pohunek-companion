# Verification

## Gate set

`scripts/gates` runs, in order and each with its own exit code:

1. `cargo fmt --all --check`
2. `cargo clippy --workspace --all-targets --locked -- -D warnings`
3. `cargo test --workspace --locked`

CI runs the same set, so a green local run is the expected predictor of a
green PR. Run it on the final revision before every push and report each
gate's real result. Never report a gate as passed when it was skipped or its
output was cut off.

## Testing policy

The app changes fast, and tests that pin UI or internal structure only slow
it down. Write the minimum set of tests that catch real regressions.

Write a test only for:

- **Non-trivial pure logic** where a bug is silent and costly: reconciling the
  daemon snapshot with the event stream, layout ordering and persistence, tray
  state derivation, config validation.
- **A fixed bug in such logic**, as a regression test that fails without the
  fix. Do not add a regression test for UI glitches or one-line fixes.

Do not write:

- tests for UI layout, Slint components, styling, or rendering;
- tests per function, getters, trivial mappings, or anything the type system
  already guarantees;
- mocks of the Pohunek daemon. Test logic on plain data (snapshots and event
  sequences as values); exercise the real daemon only through manual
  verification;
- coverage targets.

Delete a test that keeps breaking on intentional changes without ever having
caught a bug, and say so in the PR.

## Manual verification

UI and integration changes are verified by running the app against a real
local Pohunek daemon (`cargo run -p pohunek-companion`). The PR records what
was done and observed (and a screenshot for visible changes). On Linux the
session runs Wayland; macOS behaviour that was not run on a Mac is reported as
unverified, not as passed.

Agent sessions share the owner's live desktop. Never inject synthetic input
there (uinput, ydotool, wtype, xdotool, `swaymsg seat`): it lands in whatever
window has focus.

The default agent UI check is Slint's embedded MCP server, driven through
`scripts/ui-agent`. It builds a debug binary with the command-line feature
`slint/mcp` into `target/ui-agent/` (never a `Cargo.toml` feature, never in
release builds or packaging) and runs it on the headless software backend: no
window, no display, and an isolated daemon with synthetic sessions (see
"Published evidence" below). The tray icon is not part of the
backend, so a headless run still registers a tray icon on the owner's panel
while it runs.

```bash
scripts/ui-agent start                  # isolated daemon; prints http://127.0.0.1:<port>/mcp
scripts/ui-agent start --sessions 120   # plus 120 generated sessions (124 in total)
scripts/ui-agent screenshot shot.png    # PNG of the window
scripts/ui-agent tree                   # element tree with ids
scripts/ui-agent find SessionRow::row-touch
scripts/ui-agent click SessionRow::row-touch 0
scripts/ui-agent call TOOL '{"json":"args"}'   # any other Slint MCP tool
scripts/ui-agent restart                # stop and start only the app
scripts/ui-agent stop                   # always stop it when done
```

`scripts/ui-agent start [options] -- <app args>` runs the app with those
arguments (for example `-- new-session --project <id>`). `restart` keeps the
environment, the arguments, the MCP port, the isolated daemon, and the layout
file: session ids and `scripts/ui-agent layout` content survive it, which is how
a restart-persistence check runs in isolated mode. `stop` after a `restart`
still removes the daemon and the isolated directory.

A dialog the sidebar spawns (`new-session`, `confirm`) inherits the sidebar's
MCP port, cannot bind it, and runs with no MCP endpoint. To screenshot, read, or
click a dialog, start it through the script instead:

```bash
scripts/ui-agent dialog new-session --project <project id>   # prints "N http://127.0.0.1:<port>/mcp"
UI_AGENT_DIALOG=N scripts/ui-agent screenshot dialog.png      # also tree, find, click, call
scripts/ui-agent dialogs                                      # running dialogs and endpoints
scripts/ui-agent dialog-stop [N]                              # one dialog, or all
```

The dialog runs the same binary with the sidebar's recorded environment (isolated
daemon, config, backend) on its own free port; `stop` and `restart` stop every
dialog it started.

## Published evidence

The repository is public. Screenshots, logs, and PR or issue text must not
show the owner's real sessions (names, ids, working directories, Work links).

- `scripts/ui-agent start` (the default) runs the app against an isolated
  Pohunek daemon: its own `XDG_RUNTIME_DIR`, state, config, data, and cache
  directories in a temporary directory, and four synthetic `shell` sessions
  (`alpha-agent`, `beta-review`, `gamma-docs`, `delta-build`, two with fake
  Work links) spread across three synthetic git projects (`proj-alpha`,
  `proj-beta`, `proj-gamma`, registered with `pohunek project add`;
  `delta-build` has no project). `scripts/ui-agent start --sessions N` adds N
  generated `shell` sessions (`<word>-<word>-<n>`, rotating over the projects
  with every fourth left without one, every fifth with a fake Work link), for
  example `--sessions 120` for 124 sessions in total; it needs the isolated
  daemon. `scripts/ui-agent stop` stops the app, the daemon, and its
  sessions and deletes the directory. Everything published comes from this
  mode.
- For a manual `cargo run` that is meant to be published, set the same
  isolated environment instead; the owner's socket is `$XDG_RUNTIME_DIR/pohunek`.
- `scripts/ui-agent start --remote-host <ssh destination>` also starts a
  synthetic remote host for multi-host checks: an isolated daemon on the macOS
  machine `<dest>`, in its own temporary directory with its own overlay port
  (`--remote-port N`, default a random free port; never 18722), one project
  `remote-proj` and two sessions `remote-alpha` and `remote-beta`. The local
  isolated daemon and the app get the same `POHUNEK_REMOTE_PORT`, and the script
  prints the route (`netbird:<fqdn>@<port>`), also available as
  `scripts/ui-agent remote-route`, and lists it in the isolated `companion.toml` as a `[[hosts]]` entry labelled `remote`. Check it with the isolated environment:
  `POHUNEK_REMOTE_PORT=<port> pohunek --host <route> session list`.
  `remote-stop` stops only the remote daemon (a lost host: the route then
  refuses connections) and `remote-start` starts it again with the same session
  ids; the restarted daemon lists those sessions as `running` although their
  workers died with it. `stop` stops the remote daemon and its sessions and
  deletes the remote directory, then checks that no process and no directory is
  left.
  Prerequisites: NetBird up on both machines, `pohunek` and `pohunekd` (same
  version as the local CLI) and `netbird` on the remote PATH (the nix and
  homebrew profiles are added; `UI_AGENT_REMOTE_PATH` prepends more), and
  non-interactive ssh to `<dest>`. `UI_AGENT_REMOTE_EXEC` replaces the
  `ssh -o BatchMode=yes` command with another that takes the destination and
  `sh -s -- ARGS` and feeds stdin to the remote `sh`. The option never touches
  the owner's daemon on that host: the remote daemon is a `--dev-subprocess` one
  (plain child processes, no launchd jobs; the script aborts if launchd holds a
  job for one of its sessions), only processes whose environment carries the
  isolated `XDG_RUNTIME_DIR` are signalled, and the owner's port stays bound.
  Describe checks against the owner's real remote daemons in words only.
- `scripts/ui-agent start --owner-daemon` connects to the owner's real daemon,
  for diagnosing a problem that only the real data shows. Describe what was
  checked in words only; never attach or paste its screenshots, element trees,
  logs, or session lists. Do not click rows or edit anything through it.
- Raw MCP output (`tree`, `find`, `call`) can contain session names: treat it
  like a screenshot.

Element ids are `Component::id` from the `.slint` source (for example
`SessionRow::row-touch`, `SessionRow::name-text`, `AppWindow::session-list`,
`AppWindow::status-text`); give new elements that agents need to target a
stable id. Attach PR screenshots taken this way. `start --windowed` uses the
real winit backend for checks that need it; it opens a window on the owner's
screen, so use it only when headless cannot show the behaviour.

Rules for the MCP route:

- Needs `jq`, `python3`, and `curl` 7.76 or newer on the host, and for the
  isolated daemon the `pohunek` CLI plus its `pohunekd` (`POHUNEK_CLI` and
  `POHUNEKD` override the lookup).
- The server binds 127.0.0.1 without authentication, so any local process can
  drive the app while it runs. Stop it when the check is done.
- `scripts/ui-agent` runs the app with a temporary `XDG_CONFIG_HOME` whose
  `attach_command` is `["true"]`, so clicking a session row logs an `attach`
  line (`target/ui-agent/logs/companion.json`) instead of opening a terminal.
  Never click a row with the owner's real `attach_command`.
- It also sets a temporary `XDG_STATE_HOME` (inside the isolated directory in
  the default mode), so the run reads and writes its
  own layout file (`scripts/ui-agent layout` prints the path), never the
  owner's `layout.toml`. Edit that file between runs to check restore and
  corrupted-file behaviour. The path override works on macOS too: an absolute
  `XDG_STATE_HOME` wins over the platform default.
- Not covered, left to the owner as a manual check with exact steps on the
  issue: the tray icon and menu (native, not Slint-rendered) and anything that
  needs the real compositor. For compositor-level checks a nested headless
  compositor with its own socket (for example `WLR_BACKENDS=headless sway`) is
  the only allowed automated route.

macOS: the same script works on a Mac from a shell session (the headless image
is rendered in process, so no Screen Recording permission is involved). Run it
in a scratch checkout with its own `UI_AGENT_DIR`, never in the owner's
checkout.

## CI

`.github/workflows/ci.yml` is built to finish a PR in a few minutes:

- One Linux job runs the gate set with `Swatinem/rust-cache`. Only `main`
  writes the cache; PRs restore it, so a PR builds only its own changes.
- A macOS job runs clippy and tests on every push to `main`, on manual
  dispatch, and on PRs that touch platform-sensitive paths (see the `changes`
  job filter). Add a path there when a new file holds platform-specific code.
- A newer push cancels the previous run of the same PR.
- The aggregate `ci` check is the only required status check.

Keep CI fast: before adding a job or a step, measure its cost on a PR run and
state it in the PR. A slow or flaky CI is a bug: file it.
