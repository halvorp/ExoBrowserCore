#!/usr/bin/env bash
# gutted-browser one-command demo.
# Builds if needed, starts host, waits for the cert pin, launches the client.
# Ctrl-C both ends when you're done.
#
# Optional env:
#   URL         initial page for the host (default: https://example.com)
#   NAV         URL the client sends via ctrl once connected (default: unset)
#   PORT        UDP port to bind (default: 4433)
#   HOLD_SECS   run the client headless for N seconds then exit (default: unset)
#   SCREENSHOT  PPM path to save + convert to PNG after N frames (default: unset)
#   SHOT_AFTER  wait for N frames before capturing (default: 2)
#   CURSOR      "X,Y" synth cursor position for the shader (default: unset)
#   SCROLL      "DX,DY" synth scroll offset for the shader (default: unset)
#   SHAPE       0/1/2 synth cursor shape (default: unset)
#   CLIENT      "wgpu" (default) or "gtk" — pick which subscriber to launch

set -e
cd "$(dirname "$0")"

: "${URL:=https://example.com}"
: "${PORT:=4433}"
: "${SHOT_AFTER:=2}"

echo "[demo] cargo build --release" >&2
cargo build --release --quiet

# Kill any leftover host/client from a prior run so we don't hit
# "Address already in use" on :$PORT. pkill returns 1 if nothing matched;
# we don't care.
pkill -9 -f "target/release/gutted-host" 2>/dev/null || true
pkill -9 -f "target/release/gutted-client" 2>/dev/null || true
sleep 0.3

LOG=/tmp/gutted-host.log
: > "$LOG"
GBROWSER_URL="$URL" GBROWSER_LISTEN="0.0.0.0:$PORT" ./target/release/gutted-host >"$LOG" 2>&1 &
HPID=$!
trap 'kill $HPID 2>/dev/null; wait $HPID 2>/dev/null; exit' INT TERM EXIT

# Wait for the pin to appear in the log (host prints it on stdout).
# 40 × 0.1s = 4s max — enough for the QUIC bind + rustls provider install.
for _ in $(seq 1 40); do
    if ! kill -0 "$HPID" 2>/dev/null; then
        echo "[demo] host died during startup; log:" >&2
        cat "$LOG" >&2
        exit 1
    fi
    PIN=$(grep -oE '^GBROWSER_CERT_SHA256=[a-f0-9]+' "$LOG" | head -1 | cut -d= -f2 || true)
    [ -n "$PIN" ] && break
    sleep 0.1
done
if [ -z "${PIN:-}" ]; then
    echo "[demo] host never printed cert pin; log:" >&2
    cat "$LOG" >&2
    exit 1
fi
echo "[demo] host up (pid=$HPID) on :$PORT; cert pin=$PIN" >&2
echo "[demo] launching client — Ctrl-C to quit both" >&2

CLIENT_ENV=(
    "GBROWSER_CERT_SHA256=$PIN"
    "GBROWSER_SERVER=127.0.0.1:$PORT"
)
[ -n "${NAV:-}"        ] && CLIENT_ENV+=("GBROWSER_NAV=$NAV")
[ -n "${HOLD_SECS:-}"  ] && CLIENT_ENV+=("GBROWSER_HOLD_SECS=$HOLD_SECS")
[ -n "${SCREENSHOT:-}" ] && CLIENT_ENV+=("GBROWSER_SCREENSHOT=$SCREENSHOT" "GBROWSER_SCREENSHOT_AFTER=$SHOT_AFTER")
[ -n "${CURSOR:-}"     ] && CLIENT_ENV+=("GBROWSER_SYNTH_CURSOR=$CURSOR")
[ -n "${SCROLL:-}"     ] && CLIENT_ENV+=("GBROWSER_SYNTH_SCROLL=$SCROLL")
[ -n "${SHAPE:-}"      ] && CLIENT_ENV+=("GBROWSER_SYNTH_SHAPE=$SHAPE")

CLIENT_BIN=./target/release/gutted-client
case "${CLIENT:-wgpu}" in
    gtk)  CLIENT_BIN=./target/release/gutted-client-gtk ;;
    wgpu) CLIENT_BIN=./target/release/gutted-client ;;
    *)    echo "[demo] unknown CLIENT='$CLIENT' (expected wgpu|gtk)" >&2; exit 2 ;;
esac
echo "[demo] client=$CLIENT_BIN" >&2

env "${CLIENT_ENV[@]}" "$CLIENT_BIN"

if [ -n "${SCREENSHOT:-}" ] && [ -f "$SCREENSHOT" ] && command -v ffmpeg >/dev/null; then
    PNG="${SCREENSHOT%.ppm}.png"
    ffmpeg -y -loglevel error -i "$SCREENSHOT" "$PNG" && echo "[demo] wrote $PNG" >&2
fi
