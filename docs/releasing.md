# Releasing Ephemeris

Ephemeris uses [Conventional Commits](https://www.conventionalcommits.org/) and
[release-please](https://github.com/googleapis/release-please). Not every merge
to `main` cuts a release; releasable commits accumulate in a **Release PR** that
a human merges when ready.

## Versioning

| Commit type | Effect on next release |
|-------------|------------------------|
| `feat:` | minor bump |
| `fix:` | patch bump |
| `feat!:` / `fix!:` / `BREAKING CHANGE:` footer | major bump |
| `chore:`, `docs:`, `ci:`, `refactor:`, `test:`, `style:` | no release (still land on `main`) |

Examples:

```text
feat: add note search
fix: correct Retina surface sizing on macOS
chore: update beads status
ci: tighten clippy flags
```

Workspace crate versions are shared via `[workspace.package].version` in the
root `Cargo.toml`. release-please bumps that field and updates `CHANGELOG.md`.

## CI vs release

- **CI** ([`.github/workflows/ci.yml`](../.github/workflows/ci.yml)): runs on
  every PR and push to `main` (fmt, clippy, tests, aarch64 smoke cross-build).
  It never publishes a release or apt packages.
- **Release Please** ([`.github/workflows/release-please.yml`](../.github/workflows/release-please.yml)):
  on push to `main`, opens/updates the Release PR. When that PR is merged and a
  release is created, it calls the release workflow.
- **Release** ([`.github/workflows/release.yml`](../.github/workflows/release.yml)):
  cross-builds `aarch64-unknown-linux-gnu`, packages an `arm64` `.deb`, attaches
  assets to the GitHub Release, and publishes a GPG-signed apt repo to the
  `gh-pages` branch (GitHub Pages).

## Cut a release

1. Land work on `main` with Conventional Commit messages.
2. Wait for release-please to open or update the **Release Please** PR
   (changelog + version bump). Merge it when you want to ship.
3. The release workflow builds artifacts and updates the apt repo.

To rebuild packaging for an existing tag without a new Release PR, run the
**Release** workflow manually (`workflow_dispatch`) and pass the tag
(e.g. `v0.2.0`).

## Local packaging

Requires [`cross`](https://github.com/cross-rs/cross) and `dpkg-deb`:

```bash
make deb-aarch64
# → dist/ephemeris_<version>_arm64.deb
```

Or:

```bash
cross build --target aarch64-unknown-linux-gnu -p ephemeris-app --release
scripts/build-deb.sh target/aarch64-unknown-linux-gnu/release/ephemeris 0.1.0 dist/
```

## One-time setup (human)

### 1. Apt signing key

Generate a **dedicated** signing key (do not reuse a personal email key):

```bash
gpg --batch --passphrase-file <(echo 'YOUR_PASSPHRASE') \
  --quick-generate-key 'Ephemeris APT <apt@ephemeris.local>' default default 5y

gpg --armor --export-secret-keys 'apt@ephemeris.local'   # → APT_GPG_PRIVATE_KEY
gpg --armor --export 'apt@ephemeris.local'               # public; also published by CI
```

Add repository secrets (Settings → Secrets and variables → Actions):

| Secret | Value |
|--------|--------|
| `APT_GPG_PRIVATE_KEY` | Full armored private key block |
| `APT_GPG_PASSPHRASE` | Key passphrase (omit if the key has none) |

Optional repository variable:

| Variable | Value |
|----------|--------|
| `APT_GPG_NAME` | Key user-id (default `apt@ephemeris.local`) |

### 2. GitHub Pages

1. Settings → Pages → Build and deployment → Source: **Deploy from a branch**.
2. Branch: **`gh-pages`** / `/ (root)`.
3. The first successful release job creates/updates `gh-pages`.

Site URL (default project Pages):

`https://steffen030.github.io/ephemeris/`

## PineNote: install from the apt repo

On the device (Debian/Mobian aarch64):

```bash
curl -fsSL https://steffen030.github.io/ephemeris/ephemeris-archive-keyring.gpg \
  | sudo tee /usr/share/keyrings/ephemeris-archive-keyring.gpg >/dev/null

echo "deb [signed-by=/usr/share/keyrings/ephemeris-archive-keyring.gpg arch=arm64] https://steffen030.github.io/ephemeris stable main" \
  | sudo tee /etc/apt/sources.list.d/ephemeris.list

sudo apt update
sudo apt install ephemeris
```

Upgrades after later releases:

```bash
sudo apt update && sudo apt upgrade ephemeris
```
