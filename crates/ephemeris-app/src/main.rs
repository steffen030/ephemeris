use ephemeris_app::App;
use ephemeris_core::init_tracing;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_tracing();

    let app = App::new()?;
    app.run()?;

    tracing::info!("Ephemeris exiting cleanly");
    Ok(())
}
