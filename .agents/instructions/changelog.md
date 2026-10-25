# Changelog fragments

Every user-visible change adds a fragment under `changelog.d/`. Fragments are
folded into `CHANGELOG.md` when a release is prepared, and the release body
shows that section. The contract is shared by the Pohunek ecosystem; this file
is the repository's copy of its rules, and `crates/changelog` is the reference
implementation.

## When a fragment is required

- Required: anything a user, operator or packager sees or relies on (behaviour,
  CLI output, configuration, installed files, supported platforms, a fixed bug
  users hit).
- Not required, label the pull request `no-changelog`: CI, tests, refactors,
  internal docs, agent instructions, release preparation. The CI check
  `changelog` enforces this; the label exists in the repository.

## Writing a fragment

Create `changelog.d/<N>-<slug>.md`, where `N` is the issue (or pull request)
number without leading zeros and `slug` is lowercase words joined by hyphens.

```markdown
---
kind: fixed
---
Sessions that exit while the sidebar is closed no longer stay in the
"Needs owner" group.
```

- Header keys: `kind` (required): `added`, `changed`, `deprecated`, `removed`,
  `fixed` or `security`. `breaking: true` marks a breaking change (optional,
  default `false`). No other keys.
- Body: one paragraph of one to three sentences, present tense, written for the
  user: what changes for them. Markdown inline syntax only; no headings, no
  blank lines inside the paragraph.
- Keep the file UTF-8 with LF line endings.

## Commands

```bash
scripts/changelog check                       # validate every fragment (CI runs it)
scripts/changelog notes X.Y.Z                 # the release block for a folded version
scripts/changelog fold X.Y.Z [--date YYYY-MM-DD]   # release preparation only
```

`fold` is run only by the release preparation, see [docs/releasing.md](../../docs/releasing.md).
Feature pull requests never fold and never edit `CHANGELOG.md`.
