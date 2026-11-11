//! The one place that talks to PipeWire: shells out to pactl / pw-cli / pw-link, like PipeWireService.java.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EqBand {
    #[serde(rename = "type")]
    pub kind: String,
    pub freq: f64,
    pub gain: f64,
    pub q: f64,
}

impl EqBand {
    pub fn new(kind: &str, freq: f64, gain: f64, q: f64) -> Self {
        EqBand { kind: kind.into(), freq, gain, q }
    }
}

#[derive(Serialize)]
pub struct PhysicalSink {
    pub name: String,
    pub description: String,
}

pub struct AppStream {
    pub index: u32,
    pub binary: String,
    pub app_name: String,
    pub sink_name: String,
    pub pid: i32,
    pub media_role: Option<String>,
}

/// Streamer mode's mix: a null-sink every output channel's stream feed plays into; OBS captures its monitor.
pub const STREAM_SINK: &str = "sonar_stream";

const SURROUND: [&str; 8] = ["FL", "FR", "FC", "LFE", "RL", "RR", "SL", "SR"];
/// Where each 7.1 speaker sits, degrees counter-clockwise from the front (the SOFA convention); LFE as centre.
const SURROUND_AZIMUTH: [f64; 8] = [30.0, 330.0, 0.0, 0.0, 150.0, 210.0, 90.0, 270.0];

pub struct PipeWire {
    eq: Mutex<HashMap<String, Child>>,
    warned: Mutex<HashSet<String>>,
    eq_dir: PathBuf,
}

/// Lines from `pactl list <kind>` grouped per object: a block starts at a line beginning with `header`.
fn blocks<'a>(text: &'a str, header: &str) -> Vec<&'a str> {
    let mut starts: Vec<usize> = text.match_indices(header).map(|(i, _)| i)
        .filter(|&i| i == 0 || text.as_bytes()[i - 1] == b'\n').collect();
    starts.push(text.len());
    starts.windows(2).map(|w| &text[w[0] + header.len()..w[1]]).collect()
}

/// `Key: value` line in a pactl / upower block.
pub fn field<'a>(block: &'a str, key: &str) -> Option<&'a str> {
    block.lines().find_map(|l| l.trim_start().strip_prefix(key)?.strip_prefix(':')).map(str::trim)
}

/// `key = "value"` property in a pactl block.
fn prop<'a>(block: &'a str, key: &str) -> Option<&'a str> {
    let pat = format!("{key} = \"");
    let start = block.find(&pat)? + pat.len();
    block[start..].split('"').next()
}

/// SPA-JSON string literal: names come from users and devices, so escape them.
pub fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// pactl splits module args at spaces and has no escape for '"'.
fn description_arg(d: &str) -> String {
    format!("device.description=\"{}\"", d.replace('"', "'"))
}

/// A float as SPA-JSON reads it: always with a dot, so pw-cli doesn't take it for an int.
fn f(v: f64) -> String {
    format!("{v:?}")
}

fn prettify(name: &str) -> String {
    let n = name.strip_prefix("alsa_output.").or_else(|| name.strip_prefix("alsa_input.")).unwrap_or(name);
    n.replace(['.', '_', '-'], " ").trim().to_string()
}

fn describe(block: &str, name: &str) -> String {
    field(block, "Description").filter(|d| !d.is_empty()).map(String::from).unwrap_or_else(|| prettify(name))
}

/// A `pactl list sources` block that's an output's monitor rather than a mic.
fn is_output(block: &str) -> bool {
    field(block, "Monitor of Sink").is_some_and(|m| m != "n/a")
        || prop(block, "media.class").is_some_and(|c| c.contains("Sink"))
        || block.contains("device.class = \"monitor\"")
}

/// The name routing is remembered by: the binary, except under Wine/Proton, where application.name tells games apart.
fn app_key(binary: Option<&str>, app_name: &str) -> String {
    match binary {
        Some(b) if !b.is_empty() && !matches!(b, "wine" | "wine64" | "wine-preloader" | "wine64-preloader") => b.into(),
        _ => app_name.to_lowercase(),
    }
}

impl PipeWire {
    pub fn new() -> Self {
        let run = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
        PipeWire { eq: Mutex::default(), warned: Mutex::default(), eq_dir: Path::new(&run).join("sonar-eq") }
    }

    fn warn_once(&self, msg: String) {
        if self.warned.lock().unwrap().insert(msg.clone()) {
            eprintln!("WARN {msg}");
        }
    }

    /// Exit code (-1 when it can't run) and stdout+stderr. LC_ALL: we parse the output, pactl is translated on Bazzite.
    fn run(&self, cmd: &[&str]) -> (i32, String) {
        match Command::new(cmd[0]).args(&cmd[1..]).env("LC_ALL", "C.UTF-8").output() {
            Ok(o) => {
                let mut out = String::from_utf8_lossy(&o.stdout).into_owned();
                out.push_str(&String::from_utf8_lossy(&o.stderr));
                let code = o.status.code().unwrap_or(-1);
                if code != 0 {
                    self.warn_once(format!("{} {} failed (exit {code}): {}", cmd[0], cmd[1], out.lines().next().unwrap_or("")));
                }
                (code, out)
            }
            Err(e) => {
                self.warn_once(format!("Can't run {} ({e}); is it installed?", cmd[0]));
                (-1, String::new())
            }
        }
    }

    fn out(&self, cmd: &[&str]) -> String {
        self.run(cmd).1
    }

    // --- modules ---

    /// surround: 7.1, so games output surround for the spatial audio chain.
    pub fn load_null_sink(&self, sink: &str, description: &str, surround: bool) -> i64 {
        let (name, desc) = (format!("sink_name={sink}"), description_arg(description));
        let mut cmd = vec!["pactl", "load-module", "module-null-sink", &name, &desc];
        if surround {
            cmd.extend(["channels=8", "channel_map=front-left,front-right,front-center,lfe,rear-left,rear-right,side-left,side-right"]);
        }
        self.out(&cmd).trim().parse().unwrap_or(-1)
    }

    pub fn load_virtual_source(&self, name: &str, description: &str) -> i64 {
        let out = self.out(&["pactl", "load-module", "module-null-sink", &format!("sink_name={name}"),
            "media.class=Audio/Source/Virtual", "channel_map=mono", &description_arg(description)]);
        out.trim().parse().unwrap_or(-1)
    }

    pub fn unload_module(&self, id: i64) {
        self.run(&["pactl", "unload-module", &id.to_string()]);
    }

    /// Unload any module we created, e.g. leftovers from a crash.
    pub fn unload_sonar_modules(&self) {
        for line in self.out(&["pactl", "list", "short", "modules"]).lines() {
            let mut parts = line.splitn(3, '\t');
            if let (Some(id), Some(_), Some(args)) = (parts.next(), parts.next(), parts.next())
                && args.contains("sonar_")
                && let Ok(id) = id.trim().parse()
            {
                self.unload_module(id);
            }
        }
    }

    /// Recording streams on source `name` (apps using the virtual mic, and our level meter).
    pub fn source_consumers(&self, name: &str) -> usize {
        let Some(index) = self.index_to_name("sources").into_iter().find(|(_, n)| n == name).map(|(i, _)| i.to_string()) else {
            return 0;
        };
        self.out(&["pactl", "list", "short", "source-outputs"]).lines()
            .filter(|l| l.split('\t').nth(1) == Some(index.as_str())).count()
    }

    // --- EQ: one `pipewire -c <conf>` child per channel running a filter-chain (see PipeWireService.java) ---

    /// (Re)start the EQ for channel node `name`, routed to/from `device`. volume: linear gain after the EQ;
    /// stream: also feed the channel into STREAM_SINK at this gain; spatial: virtual 7.1 after the EQ.
    #[allow(clippy::too_many_arguments)]
    pub fn start_eq(&self, name: &str, description: &str, device: &str, bands: &[EqBand], mic: bool, denoise: bool,
                    volume: f64, stream: Option<f64>, spatial: bool) {
        self.stop_eq(name);
        let plugin = if mic && denoise { rnnoise_plugin() } else { None };
        let hrtf = if spatial && !mic { spatial_hrtf() } else { None };
        let conf = self.eq_dir.join(format!("{name}.conf"));
        let text = eq_conf(name, description, device, bands, mic, plugin.as_deref(), volume, stream, hrtf.as_deref());
        let started = std::fs::create_dir_all(&self.eq_dir)
            .and_then(|_| std::fs::write(&conf, text))
            .and_then(|_| Command::new("pipewire").arg("-c").arg(&conf).stdout(Stdio::null()).stderr(Stdio::null()).spawn());
        match started {
            Ok(child) => { self.eq.lock().unwrap().insert(name.into(), child); }
            Err(e) => { eprintln!("ERROR can't start EQ for {name}: {e}"); return; }
        }
        if mic {
            // WirePlumber won't link a stream into a virtual source: link it ourselves
            let out = format!("{name}_eq_out:");
            for _ in 0..30 {
                if self.out(&["pw-link", "-o"]).contains(&out) { break; }
                std::thread::sleep(Duration::from_millis(100));
            }
            self.run(&["pw-link", &format!("{name}_eq_out"), name]);
        }
    }

    pub fn is_eq_running(&self, name: &str) -> bool {
        self.eq.lock().unwrap().get_mut(name).is_some_and(|c| matches!(c.try_wait(), Ok(None)))
    }

    /// Live freq/gain/Q change; bands[i] is node b{i}.
    pub fn set_eq_params(&self, name: &str, bands: &[EqBand]) {
        let params: String = bands.iter().enumerate()
            .map(|(i, b)| format!(" \"b{i}:Freq\" {} \"b{i}:Q\" {} \"b{i}:Gain\" {}", f(b.freq), f(b.q), f(b.gain))).collect();
        self.run(&["pw-cli", "set-param", &format!("{name}_eq_in"), "Props", &format!("{{ params = [{params} ] }}")]);
    }

    /// Live gain of a chain's "vol" node; node = `<sink>_eq_in` or `<sink>_stream_in`.
    pub fn set_gain(&self, node: &str, gain: f64) {
        self.run(&["pw-cli", "set-param", node, "Props", &format!("{{ params = [ \"vol:Mult\" {} ] }}", f(gain))]);
    }

    pub fn stop_eq(&self, name: &str) {
        let child = self.eq.lock().unwrap().remove(name);
        if let Some(mut c) = child {
            await_exit(c.id());
            let _ = c.try_wait();
        }
    }

    /// Stop every EQ process, including orphans from a crashed run (found by their conf path).
    pub fn stop_all_eq(&self) {
        let names: Vec<String> = self.eq.lock().unwrap().keys().cloned().collect();
        names.iter().for_each(|n| self.stop_eq(n));
        let dir = self.eq_dir.to_string_lossy().into_owned();
        for entry in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else { continue };
            let cmdline = std::fs::read(entry.path().join("cmdline")).unwrap_or_default();
            let mut args = cmdline.split(|&b| b == 0).map(String::from_utf8_lossy);
            if args.next().is_some_and(|a| a.ends_with("pipewire")) && args.any(|a| a.contains(&dir)) {
                await_exit(pid);
            }
        }
    }

    // --- sinks / routing / volume ---

    pub fn is_up(&self) -> bool {
        self.run(&["pactl", "info"]).0 == 0
    }

    pub fn index_to_name(&self, kind: &str) -> HashMap<u32, String> {
        self.out(&["pactl", "list", "short", kind]).lines().filter_map(|l| {
            let mut p = l.split('\t');
            Some((p.next()?.trim().parse().ok()?, p.next()?.trim().to_string()))
        }).collect()
    }

    pub fn list_physical_sinks(&self) -> Vec<PhysicalSink> {
        blocks(&self.out(&["pactl", "list", "sinks"]), "Sink #").into_iter().filter_map(|b| {
            let name = field(b, "Name")?;
            (!name.starts_with("sonar_")).then(|| PhysicalSink { name: name.into(), description: describe(b, name) })
        }).collect()
    }

    pub fn list_physical_sources(&self) -> Vec<PhysicalSink> {
        blocks(&self.out(&["pactl", "list", "sources"]), "Source #").into_iter().filter_map(|b| {
            let name = field(b, "Name")?;
            (!name.starts_with("sonar_") && !is_output(b)).then(|| PhysicalSink { name: name.into(), description: describe(b, name) })
        }).collect()
    }

    fn default_of(&self, what: &str) -> String {
        let (code, out) = self.run(&["pactl", what]);
        if code == 0 { out.trim().into() } else { String::new() }
    }

    pub fn default_source(&self) -> String {
        self.default_of("get-default-source")
    }

    pub fn default_sink(&self) -> String {
        self.default_of("get-default-sink")
    }

    pub fn move_input(&self, index: u32, sink: &str) {
        self.run(&["pactl", "move-sink-input", &index.to_string(), sink]);
    }

    pub fn set_volume(&self, name: &str, volume: i32, source: bool) {
        self.run(&["pactl", if source { "set-source-volume" } else { "set-sink-volume" }, name, &format!("{volume}%")]);
    }

    pub fn set_mute(&self, name: &str, muted: bool, source: bool) {
        self.run(&["pactl", if source { "set-source-mute" } else { "set-sink-mute" }, name, if muted { "1" } else { "0" }]);
    }

    pub fn list_streams(&self) -> Vec<AppStream> {
        let sinks = self.index_to_name("sinks");
        blocks(&self.out(&["pactl", "list", "sink-inputs"]), "Sink Input #").into_iter().filter_map(|b| {
            let index = b.lines().next()?.trim().parse().ok()?;
            let app = prop(b, "application.name");
            let binary = prop(b, "application.process.binary");
            let app_name = app.or(binary)?.to_string();
            let sink: Option<u32> = field(b, "Sink").and_then(|s| s.parse().ok());
            Some(AppStream {
                index,
                binary: app_key(binary, &app_name),
                sink_name: sink.and_then(|s| sinks.get(&s).cloned()).unwrap_or_else(|| "?".into()),
                pid: prop(b, "application.process.id").and_then(|p| p.parse().ok()).unwrap_or(-1),
                media_role: prop(b, "media.role").map(String::from),
                app_name,
            })
        }).collect()
    }
}

/// SIGTERM and wait, so the process's nodes are gone before a replacement registers the same names.
fn await_exit(pid: u32) {
    let _ = Command::new("kill").arg(pid.to_string()).status();
    let proc = PathBuf::from(format!("/proc/{pid}"));
    let deadline = Instant::now() + Duration::from_secs(2);
    // a child of ours stays a zombie until reaped; its status says so
    let alive = || std::fs::read_to_string(proc.join("status")).is_ok_and(|s| !s.contains("State:\tZ"));
    while alive() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    if alive() {
        let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
    }
}

/// Record `seconds` of a mono source as a 48 kHz 16-bit WAV (shorter if the source stops).
pub fn record_wav(source: &str, seconds: u64) -> std::io::Result<Vec<u8>> {
    let mut p = Command::new("parec").args(["-d", source, "--raw", "--format=s16le", "--rate=48000", "--channels=1", "--latency-msec=20"])
        .stdout(Stdio::piped()).stderr(Stdio::null()).spawn()?;
    let pid = p.id();
    // a source that never delivers: kill parec, which ends the read
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(seconds + 3));
        let _ = Command::new("kill").arg(pid.to_string()).status();
    });
    let mut pcm = Vec::new();
    p.stdout.take().unwrap().take(seconds * 48000 * 2).read_to_end(&mut pcm)?;
    let _ = p.kill();
    let _ = p.wait();
    Ok(wav(&pcm, 48000))
}

/// 16-bit mono PCM wrapped in a WAV header.
fn wav(pcm: &[u8], rate: u32) -> Vec<u8> {
    let len = pcm.len() as u32;
    let mut b = Vec::with_capacity(44 + pcm.len());
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + len).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes()); // PCM
    b.extend_from_slice(&1u16.to_le_bytes()); // mono
    b.extend_from_slice(&rate.to_le_bytes());
    b.extend_from_slice(&(rate * 2).to_le_bytes());
    b.extend_from_slice(&2u16.to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&len.to_le_bytes());
    b.extend_from_slice(pcm);
    b
}

/// rnnoise's LADSPA plugin (noise-suppression-for-voice), if installed.
pub fn rnnoise_plugin() -> Option<PathBuf> {
    let home = std::env::var("HOME").unwrap_or_default();
    ["/usr/lib/ladspa", "/usr/lib64/ladspa", "/usr/local/lib/ladspa", &format!("{home}/.local/lib/ladspa")]
        .iter().map(|d| Path::new(d).join("librnnoise_ladspa.so")).find(|p| p.exists())
}

/// The HRTF for spatial audio (a SOFA file), or None when it or PipeWire's SOFA plugin is missing.
pub fn spatial_hrtf() -> Option<PathBuf> {
    let plugin = ["/usr/lib/spa-0.2", "/usr/lib64/spa-0.2", "/usr/lib/x86_64-linux-gnu/spa-0.2"]
        .iter().any(|d| Path::new(d).join("filter-graph/libspa-filter-graph-plugin-sofa.so").exists());
    if !plugin {
        return None;
    }
    let env: HashMap<String, String> = std::env::vars().collect();
    hrtf_candidates(&env, &std::env::var("HOME").unwrap_or_default()).into_iter().map(PathBuf::from).find(|p| p.exists())
}

/// Where a SOFA file can be: the user's override, then libmysofa's HRTFs in each XDG and package-manager data dir.
fn hrtf_candidates(env: &HashMap<String, String>, home: &str) -> Vec<String> {
    let get = |k: &str| env.get(k).filter(|v| !v.trim().is_empty()).cloned();
    let data_home = get("XDG_DATA_HOME").unwrap_or_else(|| format!("{home}/.local/share"));
    let data_dirs = get("XDG_DATA_DIRS").unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    let mut dirs = vec![data_home.clone()];
    dirs.extend(data_dirs.split(':').map(String::from));
    dirs.extend(["/usr/share", "/usr/local/share", "/home/linuxbrew/.linuxbrew/share", &format!("{home}/.linuxbrew/share"),
        &format!("{home}/.nix-profile/share"), "/nix/var/nix/profiles/default/share", "/run/current-system/sw/share",
        &format!("{home}/.guix-profile/share"), "/run/current-system/profile/share"].map(String::from));
    dirs.extend(["HOMEBREW_PREFIX", "CONDA_PREFIX"].iter().filter_map(|k| get(k)).map(|p| format!("{p}/share")));
    let mut seen = HashSet::new();
    let mut out = vec![format!("{data_home}/sonar/hrtf.sofa")];
    for d in dirs.iter().filter(|d| !d.trim().is_empty()).map(|d| d.trim_end_matches('/')) {
        if seen.insert(d.to_string()) {
            out.push(format!("{d}/libmysofa/MIT_KEMAR_normal_pinna.sofa"));
            out.push(format!("{d}/libmysofa/default.sofa"));
        }
    }
    out
}

fn gain_node(gain: f64) -> String {
    format!("          {{ type = builtin name = vol label = linear control = {{ \"Mult\" = {} }} }}\n", f(gain))
}

/// filter-chain config: optional rnnoise, a biquad per band, the `vol` gain node; plus spatial and stream chains.
#[allow(clippy::too_many_arguments)]
pub fn eq_conf(name: &str, description: &str, device: &str, bands: &[EqBand], mic: bool, rnnoise: Option<&Path>,
               volume: f64, stream: Option<f64>, hrtf: Option<&Path>) -> String {
    let mut nodes = String::new();
    let mut links = String::new();
    if let Some(p) = rnnoise {
        nodes += &format!("          {{ type = ladspa name = ns plugin = {} label = noise_suppressor_mono control = {{ \"VAD Threshold (%)\" = 50.0 }} }}\n", quote(&p.to_string_lossy()));
        links += "          { output = \"ns:Output\" input = \"b0:In\" }\n";
    }
    let flat = [EqBand::new("peaking", 1000.0, 0.0, 1.0)]; // a graph needs a node: 0 dB passes audio through
    let bands = if bands.is_empty() { &flat[..] } else { bands };
    for (i, b) in bands.iter().enumerate() {
        nodes += &format!("          {{ type = builtin name = b{i} label = bq_{} control = {{ \"Freq\" = {} \"Q\" = {} \"Gain\" = {} }} }}\n",
            b.kind, f(b.freq), f(b.q), f(b.gain));
        if i > 0 {
            links += &format!("          {{ output = \"b{}:Out\" input = \"b{i}:In\" }}\n", i - 1);
        }
    }
    nodes += &gain_node(volume);
    links += &format!("          {{ output = \"b{}:Out\" input = \"vol:In\" }}\n", bands.len() - 1);
    let (eq_in, eq_out) = (quote(&format!("{name}_eq_in")), quote(&format!("{name}_eq_out")));
    let capture = if mic {
        format!("node.name = {eq_in} target.object = {} node.passive = true", quote(device))
    } else {
        format!("node.name = {eq_in} target.object = {} stream.capture.sink = true node.passive = true", quote(name))
    };
    let playback = match (mic, hrtf) {
        (true, _) => format!("node.name = {eq_out} node.autoconnect = false"), // mic: linked by start_eq
        (false, None) => format!("node.name = {eq_out} target.object = {}", quote(device)),
        (false, Some(_)) => format!("node.name = {eq_out} target.object = {}", quote(&format!("{name}_spatial_in"))),
    };
    let (channels, position) = if mic { (1, "MONO".to_string()) } else if hrtf.is_some() { (8, SURROUND.join(" ")) } else { (2, "FL FR".into()) };
    let extra = hrtf.map(|h| spatial(name, description, device, h)).unwrap_or_default()
        + &stream.map(|g| stream_feed(name, description, g)).unwrap_or_default();
    format!(r#"context.spa-libs = {{ audio.convert.* = audioconvert/libspa-audioconvert support.* = support/libspa-support }}
context.modules = [
  {{ name = libpipewire-module-rt flags = [ ifexists nofail ] }}
  {{ name = libpipewire-module-protocol-native }}
  {{ name = libpipewire-module-client-node }}
  {{ name = libpipewire-module-adapter }}
  {{ name = libpipewire-module-filter-chain
    args = {{
      node.description = {}
      filter.graph = {{
        nodes = [
{nodes}        ]
        links = [
{links}        ]
      }}
      audio.channels = {channels}
      audio.position = [ {position} ]
      capture.props = {{ {capture} }}
      playback.props = {{ {playback} }}
    }}
  }}
{extra}]
"#, quote(&format!("{description} EQ")))
}

/// Spatial audio: each 7.1 speaker through the HRTF from its direction, mixed down to stereo at half gain.
fn spatial(name: &str, description: &str, device: &str, hrtf: &Path) -> String {
    let mut nodes = String::new();
    let mut links = String::new();
    let gains: String = (1..=8).map(|i| format!(" \"Gain {i}\" = 0.5")).collect();
    for (i, sp) in SURROUND.iter().enumerate() {
        nodes += &format!("          {{ type = sofa label = spatializer name = sp{sp} config = {{ filename = {} }} control = {{ \"Azimuth\" = {} \"Elevation\" = 0.0 \"Radius\" = 1.0 }} }}\n",
            quote(&hrtf.to_string_lossy()), f(SURROUND_AZIMUTH[i]));
        links += &format!("          {{ output = \"sp{sp}:Out L\" input = \"mixL:In {n}\" }}\n          {{ output = \"sp{sp}:Out R\" input = \"mixR:In {n}\" }}\n", n = i + 1);
    }
    for side in ["L", "R"] {
        nodes += &format!("          {{ type = builtin label = mixer name = mix{side} control = {{{gains} }} }}\n");
    }
    let inputs: Vec<String> = SURROUND.iter().map(|sp| quote(&format!("sp{sp}:In"))).collect();
    format!(r#"  {{ name = libpipewire-module-filter-chain
    args = {{
      node.description = {}
      filter.graph = {{
        nodes = [
{nodes}        ]
        links = [
{links}        ]
        inputs = [ {} ]
        outputs = [ "mixL:Out" "mixR:Out" ]
      }}
      capture.props = {{ node.name = {} media.class = Audio/Sink audio.channels = 8 audio.position = [ {} ] }}
      playback.props = {{ node.name = {} target.object = {} audio.channels = 2 audio.position = [ FL FR ] }}
    }}
  }}
"#, quote(&format!("{description} Spatial")), inputs.join(" "), quote(&format!("{name}_spatial_in")), SURROUND.join(" "),
        quote(&format!("{name}_spatial_out")), quote(device))
}

/// Second chain: the sink's monitor (before EQ and volume) into the stream mix.
fn stream_feed(name: &str, description: &str, gain: f64) -> String {
    format!(r#"  {{ name = libpipewire-module-filter-chain
    args = {{
      node.description = {}
      filter.graph = {{ nodes = [
{}      ] }}
      audio.channels = 2
      audio.position = [ FL FR ]
      capture.props = {{ node.name = {} target.object = {} stream.capture.sink = true node.passive = true }}
      playback.props = {{ node.name = {} target.object = {} }}
    }}
  }}
"#, quote(&format!("{description} Stream")), gain_node(gain), quote(&format!("{name}_stream_in")), quote(name),
        quote(&format!("{name}_stream_out")), quote(STREAM_SINK))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pactl_blocks() {
        let text = "Source #1\n\tName: alsa_input.usb-mic\n\tDescription: USB Mic\n\tMonitor of Sink: n/a\n\
                    Source #2\n\tName: alsa_output.x.monitor\n\tDescription: Monitor\n\tMonitor of Sink: alsa_output.x\n";
        let b = blocks(text, "Source #");
        assert_eq!(b.len(), 2);
        assert_eq!(field(b[0], "Name"), Some("alsa_input.usb-mic"));
        assert!(!is_output(b[0]) && is_output(b[1]));
        assert_eq!(describe("\tName: x\n", "alsa_output.usb-Foo_Bar.analog"), "usb Foo Bar analog");
        assert_eq!(prop("\t\tapplication.name = \"Fire fox\"\n", "application.name"), Some("Fire fox"));
        assert_eq!(app_key(Some("wine64-preloader"), "Elden Ring"), "elden ring");
        assert_eq!(app_key(Some("firefox"), "Firefox"), "firefox");
    }

    #[test]
    fn eq_conf_chains_bands_into_vol() {
        let c = eq_conf("sonar_game", "Game \"x\"", "dev", &[EqBand::new("lowshelf", 100.0, 3.0, 0.71)], false, None, 1.0, None, None);
        assert!(c.contains("label = bq_lowshelf control = { \"Freq\" = 100.0 \"Q\" = 0.71 \"Gain\" = 3.0 }"));
        assert!(c.contains("{ output = \"b0:Out\" input = \"vol:In\" }"));
        assert!(c.contains("node.description = \"Game \\\"x\\\" EQ\""));
        assert!(c.contains("target.object = \"dev\""));
        assert!(!c.contains("Stream") && !c.contains("sofa"));
    }

    #[test]
    fn eq_conf_spatial_and_stream() {
        let c = eq_conf("sonar_game", "Game", "dev", &[], false, None, 1.0, Some(0.5), Some(Path::new("/h.sofa")));
        assert!(c.contains("audio.channels = 8"));
        assert!(c.contains("target.object = \"sonar_game_spatial_in\""));
        assert!(c.contains("name = spSL config = { filename = \"/h.sofa\" } control = { \"Azimuth\" = 90.0"));
        assert!(c.contains("{ output = \"spSR:Out R\" input = \"mixR:In 8\" }"));
        assert!(c.contains("node.name = \"sonar_game_spatial_out\" target.object = \"dev\""));
        assert!(c.contains("node.name = \"sonar_game_stream_out\" target.object = \"sonar_stream\""));
        assert!(c.contains("\"Mult\" = 0.5"));
    }

    #[test]
    fn wav_header_and_hrtf_paths() {
        let w = wav(&[1, 2, 3, 4], 48000);
        assert_eq!((&w[0..4], &w[8..16], w.len()), (&b"RIFF"[..], &b"WAVEfmt "[..], 48));
        assert_eq!(u32::from_le_bytes(w[40..44].try_into().unwrap()), 4);
        let env = HashMap::from([("XDG_DATA_DIRS".to_string(), "/usr/share/:/opt/share".to_string())]);
        let c = hrtf_candidates(&env, "/home/u");
        assert_eq!(c[0], "/home/u/.local/share/sonar/hrtf.sofa");
        assert!(c.contains(&"/opt/share/libmysofa/default.sofa".to_string()));
        assert_eq!(c.iter().filter(|p| p.starts_with("/usr/share/")).count(), 2); // deduplicated
    }
}
