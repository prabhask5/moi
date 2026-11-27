#!/usr/bin/env bash
# Fails if anything that looks like a real secret is committed (reference/ and fixtures must stay redacted).
set -euo pipefail
cd "$(dirname "$0")/.."
files=$(git ls-files 2>/dev/null || find . -type f -not -path './target/*' -not -path './.git/*')
pattern='(/api/push/[A-Za-z0-9]{8,}|hc-ping\.com/[A-Za-z0-9_-]{16,}|AKIA[0-9A-Z]{16}|-----BEGIN [A-Z ]*PRIVATE KEY-----|(SONARR|RADARR|PROWLARR)_KEY="[0-9a-f]{32}")'
hits=$(echo "$files" | grep -v '^scripts/check-secrets.sh$' | xargs -r grep -anEo "$pattern" 2>/dev/null | grep -v 'FAKE' || true)
if [[ -n $hits ]]; then echo "Possible secrets committed:"; echo "$hits"; exit 1; fi
echo "ok"
