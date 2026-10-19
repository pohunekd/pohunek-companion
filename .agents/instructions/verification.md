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
window has focus. Automated UI interaction runs only inside a nested headless
compositor with its own socket (for example `WLR_BACKENDS=headless sway`);
anything that cannot be driven there is left to the owner as a manual check
with exact steps on the issue.

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
