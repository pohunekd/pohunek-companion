# Coding rules

## Boundaries

- The companion is an external client of the Pohunek daemon. It talks to it
  only through the `pohunek-client` crate and the `pohunek-assistant` launch
  crate, both pinned to the same Pohunek git tag, and the public protocol in
  Pohunek's `docs/public-api.md`. Other Pohunek crates are not a stable API: do
  not depend on them.
- The daemon owns session, herd, and notification state. The companion only
  reads it and calls public methods; it never infers or caches authority.
- The owner's layout (groups, order) is client-side state in the companion's
  own state file. Never write it into session metadata.
- Herd membership and Pohunek Work links (`work.link.*`, `work.role`, legacy
  `link.*`) are read-only structure for the companion. A drag never adds or
  removes a herd member or relinks a work item.
- No network calls other than to Pohunek daemons, with one exception: the setup
  screen's Install action downloads the release files of the pinned Pohunek tag
  (`setup.release_base_url`, GitHub by default) over HTTPS, and only after the
  owner clicked it. GitHub, Linear and similar providers are otherwise reached
  only through metadata Pohunek Work already wrote.

## Rust

- Edition 2024, toolchain pinned in `rust-toolchain.toml`. `cargo fmt`, and
  clippy with `-D warnings`.
- No `unsafe`. No `unwrap`/`expect` on data from the daemon, files, or the
  user; reserve them for real invariants and say why in the message.
- Errors are typed (`thiserror`) at crate boundaries and shown to the user in
  the UI; never swallow an error or replace it with a silent default.
- Keep UI code thin: state, reconciliation, layout ordering, and tray
  derivation live in plain Rust modules that do not depend on Slint, so they
  can be reasoned about and tested without a window.
- Add a dependency only when it clearly pays for itself; prefer crates already
  in the tree. Compile time and binary size are product requirements.

## Slint

- UI lives in `.slint` files under `crates/*/ui/`; Rust exposes models and
  callbacks. Do not build UI trees from Rust.
- A choice among a few fixed options is a chip row (`ChoiceChips` in
  `crates/app/ui/choice_chips.slint`), never a `ComboBox`.
- Design tokens (spacing, colours, sizes) are global `.slint` properties, not
  literals scattered across components.
- Per-session actions live in the session row's right-click context menu
  (`ContextMenuArea` in `SessionRow`). The most used ones (Open folder, Open
  link, Stop, Remove) are also small monochrome icons that appear on row hover.

## Configuration

- Tunables (timeouts, thresholds, intervals, sizes) come from
  `$XDG_CONFIG_HOME/pohunek/companion.toml` (or `~/.config/...`), never
  constants in code. Document each key next to its config struct.
- A missing or invalid required value fails at startup with a clear message
  shown in the UI or on stderr; never invent a default for it. Optional values
  may have documented defaults in the config struct.

## Security

- The local daemon socket is an owner-only boundary; never weaken path or
  permission checks that `pohunek-client` performs.
- Never log or display secrets. Session metadata is owner-controlled but
  untrusted input for rendering and for command templates: pass it as argv,
  never through a shell string.
- Never read `.env*`, key, or credential files.

## Logging

Structured JSON logs (`tracing`) to `logs/` in development and to the platform
state directory in installed builds: daemon connection lifecycle, reconcile
results, user actions (attach, layout changes), and errors.

## Comments

A comment states what the code does now or why it must be this way. Never
narrate history ("previously", "no longer", "now also", "replaces ..."); that
belongs in the PR description. One or two lines; prefer the non-obvious why.
