//! Dev-only harness: record the primary monitor for N seconds and print the
//! output path, so the wall-time vs mp4-duration relationship can be measured
//! (`cargo run -p skrino-record --example duration_check -- 8`).

use std::time::Duration;

fn main() {
    let secs: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(3);
    // Optional second arg selects an audio source: "system", "mic", else none.
    let audio = match std::env::args().nth(2).as_deref() {
        Some("system") => skrino_record::AudioSource::System,
        Some("mic") | Some("microphone") => skrino_record::AudioSource::Microphone,
        _ => skrino_record::AudioSource::None,
    };
    let output = std::env::temp_dir().join(format!("skrino-duration-check-{secs}s.mp4"));
    let opts = skrino_record::RecordOptions {
        region: None,
        fps: 30,
        capture_cursor: true,
        audio,
        output,
    };
    let rec = skrino_record::Recorder::start(opts).expect("start failed");
    std::thread::sleep(Duration::from_secs(secs));
    let path = rec.stop().expect("stop failed");
    println!("{}", path.display());
}
