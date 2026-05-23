# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A baby monitor application. A Rust binary runs on a Raspberry Pi, captures camera + microphone via GStreamer, encodes to H.264/Opus, and streams to browsers over WebRTC. The browser viewer is a single self-contained HTML page served by the same binary.

## Commands

**Run locally (Mac):**
```bash
cargo run                        # uses default "mac" feature
```

**Run on Linux/Pi (cross-compile from Mac):**
```bash
./build.sh                       # builds for aarch64, scps binary to parkerc@pchristy
./build.sh --image               # rebuild the Docker cross-compile image first
```

**Build only (no deploy):**
```bash
# Mac
cargo build

# Linux target (requires Docker)
docker run --rm -v "$(pwd):/app" -v "$(pwd)/target:/app/target" -w /app rusty-cam-builder \
  cargo build --target aarch64-unknown-linux-gnu --release --no-default-features --features linux
```

**Check / lint:**
```bash
cargo check
cargo clippy
```

There are no automated tests.

## Architecture

The binary does two things at once: it is the **signaling server** and the **camera peer**.

### Startup sequence (`main.rs` → `signaling_server.rs`)

1. `start_server()` binds on `0.0.0.0:3000` and starts the Axum HTTP server.
2. Before returning, it spawns `connect_camera_to_ws()` — the camera peer connects back to the server's own `/ws` endpoint as a first-class WebSocket client.
3. The camera peer registers itself with the fixed ID `"camera_peer"` so browsers can address offers to it.

### Signaling server (`src/signaling_server.rs`)

Pure message relay. All connected peers (camera + browsers) are stored in a `HashMap<String, Tx>` (unbounded MPSC sender per peer). The server matches messages by `offer_to` / `answer_to` / `to` fields and forwards them. No SDP parsing occurs here.

Ping loop runs every 5 seconds: sends `{"t":"ping"}` to all peers and removes dead ones.

Routes:
- `GET /home` — serves `cam.html` (embedded via `include_str!`) with `__RUSTY_CAM_VERSION__` replaced
- `ANY /ws` — WebSocket upgrade for both camera peer and browser viewers

### Camera peer (`src/camera_peer.rs`)

Handles WebRTC from the camera side. Key constraints:

- **One set of shared tracks** (`video_track`, `audio_track`) for the entire process lifetime. Each new browser viewer gets a new `RTCPeerConnection` that shares these same track objects — GStreamer captures once and fans out.
- **MAX_VIEWERS = 2**: tracked via `peer_connections: HashMap<viewer_id, RTCPeerConnection>` + `viewer_order: VecDeque`. When a third viewer connects, the oldest is evicted.
- **Two GStreamer pipelines for video**: capture pipeline (camera → raw RGBA via appsink) and encode pipeline (RGBA appsrc → x264enc → H.264 appsink). They are bridged by a dedicated `std::thread` (not Tokio) because `pull_sample()` blocks. Encoded frames flow to the async WebRTC write via a bounded `mpsc::channel(2)` — backpressure instead of unbounded queue growth.
- **Audio**: single GStreamer pipeline (`osxaudiosrc` / `alsasrc` → opusenc → appsink), same thread-per-pipeline pattern.
- A red dot is drawn on every 60th video frame as a "live" indicator (`add_live_indicator`).

### Platform features

`Cargo.toml` defines two features: `mac` (default) and `linux`. They gate the GStreamer source elements:
- Mac: `avfvideosrc device-index=0`, `osxaudiosrc`
- Linux: `v4l2src device=/dev/video0`, `alsasrc device=hw:2,0`

### Browser viewer (`src/static/cam.html`)

Embedded into the binary at compile time. Self-contained single-page app — no build step, no framework. The viewer:
1. Generates a stable `localId` per browser session (stored in `sessionStorage`).
2. Opens a WebSocket to `/ws` and immediately sends an SDP offer addressed to `"camera_peer"`.
3. Handles retry logic: up to 5 attempts with 3 s delay before requiring manual re-tap.
4. Polls `RTCPeerConnection.getStats()` every 4 s to show video/audio byte-flow indicators.

### Signal message protocol (`src/utils.rs`)

All WebSocket messages are JSON. The `SignalMessage` enum is `untagged` (discriminated by field presence):

| Variant | Fields | Direction |
|---|---|---|
| `Register` | `from` | camera→server on startup |
| `Offer` | `sdp`, `from`, `offer_to` | browser→camera |
| `Answer` | `sdp`, `from`, `answer_to` | camera→browser |
| `Candidate` | `candidate`, `sdp_mid`, `sdp_mline_index`, `from`, `to` | both directions |
| `Video` | `from`, `to` | browser→camera (request) |
| `Ping` | `t` | server→all (keepalive) |

## Deployment

The binary runs as a systemd service on the Pi. `deploy/` contains all units. `rusty-cam.path` watches the binary path so systemd auto-restarts on a new deploy — `build.sh` exploits this by `mv`-ing a `.new` sidecar into place (avoids `ETXTBSY`).

To install units on the Pi: `bash deploy/install-on-pi.sh`  
Logs: `journalctl -u rusty-cam -f`
