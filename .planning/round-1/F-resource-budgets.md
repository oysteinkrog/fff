# F. Per-Process Resource Budgets

## 1. Problem Statement

fff-mcp ships one tunable resource cap: `--max-cached-files` / `ContentCacheBudget`
(`types.rs:815-911`), which controls the in-memory mmap cache. Every other
axis of resource consumption — inotify watches, tokio worker count, RSS, and
concurrent in-flight grep calls — grows unbounded relative to the number of
running instances and the size of the indexed tree. On the observed 32-core /
64 GB box running 16 concurrent fff-mcp processes, the result is ~1.7 M kernel
inotify watches, 512 tokio worker threads plus ~256 rayon background workers,
and RSS values of 0.3–1.2 GB per process before the rescan loop begins eating
more. The two unbounded instances (`/c/users/oystein`, `/c/WORK/desktop`)
expose the complete absence of any headroom guard; adding resource budgets
gives the operator a knob to prevent one misbehaving process from starving the
others.

## 2. Code References

| Location | Role |
|---|---|
| `fff-mcp/src/main.rs:104-162` | `Args` — all existing CLI flags; `--max-cached-files` at line 156 |
| `fff-mcp/src/main.rs:194` | `#[tokio::main]` macro — hard-codes the default multi-thread runtime |
| `fff-mcp/src/main.rs:260-275` | `FilePicker::new_with_shared_state` call site — where `FilePickerOptions` is built |
| `fff-core/src/file_picker.rs:70-99` | `BACKGROUND_THREAD_POOL` (`LazyLock<rayon::ThreadPool>`) — sizes bg rayon workers at `num_cpus / 2` at first use |
| `fff-core/src/file_picker.rs:434-449` | `FilePickerOptions` struct — the natural place to carry new budget fields into the core |
| `fff-core/src/types.rs:815-911` | `ContentCacheBudget` — the only existing budget struct |
| `fff-core/src/background_watcher.rs:79` | `use_recursive` decision — Linux always takes `NonRecursive` (per-dir inotify watches) |
| `fff-core/src/background_watcher.rs:281-320` | Per-dir inotify registration loop; aborts after 16 consecutive `watch()` failures |
| `fff-core/src/background_watcher.rs:283-285` | `watched`, `consecutive_failures`, `aborted_early` locals |
| `fff-mcp/src/server.rs:182-198` | `FffServer` struct — no concurrent-call semaphore; all grep calls run freely |

## 3. Root Cause Analysis

Three independent assumptions break under the 16-instance workload.

**Thread count is a function of CPU count, not process count.** Both the tokio
runtime (`#[tokio::main]` at `main.rs:194`) and `BACKGROUND_THREAD_POOL`
(`file_picker.rs:73-99`) size themselves from `available_parallelism()` without
any knowledge of sibling processes. On a 32-core box: 32 tokio workers × 16
processes = 512 tokio threads; 16 bg-rayon workers × 16 processes = 256 rayon
workers; plus 8 fixed-name threads per process (`fff-scan`, `fff-bg-N` × 16,
`fff-scan-owner`, two notify threads, `fff-watcher-own`, `tracing-appender`) =
another ~256 threads. Total: ~1 024 OS threads for fff alone, competing with
ReSharper, Defender, .NET/MSBuild, and all the other DrvFs consumers identified
in the diagnosis.

**inotify watches accumulate without a process-level cap.** `background_watcher.rs:291-319`
registers one inotify watch per indexed directory. The only existing guard is
the ENOSPC abort at line 304: after 16 consecutive failures (which typically
means the kernel's `fs.inotify.max_user_watches` is exhausted), the loop breaks
and logs a warning. But that guard fires *after* the kernel resource is
exhausted system-wide, not before. With 107 K dirs × 16 processes = 1.7 M
desired watches against a typical system limit of 524 288, any single instance
that indexes a large repo will push the system past the cap.

**No concurrent grep limit.** `FffServer` (`server.rs:182`) holds no semaphore;
each incoming MCP tool call spawns a `tokio::task` that immediately acquires the
`SharedFilePicker` read lock and starts a rayon-parallel grep. On a 32-core box
under 16 Claude Code sessions all sending grep requests simultaneously, this is
512 concurrent rayon threads doing memory-bandwidth-saturating file I/O plus 16
concurrent bigram scans.

## 4. Proposed Changes

### What ships in v1 (three flags)

The five candidate axes from the task brief reduce to three actionable flags.
`--max-rss-mb` is **deferred** (see §4d). `--max-events-per-sec` is **deferred**
(see §4e).

---

#### 4a. `--max-inotify-watches N` / `FFF_MAX_INOTIFY_WATCHES`

**Default:** `8192`. Rationale: 8 K watches comfortably covers the typical
20–30 K directory repository while leaving 16 processes headroom within a
default 524 288 system cap (16 × 8 192 = 131 072, well under). For
Chromium-scale repos (107 K dirs) the operator sets this higher explicitly, or
relies on axis A's polling backend which does not use inotify watches at all.

**What the flag controls.** A new field `max_inotify_watches: usize` on
`FilePickerOptions` (`file_picker.rs:434`). Passed into `BackgroundWatcher::new`
and threaded to the per-dir registration loop at `background_watcher.rs:283`.

**Behavior at cap.** The existing `watched` counter (`background_watcher.rs:283`)
is already incremented per successful watch. Add a pre-check at the top of the
loop body:

```rust
// background_watcher.rs, inside the picker.for_each_dir closure:
if max_inotify_watches > 0 && watched >= max_inotify_watches {
    warn!(
        watched,
        max_inotify_watches,
        "inotify watch cap reached — remaining dirs will not be watched"
    );
    aborted_early = true;
    return ControlFlow::Break(());
}
```

This fires **before** any kernel call, so ENOSPC is never reached under normal
operation. The existing ENOSPC abort (`consecutive_failures >= 16`) remains as a
last-resort backstop. The `aborted_early` flag already propagates into the
`info!` log at line 322; extend it to a `warn!` with structured fields
`watched` and `max_inotify_watches` so `--healthcheck` can surface it.

**Interaction with ENOSPC.** The new cap is a soft ceiling set by the operator
before the process starts. ENOSPC is a hard ceiling imposed by the kernel after
the fact. The two guards are complementary: the new cap prevents ENOSPC rather
than recovering from it.

---

#### 4b. `--max-tokio-workers N` / `FFF_MAX_TOKIO_WORKERS`

**Default:** `4`. Rationale: fff-mcp's async work is almost entirely I/O-bound
waiting (MCP stdio reads, background-watcher channel receives, scan-completion
polling loop at `main.rs:286-301`). Four workers is generous. The hot CPU work
(bigram build, grep) runs on the rayon pools, not tokio workers. Cutting from
32 to 4 per process saves 28 × 16 = 448 OS threads with no throughput penalty.

**What changes.** Replace `#[tokio::main]` at `main.rs:194` with a manual
runtime builder:

```rust
// main.rs — replace #[tokio::main] async fn main() with:
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = Args::parse();
    resolve_defaults(&mut args);

    let worker_threads = args.max_tokio_workers.unwrap_or(4).max(1);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(worker_threads)
        .thread_name("fff-tokio")
        .enable_all()
        .build()?;
    rt.block_on(async_main(args))
}

async fn async_main(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    // existing body of main(), unchanged
}
```

The `Args` field:

```rust
/// Number of tokio worker threads. Defaults to 4. Lower values reduce
/// per-process OS thread count at the cost of async throughput.
/// Also settable via FFF_MAX_TOKIO_WORKERS.
#[arg(long = "max-tokio-workers", env = "FFF_MAX_TOKIO_WORKERS")]
max_tokio_workers: Option<usize>,
```

**Risk.** If any future code path puts CPU-bound work on the tokio executor
(currently none — grep and scan run on rayon), reducing to 4 workers would
create a bottleneck. The field is `Option<usize>` so `None` (no flag) falls
back to the new default of 4, and the operator can raise it if needed.

---

#### 4c. `--max-concurrent-greps N` / `FFF_MAX_CONCURRENT_GREPS`

**Default:** `4`. Rationale: each grep call saturates memory bandwidth across
all rayon background workers. Four concurrent greps per process × 16 processes
= 64 saturated greps, still a lot but bounded.

**What changes.** Add a `tokio::sync::Semaphore` to `FffServer`
(`server.rs:182`):

```rust
pub struct FffServer {
    picker: SharedFilePicker,
    frecency: SharedFrecency,
    cursor_store: Arc<Mutex<CursorStore>>,
    update_notice_sent: Arc<AtomicBool>,
    grep_semaphore: Arc<tokio::sync::Semaphore>,   // NEW
}
```

In `FffServer::new`, build it from the `max_concurrent_greps` arg (passed from
`main.rs`). In the `grep` and `multi_grep` tool handlers, acquire a permit
before calling `grep_search`:

```rust
// server.rs grep handler (before the blocking spawn):
let _permit = self.grep_semaphore.acquire().await
    .map_err(|_| McpError::internal("semaphore closed"))?;
// permit drops at end of handler, releasing the slot
```

The `find_files` handler does not need the semaphore because fuzzy search is
read-only and does not invoke rayon.

**No cap on in-flight tool calls.** The MCP protocol uses a single stdio
channel per process; the rmcp crate deserializes requests sequentially and
dispatches them as async tasks. The tokio worker limit (§4b) already caps how
many tasks run concurrently. An additional in-flight tool cap is redundant and
is **not added** in v1.

---

#### 4d. `--max-rss-mb` — deferred

RSS is not reliably readable from inside the process in a cross-platform way
on WSL1 (the `procfs` crate works on WSL2 but `/proc/self/status` VmRSS on
WSL1 lags). Axis D already provides `--max-indexable-files` which prevents the
most common cause of RSS growth (unbounded walks). Adding an RSS cap without
reliable measurement would be guesswork. Deferred to v2; the operator's
immediate defense is `--max-cached-files` (already shipped) + axis D's file
count guard.

#### 4e. `--max-events-per-sec` — deferred to axis B

This flag sits at the raw inotify event layer, one level below axis B's rescan
rate limit. Axis B (`background_watcher.rs:396`, `handle_debounced_events`)
already proposes a cooldown on how often a rescan is triggered. Adding a token
bucket on raw events (before debouncing) is a distinct mechanism, but its primary
benefit is reducing CPU in `handle_debounced_events` — which is axis B's
problem to solve. Implementing it here would duplicate axis B's work. The
correct layering: axis B owns the rescan trigger gate; if axis B's cooldown is
insufficient, axis F adds a token bucket on the raw event intake. This flag is
**reserved** for a follow-up PR on top of axis B.

## 5. Compatibility / Risk

- **`BACKGROUND_THREAD_POOL`** (`file_picker.rs:73-99`) is a `LazyLock` sized
  from `available_parallelism()` at first use. It is **not** changed by this
  axis. On a 32-core box it still creates 16 rayon workers per process. Reducing
  this pool is straightforward (add a `max_bg_rayon_workers` field to
  `FilePickerOptions` and pass it to `ThreadPoolBuilder::num_threads`) but is
  out of scope for this axis to avoid overlap with axis A/B work on the scanner.
- **Neovim plugin**: `FilePickerOptions` gains two new optional fields. Both
  default to `0` / `None` meaning "uncapped". The `fff-c` crate and Lua plugin
  are unaffected.
- **MCP protocol**: the grep semaphore introduces backpressure. If a caller
  sends more than `max_concurrent_greps` requests simultaneously, excess
  requests queue inside tokio and wait for a permit. They are not rejected.
  Timeouts are the caller's responsibility (Claude Code has its own tool-call
  timeout). This is correct behavior.
- **`#[tokio::main]` removal**: the macro expands to a `main()` + `block_on`
  wrapper. The manual replacement is identical in behavior; the only risk is
  forgetting `enable_all()` (which enables I/O and time drivers). The template
  above includes it.

## 6. Rollout Plan

Three independent diffs; each can land separately:

1. **`--max-inotify-watches`** — touches `main.rs` (new arg), `file_picker.rs`
   (new `FilePickerOptions` field), `background_watcher.rs` (pre-check in loop).
   ~40 LOC. No new dependency. Default on from day one.
2. **`--max-tokio-workers`** — touches `main.rs` only (replace macro, add arg,
   split `async_main`). ~30 LOC. No new dependency. Default on (4 workers).
3. **`--max-concurrent-greps`** — touches `main.rs` (new arg), `server.rs`
   (`FffServer` struct + handlers). ~30 LOC. `tokio::sync::Semaphore` is already
   in scope via `tokio` dep. Default on (4 concurrent greps).

Total estimated diff: ~100 LOC. All three flags are "default on" with
conservative values. No flag is needed to enable them; the operator raises the
caps if the defaults are too restrictive for their repo size.

## 7. Telemetry

Add structured tracing fields so `--healthcheck` can report live budget state:

- `max_inotify_watches`, `watches_registered`, `watches_headroom` — populated
  after the initial watch loop completes; stored as atomics on
  `BackgroundWatcher` or on a new `ResourceBudgetState` struct shared between
  watcher and healthcheck.
- `max_tokio_workers`, `tokio_worker_threads` (read from
  `tokio::runtime::Handle::current().metrics().num_workers()` — available since
  tokio 1.14).
- `max_concurrent_greps`, `grep_permits_available` (read from
  `semaphore.available_permits()`).

`healthcheck.rs` already prints a per-check table (`check()` / `warn()` helpers
at lines 4-8). Add a "Resource budgets" section that calls the above metrics and
emits `[!]` when any headroom is below 20%.

## 8. Boundary with Axes B and D

**Axis B** (rescan loop control) owns the rate at which `handle_debounced_events`
triggers `trigger_full_rescan_async`. The `--max-events-per-sec` token bucket
(deferred here) operates one layer below axis B: it governs the rate at which
raw debounced events are admitted into `handle_debounced_events` at all. The
two knobs compose: axis B reduces rescan frequency; axis F reduces the event
volume that axis B must process. Neither replaces the other.

**Axis D** (startup safety) owns `--max-indexable-files`, which caps how many
files enter `FileSync` during a walk (`file_picker.rs:1762` walk closure). Axis
F's budgets are orthogonal: they govern OS resources consumed *after* the file
list is built (inotify watches, thread counts, grep concurrency). A process
that is within axis D's file limit can still exhaust inotify watches if many
processes are running; axis F caps that. Conversely, axis D's file cap does not
reduce tokio thread count or grep concurrency.

## 9. Open Questions

1. **Default for `--max-inotify-watches`**: 8 192 is conservative. The observed
   repos have 107 K directories. With the default, ~99 % of directories will
   not be watched on large repos. Is this acceptable, or should the default be
   higher (e.g. 32 768) with a documented note that operators on 16-process
   workloads should lower it?

2. **`BACKGROUND_THREAD_POOL` cap**: the rayon pool is currently not capped by
   this axis. Should a `--max-bg-workers` flag be added here, or should it be
   addressed as part of axis A/B where the background scanning work lives?

3. **Semaphore behavior on shutdown**: when `stop_background_monitor()` is called
   (`main.rs:311-315`), pending grep waiters holding a semaphore permit will
   finish normally. Waiters blocked on `acquire()` will unblock when the tokio
   runtime drops the semaphore. Is this teardown ordering safe, or should
   `close()` be called on the semaphore explicitly before shutdown?

4. **Healthcheck metrics format**: should budget headroom be machine-readable
   JSON (one line per metric) or human-readable table rows? The existing
   `healthcheck.rs` is human-readable text. A `--healthcheck --json` flag would
   allow scripted monitoring; worth adding alongside the budget fields or
   deferred?
