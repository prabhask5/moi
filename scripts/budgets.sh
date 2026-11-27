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
  size_mb=$(awk -v b="$(wc -c <"$bin")" 'BEGIN{printf "%.1f", b/1048576}')
  ok=$(awk -v m="$size_mb" -v b="$(val binary_mb)" 'BEGIN{print (m <= b*1.1) ? "ok" : "OVER"}'); [[ $ok == ok ]] || fail=1
  report binary_mb "$(val binary_mb)" "$size_mb" "$ok"
  # best of 5, timed in python: `date +%s%N` is GNU-only
  best=$(python3 -c 'import subprocess, sys, time
runs = []
for _ in range(5):
    t = time.perf_counter_ns(); subprocess.run([sys.argv[1], "--version"], stdout=subprocess.DEVNULL); runs.append(time.perf_counter_ns() - t)
print(min(runs) // 1000000)' "$bin")
  ok=$(awk -v m="$best" -v b="$(val version_ms)" 'BEGIN{print (m <= b*1.1) ? "ok" : "OVER"}'); [[ $ok == ok ]] || fail=1
  report version_ms "$(val version_ms)" "$best" "$ok"
  # snapshot of the cestmoi fixture into a throwaway overlay: first run cold, then the best of 3 warm runs
  overlay=$(mktemp -d)
  read -r cold warm < <(MOI_ROOT=tests/fixtures/hosts/cestmoi MOI_ROOT_OVERLAY="$overlay" python3 -c 'import subprocess, sys, time
def run():
    t = time.perf_counter_ns(); subprocess.run([sys.argv[1], "snapshot", "--quiet"], stdout=subprocess.DEVNULL, check=True); return (time.perf_counter_ns() - t) // 1000000
cold = run(); print(cold, min(run() for _ in range(3)))' "$bin")
  rm -rf "$overlay"
  for pair in "snapshot_cold_ms:$cold" "snapshot_warm_ms:$warm"; do
    k=${pair%%:*}; m=${pair##*:}
    ok=$(awk -v m="$m" -v b="$(val "$k")" 'BEGIN{print (m <= b*1.1) ? "ok" : "OVER"}'); [[ $ok == ok ]] || fail=1
    report "$k" "$(val "$k")" "$m" "$ok"
  done
  for k in check_resources_storage_ms check_services_ms context_ms audit_ms daemon_rss_mb daemon_cpu_percent; do
    report "$k" "$(val "$k")" "—" "pending"
  done
} | tee -a "${GITHUB_STEP_SUMMARY:-/dev/null}"
exit $fail
