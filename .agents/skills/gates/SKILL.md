---
name: gates
description: >-
  Run the pohunek-companion gate set (fmt, clippy, tests) locally and report
  each gate's real result. Use before any commit, push, or claim that a change
  builds or is done.
---

# gates

Run `scripts/gates` from the repository root (or the worktree root). It runs
`cargo fmt --all --check`, `cargo clippy --workspace --all-targets --locked --
-D warnings`, and `cargo test --workspace --locked`, records each exit code,
prints a per-gate summary, and exits non-zero if any gate failed.

- Read the summary, not a piped tail of the output: report each gate as
  `passed` or `failed (exit N)` exactly as printed.
- If `cargo` resolves to a system toolchain that ignores `rust-toolchain.toml`,
  put rustup's `~/.cargo/bin` first on `PATH` and rerun.
- A formatting failure is fixed with `cargo fmt --all`; then rerun the gates.
- Never mark a gate passed that did not run, and never weaken a lint or test to
  get green.
