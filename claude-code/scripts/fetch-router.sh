#!/bin/sh
# SessionStart: download this version's Rust router once, in the background. Until it lands,
# run.sh uses the Python router. Never blocks or fails the session.
root="$(cd "$(dirname "$0")/.." && pwd)"
d="${CLAUDE_PLUGIN_DATA:-$HOME/.jev-steer-or-queue/claude-code}/bin"
v=$(sed -n 's/.*"version": *"\([^"]*\)".*/\1/p' "$root/.claude-plugin/plugin.json" | head -1)
[ -f "$d/.version" ] && [ "$(cat "$d/.version")" = "$v" ] && exit 0
case "$(uname -s)-$(uname -m)" in
  MINGW*|MSYS*|CYGWIN*) asset=jev-router-windows-x64.exe; out=jev-router.exe ;;
  Darwin-arm64) asset=jev-router-macos-arm64; out=jev-router ;;
  Darwin-*) asset=jev-router-macos-x64; out=jev-router ;;
  Linux-x86_64) asset=jev-router-linux-x64; out=jev-router ;;
  *) exit 0 ;;
esac
(
  mkdir -p "$d" &&
  curl -fsSL --retry 2 -o "$d/$out.part" "https://github.com/NourEldinShobier/Jev_steer_or_queue/releases/download/v$v/$asset" &&
  chmod +x "$d/$out.part" && mv -f "$d/$out.part" "$d/$out" && printf '%s' "$v" > "$d/.version"
) >/dev/null 2>&1 &
exit 0
