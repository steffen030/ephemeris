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
  on push to `main`, opens/updates the Release PR and syncs `Cargo.lock` on that
  branch in the same run. When the Release PR is merged and a release is created,
  it **waits for CI on the same commit to succeed**, then calls the release
  workflow. If CI fails, packaging/publish is skipped.
- **Release** ([`.github/workflows/release.yml`](../.github/workflows/release.yml)):
  cross-builds `aarch64-unknown-linux-gnu`, packages an `arm64` `.deb`, attaches
  assets to the GitHub Release, and publishes a GPG-signed apt repo to the
  `gh-pages` branch (GitHub Pages).
- **Dependabot** ([`.github/dependabot.yml`](../.github/dependabot.yml)): weekly
  Cargo and monthly Actions updates. Patch/minor PRs are approved and
  auto-merged after CI is green
  ([`dependabot-auto-merge.yml`](../.github/workflows/dependabot-auto-merge.yml)).
  Majors stay manual. Cargo commits use `fix(deps):` (patch bump in the next
  Release PR); Actions bumps use `ci:` (no app version bump).

## Cut a release (one click)

1. Land work on `main` with Conventional Commit messages (Dependabot can land
   patch/minor deps on its own once CI passes and branch-protection reviews are
   satisfied by the auto-approve workflow).
2. When you want to ship, open the **Release Please** PR, approve if required,
   and merge it. No separate lockfile workflow to approve.
3. CI runs on the merge commit; once it passes, packaging and apt publish run.
   If packaging is skipped (empty GitHub Release assets), re-run the **Release**
   workflow with `workflow_dispatch` and the tag (e.g. `v0.1.7`).

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

### 0. Actions permissions, auto-merge, and branch protection

Settings → Actions → General → Workflow permissions:

- **Read and write permissions**
- **Allow GitHub Actions to create and approve pull requests**

Without this, release-please cannot open the Release PR, and Dependabot
auto-approve will fail.

Settings → General → Pull Requests:

- **Allow auto-merge**

Branch protection on `main` (recommended):

- Require a pull request before merging (your current setup).
- Require status checks before merging, and add these CI job names so
  Dependabot auto-merge waits on real green builds:
  `Rustfmt`, `Clippy`, `Tests`, `Cross-compile (aarch64-linux)`.
- Keep **Allow GitHub Actions to create and approve pull requests** so the
  Dependabot workflow can satisfy the required review for patch/minor PRs.
  The Release Please PR still needs your approval/merge (the one ship click).

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

### 2. Host the apt tree (`gh-pages`)

Each release pushes a signed apt repository to the **`gh-pages`** branch.
GitHub Pages serves it at:

**https://steffen030.github.io/ephemeris/**

Settings → Pages → Source: Deploy from branch → **`gh-pages`** / `/ (root)`.

The public signing key is also checked into
[`packaging/keys/ephemeris-archive-keyring.gpg`](../packaging/keys/ephemeris-archive-keyring.gpg).

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

The package pulls in **Tesseract** (`tesseract-ocr`, `tesseract-ocr-eng`) for
handwriting See-text / searchable PDF export (OCR cascade fallback), and
**bundles `whisper-cli`** (plus its shared libraries under
`/usr/lib/ephemeris/whisper/`) because `whisper.cpp` is not in Debian trixie
yet. Voice transcription still needs a model: open Settings → Transcription →
Download after install.

**Stroke HTR** (preferred See-text path) is not shipped inside the `.deb`.
Build/export a Carbune/OnlineHTR ONNX with `scripts/export-htr-model.sh`, upload
`ephemeris-htr-v1.zip` as a GitHub Release asset under tag `htr-model-v1`, then
users install via Settings → Handwriting → Download (or copy `model.onnx` +
`charset.txt` into `~/.local/share/ephemeris/htr/`). Without the model, See-text
falls back to Tesseract.

Upgrades after later releases:

```bash
sudo apt update && sudo apt upgrade ephemeris
```
