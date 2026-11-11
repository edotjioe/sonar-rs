# sonar-rs

A Rust port of [Sonar](https://github.com/edotjioe/sonar-linux)'s backend core, to compare memory use.
Same HTTP API and PipeWire handling; serves the Java repo's `index.html` (`SONAR_STATIC`, default
`../steelseriesGG/src/main/resources/static`). State: `data/state.json`.

Everything the Java backend does: channels, EQ, routing, profiles, the Arctis base station, level meters,
Bluetooth batteries, mic test, streamer mode, spatial audio, OBS remote, overlay hotkey (KDE), auto-update.

Stop the Java service first (both own the `sonar_*` sinks and port 8080):

    systemctl --user stop sonar && cargo run --release

## Memory (2026-11-11, mixer + base station + 5 level meters + UI polling, `/proc/<pid>/smaps_rollup`)

|                     | Pss_Anon | Pss_File | RSS     |
|---------------------|----------|----------|---------|
| Java, GraalVM native | 48.7 MB  | 122 MB   | 176 MB  |
| Rust (this)          | 0.9 MB   | 0.8 MB   | 4.4 MB  |

The EQ `pipewire` child processes (~64 MB total RSS) are the same in both and not counted.
