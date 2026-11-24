# Releasing

A release is a `Release X.Y.Z` pull request followed by a tag. The tag
publishes the GitHub release through `.github/workflows/release.yml`; agents do
not create tags or releases (see
[.agents/instructions/workflow.md](../.agents/instructions/workflow.md)).

## Steps

1. Refresh the screenshots on the current `origin/main`: `scripts/capture-screenshots`.
   When `git status --short docs/screenshots` shows changes, open a pull request
   with the set and the `needs-design-ok` label and merge it after the owner's OK,
   before the release branch, so the tag contains a set whose manifest matches the
   app. The run needs the local `pohunek` CLI and `pohunekd` like `scripts/ui-agent`.
2. Start from the current `origin/main` on a branch `release-X.Y.Z`.
3. Set the workspace version in `Cargo.toml` to `X.Y.Z`, then run `cargo build`
   so `Cargo.lock` picks it up. Review the lock diff: only the workspace package
   versions change.
4. Fold the changelog fragments into the version section:
   `scripts/changelog fold X.Y.Z`. The date is today in UTC unless
   `--date YYYY-MM-DD` is given. The command deletes the folded fragments, so
   the release commit carries both the version bump and the section.
5. Review `CHANGELOG.md` and the deleted fragments in the diff. Run
   `scripts/changelog notes X.Y.Z` and check the block reads correctly.
6. Open the pull request titled `Release X.Y.Z` with the label `no-changelog`
   (the fold itself is the changelog entry). Its description lists the changes
   since the previous release, as earlier release pull requests did.
7. After the owner has approved the release, merge the pull request and tag the
   merge commit `vX.Y.Z`. Pushing the tag starts `release.yml`, which builds,
   checks and publishes the release with the changelog section first in its
   body.

## What the workflow enforces

- The tag build fails before publishing when `CHANGELOG.md` has no section for
  the version, or when fragments for the product remain in the tagged commit.
- The `pull_request` run that a release pull request triggers shows the body the
  tag would publish; the log of the `Build the release body` job prints it.
