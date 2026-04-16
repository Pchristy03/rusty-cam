#!/usr/bin/env bash
# Run on the Raspberry Pi (after this whole `deploy/` directory is on the Pi),
# from anywhere:  bash /path/to/deploy/install-on-pi.sh
set -euo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
for f in rusty-cam.service rusty-cam.path rusty-cam-restart.service; do
  if [[ ! -f "$DIR/$f" ]]; then
    echo "missing: $DIR/$f" >&2
    exit 1
  fi
done
sudo install -m 644 "$DIR/rusty-cam.service" "$DIR/rusty-cam.path" "$DIR/rusty-cam-restart.service" /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now rusty-cam.service rusty-cam.path
echo "Installed. Check: systemctl status rusty-cam"
