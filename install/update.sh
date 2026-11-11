#!/bin/sh
# Update an installed Sonar: pull, test, rebuild and restart the backend. A failed build leaves the
# running version alone. The overlay script restarts itself on the next Meta+S once its file changed.
set -e
REPO=$(cd "$(dirname "$0")/.." && pwd)
cd "$REPO"
CARGO=$(command -v cargo || echo "$HOME/.cargo/bin/cargo")

before=$(git rev-parse HEAD)
git pull --ff-only
if [ "$(git rev-parse HEAD)" = "$before" ] && [ "$1" != "--force" ]; then
	echo "Already up to date (--force rebuilds anyway)"
	exit 0
fi
# cargo replaces the binary with a new file, so the running one is unaffected until the restart
if ! { "$CARGO" test --release -q && "$CARGO" build --release -q; }; then
	echo "Build failed: still running the previous version" >&2
	exit 1
fi
# the service file and launchers are templates in install/: when they changed, re-install them
if ! git diff --quiet "$before" HEAD -- install/; then
	install/install.sh
else
	systemctl --user restart sonar-rs.service
fi
echo "Sonar updated to $(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml)"
