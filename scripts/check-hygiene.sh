#!/usr/bin/env bash
# Repo hygiene: no implementation leftovers. Runs in CI on every PR.
#   scripts/check-hygiene.sh            normal mode (during development)
#   scripts/check-hygiene.sh --release  release mode (M12 and every release): zero scaffolding allowed
#
# Normal mode allows TODOs only when tagged with the milestone or issue that removes them: TODO(M10), TODO(#12).
# Release mode allows none, and also forbids the NotImplemented scaffolding and the reference/ directory.
set -euo pipefail
cd "$(dirname "$0")/.."
release=0; [[ ${1:-} == --release ]] && release=1
fail=0
problem() { echo "✗ $*"; fail=1; }
files=$(git ls-files)
src=$(echo "$files" | grep -E '\.(rs|sh|py|toml|yml|yaml)$' | grep -v '^scripts/check-hygiene.sh$' | grep -v '^reference/' || true)   # reference/ is read-only input, removed in M12

# 1. debug and placeholder code
hits=$(echo "$src" | grep '\.rs$' | xargs -r grep -nE '\b(dbg!|todo!|unimplemented!)\(' || true)
[[ -n $hits ]] && problem "debug/placeholder macros:"$'\n'"$hits"

# 2. TODO/FIXME/XXX/HACK must name the milestone or issue that removes them (none at all in release mode)
if ((release)); then
  hits=$(echo "$src" | xargs -r grep -nE '\b(TODO|FIXME|XXX|HACK)\b' || true)
else
  hits=$(echo "$src" | xargs -r grep -nE '\b(TODO|FIXME|XXX|HACK)\b' | grep -vE '\b(TODO|FIXME)\((M[0-9]+|#[0-9]+)\)' || true)
fi
[[ -n $hits ]] && problem "untracked TODO/FIXME (tag them TODO(M<n>) or TODO(#<issue>), or remove):"$'\n'"$hits"

# 3. stray files: editor/merge/backup leftovers, scratch dirs, OS junk
hits=$(echo "$files" | grep -E '(\.orig|\.rej|\.bak|\.swp|~|\.tmp|\.log|\.DS_Store|Thumbs\.db)$|(^|/)(scratch|tmp|spike|notes-wip)/' || true)
[[ -n $hits ]] && problem "stray files:"$'\n'"$hits"

# 4. large files (fixtures should be minimal; binaries never)
while IFS= read -r f; do
  [[ -f $f ]] || continue
  s=$(wc -c <"$f")   # portable (GNU stat -c and BSD stat -f differ)
  (( s > 1048576 )) && problem "file over 1 MB: $f ($((s/1024)) KB)"
done <<<"$files"

# 5. ignored tests must say why
hits=$(echo "$files" | grep '^tests/.*\.rs$' | xargs -r grep -nE '#\[ignore\]' || true)
[[ -n $hits ]] && problem "#[ignore] without a reason — use #[ignore = \"needs MOI_IT services\"]:"$'\n'"$hits"

# 6. release mode: no scaffolding, no reference material
if ((release)); then
  grep -rqE 'NotImplemented' src/ && problem "Error::NotImplemented still exists — every command must be implemented"
  [[ -d reference ]] && problem "reference/ still exists — legacy semantics must live in tests/parity and the spec"
  grep -qE '^## M[0-9]+.*' docs/milestones.md 2>/dev/null && grep -qE '^- \[ \]' docs/milestones.md && problem "unchecked items remain in docs/milestones.md"
fi

((fail)) && exit 1
echo "ok$( ((release)) && echo ' (release mode)')"
