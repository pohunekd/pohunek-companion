# Changelog

User-facing changes of Pohunek Companion. Each version section is folded from
the fragments in `changelog.d/` when its release is prepared.

## 0.1.10 - 2026-11-24

### Fixed

- The sub-session chip in a session row can be clicked again: the row stays put while the hover actions appear, and chips sit right after the session name. ([#255](https://github.com/pohunekd/pohunek-companion/issues/255))
- The changelog check refuses only a link to the fragment's own issue or pull request in the folded repository, so a link to another repository's issue with the same number is accepted. ([#259](https://github.com/pohunekd/pohunek-companion/issues/259))
- A click on a project or state chip in the sidebar is no longer lost when a session update arrives at the same moment. ([#287](https://github.com/pohunekd/pohunek-companion/issues/287))

## 0.1.9 - 2026-11-15

Pohunek Companion 0.1.9 makes busy hosts easier to read: sessions that other sessions start now sit under their parent, a parent tells you when something below it needs you, and the New session dialog knows about agent profiles and their launch options.

**What's new**

- **Sub-sessions fold under their parent.** A session started by another session, or tied to a manager session, is listed under that parent, which shows how many sub-sessions it has and expands them in place. The Sub-sessions chip puts every session back in its own place.
- **Attention rolls up to the parent.** A parent row shows "N need(s) you" when sub-sessions below it are waiting for you, folded or expanded, and those sub-sessions still show in Needs you and the tray. A parent flagged with `pohunek.attention.children=isolate` keeps its sub-sessions out of Needs you, the tray and that count.
- **Profile picker with options.** The New session dialog lists agent profiles by their display name and keeps profiles meant for agents or hidden behind a "Show all" chip. A profile's launch options show as rows of choices, prefilled from the profile or preset, with "Reset to defaults". The dialog remembers the agent and choices you last used per host and project, and sidebar rows show the chosen options as chips.

**Upgrading**

The isolation flag, profile display names and audiences, and launch options need pohunek 0.43.0; the setup screen's Install downloads it. The companion still works with pohunek 0.41 and 0.42 daemons: there it folds sub-sessions and shows the attention count, isolates nothing, and the dialog lists agents as before while still remembering your last agent.

**Full changelog**

### Added

- The New session dialog lists agent profiles by their display name, keeps profiles meant for agents or hidden behind a "Show all" chip, and shows a row of choices for each launch option a profile offers, remembering the agent and choices per host and project. Sidebar rows show the chosen options as chips; with a pohunek 0.42 daemon the dialog works as before. ([#242](https://github.com/pohunekd/pohunek-companion/issues/242))
- Sessions started by another session, or tied to a manager session by its `manager` key, are folded under that parent: the parent shows how many sub-sessions it has and expands them in place. A sub-session that needs you still shows in "Needs you", and the new Sub-sessions chip shows every session in its own place again. ([#243](https://github.com/pohunekd/pohunek-companion/issues/243))
- A parent row shows how many of its sub-sessions need you, folded or expanded. A session flagged with `pohunek.attention.children=isolate` keeps its sub-sessions out of Needs you, the tray and that count, on hosts whose pohunek daemon supports the flag (0.43 or later); their own rows still show their state. ([#244](https://github.com/pohunekd/pohunek-companion/issues/244))

### Changed

- The companion is built against pohunek 0.43, and the setup screen's Install downloads pohunek v0.43.0. It still works with pohunek 0.42 daemons; a 0.43 daemon additionally enables the flag that keeps a session's sub-sessions out of Needs you and the tray. ([#244](https://github.com/pohunekd/pohunek-companion/issues/244))

## 0.1.8 - 2026-11-12

### Changed

- The companion works with Pohunek 0.42 and keeps showing sessions that wait for you, whichever kind of wait the daemon reports. ([#245](https://github.com/pohunekd/pohunek-companion/issues/245))

## 0.1.7 - 2026-11-10

### Fixed

- The companion connects to Pohunek 0.41 daemons again, and the assistant dialog now uses the daemon's plugin-assistant context package. ([#235](https://github.com/pohunekd/pohunek-companion/issues/235))
