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

## Install (Linux x86_64)

Each `vX.Y.Z` release has `pohunek-companion-<version>-x86_64-unknown-linux-gnu.tar.gz`
and a `.sha256` file beside it. The binary is dynamically linked against glibc.

```bash
v=0.1.0
a=pohunek-companion-$v-x86_64-unknown-linux-gnu.tar.gz
gh release download v$v -R pohunekd/pohunek-companion -p "$a*"
sha256sum -c $a.sha256
gh attestation verify $a --repo pohunekd/pohunek-companion
tar -xzf $a && ./pohunek-companion-$v-x86_64-unknown-linux-gnu/pohunek-companion --version
```

Linked at start (`ldd`): fontconfig, freetype (with expat, zlib, bzip2, libpng,
brotli), libgcc_s, libm and libc. Wayland, X11 and GL libraries are loaded at
run time by the window system layer, so a working Wayland or X11 desktop with
its usual Mesa/EGL stack is also required.

Contributors and agents start at [AGENTS.md](AGENTS.md).

Licensed under MIT. Built with Slint under the Slint Royalty-free License.
