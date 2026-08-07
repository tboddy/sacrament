#!/usr/bin/env bash
# Claude Code PostToolUse hook: surface the file Claude just edited in a running
# sacrament as an unreviewed background tab.
#
# Wired up by .claude/settings.json. Reads the hook's JSON payload from stdin and
# pulls out the edited path (tool_input.file_path), preferring jq, falling back
# to python3. Lenient by design — a hook must never block Claude, so every
# failure path just exits 0.
#
# `sacrament --review` is a no-op unless a sacrament server is already running,
# so this is harmless when the editor is closed. Override the binary with
# SACRAMENT_BIN if it isn't on PATH.

payload="$(cat)"
file=""

if command -v jq >/dev/null 2>&1; then
  file="$(printf '%s' "$payload" | jq -r '.tool_input.file_path // empty' 2>/dev/null)"
elif command -v python3 >/dev/null 2>&1; then
  file="$(printf '%s' "$payload" | python3 -c '
import json, sys
try:
    print(json.load(sys.stdin).get("tool_input", {}).get("file_path", ""))
except Exception:
    pass' 2>/dev/null)"
fi

[ -n "$file" ] || exit 0

"${SACRAMENT_BIN:-sacrament}" --review "$file" >/dev/null 2>&1 || true
exit 0
