# Changelog

## [0.1.1](https://github.com/steffen030/ephemeris/compare/v0.1.0...v0.1.1) (2026-10-08)


### Features

* **ephemeris-2ql.1:** define pal Display trait + mock backend ([4586b98](https://github.com/steffen030/ephemeris/commit/4586b98fec04ce47cd8c2f3c58c9befcd10bbf74))
* **ephemeris-core/ui:** RefreshScheduler + ink damage tracking (ephemeris-2ql.2/7) ([cab5f0e](https://github.com/steffen030/ephemeris/commit/cab5f0ead29db86962106161a5677e9030032f94))
* **ephemeris-core:** ICS https feed reader with real RFC 5545 parsing (ephemeris-yvu.1) ([e66d7d5](https://github.com/steffen030/ephemeris/commit/e66d7d510cc31b5df0af6f09105fc7a9af560628))
* **ephemeris-core:** layered TOML+XDG config system (ephemeris-fna.4) ([1b2a269](https://github.com/steffen030/ephemeris/commit/1b2a2696bb7557f0331aa6cccb32fe56505c80f5))
* **ephemeris-core:** pen-button action system + input routing (ephemeris-idd.5) ([941612f](https://github.com/steffen030/ephemeris/commit/941612f605a55a3e0d2511c0e85c21812b80acd0))
* **ephemeris-core:** profile model, storage, and active-profile state (ephemeris-uv4.1) ([2ad8b99](https://github.com/steffen030/ephemeris/commit/2ad8b9915babecf86fdface824095c15eca43259))
* **ephemeris-core:** SQLite storage layer + migrations (ephemeris-fna.3) ([ba51982](https://github.com/steffen030/ephemeris/commit/ba519827fd2833dcfcba7056d2315eae91d9a622))
* **ephemeris-core:** TaskProvider trait + LocalTaskProvider SQLite impl (ephemeris-4if.1) ([e35d1d0](https://github.com/steffen030/ephemeris/commit/e35d1d0c8867170db9822ca1f0e29f51b9259f3a))
* **ephemeris-core:** WebDAV backend wrapper via reqwest_dav (ephemeris-xo3.1) ([4155522](https://github.com/steffen030/ephemeris/commit/41555220d08c299fe5eb8c5b9466463421041913))
* **ephemeris-fna.2:** resolve merge conflict in lib.rs ([baeb440](https://github.com/steffen030/ephemeris/commit/baeb440c3eae68e0a49404edabae5947ac0c60a2))
* **ephemeris-fna.5:** add logging, error types, async runtime wiring ([7ffe963](https://github.com/steffen030/ephemeris/commit/7ffe963401b7e8ab9927d7c112397263115fe07f))
* **ephemeris-idd.2:** implement Wayland input backend (tablet_v2 + wl_touch) ([5318f0a](https://github.com/steffen030/ephemeris/commit/5318f0a7569904885507ecff97b6610d5d285ed8))
* **ephemeris-pal:** live Wayland compositor dispatch loop for WaylandInput (ephemeris-idd.9) ([0cf8488](https://github.com/steffen030/ephemeris/commit/0cf8488dd1c78eaeb938430fc1578690f2d461fd))
* **ephemeris-pal:** typed DisplayError for Display trait (ephemeris-2ql.10) ([b2859ee](https://github.com/steffen030/ephemeris/commit/b2859ee2df3a375f70b872d25e28264fda11e061))
* **ephemeris-ui:** fullscreen canvas view with live ink rendering (ephemeris-6iy.2) ([183d53b](https://github.com/steffen030/ephemeris/commit/183d53bf2466c4d5b73c3ca25b6acd13da6fdd9e))
* **ephemeris-ui:** implement Slint UI skeleton (ephemeris-ub6.1) ([3e0e1f4](https://github.com/steffen030/ephemeris/commit/3e0e1f475801fb23e0e6d2c8a16811b7137ff216))
* **ephemeris-ui:** pages + swipe navigation with auto-create (ephemeris-6iy.3) ([7581ada](https://github.com/steffen030/ephemeris/commit/7581ada495feab6164fd4bfffbcef8e00c324565))
* implement action system (pen-button modifier -&gt; actions) ([c405c03](https://github.com/steffen030/ephemeris/commit/c405c03a4d3161c28e1c92b7ad69f0d2048212eb))
* implement config system (TOML + XDG with layered overrides) ([8f83b99](https://github.com/steffen030/ephemeris/commit/8f83b994153a78100015df65e7eff782b493dee5))
* implement core domain models (Profile, Account, Note, Page, Task, CalendarEvent) ([817435c](https://github.com/steffen030/ephemeris/commit/817435c61e7cd4db8e3b8a4a3681aebb9e755bba))
* implement Display trait + PixelBuf + RefreshMode (2ql.8) ([866b9b1](https://github.com/steffen030/ephemeris/commit/866b9b1e2151126f48ec956a2bd1ab74546e96ac))
* implement ICS feed reader (read-only HTTPS feeds) ([884badc](https://github.com/steffen030/ephemeris/commit/884badc7b3a3c5d289ca97e234d7ae9273f4e738))
* implement logging, error types, and app wiring skeleton ([4841d92](https://github.com/steffen030/ephemeris/commit/4841d9247bff43c35374b93afb4213491c7236ff))
* implement Obsidian vault filesystem reader ([917be00](https://github.com/steffen030/ephemeris/commit/917be00add1a1a6484679718261628f9e839e3cf))
* implement ObsidianVault file access layer ([48b2a10](https://github.com/steffen030/ephemeris/commit/48b2a10e6bff8e5ef610b728eb7d0f56e13c8219))
* implement profile model and active-profile state ([d8106f9](https://github.com/steffen030/ephemeris/commit/d8106f94ff195df1c5f1ec794211a0b1fc3f02df))
* implement SQLite storage layer with migrations ([095d275](https://github.com/steffen030/ephemeris/commit/095d275fbc8b592d55cc8cabc191cb3833ba4f9e))
* implement stroke persistence in SQLite ([abcb8f4](https://github.com/steffen030/ephemeris/commit/abcb8f41fbd5a156a7bb4cef639d9767fc06d551))
* implement TaskProvider trait + local SQLite provider ([551c7dd](https://github.com/steffen030/ephemeris/commit/551c7dde51b0dc575906723c414c68fa46d4a5ac))
* implement UI skeleton with Slint (retained-mode framework) ([dbdb495](https://github.com/steffen030/ephemeris/commit/dbdb4957238cf27dbea6936cc4c56b60ee8aa903))
* implement Wayland input backend (tablet_v2 + wl_touch) ([28bc01f](https://github.com/steffen030/ephemeris/commit/28bc01f7db0d3f8bdda08778641f209d5deae1cd))
* implement WebDAV backend wrapper ([3b3411a](https://github.com/steffen030/ephemeris/commit/3b3411aee8694d6c64699417676eeff4d80068d4))
* MVP data layer — markdown tasks, calendar cache, task aggregator ([f35a294](https://github.com/steffen030/ephemeris/commit/f35a294a69cd8c1f02b0803d0cc798c1b289b95c))
* nav rail, note browser, rename dialog, and desktop ink input ([dd025b1](https://github.com/steffen030/ephemeris/commit/dd025b1b7448c2742ebb978ff7fe705c01f4a35e))
* page backgrounds, clippy cleanup, text rendering verification ([91a0094](https://github.com/steffen030/ephemeris/commit/91a0094ab1280139c931be792e74e84272aebe0d))
* PDF/vault export, note search, audio capture, and Connect UI ([ac10d0d](https://github.com/steffen030/ephemeris/commit/ac10d0d110578c686b8768c71e6322a3968e906f))


### Bug Fixes

* green CI for release pipeline deps and clippy ([ed631d8](https://github.com/steffen030/ephemeris/commit/ed631d8265d52d2968b50917fbad5dff6699f15b))
* use is_multiple_of() instead of manual modulo for grid pattern ([8ab3f8e](https://github.com/steffen030/ephemeris/commit/8ab3f8e1a61fa0863f9636cd6d5b76c69c00bfb4))
