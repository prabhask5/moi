Read CLAUDE.md, docs/roadmap.md and docs/agent-workflow.md. Then start the first open item of the roadmap (or the issue named with this command).

Before writing code:
1. List the acceptance criteria (from the spec files the work touches) that you will turn into tests.
2. List spec ambiguities with the decision you propose; write accepted decisions into the spec.

Then, on a branch: write the tests first in tests/, implement until they pass, then do the cleanup pass (remove what was superseded, debug code, scratch/spike files, unused fixtures and dependencies; scripts/check-hygiene.sh green), keep scripts/ci.sh green, update docs/roadmap.md and CHANGELOG.md, and open a PR describing the criteria covered, spec changes, and anything deferred.

Never run moi or tests against this machine's real system; use FixtureHost and tests/fixtures. Never weaken a parity test. Never commit secrets.
