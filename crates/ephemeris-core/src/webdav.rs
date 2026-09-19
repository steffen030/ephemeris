//! WebDAV backend wrapper for the Ephemeris sync engine.
//!
//! This module is compiled only when the **`webdav`** Cargo feature is active:
//!
//! ```toml
//! ephemeris-core = { …, features = ["webdav"] }
//! ```
//!
//! # Design
//!
//! [`WebDavClient`] is a thin async wrapper around [`reqwest_dav::Client`].  It
//! hides the underlying crate's builder ceremony behind a simple
//! `WebDavClient::new(base_url, username, password)` constructor and exposes
//! only the five operations the sync engine needs:
//!
//! | method | description |
//! |--------|-------------|
//! | [`list`]  | `PROPFIND depth:1` — list a collection |
//! | [`get`]   | `GET` — download a resource as raw bytes |
//! | [`put`]   | `PUT` — upload bytes to a path |
//! | [`delete`]| `DELETE` — remove a resource |
//! | [`mkcol`] | `MKCOL` — create a collection (directory) |
//!
//! All methods are `async` and return `crate::Result<T>`, mapping
//! [`reqwest_dav::types::Error`] into [`crate::error::AppError::Backend`].
//!
//! # Live integration tests
//!
//! Real network calls require a running WebDAV server and are therefore not
//! included here.  They are deferred to a future bead (live integration tests
//! against a local `rclone serve webdav` or Nextcloud container).  The unit
//! tests in this file cover only the pure, I/O-free helpers: URL/path joining
//! and config construction.
//!
//! [`list`]: WebDavClient::list
//! [`get`]:  WebDavClient::get
//! [`put`]:  WebDavClient::put
//! [`delete`]: WebDavClient::delete
//! [`mkcol`]: WebDavClient::mkcol

use bytes::Bytes;
pub use reqwest_dav::list_cmd::ListEntity;
use reqwest_dav::{Auth, Client, ClientBuilder, Depth};

use crate::error::{AppError, Result};

// ---------------------------------------------------------------------------
// Error conversion
// ---------------------------------------------------------------------------

/// Convert a `reqwest_dav` error into an [`AppError::Backend`].
fn dav_err(e: reqwest_dav::types::Error) -> AppError {
    AppError::Backend(format!("{e:?}"))
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// Credentials for a WebDAV endpoint (basic auth only).
///
/// Build this with [`WebDavConfig::new`] and pass it to
/// [`WebDavClient::with_config`].
#[derive(Debug, Clone)]
pub struct WebDavConfig {
    /// Fully-qualified base URL of the WebDAV root, e.g.
    /// `https://cloud.example.com/remote.php/dav/files/alice`.
    pub base_url: String,
    /// Username for HTTP Basic authentication.
    pub username: String,
    /// Password (or app-token) for HTTP Basic authentication.
    pub password: String,
}

impl WebDavConfig {
    /// Create a new config.
    ///
    /// `base_url` should not have a trailing slash; one is added internally
    /// when required.
    pub fn new(
        base_url: impl Into<String>,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            username: username.into(),
            password: password.into(),
        }
    }
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// Async WebDAV client for the Ephemeris sync engine.
///
/// Constructed via [`WebDavClient::new`] (convenience) or
/// [`WebDavClient::with_config`] (when you already have a [`WebDavConfig`]).
///
/// # Example
///
/// `ephemeris-core` does not itself depend on an async runtime, so this
/// example is `ignore`d by doctests; the sync engine drives these calls on the
/// tokio runtime it owns.
///
/// ```rust,ignore
/// use ephemeris_core::webdav::WebDavClient;
///
/// # async fn run() -> ephemeris_core::Result<()> {
/// let client = WebDavClient::new(
///     "https://cloud.example.com/dav",
///     "alice",
///     "secret",
/// )?;
///
/// client.mkcol("/calendar/2024").await?;
/// client.put("/calendar/2024/event.ics", b"BEGIN:VCALENDAR".to_vec()).await?;
/// let data = client.get("/calendar/2024/event.ics").await?;
/// println!("downloaded {} bytes", data.len());
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct WebDavClient {
    inner: Client,
}

impl WebDavClient {
    /// Build a client from individual parts (convenience wrapper around
    /// [`WebDavClient::with_config`]).
    pub fn new(
        base_url: impl Into<String>,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Result<Self> {
        Self::with_config(WebDavConfig::new(base_url, username, password))
    }

    /// Build a client from a [`WebDavConfig`].
    pub fn with_config(cfg: WebDavConfig) -> Result<Self> {
        let inner = ClientBuilder::new()
            .set_host(cfg.base_url)
            .set_auth(Auth::Basic(cfg.username, cfg.password))
            .build()
            .map_err(dav_err)?;
        Ok(Self { inner })
    }

    // -----------------------------------------------------------------------
    // Core operations
    // -----------------------------------------------------------------------

    /// List a WebDAV collection at `path` (depth 1 — immediate children only).
    ///
    /// Returns a `Vec` of [`ListEntity`] items; each is either a `File` or a
    /// `Folder`.  The first element is typically the collection itself.
    ///
    /// # Errors
    ///
    /// Returns [`AppError::Backend`] on any network or protocol error.
    pub async fn list(&self, path: &str) -> Result<Vec<ListEntity>> {
        self.inner
            .list(path, Depth::Number(1))
            .await
            .map_err(dav_err)
    }

    /// Download the resource at `path` and return its body as raw bytes.
    ///
    /// # Errors
    ///
    /// Returns [`AppError::Backend`] on any network or protocol error, or if
    /// the server responds with a non-2xx status code.
    pub async fn get(&self, path: &str) -> Result<Bytes> {
        let response = self.inner.get(path).await.map_err(dav_err)?;
        response
            .bytes()
            .await
            .map_err(|e| AppError::Backend(format!("failed to read response body: {e:?}")))
    }

    /// Upload `body` to the resource at `path` (creates or overwrites).
    ///
    /// # Errors
    ///
    /// Returns [`AppError::Backend`] on any network or protocol error.
    pub async fn put(&self, path: &str, body: impl Into<Vec<u8>>) -> Result<()> {
        self.inner.put(path, body.into()).await.map_err(dav_err)
    }

    /// Delete the resource (file or collection) at `path`.
    ///
    /// Deleting a non-empty collection removes it recursively, which is
    /// standard WebDAV behaviour.
    ///
    /// # Errors
    ///
    /// Returns [`AppError::Backend`] on any network or protocol error.
    pub async fn delete(&self, path: &str) -> Result<()> {
        self.inner.delete(path).await.map_err(dav_err)
    }

    /// Create a collection (directory) at `path` (`MKCOL`).
    ///
    /// All intermediate path segments must already exist; WebDAV does not
    /// support recursive creation in a single `MKCOL` call.
    ///
    /// # Errors
    ///
    /// Returns [`AppError::Backend`] on any network or protocol error (e.g.
    /// `405 Method Not Allowed` when the collection already exists).
    pub async fn mkcol(&self, path: &str) -> Result<()> {
        self.inner.mkcol(path).await.map_err(dav_err)
    }
}

// ---------------------------------------------------------------------------
// Pure unit tests (no network)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------
    // WebDavConfig construction
    // ------------------------------------------------------------------

    #[test]
    fn config_stores_fields() {
        let cfg = WebDavConfig::new("https://dav.example.com/path", "alice", "s3cr3t");
        assert_eq!(cfg.base_url, "https://dav.example.com/path");
        assert_eq!(cfg.username, "alice");
        assert_eq!(cfg.password, "s3cr3t");
    }

    #[test]
    fn config_accepts_empty_credentials() {
        // Anonymous-style usage — empty strings are accepted at config level.
        let cfg = WebDavConfig::new("https://dav.example.com", "", "");
        assert!(cfg.username.is_empty());
        assert!(cfg.password.is_empty());
    }

    // ------------------------------------------------------------------
    // WebDavClient construction (pure; no I/O)
    // ------------------------------------------------------------------

    #[test]
    fn client_new_succeeds_with_valid_url() {
        // Construction requires only a non-empty host and performs no network
        // I/O, so a well-formed HTTPS URL must succeed offline.
        let result = WebDavClient::new("https://cloud.example.com/dav", "bob", "hunter2");
        assert!(result.is_ok(), "expected Ok, got {result:?}");
    }

    #[test]
    fn client_with_config_roundtrip() {
        let cfg = WebDavConfig::new("https://nextcloud.example.org/remote.php/dav", "u", "p");
        let result = WebDavClient::with_config(cfg);
        assert!(result.is_ok(), "expected Ok, got {result:?}");
    }

    #[test]
    fn client_construction_defers_url_validation() {
        // `reqwest_dav::ClientBuilder::build()` only requires a non-empty host;
        // it validates/parses the URL lazily at request time, not at
        // construction. So even a syntactically odd base URL constructs
        // successfully — the error would surface on the first live call, which
        // is covered by (deferred) live integration tests.
        let result = WebDavClient::new("not a url at all !!!", "u", "p");
        assert!(
            result.is_ok(),
            "construction should defer URL validation, got {result:?}"
        );
    }

    // ------------------------------------------------------------------
    // Error mapping
    // ------------------------------------------------------------------

    #[test]
    fn dav_err_produces_backend_variant() {
        // `MissingAuthContext` is the one reqwest_dav error variant we can
        // build without pulling in its transitive error types, so it is a
        // convenient probe for the mapping into `AppError::Backend`.
        let dav_error = reqwest_dav::types::Error::MissingAuthContext;
        let app_err = dav_err(dav_error);
        match app_err {
            AppError::Backend(msg) => assert!(!msg.is_empty()),
            other => panic!("expected Backend, got {other:?}"),
        }
    }
}
