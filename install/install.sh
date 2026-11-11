#!/bin/sh
# Run Sonar's backend from this repo instead of the Java one: builds it, installs the sonar-rs user
# service (starts at login) and switches the Java sonar.service off. The UI (index.html, overlay, tray,
# launchers) still comes from the Java repo: $SONAR_JAVA_REPO, default ../steelseriesGG.
# Back to Java: systemctl --user disable --now sonar-rs && systemctl --user enable --now sonar
set -e
REPO=$(cd "$(dirname "$0")/.." && pwd)
JAVA_REPO=$(cd "${SONAR_JAVA_REPO:-$REPO/../steelseriesGG}" && pwd)
STATIC=$JAVA_REPO/src/main/resources/static
UNITS=$HOME/.config/systemd/user
[ -f "$STATIC/index.html" ] || { echo "No Sonar UI at $STATIC: set SONAR_JAVA_REPO" >&2; exit 1; }

CARGO=$(command -v cargo || echo "$HOME/.cargo/bin/cargo")
(cd "$REPO" && "$CARGO" build --release)

mkdir -p "$UNITS"
sed -e "s#@REPO@#$REPO#g" -e "s#@STATIC@#$STATIC#g" "$REPO/install/sonar-rs.service" > "$UNITS/sonar-rs.service"
systemctl --user daemon-reload
systemctl --user disable --now sonar.service 2>/dev/null || true
pkill -x sonar-rs || true  # one started by hand would hold the port
systemctl --user enable sonar-rs.service
systemctl --user restart sonar-rs.service
echo "Sonar (Rust) installed: http://localhost:8080"
