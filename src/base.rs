//! The Arctis Nova Pro Wireless base station over its vendor HID interface, and Bluetooth batteries via UPower.
//! Protocol and behaviour as in BaseStation.java: ChatMix dial, volume knob (master volume), mic mute button,
//! headset status (battery, spare, on/off) polled every 3 s.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;

use crate::Mixer;

/// HID_ID suffixes (vendor 1038) of the Nova Pro Wireless base: PC and Xbox editions.
const DEVICES: [&str; 2] = ["00001038:000012E0", "00001038:000012E5"];
const INTERFACE: &str = ":1.4/"; // the vendor interface (bInterfaceNumber 4)
const REPORT_LEN: usize = 64;
const TX: u8 = 0x06;
const RX: u8 = 0x07;
const CHATMIX_ENABLE: u8 = 0x49;
const CHATMIX: u8 = 0x45;
const VOLUME: u8 = 0x25;
const VOLUME_MAX: u8 = 56; // the knob: 0 = loudest .. 56 = quietest
const STATUS: u8 = 0xb0;
const MIC_MUTE: u8 = 0xbb;

/// battery/spare: percent; None = unknown (headset off or no base).
#[derive(Clone, Default, PartialEq, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Headset {
    connected: bool,
    charging: bool,
    battery: Option<i32>,
    spare_battery: Option<i32>,
    mic_muted: bool,
}

#[derive(Default)]
pub struct Base {
    out: Mutex<Option<File>>,
    headset: Mutex<Headset>,
    /// Status replies seen, so a refresh can wait for the next one.
    status_seq: AtomicU64,
}

/// Reply to the status request: [6] battery 0..8, [7] spare 0..8, [9] mic muted, [15] 01 off / 02 cable / 08 on.
fn parse_status(r: &[u8]) -> Headset {
    let percent = |b: u8| Some(b.min(8) as i32 * 100 / 8);
    if r[15] == 0x01 {
        return Headset { spare_battery: percent(r[7]), ..Default::default() };
    }
    Headset { connected: true, charging: r[15] == 0x02, battery: percent(r[6]), spare_battery: percent(r[7]), mic_muted: r[9] != 0 }
}

/// The base reports each side's level 0..100; the side the dial points to stays at 100.
fn mix_from_dial(game: u8, chat: u8) -> i32 {
    (chat as i32 - game as i32).clamp(-100, 100)
}

/// /dev/hidrawN of the base's vendor interface.
fn find_device() -> Option<PathBuf> {
    std::fs::read_dir("/sys/class/hidraw").ok()?.flatten().find_map(|n| {
        let uevent = std::fs::read_to_string(n.path().join("device/uevent")).ok()?;
        let real = std::fs::canonicalize(n.path().join("device")).ok()?;
        (DEVICES.iter().any(|d| uevent.contains(d)) && real.to_string_lossy().contains(INTERFACE))
            .then(|| PathBuf::from("/dev").join(n.file_name()))
    })
}

impl Base {
    fn send(&self, option: u8, value: u8) -> std::io::Result<()> {
        let mut report = [0u8; REPORT_LEN];
        (report[0], report[1], report[2]) = (TX, option, value);
        match self.out.lock().unwrap().as_mut() {
            Some(f) => f.write_all(&report),
            None => Ok(()),
        }
    }

    pub fn headset(&self) -> Headset {
        if self.out.lock().unwrap().is_none() { Headset::default() } else { self.headset.lock().unwrap().clone() }
    }

    /// Ask for the status now and wait (≤ 300 ms) for the reply.
    pub fn refresh(&self) -> Headset {
        let before = self.status_seq.load(Ordering::Relaxed);
        if self.send(STATUS, 0).is_ok() {
            for _ in 0..10 {
                if self.out.lock().unwrap().is_none() || self.status_seq.load(Ordering::Relaxed) != before { break; }
                std::thread::sleep(Duration::from_millis(30));
            }
        }
        self.headset()
    }

    pub fn poll(&self) {
        let _ = self.send(STATUS, 0);
    }

    /// Hand the dial back to the base, so it isn't left waiting for a Sonar that's gone.
    pub fn shutdown(&self) {
        let _ = self.send(CHATMIX_ENABLE, 0);
    }

    /// Find the base, enable the dial, apply its reports; on unplug or error, look again every 5 s.
    pub fn run(self: Arc<Self>, mixer: Arc<Mutex<Mixer>>) {
        let mut warned = false;
        loop {
            if let Some(dev) = find_device() {
                match OpenOptions::new().read(true).write(true).open(&dev) {
                    Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                        if !warned {
                            eprintln!("WARN Base station: no access to {}; install udev/70-steelseries-arctis.rules", dev.display());
                            warned = true;
                        }
                    }
                    Err(e) => eprintln!("Base station: {} ({e}); retrying", dev.display()),
                    Ok(mut f) => {
                        *self.out.lock().unwrap() = f.try_clone().ok();
                        let _ = self.send(CHATMIX_ENABLE, 1);
                        let _ = self.send(STATUS, 0);
                        eprintln!("Base station: listening on {}", dev.display());
                        self.listen(&mut f, &mixer);
                        *self.out.lock().unwrap() = None;
                    }
                }
            }
            std::thread::sleep(Duration::from_secs(5));
        }
    }

    fn listen(&self, f: &mut File, mixer: &Mutex<Mixer>) {
        let mut buf = [0u8; REPORT_LEN];
        let mut last_mix = i32::MIN;
        let mut hardware_mute: Option<bool> = None;
        // only on a change: the status poll mustn't undo a mute set in Sonar's own UI
        let mut mic_muted = |muted: bool, headset: &Mutex<Headset>| {
            headset.lock().unwrap().mic_muted = muted;
            if hardware_mute != Some(muted) {
                hardware_mute = Some(muted);
                mixer.lock().unwrap().set_mic_muted(muted);
            }
        };
        while let Ok(n) = f.read(&mut buf) && n > 3 {
            match (buf[0], buf[1]) {
                (RX, CHATMIX) => {
                    let mix = mix_from_dial(buf[2], buf[3]);
                    if mix != last_mix {
                        mixer.lock().unwrap().set_chat_mix(mix);
                        last_mix = mix;
                    }
                }
                (RX, VOLUME) => {
                    let v = ((VOLUME_MAX.saturating_sub(buf[2])) as f32 * 100.0 / VOLUME_MAX as f32).round() as i32;
                    mixer.lock().unwrap().set_master_volume(v);
                }
                (RX, MIC_MUTE) => mic_muted(buf[2] != 0, &self.headset),
                (TX, STATUS) => {
                    let h = parse_status(&buf);
                    let changed = h.connected != self.headset.lock().unwrap().connected || self.status_seq.load(Ordering::Relaxed) == 0;
                    if changed {
                        mixer.lock().unwrap().set_headset_connected(h.connected);
                    }
                    let (connected, muted) = (h.connected, h.mic_muted);
                    *self.headset.lock().unwrap() = h;
                    self.status_seq.fetch_add(1, Ordering::Relaxed);
                    if connected {
                        mic_muted(muted, &self.headset);
                    }
                }
                _ => {}
            }
        }
    }
}

#[derive(Serialize, PartialEq, Debug)]
pub struct Battery {
    name: String,
    percent: i32,
}

/// Bluetooth headsets/headphones that report a battery level, via UPower.
pub fn batteries() -> Vec<Battery> {
    let run = |args: &[&str]| Command::new("upower").args(args).env("LC_ALL", "C.UTF-8").output().ok()
        .filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
    run(&["-e"]).lines().filter(|p| p.contains("/headset_") || p.contains("/headphones_"))
        .filter_map(|p| parse_battery(&run(&["-i", p]))).collect()
}

fn parse_battery(info: &str) -> Option<Battery> {
    let percent = crate::pw::field(info, "percentage")?.trim_end_matches('%').split('.').next()?.parse().ok()?;
    Some(Battery { name: crate::pw::field(info, "model").unwrap_or("Headphones").into(), percent })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_dial_and_battery() {
        let mut r = [0u8; 64];
        (r[6], r[7], r[9], r[15]) = (4, 8, 1, 0x08);
        assert_eq!(parse_status(&r), Headset { connected: true, charging: false, battery: Some(50), spare_battery: Some(100), mic_muted: true });
        r[15] = 0x01;
        assert_eq!(parse_status(&r), Headset { spare_battery: Some(100), ..Default::default() });
        assert_eq!((mix_from_dial(100, 40), mix_from_dial(40, 100)), (-60, 60));
        let info = "  native-path: x\n  model:                QC35 II\n  percentage:          70%\n";
        assert_eq!(parse_battery(info), Some(Battery { name: "QC35 II".into(), percent: 70 }));
        assert_eq!(parse_battery("  model: x\n"), None);
    }
}
