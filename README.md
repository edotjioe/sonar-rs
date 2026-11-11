# sonar-rs

[Sonar](https://github.com/edotjioe/sonar-linux) — a SteelSeries GG Sonar clone for Linux on PipeWire — with
its backend in Rust instead of Java. Same features and HTTP API in ~1 MB of memory instead of ~49 MB.

Everything the Java version does: channels, EQ, app routing, profiles, the Arctis Nova Pro Wireless base
station (ChatMix dial, volume knob, mute, battery), level meters, Bluetooth batteries, mic test, streamer
mode, spatial audio, OBS remote, the Meta+S overlay (KDE hotkey), tray icon and auto-update.

This is where Sonar is developed now. It's standalone: the UI (`static/`, built into the binary), the overlay
and tray (`sonar-window.py`) and the launchers live here. They started as copies of sonar-linux's (at
`fc6c9ce`); the Java version stays as it is at v0.1.23.

## Requirements

- Linux with PipeWire + `pipewire-pulse` (provides `pactl`, `parec`) and `pw-cli`/`pw-link`.
- Rust ([rustup.rs](https://rustup.rs)) to build it.
- Window, overlay and tray: PyGObject with GTK 3 and WebKitGTK 4.1 (`python-gobject` + `webkit2gtk-4.1` on
  Arch, `python3-gobject` + `webkit2gtk4.1` on Fedora); optional gtk-layer-shell (overlay above fullscreen
  games) and Ayatana/libappindicator (tray icon).
- Optional: `noise-suppression-for-voice` (mic noise suppression), libmysofa + PipeWire's SOFA plugin
  (spatial audio), UPower (Bluetooth batteries), KDE Plasma (changing the hotkey from Sonar), OBS with its
  WebSocket server enabled.

## Install

    install/install.sh

Builds the backend and installs it as the user service `sonar-rs` (starts at login), plus the Sonar
launcher, the Meta+S overlay, the icon, the tray icon at login and the auto-update timer (off until
switched on in Preferences). Re-run it after moving the repo. The base station needs a udev rule once
(the script prints the command).

Updating: `install/update.sh` pulls, runs the tests, rebuilds and restarts; `--force` rebuilds without
anything new to pull. Settings live in `data/state.json`.

Coming from the Java version: `install.sh` switches its `sonar.service` off (the two conflict: both own the
`sonar_*` sinks and port 8080). Back to Java: `systemctl --user disable --now sonar-rs && systemctl --user
enable --now sonar`, then re-run sonar-linux's `install/install.sh` for its launchers.

## Memory

2026-11-11, from `/proc/<pid>/smaps_rollup`, with the base station, 5 level meters and OBS connected and the
mixer and OBS pages polling:

|                      | Pss_Anon | Pss_File | RSS    |
|----------------------|----------|----------|--------|
| Java, GraalVM native | 48.7 MB  | 122 MB   | 176 MB |
| Rust (this)          | 1.2 MB   | 1.0 MB   | 5.0 MB |

The EQ processes (15.9 MB private for 4 channels), the tray icon (24.4 MB) and the overlay while open
(173.7 MB, gone 60 s after it closes) are the same with either backend; see sonar-linux's README.
