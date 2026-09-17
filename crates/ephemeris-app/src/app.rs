use ephemeris_core::Result;

/// Main application controller.
pub struct App {
    version: String,
}

impl App {
    /// Create a new App instance.
    pub fn new() -> Result<Self> {
        tracing::info!("Initializing Ephemeris app");
        Ok(App { version: env!("CARGO_PKG_VERSION").to_string() })
    }

    /// Run the application.
    pub fn run(&self) -> Result<()> {
        tracing::info!("Ephemeris {} started", self.version);
        tracing::debug!("App is running");
        Ok(())
    }
}

impl Default for App {
    fn default() -> Self {
        App { version: env!("CARGO_PKG_VERSION").to_string() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_creation() {
        let app = App::new().expect("should create app");
        assert!(!app.version.is_empty());
    }

    #[test]
    fn app_run() {
        let app = App::new().expect("should create app");
        app.run().expect("should run without error");
    }
}
