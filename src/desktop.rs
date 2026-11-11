//! Desktop integration: the overlay hotkey in KDE's kglobalaccel (OverlayHotkey.java), the auto-update timer
//! (UpdateController.java), and guessing an app's channel from Steam, media.role and .desktop Categories
//! (AppClassifier.java).

use std::collections::HashMap;
use std::process::Command;
use std::sync::OnceLock;

use crate::pw::AppStream;

type Res<T> = Result<T, (u16, String)>;

// --- overlay hotkey: Qt key codes, modifier bits | key, e.g. Meta+S = 0x10000000 | 0x53 ---

const ACTION: [&str; 4] = ["dev.sonar.overlay.desktop", "_launch", "Sonar Overlay", "Sonar Overlay"];
const MODIFIERS: [(&str, u32); 4] = [("Meta", 0x1000_0000), ("Ctrl", 0x0400_0000), ("Alt", 0x0800_0000), ("Shift", 0x0200_0000)];
const MODIFIER_MASK: u32 = 0x1e00_0000;
const F1: u32 = 0x0100_0030;

/// A kglobalaccel D-Bus call; its output, or None when it fails (no KDE).
fn busctl(method: &str, signature: &str, args: &[&str]) -> Option<String> {
    let o = Command::new("busctl").args(["--user", "call", "org.kde.kglobalaccel", "/kglobalaccel", "org.kde.KGlobalAccel", method, signature])
        .args(args).output().ok()?;
    o.status.success().then(|| String::from_utf8_lossy(&o.stdout).into_owned())
}

/// The current hotkey, e.g. "Meta+S"; "" when none is set; None when KDE's kglobalaccel isn't there.
pub fn hotkey() -> Option<String> {
    let out = busctl("shortcut", "as", &[&["4"], &ACTION[..]].concat())?;
    // "ai N a b ...": the ints after the count
    Some(out.split_whitespace().nth(2).and_then(|k| k.parse().ok()).map(format).unwrap_or_default())
}

/// Bind the overlay to `combo`; refuses a key another program already uses.
pub fn set_hotkey(combo: Option<&str>) -> Res<()> {
    let key = parse(combo.unwrap_or("")).map_err(|e| (400, e))?;
    let owners = busctl("getGlobalShortcutsByKey", "i", &[&key.to_string()])
        .ok_or((503, "Changing the hotkey needs KDE Plasma; set it in your desktop's shortcut settings".to_string()))?;
    if let Some(other) = other_owner(&owners) {
        return Err((400, format!("{} is already used by {other}", format(key))));
    }
    let k = key.to_string();
    busctl("setForeignShortcut", "asai", &[&["4"], &ACTION[..], &["1", &k]].concat())
        .map(|_| ()).ok_or((503, format!("KDE refused the hotkey {}", format(key))))
}

/// "Ctrl+Alt+S" -> Qt key code. Letters, digits and F1–F24; a modifier is required except on F-keys.
fn parse(combo: &str) -> Result<u32, String> {
    if combo.trim().is_empty() {
        return Err("No hotkey given".into());
    }
    let (mut mods, mut key) = (0, None);
    for part in combo.split('+').map(str::trim) {
        if let Some((_, bit)) = MODIFIERS.iter().find(|(m, _)| *m == part) {
            mods |= bit;
        } else if key.is_some() {
            return Err(format!("Only one key besides modifiers: {combo}"));
        } else {
            key = Some(key_code(&part.to_uppercase())?);
        }
    }
    let key = key.ok_or(format!("A hotkey needs a key besides modifiers: {combo}"))?;
    if mods == 0 && key < F1 {
        return Err(format!("Use a modifier (Meta, Ctrl, Alt) with {combo}, or it would fire while typing"));
    }
    Ok(mods | key)
}

fn key_code(key: &str) -> Result<u32, String> {
    let b = key.as_bytes();
    if b.len() == 1 && b[0].is_ascii_alphanumeric() {
        return Ok(b[0] as u32); // Qt::Key_A.. and Key_0.. are their ASCII codes
    }
    match key.strip_prefix('F').and_then(|n| n.parse::<u32>().ok()) {
        Some(n @ 1..=24) if !key[1..].starts_with('0') => Ok(F1 + n - 1),
        _ => Err(format!("Unsupported key: {key} (use a letter, digit or F1–F24)")),
    }
}

/// Qt key code -> "Meta+S".
fn format(code: u32) -> String {
    let mut s: String = MODIFIERS.iter().filter(|(_, bit)| code & bit != 0).map(|(m, _)| format!("{m}+")).collect();
    let key = code & !MODIFIER_MASK;
    if key >= F1 { s += &format!("F{}", key - F1 + 1) } else { s.push(char::from_u32(key).unwrap_or('?')) }
    s
}

/// From getGlobalShortcutsByKey's output, a(ssssssaiai) — action id, action name, component id, component name,
/// context id, context name per owner — the first owner's friendly name that isn't our overlay.
fn other_owner(out: &str) -> Option<String> {
    let mut quoted = vec![];
    let mut rest = out;
    while let Some(start) = rest.find('"') {
        let end = rest[start + 1..].find('"')? + start + 1;
        quoted.push(&rest[start + 1..end]);
        rest = &rest[end + 1..];
    }
    quoted.chunks_exact(6).find(|o| o[2] != ACTION[0]).map(|o| {
        let component = [o[3], o[1], o[2]].into_iter().find(|s| !s.trim().is_empty()).unwrap_or("");
        // an app launcher's action is named after the app: "Dolphin", not "Dolphin (Dolphin)"
        if o[1].trim().is_empty() || o[1] == component { component.into() } else { format!("{component} ({})", o[1]) }
    })
}

// --- auto-update: the sonar-update.timer user unit that runs install/update.sh ---

fn systemctl(args: &[&str]) -> bool {
    Command::new("systemctl").arg("--user").args(args).output().is_ok_and(|o| o.status.success())
}

pub fn auto_update() -> bool {
    systemctl(&["is-enabled", "sonar-update.timer"])
}

/// --now: enabling starts the timer, which runs an update right away (its 2 min after login have passed).
pub fn set_auto_update(on: bool) -> Res<bool> {
    if !systemctl(&[if on { "enable" } else { "disable" }, "--now", "sonar-update.timer"]) {
        return Err((503, "Couldn't switch auto-update: re-run install/install.sh to install its timer".into()));
    }
    Ok(auto_update())
}

// --- app classifier ---

/// Channel name for an unassigned app's stream, or None when there's no signal.
pub fn classify(s: &AppStream) -> Option<&'static str> {
    if is_steam_game(s.pid) {
        return Some("Game");
    }
    by_role(s.media_role.as_deref()).or_else(|| by_categories(desktop_categories().get(&s.binary.to_lowercase())?))
}

/// Steam sets SteamAppId (non-zero) for every game it launches, native or Proton.
fn is_steam_game(pid: i32) -> bool {
    pid > 0 && std::fs::read(format!("/proc/{pid}/environ")).is_ok_and(|e| e.split(|&b| b == 0)
        .any(|v| (v.starts_with(b"SteamAppId=") || v.starts_with(b"SteamGameId=")) && !v.ends_with(b"=0")))
}

fn by_role(role: Option<&str>) -> Option<&'static str> {
    match role? {
        "Game" => Some("Game"),
        "Communication" | "Phone" => Some("Chat"),
        "Music" | "Movie" | "Video" => Some("Media"),
        _ => None,
    }
}

fn by_categories(categories: &str) -> Option<&'static str> {
    let has = |any: &[&str]| categories.split(';').any(|c| any.contains(&c));
    if has(&["Game"]) {
        Some("Game")
    } else if has(&["InstantMessaging", "Chat", "Telephony", "VideoConference"]) {
        Some("Chat")
    } else if has(&["AudioVideo", "Audio", "Video", "Player", "WebBrowser"]) {
        Some("Media") // browsers can't be split per tab: treat them as media
    } else {
        None
    }
}

/// Binary name an Exec= line launches: skips `env` and VAR=value, drops the path.
fn exec_binary(exec: &str) -> Option<String> {
    let token = exec.split_whitespace().find(|t| *t != "env" && !t.contains('='))?;
    Some(token.replace('"', "").rsplit('/').next()?.to_lowercase())
}

/// binary or desktop id (lowercased) -> Categories, from every .desktop file.
/// ponytail: built once; apps installed while Sonar runs are only picked up after a restart.
fn desktop_categories() -> &'static HashMap<String, String> {
    static MAP: OnceLock<HashMap<String, String>> = OnceLock::new();
    MAP.get_or_init(|| {
        let dirs = std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
        let home = format!("{}/.local/share", std::env::var("HOME").unwrap_or_default());
        // flatpak dirs explicitly (a systemd user service may lack them); user dirs last so they win
        let all = format!("/var/lib/flatpak/exports/share:{dirs}:{home}/flatpak/exports/share:{home}");
        let mut map = HashMap::new();
        for dir in all.split(':') {
            for f in std::fs::read_dir(format!("{dir}/applications")).into_iter().flatten().flatten() {
                let name = f.file_name().to_string_lossy().to_lowercase();
                if let (Some(id), Ok(text)) = (name.strip_suffix(".desktop"), std::fs::read_to_string(f.path())) {
                    index(id, &text, &mut map);
                }
            }
        }
        map
    })
}

fn index(id: &str, text: &str, map: &mut HashMap<String, String>) {
    let (mut exec, mut categories) = (None, None);
    for line in text.lines() {
        if line.starts_with('[') && line != "[Desktop Entry]" {
            break; // only the main section; skip [Desktop Action ...]
        }
        if let Some(e) = line.strip_prefix("Exec=") { exec.get_or_insert(e); }
        if let Some(c) = line.strip_prefix("Categories=") { categories.get_or_insert(c); }
    }
    let Some(cats) = categories else { return };
    // desktop id: "zen" -> "zen", "com.discordapp.discord" -> "discord" (flatpak ids)
    map.insert(id.into(), cats.into());
    map.insert(id.rsplit('.').next().unwrap_or(id).into(), cats.into());
    if let Some(bin) = exec.and_then(exec_binary).filter(|b| b != "flatpak") {
        map.insert(bin, cats.into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hotkeys() {
        assert_eq!(parse("Meta+S"), Ok(0x1000_0053));
        assert_eq!(format(0x1000_0053), "Meta+S");
        assert_eq!(format(parse("Ctrl+Alt+F12").unwrap()), "Ctrl+Alt+F12");
        assert_eq!(parse("F5"), Ok(F1 + 4));
        assert!(parse("S").is_err() && parse("Meta").is_err() && parse("Meta+S+T").is_err() && parse("Meta+F25").is_err());
        let ours = r#"a(ssssssaiai) 1 "_launch" "Sonar Overlay" "dev.sonar.overlay.desktop" "Sonar Overlay" "default" "Default Context" 1 268435539 1 268435539"#;
        assert_eq!(other_owner(ours), None);
        let other = r#"a(ssssssaiai) 1 "_launch" "Dolphin" "org.kde.dolphin.desktop" "Dolphin" "default" "Default Context" 1 1 1 1"#;
        assert_eq!(other_owner(other).as_deref(), Some("Dolphin"));
        let kwin = r#"a(ssssssaiai) 1 "Overview" "Toggle Overview" "kwin" "KWin" "default" "x" 1 1 1 1"#;
        assert_eq!(other_owner(kwin).as_deref(), Some("KWin (Toggle Overview)"));
    }

    #[test]
    fn categories() {
        assert_eq!(by_categories("Network;WebBrowser;"), Some("Media"));
        assert_eq!(by_categories("Network;InstantMessaging;"), Some("Chat"));
        assert_eq!(by_categories("Game;ActionGame;"), Some("Game"));
        assert_eq!(by_categories("Utility;"), None);
        assert_eq!(exec_binary("env FOO=1 \"/opt/zen/zen\" %u").as_deref(), Some("zen"));
        let mut m = HashMap::new();
        index("com.discordapp.discord", "[Desktop Entry]\nExec=/usr/bin/flatpak run x\nCategories=Chat;\n[Desktop Action n]\nCategories=Game;\n", &mut m);
        assert_eq!(m.get("discord").map(String::as_str), Some("Chat;"));
        assert!(!m.contains_key("flatpak"));
    }
}
