Read CLAUDE.md, docs/milestones.md and docs/agent-workflow.md. Then start the first unchecked milestone (or the next unchecked item of the current one).

Before writing code:
1. List the acceptance criteria (from the spec files the milestone names) that you will turn into tests.
2. List spec ambiguities with the decision you propose; write accepted decisions into the spec.

Then, on a branch named m<N>-<short-name>: write the tests first in tests/, implement until they pass, keep scripts/ci.sh green, tick the boxes in docs/milestones.md, and open a PR describing the criteria covered, spec changes, and anything deferred.

Never run moi or tests against this machine's real system; use FixtureHost and tests/fixtures. Never weaken a parity test. Never commit secrets.
