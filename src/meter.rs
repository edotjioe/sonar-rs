//! Per-channel peak meters (LevelMeter.java): a `parec` per channel at 8 kHz mono, a thread turning its
//! samples into a decaying peak. They only run while the UI polls, so the mic isn't held open otherwise.

use std::collections::HashMap;
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

const RATE: usize = 8000;
const IDLE: Duration = Duration::from_secs(5);

struct Meter {
    proc: Child,
    peak: Arc<AtomicU32>, // f32 bits, linear 0..1
}

pub struct Meters {
    meters: HashMap<String, Option<Meter>>,
    last_poll: Instant,
}

/// Max |sample| of s16le bytes, as 0..1.
fn peak(buf: &[u8]) -> f32 {
    buf.chunks_exact(2).map(|s| i16::from_le_bytes([s[0], s[1]]).unsigned_abs()).max().unwrap_or(0) as f32 / 32768.0
}

/// -60..0 dBFS as 0..100.
fn to_percent(peak: f32) -> i32 {
    if peak <= 0.0 { 0 } else { ((20.0 * peak.log10() + 60.0) / 60.0 * 100.0).clamp(0.0, 100.0).round() as i32 }
}

fn start(source: &str) -> Option<Meter> {
    let mut proc = Command::new("parec")
        .args(["--raw", "--format=s16le", "--channels=1", &format!("--rate={RATE}"), "--latency-msec=50", "--client-name=sonar-meter", "-d", source])
        .stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
    let mut out = proc.stdout.take()?;
    let peak_bits = Arc::new(AtomicU32::new(0));
    let p = peak_bits.clone();
    std::thread::Builder::new().stack_size(64 * 1024).spawn(move || {
        let mut buf = [0u8; RATE / 20 * 2]; // 50 ms
        while out.read_exact(&mut buf).is_ok() {
            let old = f32::from_bits(p.load(Ordering::Relaxed));
            p.store(peak(&buf).max(old * 0.6).to_bits(), Ordering::Relaxed);
        }
        p.store(0, Ordering::Relaxed);
    }).ok()?;
    Some(Meter { proc, peak: peak_bits })
}

impl Meters {
    pub fn new() -> Self {
        Meters { meters: HashMap::new(), last_poll: Instant::now() }
    }

    /// (channel id, source) -> level 0..100. Starts meters that aren't running; a failed one stays at 0.
    pub fn levels(&mut self, channels: impl Iterator<Item = (i64, String)>) -> HashMap<i64, i32> {
        self.last_poll = Instant::now();
        channels.map(|(id, src)| {
            let m = self.meters.entry(src).or_insert_with_key(|s| start(s));
            (id, m.as_ref().map_or(0, |m| to_percent(f32::from_bits(m.peak.load(Ordering::Relaxed)))))
        }).collect()
    }

    /// Stop a source's meter: before it goes away, or parec gets moved to the default mic.
    pub fn forget(&mut self, source: &str) {
        if let Some(Some(mut m)) = self.meters.remove(source) {
            let _ = m.proc.kill();
            let _ = m.proc.wait();
        }
    }

    pub fn stop_all(&mut self) {
        let all: Vec<String> = self.meters.keys().cloned().collect();
        all.iter().for_each(|s| self.forget(s));
    }

    pub fn stop_when_idle(&mut self) {
        if self.last_poll.elapsed() > IDLE {
            self.stop_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peaks_in_percent() {
        assert_eq!(peak(&[0x00, 0x80, 0xff, 0x3f]), 1.0); // -32768
        assert_eq!(to_percent(0.0), 0);
        assert_eq!(to_percent(1.0), 100);
        assert_eq!(to_percent(0.001), 0); // -60 dB
        assert_eq!(to_percent(0.0316), 50); // -30 dB
    }
}
