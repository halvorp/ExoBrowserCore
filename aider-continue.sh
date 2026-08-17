#!/usr/bin/env bash
# Launch aider in architect mode with the wire protocol + host + client
# entry points pre-loaded as read-only context.
#
# Architect model plans (DeepSeek V4 Pro), editor model patches (V4 Flash);
# see .aider.conf.yml.
#
# Requires OPENROUTER_API_KEY. Source .env first, or export it in your shell.

set -e

if [ -z "${OPENROUTER_API_KEY:-}" ]; then
  if [ -f .env ]; then
    set -a; . ./.env; set +a
  else
    echo "OPENROUTER_API_KEY not set and no .env file. See .env.example." >&2
    exit 1
  fi
fi

# Read-only anchors: the wire schema + the two client entry points + the
# WPE FFI shim. The architect always has these in context so it can reason
# about protocol changes without extra file-reads.
READONLY=(
  gutted-proto/src/lib.rs
  gutted-host-rs/src/main.rs
  gutted-host-rs/src/wpe.rs
  gutted-client/src/main.rs
  gutted-client/src/render.rs
  gutted-client-gtk/src/main.rs
  gutted-client-gtk/src/net.rs
  host/gutted_wpe.h
  host/gutted_wpe.c
)

RO_ARGS=()
for f in "${READONLY[@]}"; do
  RO_ARGS+=(--read "$f")
done

exec aider "${RO_ARGS[@]}" "$@"
