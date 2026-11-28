#!/usr/bin/env bash
# Repo hygiene: no implementation leftovers, no scaffolding. Runs in CI on every PR.
# No TODO/FIXME is allowed: what is left to do is an issue, not a comment.
set -euo pipefail
cd "$(dirname "$0")/.."
fail=0
problem() { echo "✗ $*"; fail=1; }
files=$(git ls-files)
src=$(echo "$files" | grep -E '\.(rs|sh|py|toml|yml|yaml)$' | grep -v '^scripts/check-hygiene.sh$' | grep -v '^reference/' || true)

# 1. debug and placeholder code
hits=$(echo "$src" | grep '\.rs$' | xargs -r grep -nE '\b(dbg!|todo!|unimplemented!)\(' || true)
[[ -n $hits ]] && problem "debug/placeholder macros:"$'\n'"$hits"

# 2. no TODO/FIXME/XXX/HACK: what is left to do is an issue
hits=$(echo "$src" | xargs -r grep -nE '\b(TODO|FIXME|XXX|HACK)\b' || true)
[[ -n $hits ]] && problem "TODO/FIXME left in the tree (open an issue instead):"$'\n'"$hits"

# 3. stray files: editor/merge/backup leftovers, scratch dirs, OS junk
# (a fixture host's or case's root/ imitates a server's filesystem, where *.bak and *.log files are the point)
hits=$(echo "$files" | grep -vE '^tests/fixtures/(hosts|cases)/[^/]+/root/' | grep -E '(\.orig|\.rej|\.bak|\.swp|~|\.tmp|\.log|\.DS_Store|Thumbs\.db)$|(^|/)(scratch|tmp|spike|notes-wip)/' || true)
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

# 6. no scaffolding, no reference material, no build-time planning documents
grep -rqE 'NotImplemented' src/ && problem "Error::NotImplemented exists — every command must be implemented"
[[ -d reference ]] && problem "reference/ exists — legacy semantics live in tests/parity and the spec"
[[ -e docs/milestones.md ]] && problem "docs/milestones.md is back — what is left to do is in docs/roadmap.md and the issues"
for file in src/*.rs src/*/*.rs; do
  # a module that is only its header comment is a stub
  grep -qvE '^[[:space:]]*(//.*)?$' "$file" || problem "empty module: $file"
done

((fail)) && exit 1
echo "ok"
