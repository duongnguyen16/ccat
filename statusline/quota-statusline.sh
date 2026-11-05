#!/usr/bin/env bash
# Quota status line wrapper for Claude Code and agy (macOS / Linux)
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
exec python3 "$DIR/quota-statusline.py" "$@"
