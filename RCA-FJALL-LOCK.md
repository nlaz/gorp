# RCA — fjall's database lock excludes concurrent readers

**Status:** drafted for upstream. Written against `fjall 3.1.5` (what `fold`
pins); the relevant source is byte-identical in 3.1.6 and 3.1.8.

**Not a bug report.** Everything below is fjall behaving as designed. The gap is
a *missing mode*, and the ask at the end is small. This document exists so the
decision is made against evidence rather than against my memory of it.

---

## 1. Summary

fjall takes a single **exclusive** advisory file lock on `<db>/lock` for the
lifetime of the `Database`. It is taken on every open, including opens that only
ever read. There is no read-only open mode. Consequently **at most one process
may have a fjall database open at all**, for any purpose.

For a persistent server this is invisible. For a one-shot CLI — a process per
invocation, several possibly running at once — it means the store is unavailable
to every process but one, and the unavailability costs 200 ms to discover.

## 2. Impact on the calling system

`semgrep` is a search CLI: one process per query, no daemon. We are evaluating
`fold` (which embeds fjall) as durable storage for a read-repair overlay — a
small, changing set of records that supersede part of a large immutable index.
The access pattern is **read-mostly**: every query reads the overlay; only a
query that detects filesystem drift writes to it.

That pattern is a poor fit for the current lock, in three ways.

**Concurrent invocations are normal.** Our own test suite runs searches in
parallel, and an agent driving the tool issues parallel tool calls. Under any
concurrency all but one process must fall back to a non-persistent path.

**The failure is expensive.** `LockedFileGuard::try_acquire` retries three times
with a 100 ms sleep between attempts, so a contended open costs **~200 ms before
it fails**. Our warm query budget is 1.8–115 ms depending on corpus. Discovering
that the fast path is unavailable would cost more than the slow path it degrades
to — the degraded case is the one that most needs to be cheap.

**Readers exclude readers, which nothing requires.** Two processes that only
read cannot corrupt each other. They are excluded because the primitive chosen is
exclusive, not because the data model demands it.

## 3. Evidence

### 3.1 The lock is exclusive, and a shared variant was available

`src/locked_file.rs`, both entry points:

```rust
pub fn create_new(path: &Path) -> crate::Result<Self> {
    // ...
    file.try_lock().map_err(|e| match e { ... })?;      // exclusive
}

pub fn try_acquire(path: &Path) -> crate::Result<Self> {
    const RETRIES: usize = 3;
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    for i in 1..=RETRIES {
        if let Err(e) = file.try_lock() {               // exclusive
            match e {
                std::fs::TryLockError::Error(e) => return Err(crate::Error::Io(e)),
                std::fs::TryLockError::WouldBlock => {
                    if i == RETRIES { return Err(crate::Error::Locked); }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }
        } else { break; }
    }
    // ...
}
```

`std::fs::File::try_lock_shared()` is stable and available on the toolchain fjall
targets (verified compiling both calls on rustc 1.96.1). The exclusive variant is
used unconditionally.

Note also the asymmetry: `create_new` fails immediately, `try_acquire` sleeps up
to 200 ms. A caller cannot opt out of the retry.

### 3.2 The lock is taken on every open

`src/db.rs`, both paths through `create_or_recover`:

```rust
// recover(), line 576
let lock_file = LockedFileGuard::try_acquire(&config.path.join(LOCK_FILE))?;

// create_new(), line 814
let lock_file = LockedFileGuard::create_new(&config.path.join(LOCK_FILE))?;
```

The guard is held in the `Database` and released in
`LockedFileGuardInner::drop`. It is marked `#[expect(unused)]` — it exists purely
for its `Drop`.

### 3.3 There is no read-only open

Grepping 3.1.5 for `read_only` / `readonly` / `ReadOnly` outside doc comments
returns nothing. The only "read-only" in the codebase refers to read-only
*transactions* (`Snapshot`), which are a construct **within** an already-open
database. `db_config.rs` exposes no knob for lock behaviour or open mode.

### 3.4 Opening a database writes to it

This is the part that makes "just take a shared lock" insufficient on its own.
`src/db.rs:590`, inside `recover`:

```rust
let active_journal = Arc::new(journal_recovery.active);
active_journal.get_writer().persist(PersistMode::SyncAll)?;
```

Recovery replays the journal and then **fsyncs it**. So opening is not a read
operation today, and a shared lock alone would let two openers both replay and
re-persist the same WAL while allocating sequence numbers from independent
counters. The lock is load-bearing precisely because open mutates.

## 4. Root cause

Two design decisions compose into the observed behaviour, and neither is wrong
on its own:

1. **Open performs recovery, and recovery writes.** This makes every open a
   writer, which makes an exclusive lock the correct primitive for the open path
   as currently written.
2. **There is no separate read-only path**, so decision 1 applies to callers that
   will never write.

The root cause is therefore not the lock. It is that **fjall has exactly one open
mode, and that mode is a writer's**. The lock is a correct consequence.

This matters for choosing a fix: the exclusive lock is a symptom. Relaxing it
without addressing (1) trades a clean failure for silent corruption — two
processes replaying and re-persisting one journal, each with its own sequence
counter and both writing the manifest.

## 5. What we are explicitly **not** asking for

**Do not remove or weaken the lock as it stands.** Given §3.4 it is the only
thing preventing multi-process corruption, and `SingleWriterTxDatabase` is named
for the invariant it enforces. A fork that drops it would be a fork that
corrupts.

## 6. Proposed upstream change

### 6.1 Primary ask — a read-only open mode

Add an open path that:

- takes a **shared** lock (`try_lock_shared`) so readers coexist with readers,
  and still excludes a writer;
- **skips journal recovery entirely** — no replay, no `persist`, no writes of any
  kind;
- never starts compaction or any background work;
- exposes only snapshot reads.

The visible semantics would be: **a read-only opener sees the database as of the
last successful persist.** Anything still in the write-ahead log is not visible.

That staleness must be documented prominently, because "silently behind" is a
bad default surprise. But it is a well-understood mode — it is what most
embedded stores offer for readers — and crucially it is *sufficient for our use
case*: we control the writer, so the writer can `persist` after each write and
readers then see everything committed.

This is the cheapest change that solves the problem, and it does not touch the
existing path at all.

### 6.2 Secondary ask — let a caller decline the retry

Independent of the above, and much smaller: make the 200 ms retry opt-out, e.g. a
`Config::lock_retries(usize)` or a non-retrying `try_open()`. `Error::Locked` is
already a typed, well-named error; the only problem is how long it takes to
arrive. A latency-sensitive caller that intends to degrade wants to know
immediately.

This alone would materially improve our situation even without §6.1.

### 6.3 Considered and rejected — a read-only mode that replays the journal

A reader that replayed the WAL *into memory* (without persisting) would see a
current view rather than a flushed one, which is strictly nicer. We are not
asking for it: the writer may be appending to or rotating the segment being read,
so it needs care that a flushed-only reader does not, and §6.1 plus a
checkpointing writer already covers our need. Worth noting as the better long
answer if fjall wants one.

## 7. What we will do downstream regardless

These are mitigations, not fixes, and we would keep them even if §6 lands —
they make the degraded path cheap and the failure typed.

1. **Preflight our own advisory lock** on a sibling file, with a single
   non-blocking `try_lock()`, before fjall ever sees the path. Every one of our
   processes obeys this protocol, so in the common case fjall's lock is
   uncontended and the 200 ms retry is never reached.
2. **`fold::Stream::try_new`** returning `Result` instead of `.unwrap()`ing the
   open. Today a lock conflict is a **panic** in a CLI, which is the worst
   available outcome; `new` keeps its signature by delegating.
3. **Degrade, don't fail.** When the overlay is unavailable we fall back to
   recomputing in memory — the behaviour we have today — so the persistent store
   is a fast path that is never load-bearing for correctness.
4. **Measure the hit rate** rather than assume it. If a realistic session sees
   the overlay unavailable most of the time, the answer is a resident server
   process (where single-writer is the natural shape), not a workaround.

## 8. Open question for the maintainers

Is the single-open constraint considered intrinsic to fjall's design, or
incidental to there being no reader path yet? The answer changes what we build:
if intrinsic, a CLI should not embed fjall directly and we would put a server in
front of it; if incidental, §6.1 makes fjall a good fit for one-shot processes
and we would rather contribute that than route around it.

---

### Reproduction — run, not sketched

```rust
// argv[1] == "hold": open and sleep. Otherwise: time an open and report.
let t = Instant::now();
let r = fjall::SingleWriterTxDatabase::builder("/tmp/fjall_lock_probe.db").open();
match r {
    Ok(_)  => println!("opened OK in {:?}", t.elapsed()),
    Err(e) => println!("Err({e:?}) after {:?}", t.elapsed()),
}
```

`fjall 3.1.8`, release profile (`opt-level = 3`, `lto = "thin"`,
`codegen-units = 4`), M-series mac:

```
uncontended, cold page cache        opened OK in   9.81 ms
uncontended, warm  (7 trials)       opened OK in   0.38 – 0.96 ms

contended (one holder, 3 probes)    Err(Locked) after 210.7 ms
                                    Err(Locked) after 202.8 ms
                                    Err(Locked) after 212.1 ms

after the holder exits              opened OK in  13.95 ms
```

Two things this settles.

**The 200 ms is real and consistent** — it is the two `sleep(100ms)` calls, and
there is no way to opt out. Discovering the store is unavailable costs more than
most of the work a CLI would have done instead.

**An uncontended open is cheap** — sub-millisecond warm, on an empty database.
So the objection here is specifically about *exclusion and the cost of
discovering it*, not about fjall being slow to open. That is why §6.2 (let a
caller decline the retry) would help materially even on its own.

### Footnote: the pinned version is yanked

`fold`'s `Cargo.lock` pins `fjall 3.1.5`, which is **yanked** on crates.io
(`cargo` refuses a fresh resolution of it). The lock implementation is
byte-identical in 3.1.5, 3.1.6 and 3.1.8, so nothing in this document depends on
the difference — but any downstream adopting `fold` will need the pin moved
before it can resolve from a clean lockfile.
