use crate::{AppError, Stroke};
use rusqlite::{params, Connection};

/// SQLite-backed storage layer for Ephemeris.
pub struct SqliteStore {
    conn: Connection,
}

impl SqliteStore {
    /// Create or open a SQLite database file.
    pub fn new(path: &str) -> crate::Result<Self> {
        let conn = Connection::open(path)
            .map_err(|e| AppError::Storage(format!("Failed to open database: {}", e)))?;

        let store = SqliteStore { conn };
        store.init_schema()?;
        Ok(store)
    }

    /// Initialize database schema if not already present.
    fn init_schema(&self) -> crate::Result<()> {
        self.conn
            .execute_batch(
                r#"
            CREATE TABLE IF NOT EXISTS strokes (
                id TEXT PRIMARY KEY,
                page_id TEXT NOT NULL,
                data TEXT NOT NULL,
                created_at INTEGER
            );
            CREATE TABLE IF NOT EXISTS profiles (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                created_at INTEGER
            );
            CREATE TABLE IF NOT EXISTS notes (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                created INTEGER,
                updated INTEGER
            );
            "#,
            )
            .map_err(|e| AppError::Storage(format!("Failed to initialize schema: {}", e)))?;
        Ok(())
    }

    /// Save a stroke to the database.
    pub fn save_stroke(&self, stroke: &Stroke) -> crate::Result<()> {
        let json = serde_json::to_string(stroke)
            .map_err(|e| AppError::Storage(format!("Serialization error: {}", e)))?;

        self.conn.execute(
            "INSERT OR REPLACE INTO strokes (id, page_id, data, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![stroke.id.0.to_string(), "", &json, 0],
        ).map_err(|e| AppError::Storage(format!("Failed to insert stroke: {}", e)))?;

        Ok(())
    }

    /// Load a stroke by ID.
    pub fn load_stroke(&self, stroke_id: &str) -> crate::Result<Option<Stroke>> {
        let mut stmt = self
            .conn
            .prepare("SELECT data FROM strokes WHERE id = ?1")
            .map_err(|e| AppError::Storage(format!("Query error: {}", e)))?;

        match stmt.query_row(params![stroke_id], |row| row.get::<_, String>(0)) {
            Ok(json) => {
                let stroke = serde_json::from_str(&json)
                    .map_err(|e| AppError::Storage(format!("Deserialization error: {}", e)))?;
                Ok(Some(stroke))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(AppError::Storage(format!("Failed to load stroke: {}", e))),
        }
    }

    /// Delete a stroke by ID.
    pub fn delete_stroke(&self, stroke_id: &str) -> crate::Result<()> {
        self.conn
            .execute("DELETE FROM strokes WHERE id = ?1", params![stroke_id])
            .map_err(|e| AppError::Storage(format!("Failed to delete stroke: {}", e)))?;
        Ok(())
    }

    /// Clear all strokes on a page.
    pub fn clear_page(&self, page_id: &str) -> crate::Result<()> {
        self.conn
            .execute("DELETE FROM strokes WHERE page_id = ?1", params![page_id])
            .map_err(|e| AppError::Storage(format!("Failed to clear page: {}", e)))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Color, Point, Tool};
    use std::fs;

    fn temp_db() -> String {
        format!("/tmp/ephemeris_test_{}.db", uuid::Uuid::new_v4())
    }

    #[test]
    fn sqlite_create_new() {
        let path = temp_db();
        let store = SqliteStore::new(&path).expect("should create");
        assert!(fs::metadata(&path).is_ok());
        fs::remove_file(&path).ok();
    }

    #[test]
    fn sqlite_save_and_load_stroke() {
        let path = temp_db();
        let store = SqliteStore::new(&path).expect("should create");

        let mut stroke = Stroke::new(Tool::Pen, Color::BLACK, 2.0);
        stroke.points.push(Point::new(10.0, 20.0, 0.5, 0.0, 0));

        store.save_stroke(&stroke).expect("should save");
        let loaded = store
            .load_stroke(&stroke.id.0.to_string())
            .expect("should load");

        assert!(loaded.is_some());
        assert_eq!(loaded.unwrap().id, stroke.id);

        fs::remove_file(&path).ok();
    }

    #[test]
    fn sqlite_delete_stroke() {
        let path = temp_db();
        let store = SqliteStore::new(&path).expect("should create");

        let stroke = Stroke::new(Tool::Pen, Color::BLACK, 2.0);
        store.save_stroke(&stroke).expect("should save");
        store
            .delete_stroke(&stroke.id.0.to_string())
            .expect("should delete");

        let loaded = store
            .load_stroke(&stroke.id.0.to_string())
            .expect("should load");
        assert!(loaded.is_none());

        fs::remove_file(&path).ok();
    }

    #[test]
    fn sqlite_clear_page() {
        let path = temp_db();
        let store = SqliteStore::new(&path).expect("should create");

        let stroke = Stroke::new(Tool::Pen, Color::BLACK, 2.0);
        store.save_stroke(&stroke).expect("should save");
        store.clear_page("page-1").expect("should clear");

        fs::remove_file(&path).ok();
    }
}
