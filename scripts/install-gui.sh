#!/usr/bin/env bash
# Rebuild and reinstall the GUI editor (`sacrament2`) into ~/.cargo/bin.
#
# Reuses the workspace's own target directory rather than letting cargo build in
# a scratch one. Without that, every install is a cold build of iced and its
# whole tree — minutes instead of seconds.
#
# Usage:
#   scripts/install-gui.sh          rebuild and install
#   scripts/install-gui.sh --tui    do the terminal editor (v1) instead
set -euo pipefail

cd "$(dirname "$0")/.."

crate=crates/gui
bin=sacrament2
if [ "${1:-}" = "--tui" ]; then
  crate=crates/tui
  bin=sacrament
fi

# A running instance keeps its own copy of the old binary, so the replacement
# only takes effect next launch. Worth saying out loud — the usual confusion is
# installing a fix and then not seeing it.
# Assigned in two steps on purpose. pgrep exits 1 when nothing matches — the
# normal case — which under `set -e` would abort before anything is built, and
# a `|| echo 0` inside the substitution appends a second line rather than
# replacing the first.
running=$(pgrep -cx "$bin" 2>/dev/null) || running=0

echo "building and installing $bin from $crate ..."
cargo install --path "$crate" --bin "$bin" --force --locked --target-dir target

installed="$HOME/.cargo/bin/$bin"
printf 'installed %s (%s)\n' "$bin" "$(date -r "$installed" '+%Y-%m-%d %H:%M')"
if command -v git >/dev/null 2>&1; then
  printf 'from commit %s%s\n' \
    "$(git rev-parse --short HEAD 2>/dev/null || echo '?')" \
    "$(git diff --quiet 2>/dev/null || echo ' + uncommitted changes')"
fi

if [ "$running" -gt 0 ]; then
  echo
  echo "note: $running instance(s) of $bin are running and still on the old"
  echo "      binary — quit and relaunch to pick this up."
fi
