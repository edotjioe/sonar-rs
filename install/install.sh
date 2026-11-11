#!/bin/sh
# Install Sonar for the current user: the backend as the sonar-rs user service (starts at login), the
# launchers (Meta+S overlay), icon and tray icon at login, and the auto-update timer (off until switched on
# in Preferences). Switches the Java sonar.service off if it's there. Re-run after moving the repo.
# The udev rule for the base station needs root: see the end.
# Back to Java: systemctl --user disable --now sonar-rs && systemctl --user enable --now sonar
set -e
REPO=$(cd "$(dirname "$0")/.." && pwd)
APPS=$HOME/.local/share/applications
UNITS=$HOME/.config/systemd/user

CARGO=$(command -v cargo || echo "$HOME/.cargo/bin/cargo")
[ -x "$CARGO" ] || { echo "Rust not found: install it from https://rustup.rs" >&2; exit 1; }
[ -f "$REPO/target/release/sonar-rs" ] || (cd "$REPO" && "$CARGO" build --release)

mkdir -p "$UNITS" "$APPS" "$HOME/.local/share/icons/hicolor/256x256/apps" "$HOME/.config/autostart"
# the templates say @REPO@ wherever the repo's path goes
for f in sonar-rs.service sonar-update.service; do
	sed "s#@REPO@#$REPO#g" "$REPO/install/$f" > "$UNITS/$f"
done
cp "$REPO/install/sonar-update.timer" "$UNITS/sonar-update.timer"
for f in dev.sonar.desktop dev.sonar.overlay.desktop; do
	sed "s#@REPO@#$REPO#g" "$REPO/install/$f" > "$APPS/$f"
done
cp "$REPO/sonar.png" "$HOME/.local/share/icons/hicolor/256x256/apps/sonar.png"
sed "s#@REPO@#$REPO#g" "$REPO/install/dev.sonar.autostart.desktop" > "$HOME/.config/autostart/dev.sonar.desktop"

systemctl --user daemon-reload
systemctl --user disable --now sonar.service 2>/dev/null || true
pkill -x sonar-rs || true  # one started by hand would hold the port
systemctl --user enable sonar-rs.service
systemctl --user restart sonar-rs.service
command -v kbuildsycoca6 >/dev/null && kbuildsycoca6 >/dev/null 2>&1 || true  # KDE: pick up the launchers / Meta+S
# the tray icon: started at login from now on; start it now if it isn't running
pgrep -f "$REPO/sonar-window.py --tray" >/dev/null || (setsid "$REPO/sonar-window.py" --tray >/dev/null 2>&1 &)

echo "Sonar installed: http://localhost:8080"
echo "Hardware ChatMix dial (Arctis Nova Pro Wireless) needs, once:"
echo "  sudo cp $REPO/udev/70-steelseries-arctis.rules /etc/udev/rules.d/ && sudo udevadm control --reload && sudo udevadm trigger"
