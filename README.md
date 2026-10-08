# Ephemeris

Ephemeris is an early-stage planner and note-taking app aimed at e-ink tablets
with a stylus — especially the [PineNote](https://pine64.org/devices/pinenote/).
The long-term goal is a calm daily surface for profiles (work / home / …),
agenda, tasks, and handwritten notes that can stay in sync with tools you
already use (Obsidian vaults, calendar feeds, and later task providers).

**Status:** pre-1.0 (`v0.1.x`). Usable for development and device experiments;
APIs, UI, and on-disk formats will still change.

## What’s working today

| Area | Current state |
|------|----------------|
| Handwritten notes | Fullscreen canvas, pen / highlighter / eraser, page swipe, stroke persistence |
| Profiles | Multiple profiles with local storage and UI switching |
| Agenda | Day / week / month views fed by ICS HTTPS / local calendar feeds |
| Tasks | Local tasks + Obsidian markdown task extract / write-back |
| Obsidian | Vault browse, FTS note search, export notes as PDF + markdown into the vault |
| Desktop / device shell | Maximized Wayland window (winit + softbuffer); design-scaled UI; finger via `Touch`, pen via Linux evdev |
| Device packaging | `arm64` `.deb` with `.desktop` launcher entry; signed apt repo on GitHub Pages |

## Not there yet

Treat these as roadmap, not product claims:

- Polished day-to-day UX on-device (refresh strategy, palm rejection, menus)
- Broad calendar / task provider support (CalDAV, Todoist, etc. are incomplete or absent)
- Reliable handwriting OCR (optional Tesseract path exists; default is a no-op)
- Multi-arch desktop packages (only `arm64` Linux debs are released)
- Stable sync story for WebDAV / cloud drives

## Architecture

Rust workspace, kept deliberately thin:

| Crate | Role |
|-------|------|
| `ephemeris-core` | Domain models, ink engine, SQLite, ICS, Obsidian, PDF export |
| `ephemeris-pal` | Platform abstraction (display / input; Wayland tablet path for devices) |
| `ephemeris-ui` | Slint UI + software compositing |
| `ephemeris-app` | Binary wiring config, storage, and UI together |

Rendering uses Slint’s software renderer so CI and headless builds stay GPU-free.

## Build from source

**Requirements:** recent Rust stable, and on Linux the usual Wayland / xkb /
fontconfig (and ALSA if you enable audio capture) development packages.

```bash
make help          # all targets
make test          # workspace tests
make clippy        # same deny-warnings bar as CI
make run           # debug build
make build-release # optimized binary → target/release/ephemeris
```

Cross-build an `arm64` Debian package (needs [`cross`](https://github.com/cross-rs/cross)
and `dpkg-deb`):

```bash
make deb-aarch64
# → dist/ephemeris_<version>_arm64.deb
```

## Install on a PineNote (or other aarch64 Debian)

Release artifacts are attached to [GitHub Releases](https://github.com/steffen030/ephemeris/releases)
(`ephemeris` binary + `ephemeris_*_arm64.deb`). A GPG-signed apt repository is
published to GitHub Pages after each release:

**https://steffen030.github.io/ephemeris/**

```bash
curl -fsSL https://steffen030.github.io/ephemeris/ephemeris-archive-keyring.gpg \
  | sudo tee /usr/share/keyrings/ephemeris-archive-keyring.gpg >/dev/null

echo "deb [signed-by=/usr/share/keyrings/ephemeris-archive-keyring.gpg arch=arm64] https://steffen030.github.io/ephemeris stable main" \
  | sudo tee /etc/apt/sources.list.d/ephemeris.list

sudo apt update && sudo apt install ephemeris
```

**PineNote pen access:** the stylus is read from `/dev/input/event*`. Your user
must be able to open those nodes (usually membership in the `input` group):

```bash
sudo usermod -aG input "$USER"
# log out and back in, then relaunch Ephemeris
```

Optional: `EPHEMERIS_SIZE=WxH` overrides the design UI resolution (default
`800x1067`, upscaled to the panel). `EPHEMERIS_GRAB_PEN=1` takes exclusive
evdev grab of the digitiser.

The public keyring also lives in-tree at
[`packaging/keys/`](packaging/keys/). Full release and packaging notes:
[`docs/releasing.md`](docs/releasing.md).

## Development workflow

- **CI** on every PR / push to `main`: rustfmt, clippy (`-D warnings`), tests,
  aarch64 smoke cross-compile. CI never publishes packages.
- **Releases** use [Conventional Commits](https://www.conventionalcommits.org/)
  and [release-please](https://github.com/googleapis/release-please). Merges
  that are only `chore` / `docs` / `ci` / … do not cut a version; releasable
  commits accumulate in a Release PR that a human merges when ready.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or see
  <http://www.apache.org/licenses/LICENSE-2.0>), or
- MIT license ([LICENSE-MIT](LICENSE-MIT) or see
  <http://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this work shall be dual-licensed as above, without additional
terms or conditions.
