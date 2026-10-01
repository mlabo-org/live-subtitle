#[path = "../src/capture.rs"]
mod capture;
use std::{io::Write, sync::mpsc, time::{Duration, Instant}};

fn main() {
    let path = std::env::temp_dir().join("capture-spike.log");
    let mut log = std::fs::File::create(&path).unwrap();
    let (tx, rx) = mpsc::channel();
    let cap = match capture::Capture::start(tx) {
        Ok(c) => c,
        Err(e) => { writeln!(log, "ERROR {e}").unwrap(); return; }
    };
    let t0 = Instant::now();
    let (mut samples, mut peak) = (0usize, 0f32);
    while t0.elapsed() < Duration::from_secs(8) {
        if let Ok(b) = rx.recv_timeout(Duration::from_millis(200)) {
            samples += b.len();
            peak = b.iter().fold(peak, |p, s| p.max(s.abs()));
        }
    }
    drop(cap);
    writeln!(log, "OK samples={samples} secs={:.2} peak={peak:.4}", samples as f32 / 16000.0).unwrap();
}
