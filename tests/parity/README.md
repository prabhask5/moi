# Parity suites (docs/spec/05-checks.md)

One directory per check and case:

```
parity/<check>/<case>/
  host/        # fixture host state (overlay on hosts/cestmoi) for this case
  prev.json    # stored state from the previous run (CPU counters, sustain flags, restart counts, cooldowns)
  expected     # two lines: status (up|down) and the exact message the legacy script would push
  README.md    # which branch of reference/cestmoi/scripts/<script> this covers
```

Rule: every branch (`if`/`case` arm that changes `bad`, `info` or the message) of `resource-check.sh`,
`storage-check.sh` and `service-check.sh` has at least one case. Messages compare byte for byte.
Never edit `expected` to make a test pass unless the spec's Deviations section lists the change.
