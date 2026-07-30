# Operations

## Deploying

Rolling restart, one worker at a time. Each receives SIGTERM, stops claiming new
jobs, finishes what it holds, and exits. Wait for the drain to complete before
moving to the next instance — a simultaneous restart of every worker empties the
in-flight set into the retry path and produces a visible latency spike.

## Alerting

Three alerts are worth paging on:

- **Queue depth rising for 15 minutes.** Either throughput dropped or arrival
  rate rose; the p99 handler duration tells you which.
- **Dead letter table growing.** A handler is failing deterministically. Read
  the last error before touching anything else.
- **Connection acquire timeouts.** The pool is saturated. Check whether a drain
  is in progress before raising the cap.

Counters and latency histograms are exported in Prometheus text format. The
histogram uses fixed exponential buckets, so the reported p99 is the upper bound
of the crossing bucket, not an interpolated value — treat it as a ceiling.

## Runbook: draining a stuck worker

1. Confirm the worker is alive but not acking (`jobs_in_flight` flat, non-zero).
2. Check for a handler blocked on a network call without a timeout. This is
   almost always the cause.
3. SIGTERM and wait the full grace period. Do not SIGKILL: the visibility
   deadline will return the jobs anyway, but SIGKILL loses the metrics flush.
4. If the jobs return and immediately hang again on another worker, the payload
   is the problem, not the worker. Move it to the dead letter table by hand.

## Capacity

One worker handles roughly 200 jobs per second of trivial work, and is bound by
handler duration for anything real. Scale on queue depth, not CPU: the workers
are almost always waiting on the database or an upstream service, so CPU sits
low even when the system is saturated.
