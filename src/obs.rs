//! Minimal obs-websocket v5 client (ObsClient.java) and the OBS remote API (ObsController.java).
//! Port and password come from OBS's own config; connects on the first request and again after OBS restarts.

use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tungstenite::{Message, WebSocket};

type Res<T> = Result<T, (u16, String)>;

const RETRY: Duration = Duration::from_secs(3); // don't knock on a closed port every poll

#[derive(Default)]
pub struct Obs {
    conn: Mutex<Conn>,
}

#[derive(Default)]
struct Conn {
    ws: Option<WebSocket<TcpStream>>,
    status: String,
    last_attempt: Option<Instant>,
    next_id: u64,
}

/// obs-websocket auth: base64(sha256(base64(sha256(password + salt)) + challenge)).
fn auth(password: &str, salt: &str, challenge: &str) -> String {
    let h = |s: String| B64.encode(Sha256::digest(s.as_bytes()));
    h(h(format!("{password}{salt}")) + challenge)
}

fn read_json(ws: &mut WebSocket<TcpStream>) -> Result<Value, String> {
    loop {
        match ws.read().map_err(|e| e.to_string())? {
            Message::Text(t) => return serde_json::from_str(t.as_str()).map_err(|e| e.to_string()),
            Message::Close(f) => return Err(f.map(|f| f.reason.to_string()).unwrap_or_default()),
            _ => {}
        }
    }
}

fn send_json(ws: &mut WebSocket<TcpStream>, v: Value) -> Result<(), String> {
    ws.send(Message::text(v.to_string())).map_err(|e| e.to_string())
}

impl Conn {
    fn connect(&mut self) -> bool {
        if self.ws.is_some() {
            return true;
        }
        if self.last_attempt.is_some_and(|t| t.elapsed() < RETRY) {
            return false;
        }
        self.last_attempt = Some(Instant::now());
        let path = PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config/obs-studio/plugin_config/obs-websocket/config.json");
        let Some(config) = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()) else {
            self.status = "OBS isn't installed (no obs-websocket config)".into();
            return false;
        };
        if config["server_enabled"] != true {
            self.status = "Enable the WebSocket server in OBS: Tools → WebSocket Server Settings".into();
            return false;
        }
        let password = (config["auth_required"] == true).then(|| config["server_password"].as_str().unwrap_or("").to_string());
        let port = config["server_port"].as_u64().unwrap_or(4455);
        let Ok(stream) = TcpStream::connect_timeout(&([127, 0, 0, 1], port as u16).into(), Duration::from_secs(1)) else {
            self.status = "OBS isn't running".into();
            return false;
        };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
        let Ok((mut ws, _)) = tungstenite::client(format!("ws://localhost:{port}"), stream) else {
            self.status = "OBS isn't running".into();
            return false;
        };
        // Hello -> Identify -> Identified; events off (eventSubscriptions 0), so only replies arrive afterwards
        let identified = (|| {
            let hello = read_json(&mut ws)?;
            let a = &hello["d"]["authentication"];
            let mut identify = json!({ "rpcVersion": 1, "eventSubscriptions": 0 });
            if let (false, Some(p)) = (a.is_null(), &password) {
                identify["authentication"] = json!(auth(p, a["salt"].as_str().unwrap_or(""), a["challenge"].as_str().unwrap_or("")));
            }
            send_json(&mut ws, json!({ "op": 1, "d": identify }))?;
            (read_json(&mut ws)?["op"] == 2).then_some(()).ok_or_else(|| "not identified".to_string())
        })();
        match identified {
            Ok(()) => { self.ws = Some(ws); true }
            Err(_) => { self.status = "OBS rejected the WebSocket password".into(); false }
        }
    }

    fn request(&mut self, kind: &str, data: Value) -> Res<Value> {
        if !self.connect() {
            return Err((503, self.status.clone()));
        }
        self.next_id += 1;
        let id = self.next_id.to_string();
        let ws = self.ws.as_mut().unwrap();
        let reply = send_json(ws, json!({ "op": 6, "d": { "requestType": kind, "requestId": id, "requestData": data } }))
            .and_then(|_| loop {
                let msg = read_json(ws)?;
                if msg["op"] == 7 && msg["d"]["requestId"] == id.as_str() {
                    break Ok(msg["d"].clone());
                }
            });
        let d = match reply {
            Ok(d) => d,
            Err(_) => {
                self.ws = None; // OBS went away: reconnect on the next request
                self.last_attempt = None;
                return Err((503, format!("OBS didn't answer {kind}")));
            }
        };
        let status = &d["requestStatus"];
        if status["result"] != true {
            return Err((400, format!("OBS: {}", status["comment"].as_str().unwrap_or(&format!("{kind} failed")))));
        }
        Ok(d["responseData"].clone())
    }
}

impl Obs {
    pub fn request(&self, kind: &str, data: Value) -> Res<Value> {
        self.conn.lock().unwrap().request(kind, data)
    }

    /// Why OBS isn't usable right now, or None when connected.
    fn unavailable(&self) -> Option<String> {
        let mut c = self.conn.lock().unwrap();
        (!c.connect()).then(|| c.status.clone())
    }

    /// Re-point OBS inputs capturing a Sonar channel at it again: Sonar's sinks are new on every start, and OBS
    /// only reconnects when the device *changes*, so hop via "default" and back, 1 s apart.
    pub fn reattach_sonar_inputs(&self) {
        if self.unavailable().is_some() {
            return;
        }
        let Ok(list) = self.request("GetInputList", json!({})) else { return };
        for input in list["inputs"].as_array().into_iter().flatten() {
            let name = input["inputName"].as_str().unwrap_or("");
            let Ok(s) = self.request("GetInputSettings", json!({ "inputName": name })) else { continue };
            let device = s["inputSettings"]["device_id"].as_str().unwrap_or("").to_string();
            if device.starts_with("sonar_") {
                let set = |d: &str| self.request("SetInputSettings", json!({ "inputName": name, "inputSettings": { "device_id": d }, "overlay": true }));
                let _ = set("default");
                std::thread::sleep(Duration::from_secs(1));
                if set(&device).is_ok() {
                    eprintln!("OBS: reattached '{name}' to {device}");
                }
            }
        }
    }

    /// GET /api/obs: recording state and each audio input's mute and device.
    pub fn status(&self) -> Res<Value> {
        if let Some(reason) = self.unavailable() {
            return Ok(json!({ "reason": reason, "recording": null, "inputs": [] }));
        }
        let rec = self.request("GetRecordStatus", json!({}))?;
        let mut inputs = vec![];
        for input in self.request("GetInputList", json!({}))?["inputs"].as_array().into_iter().flatten() {
            let name = input["inputName"].as_str().unwrap_or("");
            let by_name = json!({ "inputName": name });
            let Ok(mute) = self.request("GetInputMute", by_name.clone()) else { continue }; // no audio: a video source
            let device = self.request("GetInputSettings", by_name)?["inputSettings"]["device_id"].as_str().unwrap_or("default").to_string();
            let devices: Vec<Value> = self.request("GetInputPropertiesListPropertyItems", json!({ "inputName": name, "propertyName": "device_id" }))
                .map(|r| r["propertyItems"].as_array().into_iter().flatten().filter(|i| i["itemEnabled"] != false)
                    .map(|i| json!({ "name": i["itemName"], "value": i["itemValue"] })).collect())
                .unwrap_or_default();
            inputs.push(json!({ "name": name, "muted": mute["inputMuted"] == true, "deviceId": device, "devices": devices }));
        }
        Ok(json!({ "reason": null, "inputs": inputs,
            "recording": { "active": rec["outputActive"] == true, "paused": rec["outputPaused"] == true, "timecode": rec["outputTimecode"] } }))
    }

    /// The program scene as a small JPEG (obs-websocket has screenshots, no video stream).
    pub fn preview(&self) -> Res<Vec<u8>> {
        let scene = self.request("GetCurrentProgramScene", json!({}))?["currentProgramSceneName"].clone();
        let shot = self.request("GetSourceScreenshot", json!({ "sourceName": scene, "imageFormat": "jpg", "imageWidth": 480, "imageCompressionQuality": 70 }))?;
        let data = shot["imageData"].as_str().unwrap_or("");
        B64.decode(&data[data.find(',').map_or(0, |i| i + 1)..]).map_err(|e| (500, e.to_string()))
    }

    pub fn record(&self, b: &Value) -> Res<Value> {
        let kind = match b["action"].as_str() {
            Some("start") => "StartRecord",
            Some("stop") => "StopRecord",
            Some("pause") => "PauseRecord",
            Some("resume") => "ResumeRecord",
            _ => return Err((400, "action must be start, stop, pause or resume".into())),
        };
        self.request(kind, json!({})).map(|_| Value::Null)
    }

    pub fn input(&self, b: &Value) -> Res<Value> {
        let name = b["name"].as_str().filter(|n| !n.trim().is_empty()).ok_or((400, "Missing field: name".to_string()))?;
        if let Some(m) = b["muted"].as_bool() {
            self.request("SetInputMute", json!({ "inputName": name, "inputMuted": m }))?;
        }
        if let Some(d) = b["deviceId"].as_str() {
            self.request("SetInputSettings", json!({ "inputName": name, "inputSettings": { "device_id": d }, "overlay": true }))?;
        }
        Ok(Value::Null)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn auth_matches_the_spec_formula() {
        // python: b64(sha256(b64(sha256(b"pw" + b"salt")) + b"chal"))
        assert_eq!(super::auth("pw", "salt", "chal"), "Q0/N7u40IGqt4Ynf9v3w9R2yCDZxba9npsZLdIyWi8M=");
    }
}
