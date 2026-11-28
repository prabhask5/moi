#!/usr/bin/env bash
# Performance budgets (docs/spec/13-performance.md): every metric of budgets.toml, measured on the
# release build against the cestmoi fixture. The daemon's two are read from /proc, so on Linux only.
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
  # audit of the same state (sizes are canned in the fixture, so this is the audit without sizing)
  audit=$(MOI_ROOT=tests/fixtures/hosts/cestmoi MOI_ROOT_OVERLAY="$overlay" python3 -c 'import subprocess, sys, time
t = time.perf_counter_ns(); subprocess.run([sys.argv[1], "audit", "--quiet"], stdout=subprocess.DEVNULL); print((time.perf_counter_ns() - t) // 1000000)' "$bin")
  context=$(MOI_ROOT=tests/fixtures/hosts/cestmoi MOI_ROOT_OVERLAY="$overlay" python3 -c 'import subprocess, sys, time
def run():
    t = time.perf_counter_ns(); subprocess.run([sys.argv[1], "context"], stdout=subprocess.DEVNULL, check=True); return (time.perf_counter_ns() - t) // 1000000
print(min(run() for _ in range(3)))' "$bin")
  # check cycles (smartctl, the journal, the network and the Docker engine are the fixture's)
  time_check() {
    MOI_ROOT=tests/fixtures/hosts/cestmoi MOI_ROOT_OVERLAY="$overlay" python3 -c 'import subprocess, sys, time
def run():
    t = time.perf_counter_ns(); subprocess.run([sys.argv[1], "check", *sys.argv[2:]], stdout=subprocess.DEVNULL); return (time.perf_counter_ns() - t) // 1000000
print(min(run() for _ in range(3)))' "$bin" "$@"
  }
  resources=$(time_check resources); storage=$(time_check storage); checks=$((resources + storage))
  services=$(time_check services)
  # the daemon, idle: started on the fixture, left alone, then asked what it cost (Linux: /proc)
  daemon_rss=— daemon_cpu=—
  if [[ -d /proc/self && -r /proc/self/status ]]; then
    seconds=${MOI_BUDGET_DAEMON_SECONDS:-60}
    read -r daemon_rss daemon_cpu < <(MOI_ROOT=tests/fixtures/hosts/cestmoi MOI_ROOT_OVERLAY="$overlay" python3 -c 'import os, subprocess, sys, time
proc = subprocess.Popen([sys.argv[1], "daemon"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
ticks = os.sysconf("SC_CLK_TCK")
def cpu():
    fields = open(f"/proc/{proc.pid}/stat").read().rsplit(") ", 1)[1].split()
    return (int(fields[11]) + int(fields[12])) / ticks   # utime + stime
time.sleep(10)                      # the first checks, the boot job, the first snapshot
if proc.poll() is not None:
    sys.exit("the daemon did not stay up")
before, started = cpu(), time.monotonic()
time.sleep(float(sys.argv[2]))
used, wall = cpu() - before, time.monotonic() - started
status = open(f"/proc/{proc.pid}/status").read()
peak = int(next(l for l in status.splitlines() if l.startswith("VmHWM:")).split()[1]) / 1024
proc.terminate(); proc.wait()
print(f"{peak:.1f} {100 * used / wall:.2f}")' "$bin" "$seconds")
  fi
  rm -rf "$overlay"
  for pair in "snapshot_cold_ms:$cold" "snapshot_warm_ms:$warm" "audit_ms:$audit" "context_ms:$context" "check_resources_storage_ms:$checks" "check_services_ms:$services"; do
    k=${pair%%:*}; m=${pair##*:}
    ok=$(awk -v m="$m" -v b="$(val "$k")" 'BEGIN{print (m <= b*1.1) ? "ok" : "OVER"}'); [[ $ok == ok ]] || fail=1
    report "$k" "$(val "$k")" "$m" "$ok"
  done
  for pair in "daemon_rss_mb:$daemon_rss" "daemon_cpu_percent:$daemon_cpu"; do
    k=${pair%%:*}; m=${pair##*:}
    if [[ $m == — ]]; then
      report "$k" "$(val "$k")" "—" "Linux only"
    else
      ok=$(awk -v m="$m" -v b="$(val "$k")" 'BEGIN{print (m <= b*1.1) ? "ok" : "OVER"}'); [[ $ok == ok ]] || fail=1
      report "$k" "$(val "$k")" "$m" "$ok"
    fi
  done
} | tee -a "${GITHUB_STEP_SUMMARY:-/dev/null}"
exit $fail
