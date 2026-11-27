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

The `budgets` job prints a table (metric, budget, measured, delta) into the job summary, like stellar's `pnpm budgets`.
