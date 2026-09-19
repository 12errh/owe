#!/usr/bin/env bash
# fetch-reference.sh — pinned shallow clones of the repos we study and port from.
# Part of the reference-code workflow (docs/REFERENCE-CODE-MAP.md, ADR-013).
#
#   - Clones are PINNED: the commit recorded here is the commit we studied and
#     mapped. Re-running is idempotent; existing clones are left untouched.
#   - reference/ is gitignored and NEVER compiled into OWE.
#   - Reading reference code is free; PORTING code requires preserving the
#     original license header AND a row in ATTRIBUTION.md.
#
# Usage:  ./scripts/fetch-reference.sh          # fetch missing clones
#         ./scripts/fetch-reference.sh --refresh <name>   # re-clone one repo at its pin

set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REF_DIR="$ROOT/reference"
mkdir -p "$REF_DIR"

# name | git URL | pinned commit (pin == what we studied/mapped) | license
REPOS=(
  "swww|https://github.com/LGFae/swww.git|8a3b1bf9f3363683097aa5467d3058cc2278da1a|GPL-3.0 (renamed awww, upstream on Codeberg: https://codeberg.org/LGFae/awww — GitHub mirror used for cloning)"
  "wpaperd|https://github.com/danyspin97/wpaperd.git|483225a9c6ad4f9bbf9104f958eafee9c47d4612|GPL-3.0"
  "wallr|https://github.com/programmersd21/wallr.git|2ad4c14a3e8c44807fc98baf8698bd7462e83e27|MIT"
  "phonto|https://github.com/museslabs/phonto.git|9eaa162135626d22617bca1bdbedb52567d76074|GPL-3.0 (LICENSE verified at pin; corrects an earlier 'permissive' research note)"
  "waywe-rs|https://github.com/hack3rmann/waywe-rs.git|e86fbbbf6a6d4e09b660c0a9240989b01d0301f7|MIT (LICENSE verified at pin)"
  "we-layerd|https://github.com/Aromatic05/we-layerd.git|82f9a0695c0c1fdf01eb429fb9675750a9dde55c|⚠️ NO LICENSE at pin — READ-ONLY: never port its code (see docs/REFERENCE-CODE-MAP.md §2.6)"
)

fetch() {
  local name="$1" url="$2" pin="$3"
  local dest="$REF_DIR/$name"
  if [[ -d "$dest/.git" ]]; then
    echo "[skip] $name already present at $(git -C "$dest" rev-parse --short HEAD)"
    return 0
  fi
  echo "[clone] $name → $dest"
  git clone --depth 1 "$url" "$dest"
  if [[ "$pin" != "PIN-NOT-YET-SET" ]]; then
    git -C "$dest" fetch --depth 1 origin "$pin" && git -C "$dest" checkout --detach "$pin"
  fi
  echo "[pin]   $name @ $(git -C "$dest" rev-parse HEAD)"
}

for entry in "${REPOS[@]}"; do
  IFS='|' read -r name url pin _license <<<"$entry"
  if [[ "${1:-}" == "--refresh" && "${2:-}" == "$name" ]]; then
    rm -rf "$REF_DIR/$name"
  fi
  fetch "$name" "$url" "$pin"
done

echo
echo "Cloned/verified repos in $REF_DIR:"
for entry in "${REPOS[@]}"; do
  IFS='|' read -r name url pin _license <<<"$entry"
  if [[ -d "$REF_DIR/$name/.git" ]]; then
    printf '  %-10s %s\n' "$name" "$(git -C "$REF_DIR/$name" rev-parse HEAD)"
  fi
done
echo
echo "Pins are hardened and mirrored in ATTRIBUTION.md §1. To re-pin a repo:"
echo "  1) update the pin above AND ATTRIBUTION.md §1, re-verify verdicts in docs/REFERENCE-CODE-MAP.md"
echo "  2) run: $0 --refresh <name>"
