# Ephemeris

An astronomer’s table for tracking work and life over time — a planner and
note-taking app focused on e-ink devices with pens (especially the PineNote).

See [ephemeris.md](ephemeris.md) for product motivation and feature notes.

## Development

```bash
make help          # list targets
make test          # workspace tests
make clippy        # lint (-D warnings in CI)
make run           # debug run
```

CI (fmt, clippy, tests, aarch64 cross-compile) runs on every PR and push to
`main`. It does **not** publish releases.

## Releases and PineNote installs

Versioning uses Conventional Commits and a human-gated
[release-please](https://github.com/googleapis/release-please) Release PR.
Signed `arm64` `.deb` packages are published to a GitHub Pages apt repository
for the PineNote.

Full details: **[docs/releasing.md](docs/releasing.md)**.
