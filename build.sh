#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")"

IMAGE="${IMAGE:-rusty-cam-builder}"
CARGO_ARGS=(build --target aarch64-unknown-linux-gnu --release --no-default-features --features linux)

# Rebuild the image only when you change the Dockerfile / system deps, or pass --image.
if [[ "${1:-}" == "--image" ]] || ! docker image inspect "$IMAGE" &>/dev/null; then
  docker build -t "$IMAGE" .
fi

docker run --rm \
  -v "$(pwd):/app" \
  -v "$(pwd)/target:/app/target" \
  -w /app \
  "$IMAGE" \
  cargo "${CARGO_ARGS[@]}"

# Do not scp directly onto the running binary: Linux returns ETXTBSY ("Text file
# busy") while the service has it mapped. Upload a sidecar, then mv into place.
REMOTE="${REMOTE:-parkerc@pchristy}"
REMOTE_BIN="${REMOTE_BIN:-/home/parkerc/binary-rusty-cam}"
REMOTE_NEW="${REMOTE_BIN}.new"

scp target/aarch64-unknown-linux-gnu/release/rusty-cam "${REMOTE}:${REMOTE_NEW}"
ssh "${REMOTE}" "chmod +x '${REMOTE_NEW}' && mv -f '${REMOTE_NEW}' '${REMOTE_BIN}'"

# On the Pi, if `rusty-cam.path` is enabled (see deploy/), systemd restarts
# rusty-cam.service when this file changes—no manual reboot or ssh needed.
