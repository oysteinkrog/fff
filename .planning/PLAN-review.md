# Adversarial review — Unified Plan

Reviewer: Claude Sonnet 4.6 (adversarial agent G)
Date: 2026-05-28

---

## 1. Overall assessment

The plan is structurally sound and the phasing is correct. Axis A is the decisive
fix; axes B/C/F are belt-and-suspenders; axis D is an independent guardrail; axis
E is premature but harmless behind a flag. My concerns are not architectural but
implementational: three blocking issues — one shutdown ordering hole with the new
`AnyDebouncer`, one missing `user_initiated` propagation path through the B API, and
one misidentified risk in the C concurrent-rebuild section — need to be nailed down
in the plan before any code is written. Six non-blocking issues can be resolved during
implementation.

---

## 2. Blocking issues

### B1 — `AnyDebouncer::stop_nonblocking` vs. existing `stop()` path (Axis A)

`BackgroundWatcher::stop()` (`background_watcher.rs:362-371`) does:

```
if let Some(debouncer) = self.debouncer.lock().take() {
    debouncer.stop_nonblocking();
}
```

The current `debouncer` field is `Arc<Mutex<Option<Debouncer>>>` where `Debouncer`
is the concrete type alias (line 17). Axis A changes the field to
`Arc<Mutex<Option<AnyDebouncer>>>`. So far fine.

The problem is in the **owner thread** (lines 104–141). The owner thread holds
`owner_debouncer = Arc::clone(&debouncer)` and calls
`owner_debouncer.lock().as_mut()?.watch(...)` — but this lock acquisition happens
*while the owner thread is servicing a `watch_rx.recv()` message*. The `stop()`
path drops `watch_tx` first (line 363) which makes the `recv()` return `Err` and
the thread exits normally. This ordering is correct today.

However, after axis A the owner thread's call to `debouncer.watch()` (line 134) must
go through the `AnyDebouncer::watch()` forwarding method. The plan's pseudocode shows
a `watch()` forwarder on the enum, which is correct. **What it does not address:** when
the poll backend is used with `RecursiveMode::Recursive` (as the axis A report correctly
notes the poll path can simplify to), the owner thread's per-directory `watch()` loop
becomes a no-op or wrong. The plan says "skip the `picker.for_each_dir` loop entirely"
for the poll backend but the owner thread runs *at runtime*, not at startup — it watches
*newly discovered* directories that arrive via `watch_tx`. Those new directories still
need to be registered with a `PollWatcher`. The plan does not say whether the owner
thread should call `watch(dir, Recursive)` or `watch(dir, NonRecursive)` on the poll
backend.

**Fix required in plan:** Explicitly specify whether, on the poll backend, the owner
thread (the runtime `watch_rx` consumer) should call `debouncer.watch(dir, NonRecursive)`
or skip the call entirely. `PollWatcher` with an initial `Recursive` root registration
should cover sub-dirs already, making the per-dir runtime call either redundant or wrong
(double-counting polling work). The plan must decide this before axis A is coded.

---

### B2 — `trigger_full_rescan_async` signature change breaks the B API contract (Axis B)

Axis B adds `user_initiated: bool` to `trigger_full_rescan_async`
(`shared.rs:190`). The plan correctly identifies the call sites at
`background_watcher.rs:598` and implicitly at `scan.rs:263`. Both get `false`.

What the plan misses: `background_watcher.rs:598` (verified above) calls the function
as `shared_picker.trigger_full_rescan_async(shared_frecency)` — a method on
`SharedFilePicker`. The signature change must be applied to the public method on
`SharedFilePicker`, not just to an internal helper. Any future caller — including the
multi-tenant axis E registry's eviction code — that calls this method without the flag
won't compile. That is fine; the compiler enforces it. But axis E's design sketches
(`registry.rs::get_or_create`) do not mention triggering a rescan on new project init,
and the plan does not say whether the initial scan of a new project entry is considered
`user_initiated = true` or `false`. If it is `false`, a newly registered project with a
large file tree starts with zero tokens in the bucket and immediately hits suppression
after two scans — which may look like the daemon silently failing to index a new project.

**Fix required in plan:** Add a note in the axis B section (or in the E section) that
initial project scans spawned by the multi-tenant registry are `user_initiated = true`
(or equivalent bypass) so they are not subject to the suppression window.

---

### B3 — Axis C write-lock acquisition while bigram rebuild holds a snapshot (Axis C)

Axis C's pseudocode shows:

```
let snapshot = {
    let guard = shared_picker.read()
    ...
    PostScanUnsafeSnapshot::from_picker(picker)
}
// ... heavy CPU work with NO lock held ...
{
    let mut guard = shared_picker.write()
    picker.set_bigram_index(new_index)
}
```

The plan states "the rebuild reads files via `read_bigram_chunk` without holding the
picker lock (`bigram_filter.rs:649–651`, existing design)." This is correct for the
base-index-building phase. But `set_bigram_index` (`file_picker.rs:1223–1228`)
installs the new index AND resets the `BigramOverlay` to a fresh empty overlay. If
a concurrent grep is mid-flight — holding the read lock to execute `overlay.read()`
inside `grep_search` (`grep.rs:1855`) — the write lock acquisition in
`set_bigram_index` will block until that grep finishes. This is the correct behavior
and the plan says so.

The **unaddressed risk** is the opposite direction: what happens if the incremental
rebuild fires *during an ongoing full rescan* (`run_post_scan` at `scan.rs:304`)?
`run_post_scan` calls `set_bigram_index` with a full rebuild result while holding the
writer lock. The incremental rebuild's background task also calls `set_bigram_index`
from the `BACKGROUND_THREAD_POOL`. Two concurrent `set_bigram_index` calls would race
for the write lock; whichever loses will install a stale index after the fresher full
rebuild.

The plan mentions `rebuild_in_flight: Arc<AtomicBool>` checked before spawning, but
the check-then-spawn sequence is not atomic. More critically: a full rescan
(`run_post_scan`) does not check or set `rebuild_in_flight`. It just calls
`set_bigram_index` unconditionally. An incremental rebuild already in flight when a
full rescan completes will overwrite the fresh full-rebuild index with a stale partial
one.

**Fix required in plan:** The plan must specify that `run_post_scan` sets
`rebuild_in_flight = true` (or cancels any in-flight incremental rebuild) before
calling `set_bigram_index`. The simplest fix: `run_post_scan` loads
`rebuild_in_flight`; if true, it waits or signals the background thread to abort and
redo its rebuild after the full scan completes.

---

## 3. Non-blocking issues

### NB1 — Shutdown drains the semaphore but not in-flight greps (Axis F)

`main.rs:308–325` calls `stop_background_monitor()` when `service.waiting()` completes
or when Ctrl-C fires. `FffServer`'s grep semaphore (`tokio::sync::Semaphore`) is
dropped when `server` is dropped. The F report (§open question 3) asks whether
`semaphore.close()` is needed; the plan defers this. On a clean exit this is fine —
`waiting().await` already ensures no in-flight requests remain. On a Ctrl-C path the
signal handler calls `exit(0)` immediately (`main.rs:316`) without waiting for in-flight
greps. This is the same behavior as today (no regression), but axis F should note this
explicitly rather than deferring it, since adding the semaphore makes the behavior
visible.

### NB2 — `healthcheck.rs` exit path is excluded from `--require-git-root` (Axis D)

`main.rs:199-201` exits early for `--healthcheck` *before* the git-root check that axis
D proposes inserting at `main.rs:215-233`. If `--require-git-root` is combined with
`--healthcheck`, the healthcheck succeeds (exits 0) without performing the git-root
check. The plan says "extend `healthcheck.rs`" to cover the new checks, but does not
say whether the healthcheck flag path at line 199 should also be made git-root-aware or
whether the flags are considered disjoint. Clarify: either the healthcheck flag runs
all pre-scan checks (including git root) or it explicitly documents that it does not.

### NB3 — JSON stderr (axis D) may interleave with tracing-subscriber JSON lines (Axis D)

Axis D emits a JSON line on stderr before `exit(1)`. The MCP host captures stderr.
`main.rs:204` initializes tracing with `init_tracing(log_file, ...)`. Looking at
`fff::log::init_tracing`, if the log file path is empty or fails, the fallback is to
write to stderr. If tracing is initialized to stderr *and* the startup error fires
immediately after, the MCP host reads a stream that may be:

```
WARN fff_core::file_picker: ...
{"event":"fff_startup_error","code":"NO_GIT_ROOT",...}
```

The `code`-field line will be buried in non-JSON tracing output. The plan does not
specify whether `init_tracing` must succeed (and must NOT write to stderr) before the
JSON error line is emitted. **Resolution:** emit the structured startup error to stderr
*before* calling `init_tracing`, or always direct the tracing fallback to stdout
rather than stderr, so the JSON error line on stderr is unambiguous.

### NB4 — `fff_restart_index` in `fff-c` drops new `FilePickerOptions` fields (Axis A)

A report open question (A.4): `fff_restart_index` at `lib.rs:905–913` reconstructs
`FilePickerOptions` from the picker's current state. After axis A, the options struct
has `watcher_backend` and `poll_interval` fields. `fff_restart_index` must preserve
these or the poll backend will silently revert to native after a restart. The plan
acknowledges this as an open question but does not assign a default answer. Given that
a Neovim restart of the index on a DrvFs path would go back to CPU-burning inotify, this
must be resolved: store the resolved backend on `FilePicker` and read it back in
`fff_restart_index`. Mark this as a required code change for axis A, not an open
question.

### NB5 — Default timing interaction: poll 10 s + suppress 5 min + merge floor 30 s

The plan is silent on how these three interact in the DrvFs steady state:
- Axis A: poll every 10 s → events enter the pipeline every ~15 s worst-case.
- Axis B: suppress after 2 consecutive drain failures for 5 min.
- Axis C: overlay merge floor 30 s.

On DrvFs with polling, the event volume is 1 batch per 10 s instead of 2163 per 2 s.
This is the primary fix, so B and C are rarely exercised. That is fine. The concern is
the *native* path (ext4 or a future Linux user who does not want poll): on a repo with a
lot of legitimate churn (active compile, `npm install`), the 5-minute suppression window
may leave the index stale for 5 minutes after a major file operation. This is the "false
suppression on git checkout" risk that axis B acknowledges, but the mitigation
(`need_rescan()` bypass) is only mentioned in the B text and is absent from the PLAN.md
summary table. Add it to the risk table.

### NB6 — BACKGROUND_THREAD_POOL is process-global (Axes A + C)

`file_picker.rs:70-99`: `BACKGROUND_THREAD_POOL` is a `LazyLock<rayon::ThreadPool>`
sized at `num_cpus / 2` at first use. Both axis A's `create_debouncer` heavy work
*and* axis C's `spawn_incremental_rebuild` run on this pool. In the multi-tenant axis E
world, N projects sharing one process means N concurrent incremental rebuilds competing
for the same 16-worker pool. The plan correctly defers a `--max-bg-workers` flag to a
follow-up, but should note that in Phase 3 (multi-tenant), the pool size should be made
configurable alongside `--max-projects`. This is not a Phase 1/2 blocker.

---

## 4. Test plan additions (must-have before each axis merges)

### Axis A
- Unit test `is_polling_preferred_fs` against a known tmpfs or ext4 path (asserts
  `false`) to guard against the `f_type` cast being wrong on this architecture.
- Integration test: start fff-mcp with `--watcher-backend poll` on a temp directory,
  create a file, assert it appears in find_files results within 20 s. (Covers the
  basic poll-watcher round-trip.)
- Smoke test: start with `--watcher-backend auto` against `/c/` (DrvFs), observe that
  the `fff-watcher-own` thread is NOT in `R` state after 60 s idle. This is the
  regression test for the original bug.

### Axis B
- Unit test: call `trigger_full_rescan_async(user_initiated: false)` in a tight loop on
  a mock `SharedFilePicker`; assert that after `RESCAN_BUCKET_CAPACITY` scans are
  spawned, subsequent calls return immediately without spawning. Verify count via an
  atomic counter on the mock `ScanJob`.
- Unit test: after suppression window, assert the bucket refills and a new scan is
  spawned.
- Integration test: simulate a large overflow burst (set `overflow_count` above
  `MAX_OVERFLOW_FILES` via a test fixture), verify the suppress log line appears and
  no more than 2 rescans run within 5 minutes.

### Axis C
- Unit test `BigramOverlay::size()` with known modified/tombstone state.
- Concurrency test: spawn a `spawn_incremental_rebuild` and a concurrent
  `run_post_scan` (via a mock). Assert the final installed index is the one from
  `run_post_scan`, not the stale incremental build (validates the B3 fix).
- Regression test: verify grep results on a modified file are correct both before and
  after an incremental rebuild completes.

### Axis D
- Unit test: start fff-mcp in a tempdir with no `.git`, pass `--require-git-root`,
  assert exit code 1 and JSON line on stderr with `"code":"NO_GIT_ROOT"`.
- Unit test: start fff-mcp with `--max-indexable-files 10` against a tree with 11
  files, assert exit code 1 with `"code":"TOO_MANY_FILES"`.
- Per-PID log test: start two fff-mcp processes with no explicit `--log-file`, assert
  that each writes to a distinct path (`fff_mcp-<pid>.log`) and that the paths differ.

### Axis F
- Unit test: build `FffServer` with `max_concurrent_greps = 2`, send 4 concurrent grep
  requests via a mock MCP handler, assert at most 2 are dispatched to `grep_search`
  simultaneously.
- Verify `#[tokio::main]` removal does not break the `update_check::spawn_update_check`
  call (which uses `std::thread::spawn`, not tokio — this is fine, but add a comment
  confirming it).

---

## 5. Cross-axis coordination notes

**A before F (mandatory):** The PLAN.md conflict table correctly requires A → F because
axis F's `--max-inotify-watches` pre-check inserts into `background_watcher.rs:283`.
Axis A restructures that entire file into `create_debouncer` + `AnyDebouncer`. F's
5-line addition must be written against A's refactored layout, not the current one.
Do not write F's diff until A is merged to the working branch.

**B and C interaction (ordering recommendation):** Axis C's `spawn_incremental_rebuild`
fires from `handle_file_modify`, which runs in the debouncer handler closure. If B is
not yet in, there is no rate limit on how often a full rescan can be triggered, and a
full rescan (which calls `set_bigram_index` and resets the overlay) can repeatedly
interrupt an in-flight incremental rebuild. B should land before C to reduce the race
window, even though C is structurally independent.

**D and main.rs conflict:** All of A, D, and F touch `main.rs:194` (A's discussion
does not but F's `#[tokio::main]` replacement does). D adds new flag parsing before the
`FilePicker::new_with_shared_state` call. The recommended merge order is D → F (D adds
flags, F replaces the macro) or merge them in one PR. Either way, ensure D's
`--require-git-root` check is inserted into `async_main`, not the outer `main()` that
F creates, so it still runs after the runtime is initialized.

**E and B interaction:** If axis E is built after B, `ProjectRegistry::get_or_create`
calls `FilePicker::new_with_shared_state` for a new project, which kicks off an initial
scan. That initial scan calls `trigger_full_rescan_async` internally — which will be
gated by B's rate limiter unless marked `user_initiated = true`. Add this to the B
section's notes (see blocking issue B2 above).

---

## 6. Open questions the synthesis missed

The plan's 10 open questions are well-chosen. These are additions:

**OQ-11 — libc statfs `f_type` field type on this architecture.**
The axis A report uses `buf.f_type as u64` for the comparison. `libc::statfs.f_type`
on Linux x86-64 is `i64` (signed). The cast to `u64` of `0xFF534D42` (CIFS magic) is
fine since it fits in i64 without sign extension. However, the comparison should use
`as u32` or document that the cast is intentional and correct. A silent sign-extension
bug here would silently leave the CIFS case undetected. Should be `buf.f_type as u32`
and the magic constants narrowed to `u32`.

**OQ-12 — Log path change: what notifies the user's monitoring tooling?**
Axis D changes the default log path from `~/.cache/fff_mcp.log` (stable, known) to
`~/.cache/fff_mcp-<pid>.log` (per-PID). The plan notes "document in release notes"
but the user's actual monitoring is `rg "overflow_suppressed|drain_failures"` in the
log. After the change, that grep silently returns nothing (wrong file). The plan should
specify a one-time migration action: add a `~/.cache/fff_mcp-latest.log` symlink (as
D mentions but defers) or advise `ls -t ~/.cache/fff_mcp-*.log | head -1` to find the
current log. Deferring this without noting it in the release plan means the user will
spend time debugging "why are there no logs" after the upgrade.

**OQ-13 — `supergateway-rs` and multi-tenant: is it actually running?**
Axis E recommends keeping supergateway-rs in front for the HTTP transport. The diagnosis
and CLAUDE.md describe the user running stdio MCPs, not HTTP MCPs. It is not confirmed
that supergateway-rs is in the user's MCP config today. Before planning Phase 3 around
it, confirm the user's `.claude.json` actually routes fff-mcp through supergateway, or
plan the native `rmcp` streamable-HTTP path as the fallback.

**OQ-14 — `--max-indexable-files` default vs. the D report's own text.**
The D report (`§4c`, third paragraph) states 500 K as the `default_value_t` in the arg
declaration, then two paragraphs later says "1 000 000 is proposed." PLAN.md uses 1 M.
The D report's code snippet shows `default_value_t = 500_000` which would trip on the
normal 690 K worktrees. This is a typo in the report but it will cause a real bug if
the implementation follows the code snippet rather than the prose. The D implementation
engineer must be told: use `1_000_000`, not `500_000`.

---

## 7. Bottom line

**Approve with edits.** The three blocking issues (B1, B2, B3) must be resolved in the
plan text before implementation begins. None requires architectural rework — B1 is a
two-sentence clarification about poll-backend owner-thread behavior, B2 is a one-line
policy note about initial scans bypassing the rate limiter, and B3 requires adding a
`rebuild_in_flight` check inside `run_post_scan`. All other findings are implementable
during the PR review cycle. The phasing (A + D + F first, then B + C, then E) is
correct and should not change.
