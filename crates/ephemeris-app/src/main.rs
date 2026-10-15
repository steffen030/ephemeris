use ephemeris_app::App;
use ephemeris_core::init_tracing;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Before any other thread starts: low-priority HTR pool, one core spare.
    ephemeris_core::init_background_inference_pool();
    init_tracing();

    let app = App::new()?;
    app.run_windowed()?;

    tracing::info!("Ephemeris exiting cleanly");
    Ok(())
}
