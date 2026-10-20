# AGENTS.md

Canonical entry point for every agent working in this repository. Keep it
short: it routes to the scoped guides instead of repeating them. When a command,
convention, or boundary changes, update the guide that owns it in the same PR.

All repository text, code, comments, commit messages, issues, and PRs are in
English. Priorities: security > correctness > simplicity > speed of delivery.

## About

Pohunek Companion is a small native desktop app (Rust + Slint, Linux Wayland
and macOS) for the owner of a [Pohunek](https://github.com/zajca/pohunek)
daemon: a narrow sidebar of sessions with their live state, a drag-and-drop
layout the owner arranges, Pohunek Work and herd metadata, and a tray summary
of sessions that need the owner or sit idle. It is an external client of the
Pohunek protocol and owns no session state. See
[docs/architecture.md](docs/architecture.md).

The app is expected to change quickly. Optimize for small, fast, reversible
changes: little ceremony, few tests, a fast CI.

## Instruction routes

Read a guide when its trigger applies.

- **Every task:** [.agents/instructions/workflow.md](.agents/instructions/workflow.md)
  for autonomy, issues, branches, PRs, merge, and the rule to file problems
  instead of fixing them.
- **Any code change:** [.agents/instructions/coding.md](.agents/instructions/coding.md)
  for Rust, Slint, the Pohunek protocol boundary, configuration, security, and
  comment rules.
- **Before any commit or PR:** [.agents/instructions/verification.md](.agents/instructions/verification.md)
  for the gate set, the testing policy, CI, and evidence.
- **Architecture or product decisions:** [docs/architecture.md](docs/architecture.md)
  is authoritative; revise it explicitly in the same PR when a decision changes.

## Skills

Canonical skills live in `.agents/skills/`; `.claude/skills/` contains only
links to them. Edit only `.agents/skills/`.

- [github-workflow](.agents/skills/github-workflow/SKILL.md): issues, epics,
  milestones, project status, and writing reproducible issues.
- [deliver-issue](.agents/skills/deliver-issue/SKILL.md): take one issue from
  intake to a merged, verified, closed result without asking.
- [plan-epic](.agents/skills/plan-epic/SKILL.md): split an epic or milestone
  into ready, well-formed issues.
- [gates](.agents/skills/gates/SKILL.md): run the local gate set and report it
  honestly.

## Commands

```bash
scripts/gates                 # fmt, clippy, tests: the same set CI runs
cargo run -p pohunek-companion
scripts/ui-agent start        # headless app + isolated daemon + Slint MCP: screenshots, element tree, clicks
```

The repository is public: published screenshots, logs, and PR or issue text
come only from the isolated mode of `scripts/ui-agent` (synthetic sessions on
its own daemon). Checks against the owner's daemon are described in words only;
see "Published evidence" in verification.md.

Never read `.env*`, key, or credential files, and never print a secret value.
Never call `env` or `printenv` (the owner's agent settings stop on them):
check a variable with `test -n "$VAR"` and list names with `compgen -e`.
