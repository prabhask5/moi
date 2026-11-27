#!/usr/bin/env bash
# Performance budgets (docs/spec/13-performance.md). Measures what exists today; the rest is "pending".
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --release --quiet
bin=target/release/moi
val() { awk -F' *= *' -v k="$1" '$1==k {print $2}' budgets.toml; }
fail=0
report() { printf '| %s | %s | %s | %s |\n' "$1" "$2" "$3" "$4"; }
{
  echo "| metric | budget | measured | result |"; echo "|---|---|---|---|"
  size_mb=$(awk -v b="$(stat -c %s "$bin")" 'BEGIN{printf "%.1f", b/1048576}')
  ok=$(awk -v m="$size_mb" -v b="$(val binary_mb)" 'BEGIN{print (m <= b*1.1) ? "ok" : "OVER"}'); [[ $ok == ok ]] || fail=1
  report binary_mb "$(val binary_mb)" "$size_mb" "$ok"
  best=999999
  for _ in 1 2 3 4 5; do s=$(date +%s%N); "$bin" --version >/dev/null; e=$(( ($(date +%s%N) - s) / 1000000 )); (( e < best )) && best=$e; done
  ok=$(awk -v m="$best" -v b="$(val version_ms)" 'BEGIN{print (m <= b*1.1) ? "ok" : "OVER"}'); [[ $ok == ok ]] || fail=1
  report version_ms "$(val version_ms)" "$best" "$ok"
  for k in check_resources_storage_ms check_services_ms snapshot_cold_ms snapshot_warm_ms context_ms audit_ms daemon_rss_mb daemon_cpu_percent; do
    report "$k" "$(val "$k")" "—" "pending"
  done
} | tee -a "${GITHUB_STEP_SUMMARY:-/dev/null}"
exit $fail
