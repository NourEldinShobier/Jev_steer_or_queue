#!/bin/sh
# Runs the Rust router when it has been fetched (fast, works on Windows), else the Python one.
d="${CLAUDE_PLUGIN_DATA:-$HOME/.jev-steer-or-queue/claude-code}/bin"
for b in "$d/jev-router.exe" "$d/jev-router"; do
  [ -x "$b" ] && exec "$b" "$@"
done
exec python3 "$(dirname "$0")/jev_router.py" "$@"
