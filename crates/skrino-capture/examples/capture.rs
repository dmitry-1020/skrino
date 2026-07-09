//! Captures the whole virtual screen and saves it as a PNG in the system temp
//! directory, printing monitor info along the way.
//!
//! Run with:
//!   cargo run -p skrino-capture --example capture

use anyhow::Result;

fn main() -> Result<()> {
    env_logger_init();

    let monitors = skrino_capture::list_monitors()?;
    println!("Found {} monitor(s):", monitors.len());
    for m in &monitors {
        println!(
            "  id={} name={:?} pos=({}, {}) size={}x{} scale={} primary={}",
            m.id, m.name, m.x, m.y, m.width, m.height, m.scale_factor, m.is_primary
        );
    }

    let capture = skrino_capture::capture_virtual_screen()?;
    println!(
        "Stitched virtual screen: {}x{} at origin ({}, {})",
        capture.image.width(),
        capture.image.height(),
        capture.origin_x,
        capture.origin_y
    );

    let out_path = std::env::temp_dir().join("skrino-capture-example.png");
    capture.image.save(&out_path)?;

    let file_size = std::fs::metadata(&out_path)?.len();
    println!("Saved {} ({} bytes)", out_path.display(), file_size);

    Ok(())
}

/// Minimal stderr logger so `log::warn!` calls (e.g. DPI-size reconciliation)
/// are visible when running the example, without adding a logger dependency.
fn env_logger_init() {
    struct SimpleLogger;
    impl log::Log for SimpleLogger {
        fn enabled(&self, _metadata: &log::Metadata) -> bool {
            true
        }
        fn log(&self, record: &log::Record) {
            eprintln!("[{}] {}", record.level(), record.args());
        }
        fn flush(&self) {}
    }
    static LOGGER: SimpleLogger = SimpleLogger;
    let _ = log::set_logger(&LOGGER);
    log::set_max_level(log::LevelFilter::Info);
}
