# sonar-rs

A Rust port of [Sonar](https://github.com/edotjioe/sonar-linux)'s backend core, to compare memory use.
Same HTTP API and PipeWire handling; serves the Java repo's `index.html` (`SONAR_STATIC`, default
`../steelseriesGG/src/main/resources/static`). State: `data/state.json`.

Everything the Java backend does: channels, EQ, routing, profiles, the Arctis base station, level meters,
Bluetooth batteries, mic test, streamer mode, spatial audio, OBS remote, overlay hotkey (KDE), auto-update.

Install as the user service `sonar-rs` (starts at login; switches the Java `sonar.service` off):

    install/install.sh

Back to Java: `systemctl --user disable --now sonar-rs && systemctl --user enable --now sonar`.
The two services conflict (both own the `sonar_*` sinks and port 8080), so starting one stops the other.

## Memory (2026-11-11, full port: base station, 5 level meters, OBS connected, UI + OBS page polling, `/proc/<pid>/smaps_rollup`)

|                     | Pss_Anon | Pss_File | RSS     |
|---------------------|----------|----------|---------|
| Java, GraalVM native | 48.7 MB  | 122 MB   | 176 MB  |
| Rust (this)          | 1.2 MB   | 1.0 MB   | 5.0 MB  |

The EQ `pipewire` child processes (~64 MB total RSS) are the same in both and not counted.
