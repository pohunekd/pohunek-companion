# Pohunek Companion

A small native desktop companion for [Pohunek](https://github.com/zajca/pohunek):
a narrow sidebar of your sessions with their live state, a layout you arrange
by drag and drop, Pohunek Work and herd context, and a tray icon that shows
which sessions need you. Rust + [Slint](https://slint.dev), Linux (Wayland) and
macOS.

Status: early development. See [docs/architecture.md](docs/architecture.md) and
the [project board](https://github.com/orgs/pohunekd/projects/1).

```bash
cargo run -p pohunek-companion
scripts/gates   # fmt, clippy, tests
```

Contributors and agents start at [AGENTS.md](AGENTS.md).

Licensed under MIT. Built with Slint under the Slint Royalty-free License.
