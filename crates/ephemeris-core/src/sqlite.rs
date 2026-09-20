//! SQLite-backed storage layer for Ephemeris (ADR ephemeris-pir, ephemeris-fna.3).
//!
//! Enabled with the `sqlite` feature flag. Provides [`SqliteStore`] — a
//! single-connection store that covers all core domain models. Migrations are
//! versioned and run automatically on [`SqliteStore::open`] /
//! [`SqliteStore::open_in_memory`].
//!
//! ## Schema summary
//!
//! | Table            | Primary key   | Notes                                        |
//! |------------------|---------------|----------------------------------------------|
//! | `schema_version` | `version`     | Migration tracking                           |
//! | `profiles`       | `id` (UUID)   |                                              |
//! | `accounts`       | `id` (UUID)   | `config` stored as JSON object               |
//! | `notes`          | `id` (UUID)   |                                              |
//! | `pages`          | `id` (UUID)   | `template` stored as TEXT                    |
//! | `tasks`          | `id` (UUID)   | `tags` stored as JSON array                  |
//! | `events`         | `id` (UUID)   |                                              |
//! | `strokes`        | `id` (UUID)   | `points` stored as compact JSON array        |
//!
//! ## Stroke serialization
//!
//! `Stroke.points: Vec<Point>` is serialized as a **JSON array** using
//! `serde_json`.  Each `Point` is `{x,y,pressure,tilt,t_ms}`.  This keeps the
//! schema human-readable and avoids platform-endian issues.  If write
//! performance becomes critical, replace the `points` column with a packed
//! binary blob (5×f32 + u32 = 24 bytes/point); the column name is unchanged so
//! the migration version bump handles the format change.

#![cfg(feature = "sqlite")]

use std::path::Path;

use rusqlite::{params, Connection};

use crate::{
    model::{
        Account, AccountId, AccountKind, CalendarEvent, Color, EventId, Note, NoteId, Page, PageId,
        PageTemplate, Point, Profile, ProfileId, Stroke, StrokeId, Task, TaskId, TaskPriority,
        Tool,
    },
    AppError, Result,
};

// ── Schema migrations ────────────────────────────────────────────────────────

/// Current schema version. Bump this and add a migration step in
/// [`run_migrations`] whenever the schema changes.
const CURRENT_VERSION: u32 = 2;

/// The migration SQL for each version level, indexed `0..CURRENT_VERSION`.
/// Entry `i` takes the schema from version `i` to `i+1`.
static MIGRATIONS: &[&str] = &[
    // 0 → 1: initial schema
    r#"
    CREATE TABLE IF NOT EXISTS schema_version (
        version INTEGER NOT NULL
    );
    INSERT INTO schema_version (version) VALUES (1);

    CREATE TABLE IF NOT EXISTS profiles (
        id   TEXT NOT NULL PRIMARY KEY,
        name TEXT NOT NULL
    );

    CREATE TABLE IF NOT EXISTS accounts (
        id         TEXT NOT NULL PRIMARY KEY,
        profile_id TEXT NOT NULL REFERENCES profiles(id),
        kind       TEXT NOT NULL,
        name       TEXT NOT NULL,
        config     TEXT NOT NULL DEFAULT '{}'
    );

    CREATE TABLE IF NOT EXISTS notes (
        id      TEXT NOT NULL PRIMARY KEY,
        title   TEXT NOT NULL,
        created INTEGER NOT NULL,
        updated INTEGER NOT NULL
    );

    CREATE TABLE IF NOT EXISTS pages (
        id       TEXT    NOT NULL PRIMARY KEY,
        note_id  TEXT    NOT NULL REFERENCES notes(id),
        idx      INTEGER NOT NULL,
        template TEXT    NOT NULL DEFAULT 'Blank'
    );

    CREATE TABLE IF NOT EXISTS tasks (
        id         TEXT    NOT NULL PRIMARY KEY,
        profile_id TEXT    NOT NULL REFERENCES profiles(id),
        title      TEXT    NOT NULL,
        due        INTEGER,
        priority   INTEGER NOT NULL DEFAULT 1,
        tags       TEXT    NOT NULL DEFAULT '[]',
        source     TEXT    NOT NULL DEFAULT 'local',
        done       INTEGER NOT NULL DEFAULT 0
    );

    CREATE TABLE IF NOT EXISTS events (
        id         TEXT    NOT NULL PRIMARY KEY,
        profile_id TEXT    NOT NULL REFERENCES profiles(id),
        uid        TEXT    NOT NULL,
        start      INTEGER NOT NULL,
        end        INTEGER NOT NULL,
        title      TEXT    NOT NULL,
        location   TEXT,
        source     TEXT    NOT NULL DEFAULT 'calendar'
    );

    CREATE TABLE IF NOT EXISTS strokes (
        id         TEXT NOT NULL PRIMARY KEY,
        page_id    TEXT NOT NULL REFERENCES pages(id),
        tool       TEXT NOT NULL,
        color_r    INTEGER NOT NULL,
        color_g    INTEGER NOT NULL,
        color_b    INTEGER NOT NULL,
        color_a    INTEGER NOT NULL,
        base_width REAL    NOT NULL,
        -- Points serialized as a compact JSON array: [{x,y,pressure,tilt,t_ms},…]
        points     TEXT    NOT NULL DEFAULT '[]',
        -- Insertion order (z-order for re-rendering).
        z_order    INTEGER NOT NULL
    );
    "#,
    // 1 → 2: app_state key-value table for persisted app state (e.g. active_profile_id)
    r#"
    CREATE TABLE IF NOT EXISTS app_state (
        key   TEXT NOT NULL PRIMARY KEY,
        value TEXT NOT NULL
    );
    UPDATE schema_version SET version = 2;
    "#,
];

/// Apply any pending migrations to bring the connection up to
/// [`CURRENT_VERSION`].  Safe to call on an already-current database.
fn run_migrations(conn: &Connection) -> Result<()> {
    // Check if schema_version table exists.
    let table_exists: u32 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='schema_version'",
            [],
            |row| row.get::<_, u32>(0),
        )
        .map_err(|e| AppError::Storage(format!("migration probe failed: {e}")))?;

    // Determine current version: 0 if schema_version table doesn't exist yet.
    let current: u32 = if table_exists > 0 {
        conn.query_row("SELECT version FROM schema_version LIMIT 1", [], |row| {
            row.get::<_, u32>(0)
        })
        .unwrap_or(0)
    } else {
        0
    };

    for ver in current..CURRENT_VERSION {
        let sql = MIGRATIONS[ver as usize];
        conn.execute_batch(sql)
            .map_err(|e| AppError::Storage(format!("migration {ver}→{} failed: {e}", ver + 1)))?;
    }
    Ok(())
}

// ── Helper conversions ───────────────────────────────────────────────────────

fn account_kind_to_str(k: AccountKind) -> &'static str {
    match k {
        AccountKind::Email => "Email",
        AccountKind::CalDAV => "CalDAV",
        AccountKind::Obsidian => "Obsidian",
        AccountKind::WebDAV => "WebDAV",
        AccountKind::Local => "Local",
    }
}

fn account_kind_from_str(s: &str) -> Result<AccountKind> {
    match s {
        "Email" => Ok(AccountKind::Email),
        "CalDAV" => Ok(AccountKind::CalDAV),
        "Obsidian" => Ok(AccountKind::Obsidian),
        "WebDAV" => Ok(AccountKind::WebDAV),
        "Local" => Ok(AccountKind::Local),
        other => Err(AppError::Storage(format!("unknown AccountKind: {other}"))),
    }
}

fn page_template_to_str(t: PageTemplate) -> &'static str {
    match t {
        PageTemplate::Blank => "Blank",
        PageTemplate::Lines => "Lines",
        PageTemplate::Grid => "Grid",
        PageTemplate::Dot => "Dot",
    }
}

fn page_template_from_str(s: &str) -> Result<PageTemplate> {
    match s {
        "Blank" => Ok(PageTemplate::Blank),
        "Lines" => Ok(PageTemplate::Lines),
        "Grid" => Ok(PageTemplate::Grid),
        "Dot" => Ok(PageTemplate::Dot),
        other => Err(AppError::Storage(format!("unknown PageTemplate: {other}"))),
    }
}

fn tool_to_str(t: Tool) -> &'static str {
    match t {
        Tool::Pen => "Pen",
        Tool::Highlighter => "Highlighter",
        Tool::Eraser => "Eraser",
    }
}

fn tool_from_str(s: &str) -> Result<Tool> {
    match s {
        "Pen" => Ok(Tool::Pen),
        "Highlighter" => Ok(Tool::Highlighter),
        "Eraser" => Ok(Tool::Eraser),
        other => Err(AppError::Storage(format!("unknown Tool: {other}"))),
    }
}

fn storage_err(e: rusqlite::Error) -> AppError {
    AppError::Storage(e.to_string())
}

fn serde_err(e: serde_json::Error) -> AppError {
    AppError::Storage(format!("serialization: {e}"))
}

// ── SqliteStore ──────────────────────────────────────────────────────────────

/// A SQLite-backed store for all Ephemeris core domain models.
///
/// Opens (or creates) a database file on [`SqliteStore::open`], or a
/// temporary in-memory database on [`SqliteStore::open_in_memory`].  Both
/// constructors run pending migrations automatically.
///
/// # Thread safety
///
/// `SqliteStore` holds a single [`rusqlite::Connection`] which is `!Send`.
/// Wrap in a `Mutex<SqliteStore>` for shared access across threads.
pub struct SqliteStore {
    conn: Connection,
}

impl SqliteStore {
    /// Open (or create) the database at `path`, running any pending migrations.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path).map_err(storage_err)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
            .map_err(storage_err)?;
        run_migrations(&conn)?;
        Ok(Self { conn })
    }

    /// Open a temporary in-memory database, running migrations immediately.
    ///
    /// Useful for tests and ephemeral sessions that don't need persistence.
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().map_err(storage_err)?;
        conn.execute_batch("PRAGMA foreign_keys=ON;")
            .map_err(storage_err)?;
        run_migrations(&conn)?;
        Ok(Self { conn })
    }

    // ── Profiles ─────────────────────────────────────────────────────────────

    /// Insert or replace a profile.
    pub fn upsert_profile(&self, profile: &Profile) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR REPLACE INTO profiles (id, name) VALUES (?1, ?2)",
                params![profile.id.0.to_string(), profile.name],
            )
            .map_err(storage_err)?;
        Ok(())
    }

    /// Load a profile by id.  Returns `None` if not found.
    pub fn get_profile(&self, id: ProfileId) -> Result<Option<Profile>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, name FROM profiles WHERE id = ?1")
            .map_err(storage_err)?;
        match stmt.query_row(params![id.0.to_string()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        }) {
            Ok((id_str, name)) => {
                let uuid = id_str
                    .parse()
                    .map_err(|e| AppError::Storage(format!("bad profile uuid: {e}")))?;
                Ok(Some(Profile {
                    id: ProfileId(uuid),
                    name,
                }))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(storage_err(e)),
        }
    }

    /// List all profiles.
    pub fn list_profiles(&self) -> Result<Vec<Profile>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, name FROM profiles")
            .map_err(storage_err)?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(storage_err)?;
        let mut out = Vec::new();
        for row in rows {
            let (id_str, name) = row.map_err(storage_err)?;
            let id = id_str
                .parse()
                .map_err(|e| AppError::Storage(format!("bad profile uuid: {e}")))?;
            out.push(Profile {
                id: ProfileId(id),
                name,
            });
        }
        Ok(out)
    }

    /// Rename a profile.  Returns `true` if the row was found and updated.
    pub fn rename_profile(&self, id: ProfileId, new_name: &str) -> Result<bool> {
        let n = self
            .conn
            .execute(
                "UPDATE profiles SET name = ?1 WHERE id = ?2",
                params![new_name, id.0.to_string()],
            )
            .map_err(storage_err)?;
        Ok(n > 0)
    }

    /// Delete a profile by id.  Returns `true` if the row was present.
    pub fn delete_profile(&self, id: ProfileId) -> Result<bool> {
        let n = self
            .conn
            .execute(
                "DELETE FROM profiles WHERE id = ?1",
                params![id.0.to_string()],
            )
            .map_err(storage_err)?;
        Ok(n > 0)
    }

    // ── App state ─────────────────────────────────────────────────────────────

    /// Read an app-state value by key.  Returns `None` if not set.
    pub fn get_app_state(&self, key: &str) -> Result<Option<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT value FROM app_state WHERE key = ?1")
            .map_err(storage_err)?;
        match stmt.query_row(params![key], |row| row.get::<_, String>(0)) {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(storage_err(e)),
        }
    }

    /// Write (upsert) an app-state key-value pair.
    pub fn set_app_state(&self, key: &str, value: &str) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR REPLACE INTO app_state (key, value) VALUES (?1, ?2)",
                params![key, value],
            )
            .map_err(storage_err)?;
        Ok(())
    }

    /// Persist the active profile id.
    pub fn set_active_profile_id(&self, id: ProfileId) -> Result<()> {
        self.set_app_state("active_profile_id", &id.0.to_string())
    }

    /// Load the active profile id, if one has been persisted.
    pub fn get_active_profile_id(&self) -> Result<Option<ProfileId>> {
        match self.get_app_state("active_profile_id")? {
            None => Ok(None),
            Some(s) => {
                let uuid = s
                    .parse()
                    .map_err(|e| AppError::Storage(format!("bad active profile uuid: {e}")))?;
                Ok(Some(ProfileId(uuid)))
            }
        }
    }

    /// Clear the persisted active profile (e.g. after deleting the active profile).
    pub fn clear_active_profile_id(&self) -> Result<()> {
        self.conn
            .execute("DELETE FROM app_state WHERE key = 'active_profile_id'", [])
            .map_err(storage_err)?;
        Ok(())
    }

    // ── Accounts ──────────────────────────────────────────────────────────────

    /// Insert or replace an account.
    pub fn upsert_account(&self, account: &Account) -> Result<()> {
        let config_json = serde_json::to_string(&account.config).map_err(serde_err)?;
        self.conn
            .execute(
                "INSERT OR REPLACE INTO accounts (id, profile_id, kind, name, config)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    account.id.0.to_string(),
                    account.profile_id.0.to_string(),
                    account_kind_to_str(account.kind),
                    account.name,
                    config_json,
                ],
            )
            .map_err(storage_err)?;
        Ok(())
    }

    /// Load an account by id.  Returns `None` if not found.
    pub fn get_account(&self, id: AccountId) -> Result<Option<Account>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, profile_id, kind, name, config FROM accounts WHERE id = ?1")
            .map_err(storage_err)?;
        match stmt.query_row(params![id.0.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        }) {
            Ok((id_s, pid_s, kind_s, name, cfg_s)) => {
                let id = id_s
                    .parse()
                    .map_err(|e| AppError::Storage(format!("bad account uuid: {e}")))?;
                let profile_id = pid_s
                    .parse()
                    .map_err(|e| AppError::Storage(format!("bad profile uuid: {e}")))?;
                let kind = account_kind_from_str(&kind_s)?;
                let config = serde_json::from_str(&cfg_s).map_err(serde_err)?;
                Ok(Some(Account {
                    id: AccountId(id),
                    profile_id: ProfileId(profile_id),
                    kind,
                    name,
                    config,
                }))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(storage_err(e)),
        }
    }

    /// List all accounts belonging to a profile.
    pub fn list_accounts_for_profile(&self, profile_id: ProfileId) -> Result<Vec<Account>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, profile_id, kind, name, config
                 FROM accounts WHERE profile_id = ?1",
            )
            .map_err(storage_err)?;
        let rows = stmt
            .query_map(params![profile_id.0.to_string()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })
            .map_err(storage_err)?;
        let mut out = Vec::new();
        for row in rows {
            let (id_s, pid_s, kind_s, name, cfg_s) = row.map_err(storage_err)?;
            let id = id_s
                .parse()
                .map_err(|e| AppError::Storage(format!("bad account uuid: {e}")))?;
            let pid = pid_s
                .parse()
                .map_err(|e| AppError::Storage(format!("bad profile uuid: {e}")))?;
            let kind = account_kind_from_str(&kind_s)?;
            let config = serde_json::from_str(&cfg_s).map_err(serde_err)?;
            out.push(Account {
                id: AccountId(id),
                profile_id: ProfileId(pid),
                kind,
                name,
                config,
            });
        }
        Ok(out)
    }

    /// Delete an account by id.  Returns `true` if the row was present.
    pub fn delete_account(&self, id: AccountId) -> Result<bool> {
        let n = self
            .conn
            .execute(
                "DELETE FROM accounts WHERE id = ?1",
                params![id.0.to_string()],
            )
            .map_err(storage_err)?;
        Ok(n > 0)
    }

    // ── Notes ─────────────────────────────────────────────────────────────────

    /// Insert or replace a note.
    pub fn upsert_note(&self, note: &Note) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR REPLACE INTO notes (id, title, created, updated)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    note.id.0.to_string(),
                    note.title,
                    note.created as i64,
                    note.updated as i64,
                ],
            )
            .map_err(storage_err)?;
        Ok(())
    }

    /// Load a note by id.  Returns `None` if not found.
    pub fn get_note(&self, id: NoteId) -> Result<Option<Note>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, title, created, updated FROM notes WHERE id = ?1")
            .map_err(storage_err)?;
        match stmt.query_row(params![id.0.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
            ))
        }) {
            Ok((id_s, title, created, updated)) => {
                let id = id_s
                    .parse()
                    .map_err(|e| AppError::Storage(format!("bad note uuid: {e}")))?;
                Ok(Some(Note {
                    id: NoteId(id),
                    title,
                    created: created as u64,
                    updated: updated as u64,
                }))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(storage_err(e)),
        }
    }

    /// List all notes, ordered by `created` ascending.
    pub fn list_notes(&self) -> Result<Vec<Note>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, title, created, updated FROM notes ORDER BY created ASC")
            .map_err(storage_err)?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })
            .map_err(storage_err)?;
        let mut out = Vec::new();
        for row in rows {
            let (id_s, title, created, updated) = row.map_err(storage_err)?;
            let id = id_s
                .parse()
                .map_err(|e| AppError::Storage(format!("bad note uuid: {e}")))?;
            out.push(Note {
                id: NoteId(id),
                title,
                created: created as u64,
                updated: updated as u64,
            });
        }
        Ok(out)
    }

    /// Delete a note by id.  Returns `true` if the row was present.
    pub fn delete_note(&self, id: NoteId) -> Result<bool> {
        let n = self
            .conn
            .execute("DELETE FROM notes WHERE id = ?1", params![id.0.to_string()])
            .map_err(storage_err)?;
        Ok(n > 0)
    }

    // ── Pages ─────────────────────────────────────────────────────────────────

    /// Insert or replace a page.
    pub fn upsert_page(&self, page: &Page) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR REPLACE INTO pages (id, note_id, idx, template)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    page.id.0.to_string(),
                    page.note_id.0.to_string(),
                    page.index as i64,
                    page_template_to_str(page.template),
                ],
            )
            .map_err(storage_err)?;
        Ok(())
    }

    /// Load a page by id.  Returns `None` if not found.
    pub fn get_page(&self, id: PageId) -> Result<Option<Page>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, note_id, idx, template FROM pages WHERE id = ?1")
            .map_err(storage_err)?;
        match stmt.query_row(params![id.0.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
            ))
        }) {
            Ok((id_s, nid_s, idx, tmpl_s)) => {
                let id = id_s
                    .parse()
                    .map_err(|e| AppError::Storage(format!("bad page uuid: {e}")))?;
                let note_id = nid_s
                    .parse()
                    .map_err(|e| AppError::Storage(format!("bad note uuid: {e}")))?;
                let template = page_template_from_str(&tmpl_s)?;
                Ok(Some(Page {
                    id: PageId(id),
                    note_id: NoteId(note_id),
                    index: idx as usize,
                    template,
                }))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(storage_err(e)),
        }
    }

    /// List all pages belonging to a note, ordered by `idx` ascending.
    pub fn list_pages_for_note(&self, note_id: NoteId) -> Result<Vec<Page>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, note_id, idx, template
                 FROM pages WHERE note_id = ?1 ORDER BY idx ASC",
            )
            .map_err(storage_err)?;
        let rows = stmt
            .query_map(params![note_id.0.to_string()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })
            .map_err(storage_err)?;
        let mut out = Vec::new();
        for row in rows {
            let (id_s, nid_s, idx, tmpl_s) = row.map_err(storage_err)?;
            let id = id_s
                .parse()
                .map_err(|e| AppError::Storage(format!("bad page uuid: {e}")))?;
            let note_id = nid_s
                .parse()
                .map_err(|e| AppError::Storage(format!("bad note uuid: {e}")))?;
            let template = page_template_from_str(&tmpl_s)?;
            out.push(Page {
                id: PageId(id),
                note_id: NoteId(note_id),
                index: idx as usize,
                template,
            });
        }
        Ok(out)
    }

    /// Delete a page by id.  Returns `true` if the row was present.
    pub fn delete_page(&self, id: PageId) -> Result<bool> {
        let n = self
            .conn
            .execute("DELETE FROM pages WHERE id = ?1", params![id.0.to_string()])
            .map_err(storage_err)?;
        Ok(n > 0)
    }

    // ── Tasks ─────────────────────────────────────────────────────────────────

    /// Insert or replace a task.
    pub fn upsert_task(&self, task: &Task) -> Result<()> {
        let tags_json = serde_json::to_string(&task.tags).map_err(serde_err)?;
        self.conn
            .execute(
                "INSERT OR REPLACE INTO tasks
                 (id, profile_id, title, due, priority, tags, source, done)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    task.id.0.to_string(),
                    task.profile_id.0.to_string(),
                    task.title,
                    task.due.map(|d| d as i64),
                    task.priority as i64,
                    tags_json,
                    task.source,
                    task.done as i64,
                ],
            )
            .map_err(storage_err)?;
        Ok(())
    }

    /// Load a task by id.  Returns `None` if not found.
    pub fn get_task(&self, id: TaskId) -> Result<Option<Task>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, profile_id, title, due, priority, tags, source, done
                 FROM tasks WHERE id = ?1",
            )
            .map_err(storage_err)?;
        match stmt.query_row(params![id.0.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<i64>>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, i64>(7)?,
            ))
        }) {
            Ok((id_s, pid_s, title, due, prio, tags_s, source, done)) => {
                let id = id_s
                    .parse()
                    .map_err(|e| AppError::Storage(format!("bad task uuid: {e}")))?;
                let profile_id = pid_s
                    .parse()
                    .map_err(|e| AppError::Storage(format!("bad profile uuid: {e}")))?;
                let priority = match prio {
                    0 => TaskPriority::Low,
                    2 => TaskPriority::High,
                    _ => TaskPriority::Medium,
                };
                let tags: Vec<String> = serde_json::from_str(&tags_s).map_err(serde_err)?;
                Ok(Some(Task {
                    id: TaskId(id),
                    profile_id: ProfileId(profile_id),
                    title,
                    due: due.map(|d| d as u64),
                    priority,
                    tags,
                    source,
                    done: done != 0,
                }))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(storage_err(e)),
        }
    }

    /// List all tasks for a profile.
    pub fn list_tasks_for_profile(&self, profile_id: ProfileId) -> Result<Vec<Task>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, profile_id, title, due, priority, tags, source, done
                 FROM tasks WHERE profile_id = ?1",
            )
            .map_err(storage_err)?;
        let rows = stmt
            .query_map(params![profile_id.0.to_string()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, i64>(7)?,
                ))
            })
            .map_err(storage_err)?;
        let mut out = Vec::new();
        for row in rows {
            let (id_s, pid_s, title, due, prio, tags_s, source, done) = row.map_err(storage_err)?;
            let id = id_s
                .parse()
                .map_err(|e| AppError::Storage(format!("bad task uuid: {e}")))?;
            let pid = pid_s
                .parse()
                .map_err(|e| AppError::Storage(format!("bad profile uuid: {e}")))?;
            let priority = match prio {
                0 => TaskPriority::Low,
                2 => TaskPriority::High,
                _ => TaskPriority::Medium,
            };
            let tags: Vec<String> = serde_json::from_str(&tags_s).map_err(serde_err)?;
            out.push(Task {
                id: TaskId(id),
                profile_id: ProfileId(pid),
                title,
                due: due.map(|d| d as u64),
                priority,
                tags,
                source,
                done: done != 0,
            });
        }
        Ok(out)
    }

    /// Delete a task by id.  Returns `true` if the row was present.
    pub fn delete_task(&self, id: TaskId) -> Result<bool> {
        let n = self
            .conn
            .execute("DELETE FROM tasks WHERE id = ?1", params![id.0.to_string()])
            .map_err(storage_err)?;
        Ok(n > 0)
    }

    // ── Calendar events ───────────────────────────────────────────────────────

    /// Insert or replace a calendar event.
    pub fn upsert_event(&self, event: &CalendarEvent) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR REPLACE INTO events
                 (id, profile_id, uid, start, end, title, location, source)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    event.id.0.to_string(),
                    event.profile_id.0.to_string(),
                    event.uid,
                    event.start as i64,
                    event.end as i64,
                    event.title,
                    event.location.as_deref(),
                    event.source,
                ],
            )
            .map_err(storage_err)?;
        Ok(())
    }

    /// Load a calendar event by id.  Returns `None` if not found.
    pub fn get_event(&self, id: EventId) -> Result<Option<CalendarEvent>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, profile_id, uid, start, end, title, location, source
                 FROM events WHERE id = ?1",
            )
            .map_err(storage_err)?;
        match stmt.query_row(params![id.0.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, String>(7)?,
            ))
        }) {
            Ok((id_s, pid_s, uid, start, end, title, location, source)) => {
                let id = id_s
                    .parse()
                    .map_err(|e| AppError::Storage(format!("bad event uuid: {e}")))?;
                let profile_id = pid_s
                    .parse()
                    .map_err(|e| AppError::Storage(format!("bad profile uuid: {e}")))?;
                Ok(Some(CalendarEvent {
                    id: EventId(id),
                    profile_id: ProfileId(profile_id),
                    uid,
                    start: start as u64,
                    end: end as u64,
                    title,
                    location,
                    source,
                }))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(storage_err(e)),
        }
    }

    /// List all events for a profile.
    pub fn list_events_for_profile(&self, profile_id: ProfileId) -> Result<Vec<CalendarEvent>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, profile_id, uid, start, end, title, location, source
                 FROM events WHERE profile_id = ?1 ORDER BY start ASC",
            )
            .map_err(storage_err)?;
        let rows = stmt
            .query_map(params![profile_id.0.to_string()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, String>(7)?,
                ))
            })
            .map_err(storage_err)?;
        let mut out = Vec::new();
        for row in rows {
            let (id_s, pid_s, uid, start, end, title, location, source) =
                row.map_err(storage_err)?;
            let id = id_s
                .parse()
                .map_err(|e| AppError::Storage(format!("bad event uuid: {e}")))?;
            let pid = pid_s
                .parse()
                .map_err(|e| AppError::Storage(format!("bad profile uuid: {e}")))?;
            out.push(CalendarEvent {
                id: EventId(id),
                profile_id: ProfileId(pid),
                uid,
                start: start as u64,
                end: end as u64,
                title,
                location,
                source,
            });
        }
        Ok(out)
    }

    /// Delete a calendar event by id.  Returns `true` if the row was present.
    pub fn delete_event(&self, id: EventId) -> Result<bool> {
        let n = self
            .conn
            .execute(
                "DELETE FROM events WHERE id = ?1",
                params![id.0.to_string()],
            )
            .map_err(storage_err)?;
        Ok(n > 0)
    }

    // ── Strokes ───────────────────────────────────────────────────────────────

    /// Insert or replace a stroke.
    ///
    /// `z_order` tracks insertion order: new strokes get
    /// `MAX(z_order) + 1` unless the row already exists (replace preserves the
    /// existing `z_order` via the subquery).
    pub fn upsert_stroke(&self, page_id: PageId, stroke: &Stroke) -> Result<()> {
        // Points serialized as a compact JSON array (see module-level doc).
        let points_json = serde_json::to_string(&stroke.points).map_err(serde_err)?;
        self.conn
            .execute(
                "INSERT INTO strokes
                 (id, page_id, tool, color_r, color_g, color_b, color_a, base_width, points, z_order)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9,
                         COALESCE((SELECT z_order FROM strokes WHERE id = ?1),
                                  (SELECT COALESCE(MAX(z_order), 0) + 1 FROM strokes WHERE page_id = ?2)))
                 ON CONFLICT(id) DO UPDATE SET
                     tool       = excluded.tool,
                     color_r    = excluded.color_r,
                     color_g    = excluded.color_g,
                     color_b    = excluded.color_b,
                     color_a    = excluded.color_a,
                     base_width = excluded.base_width,
                     points     = excluded.points",
                params![
                    stroke.id.0.to_string(),
                    page_id.0.to_string(),
                    tool_to_str(stroke.tool),
                    stroke.color.r as i64,
                    stroke.color.g as i64,
                    stroke.color.b as i64,
                    stroke.color.a as i64,
                    stroke.base_width as f64,
                    points_json,
                ],
            )
            .map_err(storage_err)?;
        Ok(())
    }

    /// Load a stroke by id.  Returns `None` if not found.
    pub fn get_stroke(&self, id: StrokeId) -> Result<Option<Stroke>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, tool, color_r, color_g, color_b, color_a, base_width, points
                 FROM strokes WHERE id = ?1",
            )
            .map_err(storage_err)?;
        match stmt.query_row(params![id.0.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, f64>(6)?,
                row.get::<_, String>(7)?,
            ))
        }) {
            Ok((id_s, tool_s, r, g, b, a, bw, pts_s)) => {
                let id = id_s
                    .parse()
                    .map_err(|e| AppError::Storage(format!("bad stroke uuid: {e}")))?;
                let tool = tool_from_str(&tool_s)?;
                let points: Vec<Point> = serde_json::from_str(&pts_s).map_err(serde_err)?;
                Ok(Some(Stroke {
                    id: StrokeId(id),
                    tool,
                    color: Color {
                        r: r as u8,
                        g: g as u8,
                        b: b as u8,
                        a: a as u8,
                    },
                    base_width: bw as f32,
                    points,
                }))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(storage_err(e)),
        }
    }

    /// Load all strokes for a page in z-order (insertion order).
    pub fn list_strokes_for_page(&self, page_id: PageId) -> Result<Vec<Stroke>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, tool, color_r, color_g, color_b, color_a, base_width, points
                 FROM strokes WHERE page_id = ?1 ORDER BY z_order ASC",
            )
            .map_err(storage_err)?;
        let rows = stmt
            .query_map(params![page_id.0.to_string()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, f64>(6)?,
                    row.get::<_, String>(7)?,
                ))
            })
            .map_err(storage_err)?;
        let mut out = Vec::new();
        for row in rows {
            let (id_s, tool_s, r, g, b, a, bw, pts_s) = row.map_err(storage_err)?;
            let id = id_s
                .parse()
                .map_err(|e| AppError::Storage(format!("bad stroke uuid: {e}")))?;
            let tool = tool_from_str(&tool_s)?;
            let points: Vec<Point> = serde_json::from_str(&pts_s).map_err(serde_err)?;
            out.push(Stroke {
                id: StrokeId(id),
                tool,
                color: Color {
                    r: r as u8,
                    g: g as u8,
                    b: b as u8,
                    a: a as u8,
                },
                base_width: bw as f32,
                points,
            });
        }
        Ok(out)
    }

    /// Delete a stroke by id.  Returns `true` if the row was present.
    pub fn delete_stroke(&self, id: StrokeId) -> Result<bool> {
        let n = self
            .conn
            .execute(
                "DELETE FROM strokes WHERE id = ?1",
                params![id.0.to_string()],
            )
            .map_err(storage_err)?;
        Ok(n > 0)
    }

    /// Delete all strokes on a page.
    pub fn clear_page_strokes(&self, page_id: PageId) -> Result<u64> {
        let n = self
            .conn
            .execute(
                "DELETE FROM strokes WHERE page_id = ?1",
                params![page_id.0.to_string()],
            )
            .map_err(storage_err)?;
        Ok(n as u64)
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AccountKind, PageTemplate, Point, TaskPriority, Tool};

    /// Open a fresh in-memory store for each test.
    fn open() -> SqliteStore {
        SqliteStore::open_in_memory().expect("in-memory store must open")
    }

    // ── Migration ────────────────────────────────────────────────────────────

    #[test]
    fn migrations_run_idempotently() {
        let store = open();
        // Running again on an already-migrated connection must not panic.
        run_migrations(&store.conn).expect("re-running migrations must be a no-op");
    }

    // ── Profile CRUD ─────────────────────────────────────────────────────────

    #[test]
    fn profile_insert_get_delete() {
        let store = open();
        let p = Profile::new("Work");
        store.upsert_profile(&p).unwrap();

        let loaded = store.get_profile(p.id).unwrap().expect("must be found");
        assert_eq!(loaded.id, p.id);
        assert_eq!(loaded.name, "Work");

        assert!(
            store.delete_profile(p.id).unwrap(),
            "must return true on first delete"
        );
        assert!(
            !store.delete_profile(p.id).unwrap(),
            "must return false when already gone"
        );
        assert!(store.get_profile(p.id).unwrap().is_none());
    }

    #[test]
    fn profile_list() {
        let store = open();
        let p1 = Profile::new("Home");
        let p2 = Profile::new("Work");
        store.upsert_profile(&p1).unwrap();
        store.upsert_profile(&p2).unwrap();

        let profiles = store.list_profiles().unwrap();
        assert_eq!(profiles.len(), 2);
    }

    #[test]
    fn profile_upsert_updates_name() {
        let store = open();
        let mut p = Profile::new("Old");
        store.upsert_profile(&p).unwrap();
        p.name = "New".to_string();
        store.upsert_profile(&p).unwrap();

        let loaded = store.get_profile(p.id).unwrap().expect("must exist");
        assert_eq!(loaded.name, "New");
    }

    // ── Profile rename ───────────────────────────────────────────────────────

    #[test]
    fn profile_rename() {
        let store = open();
        let p = Profile::new("Old Name");
        store.upsert_profile(&p).unwrap();

        assert!(store.rename_profile(p.id, "New Name").unwrap());
        let loaded = store.get_profile(p.id).unwrap().expect("must exist");
        assert_eq!(loaded.name, "New Name");
    }

    #[test]
    fn profile_rename_missing_returns_false() {
        let store = open();
        let fake_id = ProfileId::new();
        assert!(!store.rename_profile(fake_id, "Ghost").unwrap());
    }

    // ── App state ────────────────────────────────────────────────────────────

    #[test]
    fn app_state_set_get_clear() {
        let store = open();
        assert!(store.get_app_state("foo").unwrap().is_none());
        store.set_app_state("foo", "bar").unwrap();
        assert_eq!(store.get_app_state("foo").unwrap().as_deref(), Some("bar"));
        store.set_app_state("foo", "baz").unwrap();
        assert_eq!(store.get_app_state("foo").unwrap().as_deref(), Some("baz"));
    }

    #[test]
    fn active_profile_id_roundtrip() {
        let store = open();
        let p = Profile::new("Work");
        store.upsert_profile(&p).unwrap();

        assert!(store.get_active_profile_id().unwrap().is_none());
        store.set_active_profile_id(p.id).unwrap();
        assert_eq!(store.get_active_profile_id().unwrap(), Some(p.id));
        store.clear_active_profile_id().unwrap();
        assert!(store.get_active_profile_id().unwrap().is_none());
    }

    #[test]
    fn active_profile_persists_across_reopen() {
        use std::io::Write;
        let path = {
            let mut p = std::env::temp_dir();
            p.push(format!("ephemeris_test_{}.db", uuid::Uuid::new_v4()));
            p
        };
        let profile_id = {
            let store = SqliteStore::open(&path).unwrap();
            let p = Profile::new("Persistent");
            store.upsert_profile(&p).unwrap();
            store.set_active_profile_id(p.id).unwrap();
            p.id
        };
        // Reopen and verify active profile survived.
        let store2 = SqliteStore::open(&path).unwrap();
        assert_eq!(store2.get_active_profile_id().unwrap(), Some(profile_id));
        let _ = std::fs::remove_file(&path);
    }

    // ── Account CRUD ─────────────────────────────────────────────────────────

    #[test]
    fn account_crud() {
        let store = open();
        let profile = Profile::new("Test");
        store.upsert_profile(&profile).unwrap();

        let mut account = Account::new(profile.id, AccountKind::CalDAV, "work@example.com");
        account
            .config
            .insert("url".to_string(), "https://dav.example.com".to_string());
        store.upsert_account(&account).unwrap();

        let loaded = store
            .get_account(account.id)
            .unwrap()
            .expect("must be found");
        assert_eq!(loaded.kind, AccountKind::CalDAV);
        assert_eq!(
            loaded.config.get("url").map(String::as_str),
            Some("https://dav.example.com")
        );

        let list = store.list_accounts_for_profile(profile.id).unwrap();
        assert_eq!(list.len(), 1);

        assert!(store.delete_account(account.id).unwrap());
        assert!(store.get_account(account.id).unwrap().is_none());
    }

    #[test]
    fn account_kind_all_variants() {
        let store = open();
        let profile = Profile::new("Test");
        store.upsert_profile(&profile).unwrap();

        for kind in [
            AccountKind::Email,
            AccountKind::CalDAV,
            AccountKind::Obsidian,
            AccountKind::WebDAV,
            AccountKind::Local,
        ] {
            let acc = Account::new(profile.id, kind, format!("{kind:?}"));
            store.upsert_account(&acc).unwrap();
            let loaded = store.get_account(acc.id).unwrap().expect("must be found");
            assert_eq!(loaded.kind, kind);
        }
    }

    // ── Note CRUD ─────────────────────────────────────────────────────────────

    #[test]
    fn note_crud() {
        let store = open();
        let note = Note::new("Daily Log", 1_000);
        store.upsert_note(&note).unwrap();

        let loaded = store.get_note(note.id).unwrap().expect("must be found");
        assert_eq!(loaded.title, "Daily Log");
        assert_eq!(loaded.created, 1_000);

        let list = store.list_notes().unwrap();
        assert_eq!(list.len(), 1);

        assert!(store.delete_note(note.id).unwrap());
        assert!(store.get_note(note.id).unwrap().is_none());
    }

    #[test]
    fn note_upsert_updates_title() {
        let store = open();
        let mut note = Note::new("Old Title", 500);
        store.upsert_note(&note).unwrap();
        note.title = "New Title".to_string();
        note.updated = 600;
        store.upsert_note(&note).unwrap();

        let loaded = store.get_note(note.id).unwrap().expect("must exist");
        assert_eq!(loaded.title, "New Title");
        assert_eq!(loaded.updated, 600);
    }

    // ── Page CRUD ─────────────────────────────────────────────────────────────

    #[test]
    fn page_crud() {
        let store = open();
        let note = Note::new("Notebook", 100);
        store.upsert_note(&note).unwrap();

        let page = Page::new(note.id, 0, PageTemplate::Grid);
        store.upsert_page(&page).unwrap();

        let loaded = store.get_page(page.id).unwrap().expect("must be found");
        assert_eq!(loaded.template, PageTemplate::Grid);
        assert_eq!(loaded.note_id, note.id);

        let pages = store.list_pages_for_note(note.id).unwrap();
        assert_eq!(pages.len(), 1);

        assert!(store.delete_page(page.id).unwrap());
        assert!(store.get_page(page.id).unwrap().is_none());
    }

    #[test]
    fn page_template_all_variants() {
        let store = open();
        let note = Note::new("Templates", 0);
        store.upsert_note(&note).unwrap();

        for template in [PageTemplate::Blank, PageTemplate::Grid, PageTemplate::Dot] {
            let page = Page::new(note.id, 0, template);
            store.upsert_page(&page).unwrap();
            let loaded = store.get_page(page.id).unwrap().expect("must be found");
            assert_eq!(loaded.template, template);
        }
    }

    // ── Task CRUD ─────────────────────────────────────────────────────────────

    #[test]
    fn task_crud() {
        let store = open();
        let profile = Profile::new("Worker");
        store.upsert_profile(&profile).unwrap();

        let mut task = Task::new("Write report", profile.id);
        task.priority = TaskPriority::High;
        task.tags = vec!["urgent".to_string(), "work".to_string()];
        task.due = Some(9_999);
        store.upsert_task(&task).unwrap();

        let loaded = store.get_task(task.id).unwrap().expect("must be found");
        assert_eq!(loaded.title, "Write report");
        assert_eq!(loaded.priority, TaskPriority::High);
        assert_eq!(loaded.tags, vec!["urgent", "work"]);
        assert_eq!(loaded.due, Some(9_999));
        assert!(!loaded.done);

        let list = store.list_tasks_for_profile(profile.id).unwrap();
        assert_eq!(list.len(), 1);

        assert!(store.delete_task(task.id).unwrap());
        assert!(store.get_task(task.id).unwrap().is_none());
    }

    #[test]
    fn task_priority_all_variants() {
        let store = open();
        let profile = Profile::new("Prio");
        store.upsert_profile(&profile).unwrap();

        for priority in [TaskPriority::Low, TaskPriority::Medium, TaskPriority::High] {
            let mut task = Task::new(format!("{priority:?}"), profile.id);
            task.priority = priority;
            store.upsert_task(&task).unwrap();
            let loaded = store.get_task(task.id).unwrap().expect("must be found");
            assert_eq!(loaded.priority, priority);
        }
    }

    // ── CalendarEvent CRUD ────────────────────────────────────────────────────

    #[test]
    fn event_crud() {
        let store = open();
        let profile = Profile::new("Calendar User");
        store.upsert_profile(&profile).unwrap();

        let mut event = CalendarEvent::new("uid-abc", 1_000, 2_000, "Team Standup", profile.id);
        event.location = Some("Room 42".to_string());
        store.upsert_event(&event).unwrap();

        let loaded = store.get_event(event.id).unwrap().expect("must be found");
        assert_eq!(loaded.uid, "uid-abc");
        assert_eq!(loaded.start, 1_000);
        assert_eq!(loaded.end, 2_000);
        assert_eq!(loaded.location, Some("Room 42".to_string()));

        let list = store.list_events_for_profile(profile.id).unwrap();
        assert_eq!(list.len(), 1);

        assert!(store.delete_event(event.id).unwrap());
        assert!(store.get_event(event.id).unwrap().is_none());
    }

    #[test]
    fn event_without_location() {
        let store = open();
        let profile = Profile::new("P");
        store.upsert_profile(&profile).unwrap();

        let event = CalendarEvent::new("uid-xyz", 0, 100, "Quick call", profile.id);
        store.upsert_event(&event).unwrap();

        let loaded = store.get_event(event.id).unwrap().expect("must be found");
        assert!(loaded.location.is_none());
    }

    // ── Stroke CRUD ───────────────────────────────────────────────────────────

    fn make_stroke() -> Stroke {
        let mut s = Stroke::new(Tool::Pen, Color::BLACK, 2.0);
        s.points.push(Point::new(0.0, 0.0, 0.5, 0.0, 0));
        s.points.push(Point::new(10.0, 10.0, 0.8, 5.0, 16));
        s
    }

    fn make_page_in_store(store: &SqliteStore) -> Page {
        let note = Note::new("Note", 0);
        store.upsert_note(&note).unwrap();
        let page = Page::new(note.id, 0, PageTemplate::Blank);
        store.upsert_page(&page).unwrap();
        page
    }

    #[test]
    fn stroke_crud() {
        let store = open();
        let page = make_page_in_store(&store);

        let s = make_stroke();
        store.upsert_stroke(page.id, &s).unwrap();

        let loaded = store.get_stroke(s.id).unwrap().expect("must be found");
        assert_eq!(loaded.id, s.id);
        assert_eq!(loaded.tool, Tool::Pen);
        assert_eq!(loaded.color, Color::BLACK);
        assert!((loaded.base_width - 2.0_f32).abs() < 1e-5);
        assert_eq!(loaded.points.len(), 2);

        let list = store.list_strokes_for_page(page.id).unwrap();
        assert_eq!(list.len(), 1);

        assert!(store.delete_stroke(s.id).unwrap());
        assert!(store.get_stroke(s.id).unwrap().is_none());
    }

    #[test]
    fn stroke_upsert_updates_and_preserves_zorder() {
        let store = open();
        let page = make_page_in_store(&store);

        let s1 = make_stroke();
        let s2 = make_stroke();
        store.upsert_stroke(page.id, &s1).unwrap();
        store.upsert_stroke(page.id, &s2).unwrap();

        // Update s1 — z_order must stay at its original position.
        let mut s1_updated = s1.clone();
        s1_updated.base_width = 9.0;
        store.upsert_stroke(page.id, &s1_updated).unwrap();

        let list = store.list_strokes_for_page(page.id).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, s1.id, "s1 must remain first in z-order");
        assert!((list[0].base_width - 9.0_f32).abs() < 1e-5);
    }

    #[test]
    fn stroke_clear_page() {
        let store = open();
        let page = make_page_in_store(&store);

        for _ in 0..3 {
            store.upsert_stroke(page.id, &make_stroke()).unwrap();
        }
        assert_eq!(store.list_strokes_for_page(page.id).unwrap().len(), 3);

        let deleted = store.clear_page_strokes(page.id).unwrap();
        assert_eq!(deleted, 3);
        assert!(store.list_strokes_for_page(page.id).unwrap().is_empty());
    }

    #[test]
    fn stroke_points_roundtrip_with_all_fields() {
        let store = open();
        let page = make_page_in_store(&store);

        let mut s = Stroke::new(Tool::Highlighter, Color::rgb(255, 200, 0), 4.5);
        s.points.push(Point::new(1.1, 2.2, 0.33, -45.0, 0));
        s.points.push(Point::new(3.3, 4.4, 0.77, 90.0, 100));
        store.upsert_stroke(page.id, &s).unwrap();

        let loaded = store.get_stroke(s.id).unwrap().expect("must be found");
        assert_eq!(loaded.points[0].x, s.points[0].x);
        assert_eq!(loaded.points[1].tilt, s.points[1].tilt);
        assert_eq!(loaded.tool, Tool::Highlighter);
    }

    // ── Integration: full lifecycle across all models ─────────────────────────

    /// Opens a temp in-memory DB, runs migrations, then CRUDs each model in a
    /// realistic sequence (profile → account → note → page → stroke → task →
    /// event) and verifies every round-trip.
    #[test]
    fn full_lifecycle_all_models() {
        let store = SqliteStore::open_in_memory().expect("open in-memory DB");

        // Profile
        let profile = Profile::new("Integration User");
        store.upsert_profile(&profile).unwrap();
        let p = store
            .get_profile(profile.id)
            .unwrap()
            .expect("profile must exist");
        assert_eq!(p.name, profile.name);

        // Account
        let mut account = Account::new(profile.id, AccountKind::WebDAV, "my vault");
        account
            .config
            .insert("server".to_string(), "https://dav.example.com".to_string());
        store.upsert_account(&account).unwrap();
        let a = store
            .get_account(account.id)
            .unwrap()
            .expect("account must exist");
        assert_eq!(a.kind, AccountKind::WebDAV);
        assert_eq!(a.config["server"], "https://dav.example.com");

        // Note
        let note = Note::new("My Notebook", 1_000);
        store.upsert_note(&note).unwrap();
        let n = store.get_note(note.id).unwrap().expect("note must exist");
        assert_eq!(n.title, "My Notebook");

        // Page
        let page = Page::new(note.id, 0, PageTemplate::Dot);
        store.upsert_page(&page).unwrap();
        let pg = store.get_page(page.id).unwrap().expect("page must exist");
        assert_eq!(pg.template, PageTemplate::Dot);

        // Stroke
        let mut stroke = Stroke::new(Tool::Pen, Color::rgb(0, 128, 255), 2.0);
        stroke.points.push(Point::new(5.0, 5.0, 0.6, 0.0, 0));
        stroke.points.push(Point::new(50.0, 50.0, 0.9, 10.0, 32));
        store.upsert_stroke(page.id, &stroke).unwrap();
        let s = store
            .get_stroke(stroke.id)
            .unwrap()
            .expect("stroke must exist");
        assert_eq!(s.points.len(), 2);
        assert_eq!(s.color, Color::rgb(0, 128, 255));

        // Task
        let mut task = Task::new("Review design doc", profile.id);
        task.priority = TaskPriority::High;
        task.tags = vec!["design".to_string()];
        task.due = Some(2_000);
        store.upsert_task(&task).unwrap();
        let t = store.get_task(task.id).unwrap().expect("task must exist");
        assert_eq!(t.priority, TaskPriority::High);
        assert_eq!(t.due, Some(2_000));

        // CalendarEvent
        let mut event = CalendarEvent::new("evt-001", 3_000, 4_000, "Design Review", profile.id);
        event.location = Some("Meeting Room 1".to_string());
        store.upsert_event(&event).unwrap();
        let ev = store
            .get_event(event.id)
            .unwrap()
            .expect("event must exist");
        assert_eq!(ev.title, "Design Review");
        assert_eq!(ev.location, Some("Meeting Room 1".to_string()));

        // Delete everything and confirm absence
        assert!(store.delete_stroke(stroke.id).unwrap());
        assert!(store.delete_page(page.id).unwrap());
        assert!(store.delete_note(note.id).unwrap());
        assert!(store.delete_task(task.id).unwrap());
        assert!(store.delete_event(event.id).unwrap());
        assert!(store.delete_account(account.id).unwrap());
        assert!(store.delete_profile(profile.id).unwrap());

        assert!(store.get_profile(profile.id).unwrap().is_none());
        assert!(store.get_note(note.id).unwrap().is_none());
        assert!(store.get_stroke(stroke.id).unwrap().is_none());
    }
}
