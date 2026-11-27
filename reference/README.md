# Reference material

Read-only inputs for building moi. Not compiled, not shipped.

| Path | What | Used for |
|---|---|---|
| `serverstate/` | The working bash + Python prototype moi replaces (snapshot, context, audit, push/pull, restore, watch, uninstall) | Layout, collectors, context content/order, audit rules, restore phases and safety rules (specs 02–04, 08, 09) |
| `cestmoi/scripts/` | The first real server's legacy health-check, backup and self-heal scripts, **redacted** (push tokens → `FAKE_TOKEN`, API keys → `FAKE_API_KEY`, domains → `example.com`) | Exact semantics and message formats the built-in checks must reproduce (spec 05 parity), backup guards (07), heal rules (10), migration plan (11) |
| `cestmoi/cron.d.txt` | How the legacy scripts are scheduled | Migration (11) |
| `cestmoi/PROFILE.md` | What runs on cestmoi (from its first serverstate snapshot), non-secret | Fixture host design, discovery tests, migration plan |

Keep this directory free of secrets. If you add material, redact first and run `scripts/check-secrets.sh`.
