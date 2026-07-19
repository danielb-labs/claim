# Hub status

Where the hub is: the ledger head, the registry version, the review-queue depth, and
how many ingests it has rejected. Derived at read time; an empty hub reports truthful
zeros, never a fabricated "healthy".

| field | value |
|---|---|
| ledger head | {{ ledger_head }} |
| registry version | {{ registry_version }} |
| in review queue | {{ queued }} |
| rejected ingests | {{ rejection_count }} |

_As of ledger head {{ as_of.ledger_head }}, registry version {{ as_of.registry_version }}, clock {{ as_of.clock }}._

A rising **rejected ingests** count is the hub telling you telemetry is being turned
away while the claims it would refresh go stale — watch it. The machine-readable JSON
position is at `/status`; the review queue is at [/ui/queue.md](/ui/queue.md).
