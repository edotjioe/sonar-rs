//! Sonar backend spike in Rust: same HTTP API and PipeWire handling as the Java version's core
//! (channels, EQ, volume/mute, devices, ChatMix, master volume, routing, profiles, on-demand mic).
//! Plus the Arctis base station, level meters, Bluetooth batteries, mic test, streamer mode, spatial audio,
//! OBS remote, overlay hotkey and auto-update: everything the Java backend does.
//!
//! State lives in data/state.json. Its shapes match the Java API's JSON, so
//! `{"channels": GET /api/channels, "prefs": GET /api/preferences, "profiles": GET /api/eq .profiles}` imports one.

mod base;
mod desktop;
mod meter;
mod obs;
mod pw;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tiny_http::{Header, Method, Request, Response, Server};

use base::Base;
use desktop::classify;
use obs::Obs;
use meter::Meters;
use pw::{EqBand, PipeWire};

const DEFAULT_CHANNELS: [&str; 4] = ["Game", "Chat", "Media", "Aux"];
const MAX_BANDS: usize = 10;
const MAX_GAIN: f64 = 12.0;
const MIC_MAX_VOLUME: i32 = 200;
const TYPES: [&str; 7] = ["peaking", "lowshelf", "highshelf", "lowpass", "highpass", "bandpass", "notch"];

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Channel {
    id: i64,
    name: String,
    sink_name: String,
    device_name: Option<String>,
    volume: i32,
    muted: bool,
    stream_volume: i32,
    stream_muted: bool,
    position: i32,
    eq: Vec<EqBand>,
    tone: [f64; 3],
    preamp: f64,
    mic: bool,
    noise_suppression: bool,
    spatial: bool,
    /// The EQ this channel had on each output it played to (key: output_key), so it comes back there.
    eq_by_output: HashMap<String, SavedEq>,
    /// The output key the current EQ belongs to; None until first known.
    eq_output: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
struct SavedEq {
    bands: Vec<EqBand>,
    tone: [f64; 3],
    preamp: f64,
}

impl Default for Channel {
    fn default() -> Self {
        Channel { id: 0, name: String::new(), sink_name: String::new(), device_name: None, volume: 100, muted: false,
            stream_volume: 100, stream_muted: false, position: 0, eq: vec![], tone: [0.0; 3], preamp: 0.0, mic: false,
            noise_suppression: false, spatial: false, eq_by_output: HashMap::new(), eq_output: None }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Prefs {
    hotkey_opens: String,
    auto_route: bool,
    chat_mix: i32,
    master_volume: i32,
    meters: bool,
    streamer_mode: bool,
}

impl Default for Prefs {
    fn default() -> Self {
        Prefs { hotkey_opens: "overlay".into(), auto_route: true, chat_mix: 0, master_volume: 100, meters: true, streamer_mode: false }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Profile {
    id: i64,
    name: String,
    bands: Vec<EqBand>,
    tone: [f64; 3],
    preamp: f64,
    built_in: bool,
    kind: String,
}

impl Default for Profile {
    fn default() -> Self {
        Profile { id: 0, name: String::new(), bands: vec![], tone: [0.0; 3], preamp: 0.0, built_in: false, kind: "output".into() }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Assignment {
    display_name: Option<String>,
    channel_id: i64,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct State {
    channels: Vec<Channel>,
    prefs: Prefs,
    profiles: Vec<Profile>,
    /// app binary -> its channel
    assignments: HashMap<String, Assignment>,
}

type Res<T> = Result<T, (u16, String)>;

fn bad<T>(msg: impl Into<String>) -> Res<T> {
    Err((400, msg.into()))
}

fn sink_name_for(name: &str) -> String {
    let mut s = String::from("sonar_");
    let mut gap = false;
    for ch in name.trim().to_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            s.push(ch);
            gap = false;
        } else if !gap {
            s.push('_');
            gap = true;
        }
    }
    s
}

fn check_name(name: Option<&str>) -> Res<String> {
    match name.map(str::trim) {
        Some(n) if n.chars().any(|c| c.is_ascii_alphanumeric()) => Ok(n.into()),
        _ => bad("Channel name needs at least one letter or digit"),
    }
}

fn clamp(v: f64, min: f64, max: f64) -> f64 {
    if v.is_nan() { min } else { v.clamp(min, max) }
}

fn check_bands(bands: Vec<EqBand>) -> Res<Vec<EqBand>> {
    if bands.len() > MAX_BANDS {
        return bad(format!("EQ takes at most {MAX_BANDS} bands"));
    }
    bands.into_iter().map(|b| {
        if !TYPES.contains(&b.kind.as_str()) {
            return bad(format!("Unknown EQ band type: {}", b.kind));
        }
        Ok(EqBand { freq: clamp(b.freq, 20.0, 20000.0), gain: clamp(b.gain, -MAX_GAIN, MAX_GAIN), q: clamp(b.q, 0.1, 10.0), kind: b.kind })
    }).collect()
}

fn check_tone(t: [f64; 3]) -> [f64; 3] {
    t.map(|g| clamp(g, -MAX_GAIN, MAX_GAIN))
}

fn check_preamp(p: f64) -> f64 {
    clamp(p, -24.0, MAX_GAIN)
}

/// GG's Bass/Voice/Treble: fixed filters after the bands.
fn tone_bands(t: [f64; 3]) -> [EqBand; 3] {
    [EqBand::new("lowshelf", 100.0, t[0], 0.71), EqBand::new("peaking", 2000.0, t[1], 0.7), EqBand::new("highshelf", 6000.0, t[2], 0.71)]
}

fn chain(c: &Channel) -> Vec<EqBand> {
    c.eq.iter().cloned().chain(tone_bands(c.tone)).collect()
}

fn cube(volume: i32) -> f64 {
    (volume as f64 / 100.0).powi(3)
}

/// Towards Chat (mix > 0) fades Game; towards Game (mix < 0) fades Chat.
fn mixed(volume: i32, sink: &str, mix: i32) -> i32 {
    if sink == "sonar_game" && mix > 0 {
        return (volume as f32 * (100 - mix) as f32 / 100.0).round() as i32;
    }
    if sink == "sonar_chat" && mix < 0 {
        return (volume as f32 * (100 + mix) as f32 / 100.0).round() as i32;
    }
    volume
}

fn presets() -> Vec<(&'static str, &'static str, Vec<EqBand>)> {
    let b = EqBand::new;
    vec![
        ("Flat", "both", vec![]),
        ("Bass Boost", "output", vec![b("lowshelf", 120.0, 6.0, 0.71)]),
        ("Footsteps", "output", vec![b("lowshelf", 200.0, -4.0, 0.71), b("peaking", 3000.0, 5.0, 0.9)]),
        ("Voice Clarity", "both", vec![b("highpass", 90.0, 0.0, 0.71), b("peaking", 300.0, -3.0, 1.0), b("peaking", 3000.0, 4.0, 0.8)]),
        ("Treble Boost", "output", vec![b("highshelf", 4000.0, 6.0, 0.71)]),
        ("Broadcast", "mic", vec![b("highpass", 80.0, 0.0, 0.71), b("lowshelf", 180.0, 2.0, 0.71), b("peaking", 350.0, -2.5, 1.0),
            b("peaking", 4000.0, 3.0, 0.8), b("peaking", 7500.0, -2.0, 3.0)]),
    ]
}

/// Which output a channel's EQ is for: its device, and for the Arctis base also whether the headset is on
/// (off, the base plays to the speakers on its line out, on the same PipeWire device). None = not known yet.
/// ponytail: the base station's device is recognised by name; fine while base.rs only knows the Nova Pro.
fn output_key(c: &Channel, headset_on: Option<bool>) -> Option<String> {
    let device = c.device_name.as_ref()?;
    if c.mic || !device.contains("Arctis_Nova_Pro") {
        return Some(device.clone());
    }
    headset_on.map(|on| if on { device.clone() } else { format!("{device}#speakers") })
}

/// Point the channel's EQ at its current output: store the previous output's, bring back this one's.
/// True when the EQ changed (the caller restarts it).
fn follow_output(c: &mut Channel, headset_on: Option<bool>) -> bool {
    let Some(key) = output_key(c, headset_on) else { return false };
    if c.eq_output.as_ref() == Some(&key) {
        return false;
    }
    if let Some(old) = c.eq_output.take() {
        c.eq_by_output.insert(old, SavedEq { bands: c.eq.clone(), tone: c.tone, preamp: c.preamp });
    }
    c.eq_output = Some(key.clone());
    let Some(saved) = c.eq_by_output.get(&key).cloned() else { return false };
    (c.eq, c.tone, c.preamp) = (saved.bands, saved.tone, saved.preamp);
    true
}

fn meter_source(c: &Channel) -> String {
    if c.mic { c.sink_name.clone() } else { format!("{}.monitor", c.sink_name) }
}

struct Mixer {
    pw: PipeWire,
    state: State,
    file: PathBuf,
    /// sink name -> null-sink module id (runtime only: PipeWire reuses ids)
    modules: HashMap<String, i64>,
    mic_last_used: HashMap<String, Instant>,
    missing_before: bool,
    meters: Meters,
    /// Headset on/off as the base station reports it; None until it does (or without one).
    headset_on: Option<bool>,
    /// Streamer mode's sink module (runtime only).
    stream_module: i64,
    obs: Arc<Obs>,
}

impl Mixer {
    fn save(&self) {
        let tmp = self.file.with_extension("tmp");
        let ok = std::fs::write(&tmp, serde_json::to_vec_pretty(&self.state).unwrap()).and_then(|_| std::fs::rename(&tmp, &self.file));
        if let Err(e) = ok {
            eprintln!("ERROR can't save {}: {e}", self.file.display());
        }
    }

    fn idx(&self, id: i64) -> Res<usize> {
        self.state.channels.iter().position(|c| c.id == id).ok_or((400, format!("Unknown channel: {id}")))
    }

    fn output_idx(&self, id: i64) -> Res<usize> {
        let i = self.idx(id)?;
        if self.state.channels[i].mic { bad("Not for the mic channel") } else { Ok(i) }
    }

    fn gain(&self, c: &Channel) -> f64 {
        let preamp = 10f64.powf(c.preamp / 20.0);
        if c.mic {
            return preamp;
        }
        if c.muted {
            return 0.0;
        }
        let p = &self.state.prefs;
        // on the Arctis headset the base applies its volume knob itself
        let in_hardware = output_key(c, self.headset_on).is_some_and(|k| k.contains("Arctis_Nova_Pro") && !k.ends_with("#speakers"));
        let master = if in_hardware { 1.0 } else { cube(p.master_volume) };
        preamp * master * cube(mixed(c.volume, &c.sink_name, p.chat_mix))
    }

    fn stream_gain(c: &Channel) -> f64 {
        if c.stream_muted { 0.0 } else { cube(c.stream_volume) }
    }

    fn apply_stream_gain(&self, i: usize) {
        let c = &self.state.channels[i];
        self.pw.set_gain(&format!("{}_stream_in", c.sink_name), Self::stream_gain(c));
    }

    fn set_chat_mix(&mut self, mix: i32) {
        self.state.prefs.chat_mix = mix.clamp(-100, 100);
        self.save();
        for i in 0..self.state.channels.len() {
            if ["sonar_game", "sonar_chat"].contains(&self.state.channels[i].sink_name.as_str()) { self.apply_gain(i); }
        }
    }

    fn set_master_volume(&mut self, volume: i32) {
        self.state.prefs.master_volume = volume.clamp(0, 100);
        self.save();
        for i in 0..self.state.channels.len() {
            if !self.state.channels[i].mic { self.apply_gain(i); }
        }
    }

    /// The base saw the headset turn on or off: its channels switch to the EQ they had there.
    fn set_headset_connected(&mut self, on: bool) {
        self.headset_on = Some(on);
        let mut changed = false;
        for i in 0..self.state.channels.len() {
            let c = &mut self.state.channels[i];
            if c.mic { continue; }
            let before = c.eq_output.clone();
            if follow_output(c, Some(on)) { self.start_eq(i) } else { self.apply_gain(i) } // master moves between knob and Sonar
            changed |= before != self.state.channels[i].eq_output;
        }
        if changed { self.save(); }
    }

    /// The headset's own mute button: mirror it onto the mic channel.
    fn set_mic_muted(&mut self, muted: bool) {
        let mut changed = false;
        for c in self.state.channels.iter_mut().filter(|c| c.mic && c.muted != muted) {
            c.muted = muted;
            self.pw.set_mute(&c.sink_name, muted, true);
            changed = true;
        }
        if changed { self.save(); }
    }

    /// Before the channels: the stream sink, if streamer mode is on.
    fn load_stream_sink(&mut self) {
        self.stream_module = -1;
        if self.state.prefs.streamer_mode {
            self.stream_module = self.pw.load_null_sink(pw::STREAM_SINK, "Sonar Stream", false);
            self.pw.set_volume(pw::STREAM_SINK, 100, false); // WirePlumber may restore an old volume
            self.pw.set_mute(pw::STREAM_SINK, false, false);
        }
    }

    /// Streamer mode: the Sonar Stream sink plus a feed into it from every output channel (restarts their EQs).
    fn set_streamer_mode(&mut self, on: bool) {
        self.state.prefs.streamer_mode = on;
        self.save();
        if on && self.stream_module < 0 {
            self.load_stream_sink();
        }
        for i in 0..self.state.channels.len() {
            if !self.state.channels[i].mic { self.start_eq(i); }
        }
        if !on && self.stream_module >= 0 {
            self.pw.unload_module(self.stream_module); // after the restarts: no feed plays into it any more
            self.stream_module = -1;
        }
    }

    /// Virtual 7.1: reloads the channel's sink (its apps blip to the default output until routing moves them back).
    fn set_spatial(&mut self, i: usize, on: bool) -> Res<()> {
        if on && pw::spatial_hrtf().is_none() {
            return Err((503, "Spatial audio needs PipeWire's SOFA plugin and an HRTF: install libmysofa (or put a .sofa file at ~/.local/share/sonar/hrtf.sofa)".into()));
        }
        self.state.channels[i].spatial = on;
        let sink = self.state.channels[i].sink_name.clone();
        self.meters.forget(&meter_source(&self.state.channels[i])); // its parec dies with the old sink
        self.pw.stop_eq(&sink);
        if let Some(m) = self.modules.remove(&sink).filter(|&m| m >= 0) {
            self.pw.unload_module(m);
        }
        self.apply_channel(i);
        Ok(())
    }

    fn apply_gain(&self, i: usize) {
        let c = &self.state.channels[i];
        self.pw.set_gain(&format!("{}_eq_in", c.sink_name), self.gain(c));
    }

    fn start_eq(&self, i: usize) {
        let c = &self.state.channels[i];
        let stream = (!c.mic && self.state.prefs.streamer_mode).then(|| Self::stream_gain(c));
        self.pw.start_eq(&c.sink_name, &c.name, c.device_name.as_deref().unwrap_or(""), &chain(c), c.mic, c.noise_suppression,
            self.gain(c), stream, c.spatial);
    }

    /// After a setting the EQ process is built from changed: restart it, unless it's the mic's and that's off.
    fn restart_eq(&self, i: usize) {
        let c = &self.state.channels[i];
        if !c.mic || self.pw.is_eq_running(&c.sink_name) {
            self.start_eq(i);
        }
    }

    /// (Re)create a channel's modules and re-apply its volume/mute.
    fn apply_channel(&mut self, i: usize) {
        let c = &self.state.channels[i];
        let mut device = c.device_name.clone().unwrap_or_default();
        if device.is_empty() || device.contains(char::is_whitespace) {
            device = if c.mic { self.pw.default_source() } else { self.pw.default_sink() };
        }
        if c.mic {
            device = self.real_mic(device);
        }
        let c = &mut self.state.channels[i];
        c.device_name = Some(device);
        follow_output(c, self.headset_on);
        let (sink, name, mic, spatial) = (c.sink_name.clone(), c.name.clone(), c.mic, c.spatial);
        let module = if mic { self.pw.load_virtual_source(&sink, &name) } else { self.pw.load_null_sink(&sink, &name, spatial) };
        self.modules.insert(sink.clone(), module);
        let c = &self.state.channels[i];
        if mic {
            self.pw.set_volume(&sink, c.volume, true); // its EQ runs on demand: see sync_mic
            self.pw.set_mute(&sink, c.muted, true);
        } else {
            self.start_eq(i);
            self.pw.set_volume(&sink, 100, false); // WirePlumber may restore an old volume
            self.pw.set_mute(&sink, false, false);
        }
    }

    /// `device`, unless that's an output's monitor: then the default mic or the first one.
    fn real_mic(&self, device: String) -> String {
        let mics: Vec<String> = self.pw.list_physical_sources().into_iter().map(|m| m.name).collect();
        if mics.is_empty() || mics.contains(&device) || !self.pw.index_to_name("sources").values().any(|n| *n == device) {
            return device;
        }
        let def = self.pw.default_source();
        if mics.contains(&def) { def } else { mics[0].clone() }
    }

    fn create_channel(&mut self, name: Option<&str>, mic: bool) -> Res<()> {
        let name = check_name(name)?;
        let sink = sink_name_for(&name);
        if sink == pw::STREAM_SINK || self.state.channels.iter().any(|c| c.sink_name == sink || c.name == name) {
            return bad(format!("A channel with that name already exists: {name}"));
        }
        let id = self.state.channels.iter().map(|c| c.id).max().unwrap_or(0) + 1;
        let position = self.state.channels.iter().map(|c| c.position).max().unwrap_or(-1) + 1;
        self.state.channels.push(Channel { id, name, sink_name: sink, mic, position, ..Default::default() });
        self.apply_channel(self.state.channels.len() - 1);
        self.save();
        Ok(())
    }

    fn is_deletable(c: &Channel) -> bool {
        !c.mic && !DEFAULT_CHANNELS.iter().any(|n| sink_name_for(n) == c.sink_name)
    }

    fn delete_channel(&mut self, id: i64) -> Res<()> {
        let i = self.idx(id)?;
        if !Self::is_deletable(&self.state.channels[i]) {
            return bad(format!("Built-in channels can't be deleted: {}", self.state.channels[i].name));
        }
        let c = self.state.channels.remove(i);
        self.meters.forget(&meter_source(&c));
        self.pw.stop_eq(&c.sink_name);
        if let Some(m) = self.modules.remove(&c.sink_name).filter(|&m| m >= 0) {
            self.pw.unload_module(m);
        }
        self.state.assignments.retain(|_, a| a.channel_id != id);
        self.save();
        Ok(())
    }

    fn route(&mut self, binary: String, display_name: Option<String>, channel_id: Option<i64>) -> Res<()> {
        let target = match channel_id {
            None => {
                self.state.assignments.remove(&binary);
                self.pw.default_sink()
            }
            Some(id) => {
                let c = &self.state.channels[self.idx(id)?];
                if c.mic {
                    return bad(format!("Apps can't play into the mic channel: {}", c.name));
                }
                let sink = c.sink_name.clone();
                let old = self.state.assignments.remove(&binary).and_then(|a| a.display_name);
                let display_name = display_name.filter(|d| !d.trim().is_empty()).or(old);
                self.state.assignments.insert(binary.clone(), Assignment { display_name, channel_id: id });
                sink
            }
        };
        self.save();
        for s in self.pw.list_streams().into_iter().filter(|s| s.binary == binary) {
            self.pw.move_input(s.index, &target);
        }
        Ok(())
    }

    /// Run the mic chain only while something records from the virtual mic; stop 5 s after the last user.
    fn sync_mic(&mut self) {
        let now = Instant::now();
        for i in 0..self.state.channels.len() {
            let sink = self.state.channels[i].sink_name.clone();
            if !self.state.channels[i].mic {
                continue;
            }
            let running = self.pw.is_eq_running(&sink);
            if self.pw.source_consumers(&sink) > 0 {
                self.mic_last_used.insert(sink, now);
                if !running {
                    self.start_eq(i);
                }
            } else if running && self.mic_last_used.get(&sink).is_none_or(|t| now - *t > Duration::from_secs(5)) {
                self.pw.stop_eq(&sink);
            }
        }
    }

    /// Keep assigned apps on their channel; unassigned ones go where `classify` guesses (StreamWatcher.java).
    fn enforce_routing(&self) {
        let sink_of_id: HashMap<i64, &str> = self.state.channels.iter().map(|c| (c.id, c.sink_name.as_str())).collect();
        let sink_of_name: HashMap<&str, &str> = self.state.channels.iter().filter(|c| !c.mic)
            .map(|c| (c.name.as_str(), c.sink_name.as_str())).collect();
        for s in self.pw.list_streams() {
            let target = match self.state.assignments.get(&s.binary) {
                Some(a) => sink_of_id.get(&a.channel_id).copied(),
                None if self.state.prefs.auto_route => classify(&s).and_then(|n| sink_of_name.get(n).copied()),
                None => None,
            };
            if let Some(t) = target.filter(|t| *t != s.sink_name) {
                self.pw.move_input(s.index, t);
            }
        }
    }

    /// Clear leftovers, seed presets, ensure the default channels and the mic, restore everything.
    fn bootstrap(&mut self) {
        self.meters.stop_all(); // their parec dies with the old nodes
        self.pw.stop_all_eq();
        self.pw.unload_sonar_modules();
        self.load_stream_sink(); // before the channels: their stream feeds play into it
        for (name, kind, bands) in presets() {
            match self.state.profiles.iter_mut().find(|p| p.name == name) {
                Some(p) if p.built_in => { p.bands = bands; p.kind = kind.into(); }
                Some(_) => {}
                None => {
                    let id = self.state.profiles.iter().map(|p| p.id).max().unwrap_or(0) + 1;
                    self.state.profiles.push(Profile { id, name: name.into(), bands, built_in: true, kind: kind.into(), ..Default::default() });
                }
            }
        }
        for i in 0..self.state.channels.len() {
            self.apply_channel(i);
        }
        for name in DEFAULT_CHANNELS {
            if !self.state.channels.iter().any(|c| c.sink_name == sink_name_for(name)) {
                let _ = self.create_channel(Some(name), false);
            }
        }
        if !self.state.channels.iter().any(|c| c.sink_name == "sonar_mic") {
            let _ = self.create_channel(Some("Mic"), true);
        }
        self.save();
        // our sinks are brand new: an already-running OBS lost its capture of the old ones
        let obs = self.obs.clone();
        std::thread::spawn(move || obs.reattach_sonar_inputs());
    }

    /// Channels gone on two checks in a row: PipeWire restarted, rebuild them all.
    fn recover(&mut self) {
        if !self.pw.is_up() {
            return;
        }
        let mut live: HashSet<String> = self.pw.index_to_name("sinks").into_values().collect();
        live.extend(self.pw.index_to_name("sources").into_values());
        let missing = !self.state.channels.iter().all(|c| live.contains(&c.sink_name));
        if missing && self.missing_before {
            eprintln!("WARN Sonar channels are gone from PipeWire (did it restart?); rebuilding them");
            self.bootstrap();
            self.missing_before = false;
        } else {
            self.missing_before = missing;
        }
    }

    fn shutdown(&mut self) {
        self.meters.stop_all();
        self.pw.stop_all_eq();
        self.pw.unload_sonar_modules();
    }

    fn channels_view(&self) -> Value {
        let mut live: HashSet<String> = self.pw.index_to_name("sinks").into_values().collect();
        live.extend(self.pw.index_to_name("sources").into_values());
        let desc: HashMap<String, String> = self.pw.list_physical_sources().into_iter().chain(self.pw.list_physical_sinks())
            .map(|d| (d.name, d.description)).collect();
        let mut chans: Vec<&Channel> = self.state.channels.iter().collect();
        chans.sort_by_key(|c| c.position);
        Value::Array(chans.into_iter().map(|c| {
            let mut v = serde_json::to_value(c).unwrap();
            let dev = c.device_name.clone().unwrap_or_default();
            v["deviceDescription"] = json!(desc.get(&dev).unwrap_or(&dev));
            v["active"] = json!(live.contains(&c.sink_name));
            v["deletable"] = json!(Self::is_deletable(c));
            v
        }).collect())
    }

    fn apps_view(&self) -> Value {
        let mut seen = HashSet::new();
        Value::Array(self.pw.list_streams().into_iter().filter(|s| seen.insert(format!("{}\0{}", s.binary, s.app_name))).map(|s| {
            let ch = self.state.assignments.get(&s.binary)
                .and_then(|a| self.state.channels.iter().find(|c| c.id == a.channel_id));
            json!({ "index": s.index, "binary": s.binary, "appName": s.app_name, "sinkName": s.sink_name,
                "channelId": ch.map(|c| c.id), "channelName": ch.map(|c| &c.name),
                "autoChannel": if self.state.prefs.auto_route { classify(&s) } else { None } })
        }).collect())
    }

    /// One API call. Mirrors MixerController.java.
    fn handle(&mut self, method: &Method, path: &str, body: &[u8]) -> Res<Value> {
        let b: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
        let int = |k: &str| b[k].as_i64().map(|v| v as i32).ok_or((400, format!("Missing field: {k}")));
        let flag = |k: &str| b[k].as_bool().ok_or((400, format!("Missing field: {k}")));
        let num = |k: &str| b[k].as_f64().ok_or((400, format!("Missing field: {k}")));
        let seg: Vec<&str> = path.trim_start_matches("/api/").split('/').collect();
        let ok = Ok(Value::Null);
        use Method::*;
        match (method, seg.as_slice()) {
            (Get, ["channels"]) => Ok(self.channels_view()),
            (Get, ["devices"]) => Ok(json!(self.pw.list_physical_sinks())),
            (Get, ["sources"]) => Ok(json!(self.pw.list_physical_sources())),
            (Get, ["apps"]) => Ok(self.apps_view()),
            (Get, ["preferences"]) => Ok(json!(self.state.prefs)),
            (Get, ["eq"]) => Ok(json!({ "maxGain": MAX_GAIN, "profiles": self.state.profiles })),
            (Get, ["version"]) => Ok(json!({ "version": env!("CARGO_PKG_VERSION") })),
            (Get, ["levels"]) => {
                let chans: Vec<(i64, String)> = self.state.channels.iter().map(|c| (c.id, meter_source(c))).collect();
                Ok(json!(self.meters.levels(chans.into_iter())))
            }
            (Put, ["preferences"]) => {
                let p = &mut self.state.prefs;
                if let Some(v) = b["hotkeyOpens"].as_str() { p.hotkey_opens = if v == "window" { "window" } else { "overlay" }.into(); }
                if let Some(v) = b["autoRoute"].as_bool() { p.auto_route = v; }
                if let Some(v) = b["meters"].as_bool() { p.meters = v; }
                self.save();
                Ok(json!(self.state.prefs))
            }
            (Post, ["channels"]) => self.create_channel(b["name"].as_str(), false).map(|_| Value::Null),
            (Post, ["channels", "order"]) => {
                let ids: Vec<i64> = serde_json::from_value(b).map_err(|e| (400, e.to_string()))?;
                let rank = |id: i64| ids.iter().position(|&x| x == id).unwrap_or(usize::MAX);
                self.state.channels.sort_by_key(|c| (rank(c.id), c.position));
                self.state.channels.iter_mut().enumerate().for_each(|(i, c)| c.position = i as i32);
                self.save();
                ok
            }
            (Delete, ["channels", id]) => self.delete_channel(parse_id(id)?).map(|_| Value::Null),
            (Post, ["channels", id, action]) => {
                let id = parse_id(id)?;
                let i = self.idx(id)?;
                match *action {
                    "name" => {
                        let name = check_name(b["name"].as_str())?;
                        if self.state.channels.iter().any(|c| c.name == name && c.id != id) {
                            return bad(format!("A channel with that name already exists: {name}"));
                        }
                        self.state.channels[i].name = name;
                    }
                    "volume" => {
                        let c = &mut self.state.channels[i];
                        c.volume = int("volume")?.clamp(0, if c.mic { MIC_MAX_VOLUME } else { 100 });
                        if c.mic { self.pw.set_volume(&c.sink_name, c.volume, true) } else { self.apply_gain(i) }
                    }
                    "mute" => {
                        let c = &mut self.state.channels[i];
                        c.muted = flag("muted")?;
                        if c.mic { self.pw.set_mute(&c.sink_name, c.muted, true) } else { self.apply_gain(i) }
                    }
                    "device" => {
                        let dev = b["deviceName"].as_str().filter(|d| !d.trim().is_empty()).ok_or((400, "Missing field: deviceName".to_string()))?;
                        self.state.channels[i].device_name = Some(dev.into());
                        follow_output(&mut self.state.channels[i], self.headset_on);
                        self.restart_eq(i);
                    }
                    "noise" => {
                        let on = flag("enabled")?;
                        if !self.state.channels[i].mic {
                            return bad("Noise suppression is only for the mic channel");
                        }
                        if on && pw::rnnoise_plugin().is_none() {
                            return Err((503, "Noise suppression needs the rnnoise plugin: install noise-suppression-for-voice".into()));
                        }
                        self.state.channels[i].noise_suppression = on;
                        self.restart_eq(i);
                    }
                    "eq" => {
                        let bands = check_bands(serde_json::from_value(b["bands"].clone()).map_err(|e| (400, e.to_string()))?)?;
                        let c = &mut self.state.channels[i];
                        let same_types = c.eq.iter().map(|b| &b.kind).eq(bands.iter().map(|b| &b.kind));
                        c.eq = bands;
                        if same_types { self.pw.set_eq_params(&c.sink_name, &chain(c)) } else { self.restart_eq(i) }
                    }
                    "tone" => {
                        let t: [f64; 3] = serde_json::from_value(b["tone"].clone()).map_err(|_| (400, "Tone needs 3 gains: bass, voice, treble".to_string()))?;
                        let c = &mut self.state.channels[i];
                        c.tone = check_tone(t);
                        self.pw.set_eq_params(&c.sink_name, &chain(c));
                    }
                    "preamp" => {
                        self.state.channels[i].preamp = check_preamp(num("preamp")?);
                        self.apply_gain(i);
                    }
                    "stream-volume" => {
                        self.output_idx(id)?;
                        self.state.channels[i].stream_volume = int("volume")?.clamp(0, 100);
                        self.apply_stream_gain(i);
                    }
                    "stream-mute" => {
                        self.output_idx(id)?;
                        self.state.channels[i].stream_muted = flag("muted")?;
                        self.apply_stream_gain(i);
                    }
                    "spatial" => {
                        self.output_idx(id)?;
                        self.set_spatial(i, flag("enabled")?)?;
                    }
                    _ => return Err((404, "Not found".into())),
                }
                self.save();
                ok
            }
            (Post, ["route"]) => {
                let binary = b["binary"].as_str().ok_or((400, "Missing field: binary".to_string()))?.to_string();
                self.route(binary, b["displayName"].as_str().map(String::from), b["channelId"].as_i64())?;
                ok
            }
            (Post, ["chatmix"]) => { self.set_chat_mix(int("mix")?); ok }
            (Post, ["master"]) => { self.set_master_volume(int("volume")?); ok }
            (Post, ["streamer"]) => { self.set_streamer_mode(flag("enabled")?); ok }
            (Post, ["eq", "profiles"]) => {
                let name = check_name(b["name"].as_str())?;
                let bands = check_bands(serde_json::from_value(b["bands"].clone()).map_err(|e| (400, e.to_string()))?)?;
                let tone = check_tone(serde_json::from_value(b["tone"].clone()).map_err(|_| (400, "Tone needs 3 gains: bass, voice, treble".to_string()))?);
                let preamp = check_preamp(b["preamp"].as_f64().unwrap_or(0.0));
                let mic = b["mic"].as_bool().unwrap_or(false);
                let next = self.state.profiles.iter().map(|p| p.id).max().unwrap_or(0) + 1;
                let p = match self.state.profiles.iter().position(|p| p.name == name) {
                    Some(i) => {
                        let p = &self.state.profiles[i];
                        if p.built_in { return bad(format!("Can't overwrite a built-in profile: {name}")); }
                        if p.kind != "both" && p.kind != if mic { "mic" } else { "output" } {
                            return bad(format!("An {} EQ profile is already called {name}", if mic { "output" } else { "mic" }));
                        }
                        &mut self.state.profiles[i]
                    }
                    None => {
                        self.state.profiles.push(Profile { id: next, name, kind: if mic { "mic" } else { "output" }.into(), ..Default::default() });
                        self.state.profiles.last_mut().unwrap()
                    }
                };
                (p.bands, p.tone, p.preamp) = (bands, tone, preamp);
                let v = json!(p);
                self.save();
                Ok(v)
            }
            (Delete, ["eq", "profiles", id]) => {
                let id = parse_id(id)?;
                let i = self.state.profiles.iter().position(|p| p.id == id).ok_or((400, format!("Unknown profile: {id}")))?;
                if self.state.profiles[i].built_in {
                    return bad(format!("Can't delete a built-in profile: {}", self.state.profiles[i].name));
                }
                self.state.profiles.remove(i);
                self.save();
                ok
            }
            _ => Err((404, "Not found".into())),
        }
    }
}

fn parse_id(s: &str) -> Res<i64> {
    s.parse().map_err(|_| (400, format!("Bad id: {s}")))
}

fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("css") => "text/css",
        Some("js") => "text/javascript",
        Some("png") => "image/png",
        Some("woff2") => "font/woff2",
        Some("svg") => "image/svg+xml",
        _ => "application/octet-stream",
    }
}

fn header(k: &str, v: &str) -> Header {
    Header::from_bytes(k, v).unwrap()
}

type Resp = Response<std::io::Cursor<Vec<u8>>>;

fn json_resp(result: Res<Value>) -> Resp {
    match result {
        Ok(Value::Null) => Response::from_data(vec![]),
        Ok(v) => Response::from_data(serde_json::to_vec(&v).unwrap()).with_header(header("Content-Type", "application/json")),
        Err((code, msg)) => Response::from_string(msg).with_status_code(code),
    }
}

/// Mic test: a few seconds of the virtual mic (noise suppression, EQ, volume) as WAV. Records without the mixer lock.
fn mic_test(mixer: &Mutex<Mixer>, seconds: u64) -> Resp {
    let sink = {
        let mut mx = mixer.lock().unwrap();
        let Some(i) = mx.state.channels.iter().position(|c| c.mic) else {
            return json_resp(bad("There's no mic channel"));
        };
        let sink = mx.state.channels[i].sink_name.clone();
        mx.mic_last_used.insert(sink.clone(), Instant::now());
        if !mx.pw.is_eq_running(&sink) {
            mx.start_eq(i); // now, rather than on sync_mic's next tick: the recording would start silent
        }
        sink
    };
    match pw::record_wav(&sink, seconds) {
        Ok(wav) => Response::from_data(wav).with_header(header("Content-Type", "audio/wav")),
        Err(e) => Response::from_string(format!("Can't record {sink}: {e}")).with_status_code(500),
    }
}

fn body_json(req: &mut Request) -> Value {
    let mut body = Vec::new();
    let _ = req.as_reader().read_to_end(&mut body);
    serde_json::from_slice(&body).unwrap_or(Value::Null)
}

fn respond(req: Request, resp: Resp) {
    let _ = req.respond(resp.with_header(header("Cache-Control", "no-store")));
}

fn main() {
    let static_dir = PathBuf::from(std::env::var("SONAR_STATIC").unwrap_or_else(|_| "../steelseriesGG/src/main/resources/static".into()));
    let addr = std::env::var("SONAR_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".into());
    std::fs::create_dir_all("data").expect("can't create data/");
    let file = PathBuf::from("data/state.json");
    let state = std::fs::read(&file).ok().map(|b| serde_json::from_slice(&b).expect("data/state.json is corrupt")).unwrap_or_default();
    let obs = Arc::new(Obs::default());
    let mixer = Arc::new(Mutex::new(Mixer { pw: PipeWire::new(), state, file, modules: HashMap::new(), mic_last_used: HashMap::new(),
        missing_before: false, meters: Meters::new(), headset_on: None, stream_module: -1, obs: obs.clone() }));
    // listen first: another Sonar on the port means another one owns the sinks too
    let server = Server::http(&addr).expect("can't listen");
    mixer.lock().unwrap().bootstrap();

    let base = Arc::new(Base::default());
    let (b, m) = (base.clone(), mixer.clone());
    std::thread::spawn(move || b.run(m));

    let (b, m) = (base.clone(), mixer.clone());
    ctrlc::set_handler(move || {
        b.shutdown();
        // ponytail: waits for the lock, so a request mid-EQ-restart delays shutdown by up to ~2 s
        m.lock().unwrap().shutdown();
        std::process::exit(0);
    }).expect("can't install signal handler");

    let (b, m) = (base.clone(), mixer.clone());
    std::thread::spawn(move || {
        for tick in 0u64.. {
            std::thread::sleep(Duration::from_secs(1));
            if tick % 3 == 0 { b.poll(); } // the reply also tells when the headset turned on or off
            let mut mx = m.lock().unwrap();
            mx.sync_mic();
            mx.enforce_routing();
            mx.meters.stop_when_idle();
            if tick % 3 == 2 { mx.recover(); }
        }
    });

    eprintln!("Sonar (Rust) on http://{addr}");
    // ponytail: one request at a time, like the Java version's single mixer lock; only the 5 s mic test gets a thread
    for mut req in server.incoming_requests() {
        let url = req.url().split('?').next().unwrap_or("/").to_string();
        let resp = match (req.method(), url.as_str()) {
            // outside the mixer lock: the base station's reader takes it while the refresh waits for a reply
            (Method::Get, "/api/headset") => json_resp(Ok(json!(base.refresh()))),
            (Method::Get, "/api/batteries") => json_resp(Ok(json!(base::batteries()))),
            // these don't touch the mixer: no lock, so a slow OBS or busctl doesn't hold up the UI
            (Method::Get, "/api/hotkey") => json_resp(Ok(json!({ "key": desktop::hotkey() }))),
            (Method::Put, "/api/hotkey") => {
                let b = body_json(&mut req);
                json_resp(desktop::set_hotkey(b["key"].as_str()).map(|_| json!({ "key": desktop::hotkey() })))
            }
            (Method::Get, "/api/auto-update") => json_resp(Ok(json!({ "enabled": desktop::auto_update() }))),
            (Method::Put, "/api/auto-update") => {
                let b = body_json(&mut req);
                json_resp(desktop::set_auto_update(b["enabled"] == true).map(|on| json!({ "enabled": on })))
            }
            (Method::Get, "/api/obs") => json_resp(obs.status()),
            (Method::Get, "/api/obs/preview") => match obs.preview() {
                Ok(jpg) => Response::from_data(jpg).with_header(header("Content-Type", "image/jpeg")),
                Err(e) => json_resp(Err(e)),
            },
            (Method::Post, "/api/obs/record") => { let b = body_json(&mut req); json_resp(obs.record(&b)) }
            (Method::Post, "/api/obs/input") => { let b = body_json(&mut req); json_resp(obs.input(&b)) }
            (Method::Post, "/api/mic/test") => {
                let m = mixer.clone();
                std::thread::spawn(move || { let r = mic_test(&m, 5); respond(req, r); });
                continue;
            }
            (_, u) if u.starts_with("/api/") => {
                let mut body = Vec::new();
                let _ = req.as_reader().read_to_end(&mut body);
                let result = mixer.lock().unwrap().handle(req.method(), &url, &body);
                json_resp(result)
            }
            _ => {
                let rel = if url == "/" { "index.html" } else { url.trim_start_matches('/') };
                match (!rel.contains("..")).then(|| std::fs::read(static_dir.join(rel))).and_then(Result::ok) {
                    Some(data) => Response::from_data(data).with_header(header("Content-Type", content_type(rel))),
                    None => Response::from_string("Not found").with_status_code(404),
                }
            }
        };
        respond(req, resp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_gains_and_checks() {
        assert_eq!(sink_name_for(" Voice  Chat! "), "sonar_voice_chat_");
        assert_eq!(mixed(80, "sonar_game", 50), 40);
        assert_eq!(mixed(80, "sonar_chat", 50), 80);
        assert_eq!(mixed(80, "sonar_chat", -25), 60);
        assert!(check_name(Some("  !! ")).is_err());
        let b = check_bands(vec![EqBand::new("peaking", 5.0, 40.0, f64::NAN)]).unwrap();
        assert_eq!((b[0].freq, b[0].gain, b[0].q), (20.0, 12.0, 0.1));
        assert!(check_bands(vec![EqBand::new("allpass", 1.0, 0.0, 1.0)]).is_err());
        // the Java API's JSON loads as state
        let c: Channel = serde_json::from_str(r#"{"id":3,"name":"Media","sinkName":"sonar_media","active":true,"eq":[{"type":"lowshelf","freq":120.0,"gain":6.0,"q":0.71}]}"#).unwrap();
        assert_eq!((c.id, c.volume, c.eq.len()), (3, 100, 1));
    }

    #[test]
    fn eq_follows_the_output() {
        let dev = "alsa_output.usb-SteelSeries_Arctis_Nova_Pro_Wireless-00.analog-stereo";
        let mut c = Channel { device_name: Some(dev.into()), ..Default::default() };
        assert!(!follow_output(&mut c, None)); // headset state unknown: nothing to do
        assert!(!follow_output(&mut c, Some(true))); // first visit keeps the current EQ
        c.eq = vec![EqBand::new("peaking", 100.0, 3.0, 1.0)];
        assert!(!follow_output(&mut c, Some(false))); // speakers, first visit: keeps it too
        c.eq = vec![];
        assert!(follow_output(&mut c, Some(true))); // back on the headset: its EQ returns
        assert_eq!(c.eq.len(), 1);
        assert_eq!(c.eq_output.as_deref(), Some(dev));
        assert!(follow_output(&mut c, Some(false)));
        assert!(c.eq.is_empty());
        let mic = Channel { mic: true, device_name: Some(dev.into()), ..Default::default() };
        assert_eq!(output_key(&mic, None).as_deref(), Some(dev));
    }
}
