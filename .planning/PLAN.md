# Unified plan — fff CPU/resource fixes on `oysteinkrog/fff`

Synthesizes 6 round-1 reports (`.planning/round-1/{A..F}-*.md`). Authoritative
detail lives in those reports; this document is the cross-axis view.

## Executive summary

The root cause of fff-mcp's runaway CPU on the user's machine is a single loop:
DrvFs surfaces Windows-side filesystem noise as inotify events → fff's overflow
buffer trips at >1024 stale entries → triggers a full rescan + bigram rebuild
(~114 s on the user's 690 K-file repo) → DrvFs delivers more events while that
runs → loop never exits. 16 instances ran it in parallel.

Two amplifying conditions: (1) two instances were accidentally rooted at
non-git directories and indexed huge unbounded trees; (2) all 16 instances
shared one log file, masking what was happening.

The **single most impactful change** is axis A (watcher backend): switching to
PollWatcher on DrvFs/9p/NFS/CIFS makes the inotify storm physically impossible
and lets the rest of the pipeline rest. Axes B/C/F/D harden the pipeline so
that even on a noisy filesystem nothing spirals. Axis E (multi-tenant) is a
separate memory-and-watch-count optimisation that only pays off once A–D are
in.

## Recommended phasing

```
Phase 1 — independent, ship in parallel       (small fast wins, defaults on)
├── A. Watcher backend selection (auto/native/poll, DrvFs detection)
├── D. Startup safety (max-indexable-files, --require-git-root, per-PID log)
└── F. Resource budgets (max-inotify-watches, max-tokio-workers, max-concurrent-greps)

Phase 2 — depends on Phase 1 being deployed   (complete the loop fix)
├── B. Rescan rate limit (token bucket on trigger_full_rescan_async)
└── C. Incremental bigram (overlay-driven background rebuild)

Phase 3 — only after Phase 1+2 are proven     (consolidation)
└── E. Multi-tenant single daemon
```

**Reasoning:** Phase 1 is three small additive PRs that defang the two
observed failure modes (unbounded walks, inotify storm) without touching the
scan/index code paths. Phase 2 hardens the rescan/index logic against any
remaining churn. Phase 3 trades 16 × RSS for 1 × N × RSS only after per-project
cost is acceptable.

Total estimated diff across all 6 axes: **~1000–1100 LOC** new+changed,
spread across `fff-mcp/src/main.rs`, `fff-mcp/src/server.rs`,
`fff-core/src/{file_picker,background_watcher,scan,shared,bigram_filter,error}.rs`,
and one new `fff-mcp/src/registry.rs` (axis E only).

## Per-axis summary + decisions to lock

### Axis A — Watcher backend
- `WatcherBackend::{Auto, Native, Poll}` enum, default `Auto`.
- `is_polling_preferred_fs(path)` on Linux: `statfs().f_type` ∈ {`0x53464846`
  WSL1 DrvFs, `0x01021997` 9p, `0xFF534D42` CIFS, `0x6969` NFS}.
- New `AnyDebouncer { Native(...), Poll(...) }` enum (notify-rs ties
  `Debouncer<T>` to `T`).
- CLI: `--watcher-backend {auto,native,poll,off}` (default `auto`),
  `--poll-interval-secs 10`.
- ~210 LOC. **Default on.**
- Open: confirm `0x53464846` is the correct WSL1 DrvFs magic on this machine.

### Axis B — Rescan rate limit
- Gate placed in `shared.rs::trigger_full_rescan_async` (single choke point).
- Token bucket capacity **2**, suppress window **5 min** after 2 consecutive
  rescans that fail to drain the overflow buffer.
- New atomics on `ScanSignals`: `last_auto_rescan_secs`,
  `consecutive_drain_failures`.
- User-initiated rescans (Neovim, future MCP) and kernel-flagged
  `need_rescan()` bypass the bucket.
- CLI/option: `FilePickerOptions::rescan_rate_limit: bool` (default `true`).
- ~80 LOC. **Default on.**

### Axis C — Incremental bigram
- The `BigramOverlay` already exists and is consulted at query time — it's
  just never merged back into the base index. The fix is a **background rebuild
  triggered by overlay size**, not in-place mutation (the dense bitset layout
  forbids in-place).
- Trigger: `overlay.size() >= 512` AND `>= 30 s since last merge` AND no
  rebuild in flight → background rebuild via `BACKGROUND_THREAD_POOL`.
- Two new atomics on `FilePicker`: `rebuild_in_flight`, `last_merge_instant`.
- New `BigramOverlay::size()` method.
- ~140 LOC. **Default on** behind feature flag `incremental-bigram` for one
  soak release, then flag removed.

### Axis D — Startup safety
- `--max-indexable-files` (env `FFF_MAX_INDEXABLE_FILES`, default **1 000 000**).
  Early-abort in walk closure (`file_picker.rs:1762`) via `WalkState::Quit`.
  Returns `Err(Error::TooManyFiles{...})` from `walk_filesystem`.
- `--require-git-root` (default **off** in binary, **on** in MCP launch
  config). Refuses to start if `Repository::discover` finds no git workdir.
- `--root-must-be-under <path>` (multi-valued, default empty). Allowlist
  check after path resolution.
- `--validate-root` mode: runs only the cheap checks, exits 0/1.
- Per-PID default log: `~/.cache/fff_mcp-<pid>.log`.
- Stderr JSON line on startup failure: `{event,code,base_path,detail}`.
- ~150–200 LOC. **Default on for the file-count cap and per-PID log;
  default off for `--require-git-root`** (opt-in via MCP config).

### Axis E — Multi-tenant daemon
- New `--multi-tenant` mode, optional `--listen 127.0.0.1:9876`.
- `registry.rs` with `ProjectRegistry { Arc<RwLock<HashMap<PathBuf, ProjectEntry>>> }`.
- All three tool params gain optional `projectPath: Option<String>`. In MT
  mode, **absent `projectPath` errors out** (no silent fallback).
- `--max-projects` default **12**; LRU eviction with `--evict-after` default
  **30 min**.
- Reuse existing `SharedFilePicker` / `SharedFrecency` primitives; no fff-core
  changes.
- HTTP transport: **keep supergateway-rs** in front, do not grow native HTTP.
- ~400 LOC, all additive, behind `--multi-tenant` flag.
- Open: whether to deduplicate worktrees by `.git/workdir()` (saves memory,
  may miss worktree-private files) or one entry per worktree.

### Axis F — Resource budgets
- `--max-inotify-watches N` (env `FFF_MAX_INOTIFY_WATCHES`, default **8192**).
  Pre-check inserted at `background_watcher.rs:283` *before* the kernel
  `debouncer.watch()` call so ENOSPC is never reached.
- `--max-tokio-workers N` (env `FFF_MAX_TOKIO_WORKERS`, default **4**).
  Requires replacing `#[tokio::main]` with a manual `Runtime::Builder` at
  `main.rs:194`.
- `--max-concurrent-greps N` (env `FFF_MAX_CONCURRENT_GREPS`, default **4**).
  `tokio::sync::Semaphore` on `FffServer`; `grep`/`multi_grep` acquire a
  permit, `find_files` does not.
- Deferred: `--max-rss-mb` (no reliable WSL1 measurement),
  `--max-events-per-sec` (sits below axis B's gate; layer on later).
- ~100 LOC. **Default on**, conservative defaults.

## File-conflict map (for ordering)

| File | A | B | C | D | E | F |
|---|---|---|---|---|---|---|
| `fff-mcp/src/main.rs` | + flag, options | – | – | + flags, log path, walk error path | + MT flag, listener | + flags, replace `#[tokio::main]` |
| `fff-mcp/src/server.rs` | – | – | – | – | + project_path, registry plumb | + semaphore |
| `fff-mcp/src/registry.rs` (new) | – | – | – | – | new file | – |
| `fff-mcp/src/healthcheck.rs` | – | – | – | extend | – | + budget readout |
| `fff-core/src/file_picker.rs` | + FilePickerOptions fields, FilePicker fields | – | + handle_create_or_modify trigger, atomics | + FilePickerOptions field | – | + FilePickerOptions field |
| `fff-core/src/background_watcher.rs` | major refactor (AnyDebouncer, create_debouncer split) | – | – | – | – | + per-dir watch cap |
| `fff-core/src/scan.rs` | – | + drain-failure counter at requeue | – | + error propagation from walk | – | – |
| `fff-core/src/shared.rs` | – | + token bucket in trigger_full_rescan_async, user_initiated flag | – | – | – | – |
| `fff-core/src/bigram_filter.rs` | – | – | + `BigramOverlay::size()`, BigramRebuildState | – | – | – |
| `fff-core/src/error.rs` | – | – | – | + `TooManyFiles` variant | – | – |
| `crates/fff-c/src/lib.rs` | one-line opts update | – | – | one-line opts update | – | one-line opts update |

**Hard conflicts:** A and F both touch `background_watcher.rs`. A is a major
refactor; F adds one pre-check. **Order A → F** (F is a 5-line addition once A
is in).

**Soft conflicts:** All axes that add to `FilePickerOptions` and `Args` — six
new fields total — are purely additive. Merging in any order works.

## Headline open questions for Oystein

These need answers (or deferral with a sensible default) before I write code:

| # | Question | Default if you don't decide |
|---|---|---|
| 1 | Confirm WSL1 DrvFs `statfs().f_type` is `0x53464846` (axis A). Want me to verify on your box before coding? | I run a one-line check first, fix the constant if wrong, then proceed. |
| 2 | `--max-indexable-files` default: **1 000 000** (catches the bad cases, leaves headroom for legitimately huge repos) or **750 000** (tighter, may fire on a future big monorepo)? | **1 000 000** as proposed by D. |
| 3 | `--require-git-root`: ship default **off** in the binary and turn it on in MCP config, or default **on** for `FFFMode::Ai`? | Off in binary, on in MCP config. |
| 4 | Default `--max-inotify-watches`: agent F proposed **8192** (conservative). For 107 K-dir repos this loses fidelity. Raise to **32768**, or trust axis A's poll fallback to handle big repos? | **32768** if shipping in parallel with axis A (poll covers DrvFs anyway); **8192** if A slips. |
| 5 | Default `--max-tokio-workers`: **4** is correct for I/O-only workload but may regress if any future feature puts CPU work on tokio. Keep **4**? | **4**. |
| 6 | Axis C `OVERLAY_MERGE_THRESHOLD`: **512** is a guess. Instrument and tune later, or pick a fixed value now? | Ship **512** + tracing field; tune in v2. |
| 7 | Axis E worktree deduplication: key the project registry by `.git/workdir()` (16 sessions on 12 sibling worktrees → 1 entry) or by submitted `project_path` (1 entry per worktree)? | Key by `workdir()` — this is the whole point of the consolidation. |
| 8 | Axis E: ship in Phase 3 as planned, or skip it entirely if Phase 1+2 deliver enough relief? | Ship in Phase 3; small cost relative to the win for your workload. |
| 9 | Co-author tag on commits — keep the `Claude Opus 4.7 …` line from your global CLAUDE.md? | Yes (per CLAUDE.md). |
| 10 | Where to land the work: one PR per axis (6 PRs), or one branch per phase (3 PRs)? | One PR per axis — easier to review and revert per-axis. |

## Compatibility envelope

- **Neovim plugin (`fff-c` + `lua/`):** Each axis is structured so the Lua
  side requires no changes. The C ABI is preserved. The only Rust-side change
  to `fff-c/src/lib.rs` is updating the two `FilePickerOptions` construction
  sites to include the new fields (with `..Default::default()` or explicit
  defaults).
- **MCP protocol:** All new fields on tool params are optional. Existing
  Claude Code clients keep working without modification.
- **Upstream-ability:** Every axis is designed to be a candidate for an
  upstream PR. Defaults are conservative; flags are off-by-default where a
  Linux/native user would notice; behavior on small / native repos is
  unchanged.

## What "done" looks like

After Phase 1 lands (axes A + D + F) in the user's environment:
- No process uses more than 4 CPU% at idle.
- No process indexes a tree larger than 1 M files.
- `~/.cache/fff_mcp-<pid>.log` exists per process; no shared file.
- DrvFs sessions use PollWatcher; the `fff-watcher-own` thread is not in
  `R` state at idle.

After Phase 2 (axes B + C):
- Repeated rescans cannot pile up; even adversarial event volumes settle to
  a steady state within seconds.
- Bigram rebuild happens at most once per 30 s and only when the overlay
  actually grew.

After Phase 3 (axis E):
- One fff-mcp daemon process serves all Claude Code sessions for the user's
  monorepo. Kernel watch count drops from ~1.7 M to ~107 K. Total RSS drops
  by ~10–12 GB on the observed workload.

## Risk profile

| Axis | Risk | Likelihood | Mitigation |
|---|---|---|---|
| A | Wrong `f_type` magic ships → wrong backend chosen | Low | Verify on the user's box before coding (question 1). Unit test the detector. |
| A | Poll backend misses files briefly (10 s window) | Known trade | Documented; Claude Code search latency is dominated by other factors. |
| B | False suppression on legitimate big-batch events (git checkout) | Low | `need_rescan()` kernel flag and user-initiated rescans both bypass. |
| C | Concurrent rebuild vs grep races | Low | `parking_lot::RwLock` is write-preferring; overlay is the existing correctness safety net. |
| D | Operator's legit big repo gets walked, hits cap, fails to start | Low | 1 M default is well above 690 K observed. Operator can raise or set to 0. |
| E | Multi-tenant introduces a new failure surface | Medium | Behind `--multi-tenant`; default startup is unchanged. |
| F | 8 K watches too low for a single big repo | Medium | Bump to 32 K (question 4); axis A's poll backend bypasses watches entirely. |

## Adversarial review status

**Complete.** Full review at `.planning/PLAN-review.md`. Verdict: **approve
with edits.** Three blocking plan-text fixes (folded in below); six non-blocking
issues to handle during implementation; four extra open questions added.

## Review-resolution addendum

Folding the three blocking issues from the adversarial review into the plan so
implementation can start cleanly.

### Resolution to B1 — Poll-backend owner-thread behavior (axis A)

`PollWatcher` with an initial `RecursiveMode::Recursive` root registration
already scans the whole subtree. When the runtime owner thread receives a new
directory via `watch_tx`, **on the poll backend it must skip the
`debouncer.watch()` call entirely.** On the native (inotify) backend the call
remains as today (`debouncer.watch(&dir, RecursiveMode::NonRecursive)`).

Implementation: store the resolved `effective_backend` as a field on
`BackgroundWatcher` (next to `debouncer`, `watch_tx`, `owner_thread`). The
owner thread captures a clone of it. The match in the owner loop:

```rust
#[cfg(target_os = "linux")]
match effective_backend {
    WatcherBackend::Native => {
        let mut guard = owner_debouncer.lock();
        if let Some(d) = guard.as_mut() {
            if let Err(e) = d.watch(&dir, RecursiveMode::NonRecursive) {
                warn!(?e, dir = %dir.display(), "watch failed for new dir");
            }
        }
    }
    WatcherBackend::Poll => {
        // PollWatcher with Recursive root already covers this dir;
        // skip the no-op call. Still run track_files_from_new_directories below.
    }
    WatcherBackend::Auto => unreachable!("resolved by BackgroundWatcher::new"),
}
```

`track_files_from_new_directories` runs in both branches (it's how the picker
learns about new files between polls).

### Resolution to B2 — Initial project scans bypass rate limit (axes B + E)

When axis E's `ProjectRegistry::get_or_create` lazily initializes a new project,
the initial `FilePicker::new_with_shared_state` spawns a scan that, if axis B's
rate limit is in effect, could be wrongly suppressed.

**Decision:** initial scans from project registration are treated as
`user_initiated = true` and bypass the bucket. The user explicitly named the
project; the registry's scan is "user-driven" in the same sense as a
Neovim-initiated rescan.

Implementation: axis B's gate check (in `trigger_full_rescan_async`) examines
`user_initiated`; axis E passes `true` from the registry path. Document this
in both axis B's and axis E's sections.

### Resolution to B3 — Full-rescan vs incremental-rebuild race (axes B + C)

`set_bigram_index` (`file_picker.rs:1223-1228`) is called from both
`run_post_scan` (full rebuild) and the proposed incremental rebuild. Two
concurrent installs can race; whichever wins last persists, potentially a
stale partial result overwriting a fresh full result.

**Decision: install-by-generation pattern.** Add a `bigram_generation:
Arc<AtomicU64>` to `FilePicker`. `set_bigram_index` becomes
`set_bigram_index(index, generation_at_spawn)`:

```rust
pub fn set_bigram_index(&mut self, index: BigramFilter, gen_at_spawn: u64) -> bool {
    let current = self.bigram_generation.load(Ordering::Acquire);
    if gen_at_spawn < current {
        return false;  // a fresher install already happened; discard
    }
    self.bigram_index = Some(Arc::new(index));
    self.bigram_overlay = Arc::new(RwLock::new(BigramOverlay::default()));
    self.bigram_generation.fetch_add(1, Ordering::AcqRel);
    true
}
```

`spawn_incremental_rebuild` captures the generation at spawn time:

```rust
let gen_at_spawn = picker.bigram_generation.load(Ordering::Acquire);
// ... heavy CPU work ...
let mut guard = shared_picker.write()?;
let picker = guard.as_mut()?;
picker.set_bigram_index(new_index, gen_at_spawn);  // discarded if stale
```

`run_post_scan` does the same dance. The incremental rebuild's stale result
gets dropped silently; the next event will trigger another incremental rebuild
that runs on top of the fresher full-rebuilt base. ~10 LOC, no new dependency,
no waiting.

## Resolved open questions (my recommended answers)

The 14 open questions resolved to defaults so implementation can proceed.
Override any of these before approval and I'll re-do the affected axis.

| # | Question | Resolved to | Source |
|---|---|---|---|
| 1 | Confirm WSL1 DrvFs `f_type` is `0x53464846` | **Verify on box first** — I run a one-line check before writing code. | A.1 |
| 2 | `--max-indexable-files` default | **1 000 000** | D.1 |
| 3 | `--require-git-root` default | **Off in binary; on in MCP launch config** | D.3 |
| 4 | `--max-inotify-watches` default | **32 768** (axis A's poll backend covers DrvFs anyway) | F |
| 5 | `--max-tokio-workers` default | **4** | F |
| 6 | `OVERLAY_MERGE_THRESHOLD` default | **512** + tracing field for tuning | C |
| 7 | Worktree dedup in axis E | **Key by `.git/workdir()`** — this is the consolidation's point | E |
| 8 | Ship axis E at all? | **Yes**, Phase 3 only | E |
| 9 | Co-author tag on commits | **Yes** (per CLAUDE.md global rule) | global |
| 10 | PR strategy | **One PR per axis** (6 PRs total + Phase-3 prerequisite for E) | global |
| 11 | `f_type` cast type | **`as u32`** (not `u64` — avoids silent CIFS miss) | OQ-11 |
| 12 | Log migration UX | **`fff_mcp-latest.log` symlink** updated atomically at startup | OQ-12 |
| 13 | supergateway-rs in user's MCP config | **Verify before Phase 3** — not a blocker for Phase 1/2 | OQ-13 |
| 14 | Axis D report `default_value_t = 500_000` typo | **Implement as `1_000_000`** (matches PLAN.md text) | OQ-14 |

## Non-blocking issues from review (handle during implementation)

| Ref | Item | Action during implementation |
|---|---|---|
| NB1 | Semaphore on Ctrl-C teardown | Document existing behavior in F's PR description. No code change. |
| NB2 | `--healthcheck` skips `--require-git-root` gate | Extend `healthcheck.rs` to run git-root check when flag is set. |
| NB3 | JSON stderr line vs tracing output collision | Emit JSON error line **before** `init_tracing`; direct tracing fallback to stdout. |
| NB4 | `fff_restart_index` drops watcher_backend | Promote A.4 from "open question" to required code change: store backend on `FilePicker`, read in restart. |
| NB5 | Default timing interaction (poll 10 s + suppress 5 min) | Add `need_rescan()` bypass to PLAN.md risk row (done). |
| NB6 | `BACKGROUND_THREAD_POOL` is process-global | Note in axis E's PR: when Phase 3 lands, add `--max-bg-workers` follow-up. |

## Test-plan additions (from review §4)

These are now mandatory pre-merge tests for each axis:

- **A**: unit test `is_polling_preferred_fs` on tmpfs; integration test poll-backend file detection; smoke test `fff-watcher-own` not in `R` state on DrvFs.
- **B**: unit test bucket exhaustion + refill; integration test suppression log line.
- **C**: unit test `BigramOverlay::size()`; concurrency test for the B3 race (validates the generation-counter fix); regression test for grep correctness across rebuild.
- **D**: unit tests for `NO_GIT_ROOT` exit, `TOO_MANY_FILES` exit, distinct per-PID log paths.
- **F**: unit test grep semaphore enforcement; comment verifying `spawn_update_check` survives the `#[tokio::main]` removal.

## Updated cross-axis coordination

| Order | Why |
|---|---|
| A → F | F's per-dir cap goes into A's refactored `create_debouncer`; cannot write F's diff until A is on the branch. |
| D / F can land together | Both touch `main.rs:Args` and `FilePickerOptions`, both additive. Single PR is fine. |
| B before C | C's `spawn_incremental_rebuild` race window is smaller once B's rate limit is in effect. Even though structurally independent, B first reduces flakiness. |
| E after Phase 1+2 | E's registry uses B's rate limiter and C's incremental rebuild correctly only after both are in. |
