# 13 — Performance budgets

Speed is a product requirement. Budgets live in `budgets.toml` and are enforced by the `budgets` CI job (release build, fixture hosts, median of 5 runs, fails if any metric exceeds its budget by more than 10 %).

| Metric | Budget | Measured on |
|---|---|---|
| `moi --version` wall time | 10 ms | — |
| Binary size (x86_64 musl, stripped) | 15 MB | release build |
| `resources` + `storage` check cycle (smartctl mocked) | 50 ms | cestmoi fixture |
| `services` check excluding network I/O (HTTP/exec mocked) | 200 ms | cestmoi fixture (36 containers, 7 services) |
| Snapshot, cold | 2 s | cestmoi fixture |
| Snapshot, warm (nothing changed) | 500 ms | cestmoi fixture |
| `moi context` | 200 ms | cestmoi fixture |
| `moi audit` excluding directory sizing | 10 s | cestmoi fixture |
| Daemon idle RSS | 30 MB | 10-minute run, fixture docker events replayed |
| Daemon idle CPU | 0.5 % | same |

How speed is achieved (rules, not suggestions):
- No subprocess per check or per container. Docker via the Engine API (one `containers/json` + parallel `inspect`s), kernel data from `/proc` and `/sys`.
- Snapshot collectors in parallel; file capture incremental by `(size, mtime, inode)` cache.
- One long-running daemon; CLI commands talk to it over the socket instead of recomputing.
- Heavy work (`du`, restic, dumps) runs with `nice 19` + `ionice idle` and never on the check path.

### Decisions (M12)

- **Daemon idle RSS and CPU** are measured by `scripts/budgets.sh` on Linux: the release binary runs `moi daemon` on the cestmoi fixture, gets 10 s for its first checks and boot job, and is then left alone for `MOI_BUDGET_DAEMON_SECONDS` (60 in CI rather than the ten minutes above: the fixture's clock stands still, so nothing new becomes due and a longer wait measures the same sleep). RSS is the peak (`VmHWM`), CPU is `utime + stime` over the wall time, both from `/proc`. The fixture's Docker event stream is the fixture's canned one. On other systems the two rows say `Linux only`.
- **A job's time limit**: a thread cannot be stopped from outside, so a limit is a report and then a restart. Limits: a check its own timeout plus a minute, the daily job and a snapshot 30 minutes, boot and verify 20, a heal rule 15, the firewall 5; backups and drills none. At the limit the daemon logs a warning once; at twice the limit it logs an error and exits, and systemd (`Restart=always`) starts a fresh one.
- **`state.history_keep`**: after the daily snapshot, commits older than the limit are folded into one — the state as of the last of them, dated like it, saying how many snapshots it stands for. Newer commits keep everything but their ids. The newest commit is never folded, and nothing happens while fewer than two commits are that old. Objects that nothing reaches afterwards, and the reflog, are deleted so the space really comes back.

The `budgets` job prints a table (metric, budget, measured, delta) into the job summary, like stellar's `pnpm budgets`.
