# B. Rescan loop control

## Problem statement

The overflow-triggered full rescan has no cooldown, no backoff, and no
per-window event budget. On a DrvFs/WSL1 mount with constant Windows-side
noise (Defender, ReSharper, OneDrive), `handle_debounced_events`
(`background_watcher.rs:596`) checks `overflow_count > MAX_OVERFLOW_FILES`
on every 25 ms tick and calls `trigger_full_rescan_async` whenever the
overflow buffer exceeds 1024 files. A single rescan takes 37 s to walk and
77 s to rebuild the bigram index (114 s total). Because events keep arriving
faster than a scan can finish, `rescan_pending` is always set when the scan
drains, and `scan.rs:259-277` immediately spawns a follow-up scan. The
process enters a permanent 114-second loop that saturates `fff-watcher-own`
at ~99 % CPU and grows RSS toward 1.2 GB per process.

## Code references

| Location | What it does |
|---|---|
| `background_watcher.rs:27` | `DEBOUNCE_TIMEOUT = 50ms`, tick = 25 ms |
| `background_watcher.rs:596–600` | Overflow check: `if need_full_rescan \|\| overflow_count > MAX_OVERFLOW_FILES` → `trigger_full_rescan_async` |
| `file_picker.rs:68` | `MAX_OVERFLOW_FILES = 1024` |
| `file_picker.rs:589` | `overflow_count = picker.get_overflow_files().len()` |
| `shared.rs:190–212` | `trigger_full_rescan_async`: spawns immediately or sets `rescan_pending` |
| `shared.rs:200–207` | Sets `rescan_pending = true` if a scan is already active |
| `scan.rs:259–277` | End of scan: drains `rescan_pending` → unconditionally spawns follow-up |
| `scan.rs:19–32` | `ScanSignals` struct — current atomic flags |

There is no timestamp, counter, or rate-limit state anywhere in `ScanSignals`,
`SharedPickerInner`, or `FilePicker`.

## Root cause analysis

`trigger_full_rescan_async` is a pure binary latch: either start now or set
`rescan_pending`. There is no memory of when the last rescan started, how
many consecutive rescans have failed to drain the overflow buffer, or whether
overflow events are still arriving at a rate that makes a fresh scan pointless.
The assumption embedded in the code is that overflow is rare and transient —
true on a native ext4 repo, false on DrvFs where Defender alone generates
continuous inotify traffic across 107 K watched directories. The feedback loop
completes at `scan.rs:260`: as long as `rescan_pending` is true at the end of
any scan, a new scan is spawned with zero delay.

## Proposed change

### Gate placement: `shared.rs` — inside `trigger_full_rescan_async`

Three candidate sites:

1. **`background_watcher.rs:596`** — event handler. Early, but this is a free
   function called from a closure; adding `Instant` state here requires either
   a new parameter threaded from the debouncer closure capture, or a new
   `Arc<Mutex<Instant>>` stored in `BackgroundWatcher`. Neither is clean.

2. **`scan.rs:259–277`** — requeue site. Catches the end-of-scan requeue path
   but misses the case where a new overflow event fires after the requeue check
   (i.e. events arriving between `rescan_pending.swap(false)` and the next
   debouncer tick would start a third scan with no rate limit).

3. **`shared.rs:190`** — `trigger_full_rescan_async`. **Best placement.** This
   is the single choke point through which all rescan requests pass — both from
   the event handler (`background_watcher.rs:517, 598`) and from the end-of-scan
   requeue (`scan.rs:263`). A rate limit here covers all callers with a single
   change and a small diff.

### Mechanism: token bucket (recommended)

A **simple cooldown** (block all calls for N seconds after a rescan) would
suppress user-initiated rescans during the cooldown window, which is
unacceptable (see below). A **token bucket** naturally handles two call
classes: automatic overflow rescans burn tokens and can be suppressed;
user-initiated rescans bypass the bucket entirely.

Exponential backoff is more complex to implement and harder to tune. A single
30-second cooldown window with a small token count achieves equivalent
protection with less state.

**Token bucket spec:**

```
RESCAN_BUCKET_CAPACITY  = 2       # allow up to 2 consecutive automatic rescans
RESCAN_REFILL_INTERVAL  = 120 s   # one new token every 2 minutes
```

With these defaults: after 2 back-to-back overflow rescans the bucket is empty;
the third overflow trigger is silently dropped (no scan spawned, no deferred
pending set). The bucket refills after 2 minutes of quiet. On a normal Linux
ext4 repo a rescan is rare; the bucket never empties.

### Tip-over condition ("fail to drain")

Define "fail to drain" as: overflow_count is still above `MAX_OVERFLOW_FILES`
at the point the rescan-requeue check runs (`scan.rs:260`). When the bucket is
already empty at that moment, skip setting `rescan_pending` and emit a
structured warning log line. This prevents the requeue path from bypassing the
bucket via the atomic flag.

Formally: a rescan "failed to drain" if the overflow buffer is non-empty at
scan completion. After `RESCAN_BUCKET_CAPACITY` consecutive drained=false
outcomes, enter **suppressed mode**: ignore all automatic overflow triggers
until `RESCAN_SUPPRESS_DURATION = 5 min` elapses. Log one `warn!` per
suppression entry and one `info!` on exit.

### Before (current hot path)

```
// background_watcher.rs:596
if need_full_rescan || overflow_count > MAX_OVERFLOW_FILES {
    trigger_full_rescan_async(shared_frecency)?;  // always fires
}

// scan.rs:260
if signals.rescan_pending.swap(false, Ordering::AcqRel) {
    follow_up.spawn();  // always requeues
}
```

### After (proposed)

```rust
// scan.rs: ScanSignals gains two new atomic fields
pub(crate) struct ScanSignals {
    // ... existing fields ...
    /// Unix timestamp (seconds) of the last automatically triggered rescan.
    /// 0 = never. Written by trigger_full_rescan_async, read by rate-limiter.
    pub(crate) last_auto_rescan_secs: Arc<AtomicU64>,
    /// Count of consecutive rescans that did not drain the overflow buffer.
    pub(crate) consecutive_drain_failures: Arc<AtomicU32>,
}

// shared.rs: trigger_full_rescan_async gains a `user_initiated` flag
pub fn trigger_full_rescan_async(
    &self,
    shared_frecency: &SharedFrecency,
    user_initiated: bool,
) -> Result<(), Error> {
    if !user_initiated {
        let guard = self.read()?;
        if let Some(picker) = guard.as_ref() {
            let sigs = picker.scan_signals();
            let now = unix_secs_now();
            let last = sigs.last_auto_rescan_secs.load(Ordering::Acquire);
            let failures = sigs.consecutive_drain_failures.load(Ordering::Acquire);

            // Suppressed mode: too many consecutive failures
            if failures >= RESCAN_BUCKET_CAPACITY as u32
                && now.saturating_sub(last) < RESCAN_SUPPRESS_DURATION_SECS
            {
                warn!(
                    consecutive_drain_failures = failures,
                    secs_since_last_rescan = now.saturating_sub(last),
                    overflow_suppressed = true,
                    "Overflow-triggered rescan suppressed — bucket exhausted; \
                     watcher noise exceeds scan rate on this filesystem"
                );
                return Ok(());  // drop the request; do NOT set rescan_pending
            }

            sigs.last_auto_rescan_secs.store(now, Ordering::Release);
        }
        drop(guard);
    }
    // ... rest of existing logic unchanged ...
}

// scan.rs:259-277: check drain outcome before requeuing
let overflow_len = shared_picker.read()
    .ok()
    .and_then(|g| g.as_ref().map(|p| p.get_overflow_files().len()))
    .unwrap_or(0);

if overflow_len > MAX_OVERFLOW_FILES {
    signals.consecutive_drain_failures.fetch_add(1, Ordering::AcqRel);
} else {
    signals.consecutive_drain_failures.store(0, Ordering::Release);
}

if !signals.cancelled.load(Ordering::Acquire)
    && signals.rescan_pending.swap(false, Ordering::AcqRel)
{
    match Self::new_rescan(&shared_picker, &shared_frecency) {
        Ok(Some(follow_up)) => {
            info!(overflow_len, "Rescheduling deferred rescan after current scan finished");
            follow_up.spawn();
        }
        // ...
    }
}
```

### User-initiated rescans bypass the rate limit

`trigger_full_rescan_async` is called from the event handler with
`user_initiated: false`. The Neovim plugin and any MCP-facing public API should
pass `user_initiated: true`. The existing callers in `background_watcher.rs`
(lines 517 and 598) are both watcher-driven and get `false`. The caller in
`scan.rs:263` (end-of-scan requeue) is also automatic and gets `false`. Any
future public API (`SharedFilePicker::force_rescan()`) wrapping user intent
passes `true` and is never gated. This preserves the invariant that a
deliberate `/fff rescan` from inside Neovim always works.

## Compatibility / risk

- **Neovim plugin (`lua/`, `fff-c` crate):** The plugin does not call
  `trigger_full_rescan_async` directly — it goes through the `SharedFilePicker`
  public surface. Adding a `user_initiated` parameter requires updating both
  internal call sites (two lines), but the public `SharedFilePicker` method
  signature is crate-internal (`pub fn` visible within `fff-core`). No ABI
  change to the C FFI layer (`fff-c`) is needed unless `fff-c` exposes a
  rescan function (check `crates/fff-c/src/` before landing).
- **False suppression on legitimate large-repo churn:** A `git checkout` of a
  branch that touches 50 K files will legitimately overflow the buffer and
  needs a rescan. With `RESCAN_BUCKET_CAPACITY = 2`, it gets two attempts;
  if both complete while new events are still arriving, the third attempt is
  gated for up to 5 minutes. Mitigation: treat a `need_rescan()` kernel flag
  (`background_watcher.rs:430`) as `user_initiated = true` since it signals a
  kernel-side buffer overflow rather than fff's own overflow arena.
- **New `AtomicU64` / `AtomicU32` in `ScanSignals`:** Two extra `Arc`-wrapped
  atomics. Negligible memory cost; no lock contention.
- **Tests needed:** a unit test that calls `trigger_full_rescan_async` with
  `user_initiated: false` in a tight loop and asserts that at most
  `RESCAN_BUCKET_CAPACITY` scans are spawned within `RESCAN_SUPPRESS_DURATION`.

## Rollout plan

- **Flag:** `FilePickerOptions::rescan_rate_limit: bool` defaulting to `true`.
  Set to `false` to restore current behavior. The flag is wired through to
  `ScanSignals` at construction so the bucket check is a single branch.
- **Default:** on. The change is purely protective; it has no effect until the
  bucket empties, which requires two rapid sequential auto-rescans.
- **Independence:** This axis is fully independent of A (watcher backend),
  C (bigram rebuild cost), D (startup safety), E (multi-tenant), and F
  (resource budgets). It can land as a standalone PR on the `oysteinkrog/fff`
  fork. Estimated diff: ~80 LOC including the new fields, the bucket check,
  and the drain-failure counter update.

## Telemetry additions

The following structured fields, added to existing or new `tracing` events,
would have made the observed loop self-diagnosing from a single `rg
"overflow_suppressed\|drain_failures\|auto_rescan"` in the log:

```
trigger_full_rescan_async:
  + user_initiated = bool
  + overflow_suppressed = bool       (new — logged on suppression)
  + consecutive_drain_failures = u32

end-of-scan requeue check (scan.rs:259):
  + overflow_len = usize             (how full the buffer is at scan end)
  + drain_failures = u32             (running count)
  + rescan_suppressed = bool

"Event processing summary" log (background_watcher.rs:529):
  + secs_since_last_auto_rescan = u64   (already computable from the new atomic)
```

## Open questions

1. **`fff-c` exposure:** Does `crates/fff-c/src/` expose a `rescan` function
   that calls `trigger_full_rescan_async`? If so, is it called from user
   interaction (Neovim `:FffRescan`) or automatically? Determines whether that
   call site gets `user_initiated = true` or `false`.

2. **Suppress duration vs. refill:** Should the bucket refill one token every
   `REFILL_INTERVAL` (classic token bucket) or use a flat suppress-then-reset
   approach? The flat approach is simpler to reason about in logs; the true
   token bucket allows a burst of 2 rescans even after a quiet period. Oystein
   to decide which is more intuitive for a power user watching the logs.

3. **Kernel overflow events as bypass:** `need_rescan()` events
   (`background_watcher.rs:430`) mean the *kernel* dropped events — the index
   is definitely stale. Should these bypass the rate limit unconditionally?
   Recommendation: yes, treat them identically to `user_initiated = true`.

4. **Config exposure:** Should `RESCAN_BUCKET_CAPACITY` and
   `RESCAN_SUPPRESS_DURATION` be CLI flags / env vars, or compile-time
   constants? Given that the current codebase has no existing tunables for
   watcher behavior, constants are simpler for the first ship; env-var overrides
   can follow if users report false suppressions.
