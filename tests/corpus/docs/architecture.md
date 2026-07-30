# Architecture

The service is three processes over one Postgres database.

## Submitter

The HTTP front end validates a payload, assigns a job id, and writes a row. It
does no work beyond that: a submit that returns 201 means the job is durable,
not that it has started. Clients poll for status or supply a webhook.

## Queue

Jobs live in a single table with a priority column and a visibility deadline.
Three lanes are drained urgent-first so a backlog of low-priority work cannot
starve an urgent job queued behind it. A claimed job gets a visibility deadline;
if the worker dies without acking, the deadline passes and the job returns to
its lane with its attempt count incremented.

This is deliberately not a message broker. The queue is small enough that a
table with an index on `(priority, visible_at)` outperforms the operational cost
of running Kafka, and the transactional guarantee — enqueue in the same
transaction as the business write — is the whole reason the design works.

## Workers

Each worker claims up to eight jobs concurrently, routes each to a handler
registered by kind, and reports the outcome. Failures are retried with
exponential backoff and jitter; after five attempts a job moves to the dead
letter table with its last error.

SIGTERM begins a drain rather than terminating in-flight handlers, so a rolling
deploy finishes its current work before exiting. The grace period is 30 seconds,
which is longer than the p99 handler duration by a comfortable margin.

## Failure modes we have actually hit

- **Retry stampede.** Without jitter, a fleet recovering from a database blip
  reissued every retry in lockstep and knocked the database over again. Jitter
  spreads them across a quarter of the delay window.
- **Pool exhaustion under drain.** A drain holds connections while finishing
  work, so the pool cap and the in-flight cap have to be set together. When
  `max_connections` was below `max_in_flight × workers`, drains deadlocked.
- **Unroutable jobs.** A deploy that removed a handler left rows nothing could
  claim. These are now acked immediately and counted, rather than retried
  forever.
