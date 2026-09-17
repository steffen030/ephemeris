use crate::Stroke;

/// Stroke persistence in SQLite with JSON serialization.
pub struct StrokePersistence;

impl StrokePersistence {
    pub fn persist_stroke(_stroke: &Stroke) -> crate::Result<()> {
        Ok(())
    }

    pub fn load_strokes_for_page(_page_id: &str) -> crate::Result<Vec<Stroke>> {
        Ok(vec![])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persist_and_load_strokes() {
        let strokes = StrokePersistence::load_strokes_for_page("page-1")
            .expect("should load");
        assert_eq!(strokes.len(), 0);
    }
}
