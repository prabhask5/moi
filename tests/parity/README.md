# Parity suites (docs/spec/05-checks.md)

One directory per check and case:

```
parity/<check>/<case>/
  host/        # this case's machine state: a fixture *case* layered over hosts/cestmoi (see ../fixtures/README.md):
               #   root/ (files that differ), commands/ (canned outputs that differ, <key>.missing, <key>.timeout),
               #   docker/ (API answers that differ), statvfs, absent, unwritable
  prev.json    # what the previous run left: {"values": {...}, "sustained": {condition: runs}}
  expected     # two lines: status (up|down), then the exact message the legacy script would push
               # (the message may end in a space; only the file's final newline is not part of it)
  README.md    # which branch of reference/cestmoi/scripts/<script> this covers
```

Rule: every branch (`if`/`case` arm that changes `bad`, `info` or the message) of `resource-check.sh`,
`storage-check.sh` and `service-check.sh` has at least one case; `tests/parity.rs` lists the cases each suite
must have. Messages compare byte for byte.

For `resources`, `tests/parity.rs` also runs the legacy script itself on every case and compares its output
with `expected`. Never edit `expected` to make a test pass unless the spec's Deviations section lists the change.
