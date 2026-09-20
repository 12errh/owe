#!/usr/bin/env bash
# Pin the Caelestia CLI/IPC surface against a live shell (IMPLEMENTATION-PLAN P3,
# gate prerequisite OQ-2).
#
# Why this exists: FR-SHELL-3 documented `caelestia wallpaper -f <file> [-m <monitor>]`
# from the project's research notes, not from the tool. The real CLI has no monitor
# flag on either surface, and a backend built on the research note would have shipped
# a flag that does not exist. This script records what the tool actually accepts, so
# the backend's command construction is checked against captured text rather than
# against a recollection.
#
#   ./scripts/record-caelestia-cli.sh          # record (needs a live Caelestia shell)
#   ./scripts/record-caelestia-cli.sh --check  # diff against the committed capture
#
# `caelestia shell -s` talks to the running shell, so recording also proves the shell
# is up. It is the one command here that can fail for a reason other than a missing
# binary, and that is the point: it is the IPC half of the surface.
set -uo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
out="$root/crates/owe-shell-caelestia/tests/fixtures/cli"
mode="${1:-record}"

bin="$(command -v caelestia || true)"
if [ -z "$bin" ]; then
  echo "record-caelestia-cli: no \`caelestia\` on PATH — nothing to record" >&2
  exit 2
fi

mkdir -p "$out"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# One file per invocation. Names are the contract: the crate's tests read these.
capture() {
  local file="$1"
  shift
  if timeout 20 "$bin" "$@" >"$work/$file" 2>&1; then
    echo "  recorded $file   (caelestia $*)"
  else
    echo "record-caelestia-cli: \`caelestia $*\` failed — not recording it" >&2
    return 1
  fi
}

echo "record-caelestia-cli: recording from $bin"
capture version.txt -v || true
capture help.txt --help
capture wallpaper-help.txt wallpaper --help
capture scheme-help.txt scheme --help
capture shell-help.txt shell --help
# The IPC surface of the running shell: the only capture that needs a live shell.
if ! capture shell-ipc.txt shell -s; then
  echo "record-caelestia-cli: is Caelestia running? \`caelestia shell -s\` needs the shell." >&2
  exit 3
fi

# Provenance travels with the fixtures: a future reader must be able to tell a
# capture from a hand-written file, and know which machine it came from.
{
  echo "# Recorded by scripts/record-caelestia-cli.sh"
  echo "# Host: $(uname -srm)"
  echo "# Binary: $bin"
  echo "# Date: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "#"
  echo "# These files are verbatim output. They are the source of truth for the"
  echo "# command shapes in owe-shell-caelestia: if the tool changes, re-record and"
  echo "# the diff shows exactly what moved."
} >"$work/PROVENANCE.txt"

if [ "$mode" = "--check" ]; then
  status=0
  for file in "$work"/*; do
    name="$(basename "$file")"
    # Provenance carries a timestamp and a hostname by design, so comparing it
    # would report a change on every run and hide real ones.
    [ "$name" = "PROVENANCE.txt" ] && continue
    if [ ! -f "$out/$name" ]; then
      echo "MISSING  $name (committed capture does not have it)" >&2
      status=1
    elif ! diff -q "$out/$name" "$file" >/dev/null; then
      echo "CHANGED  $name" >&2
      diff -u "$out/$name" "$file" | head -40 >&2
      status=1
    fi
  done
  [ "$status" = 0 ] && echo "record-caelestia-cli: capture is current"
  exit "$status"
fi

cp "$work"/*.txt "$out/"
echo "record-caelestia-cli: wrote $(ls -1 "$out" | wc -l) files to ${out#"$root/"}"
