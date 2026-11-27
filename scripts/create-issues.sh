#!/usr/bin/env bash
# Create one GitHub issue per milestone in docs/milestones.md (needs `gh auth login`). Idempotent by title.
set -euo pipefail
cd "$(dirname "$0")/.."
gh label create milestone --color 5319e7 --description "moi milestone (docs/milestones.md)" 2>/dev/null || true
existing=$(gh issue list --label milestone --state all --limit 100 --json title -q '.[].title')
python3 - <<'PY' | while IFS=$'\t' read -r title file; do
import re, tempfile
text = open("docs/milestones.md").read()
for block in re.split(r"\n(?=## M\d+)", text):
    m = re.match(r"## (M\d+ — .+)", block)
    if not m or "✅" in m.group(1):
        continue
    body = block.split("\n", 1)[1].strip() + "\n\nSee CLAUDE.md, docs/agent-workflow.md and the spec sections listed above."
    f = tempfile.NamedTemporaryFile("w", delete=False, suffix=".md"); f.write(body); f.close()
    print(f"{m.group(1)}\t{f.name}")
PY
  if grep -qxF "$title" <<<"$existing"; then echo "exists: $title"; else gh issue create --title "$title" --label milestone --body-file "$file" >/dev/null && echo "created: $title"; fi
  rm -f "$file"
done
