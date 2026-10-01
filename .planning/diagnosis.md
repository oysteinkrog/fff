# fff-mcp CPU diagnosis (shared brief for planning agents)

Source repo (cloned for analysis): `/c/work/fff.nvim`
Fork to modify: `oysteinkrog/fff` (remote name: `oysteinkrog`)
Upstream: `dmtrKovalenko/fff` (remote name: `origin`)

## Observed symptom

On a 32-core / 64 GB WSL1 (Ubuntu-on-Windows) box, the user runs ~16 concurrent
Claude Code sessions, each in its own tty, each launching its own `fff-mcp`
(stdio MCP server) rooted at the session's cwd. After ~3 hours of uptime:

- 16 fff-mcp processes alive, one per tty
- Cumulative CPU per process: 70-90% sustained (one had 149 min CPU / 210 min wall)
- Top thread in `top -H`: `fff-watcher-own` at 99.9% CPU
- 53 % system CPU, 43 % user CPU when sampled
- Memory: 16 × 0.3-1.2 GB RSS, several with 12-14 GB VSZ
- System swap grew from 0 to 13 GB during the session

## Working directories indexed

The user runs Claude from per-worktree directories. Two of the 16 instances
are anomalous (no git root → indexes cwd unbounded):

```
PID 15443 /c/WORK/desktop          ← umbrella over all 16 worktrees, no git root here
PID 28274 /c/users/oystein         ← whole Windows home dir, no git root
```

The rest are normal per-worktree, each ~690 K files / ~107 K directories per
worktree (the Initial Force Desktop monorepo is Chromium-scale).

## Filesystem context

- Mount: `C:\ on /c type drvfs (rw,noatime,uid=1000,gid=1000,metadata,case=off)`
- WSL1 / DrvFs surfaces Windows-side filesystem activity as inotify events on
  the Linux side
- Sources of Windows-side activity that show up as inotify events:
  Defender real-time scan, Windows Search indexer, OneDrive, .NET/MSBuild
  watch tasks, ReSharper indexing, antivirus, Spotlight-equivalents
- inotify on DrvFs is functional but **noisy**: lots of spurious / non-content
  events. There is no kernel-level filter for "only modifications".

## Self-perpetuating loop seen in logs

From `~/.cache/fff_mcp.log`:

```
10:45:55  add_new_file ...Help.feature.cs                 (~50 of these in 2s)
10:45:57  Event processing summary: 7 to remove, ... overflow_count=2163
10:45:57  Watcher faced limit of index overflow. Triggering rescan
10:45:57  Full rescan requested while another scan is active — deferred
10:46:32  run_post_scan close time.busy=74.6s
10:46:32  Rescheduling deferred rescan after current scan finished
10:47:10  SCAN: File walking completed in 37.3s for 690505 files
10:48:29  run_post_scan close time.busy=77.6s
```

Sequence each iteration:

1. notify-rs (v9.0.0-rc.4) + notify_debouncer_full ticks every 25 ms
   (DEBOUNCE_TIMEOUT = 50 ms, tick = TIMEOUT/2). See
   `crates/fff-core/src/background_watcher.rs:27`.
2. DrvFs delivers Windows-side touches as inotify events.
3. `handle_debounced_events` (`background_watcher.rs:396`) walks events;
   for each path it calls libgit2 `repo.is_path_ignored()` — a slow DrvFs
   `.gitignore` lookup.
4. Picker's overflow index trips (`overflow_count > threshold`) →
   `Watcher faced limit of index overflow. Triggering rescan` triggers
   `trigger_full_rescan_async`.
5. The rescan walks 690 K files (~37 s), then `run_post_scan`
   (`scan.rs:285-319`) rebuilds the **bigram inverted index from scratch**
   over all indexable files (~77 s).
6. While that runs, a second rescan is queued via `signals.rescan_pending`
   (`scan.rs:259-277`).
7. DrvFs keeps delivering events; goto 4. Steady-state spin.

## Per-process architecture (current)

- `main.rs:208-213` — base_path defaults to `std::env::current_dir()`,
  rebased via `Repository::discover(&base_path)` to the git workdir.
  Fallback: indexes the bare cwd if no repo is found.
- Single SharedFilePicker (Arc<RwLock<Option<FilePicker>>>) per process.
- Tokio multi-thread runtime (default ~32 workers on 32-core box).
- Per-process: 1 fff-scan thread, 16 fff-bg-N background workers, 1
  fff-scan-owner, 1 notify-rs inotify, 1 notify-rs debouncer, 1
  fff-watcher-own, 1 tracing-appender, plus tokio-rt-workers.
  → ~54-86 threads per fff-mcp.
- Per-dir NonRecursive inotify watches on Linux
  (`background_watcher.rs:240` comment): one inotify watch per indexed
  directory. For 107 K dirs × 16 processes = ~1.7 M kernel watches in
  steady state.
- MCP tools (`fff-mcp/src/server.rs:85-179`) take no path/project param —
  base_path is process-level state, baked at startup.

## Default log file (footgun)

`fff-mcp/src/main.rs:178-185`: every instance defaults its log file to
`~/.cache/fff_mcp.log` (or `%LOCALAPPDATA%\fff_mcp.log` on Windows).
16 processes append to the same file. Currently 13 MB; appender backpressure
visible (last log entry 2.5 hours stale).

## Thread-safety primitives already present

`crates/fff-core/src/lib.rs:23` documents that `SharedFilePicker`,
`SharedFrecency`, `SharedQueryTracker` are `Arc<RwLock<Option<T>>>` wrappers
designed for concurrent access. Multiple concurrent MCP grep/find calls
already share the read lock and run in parallel without contention.

## Distinct problem axes (each agent will own one)

A. **Watcher backend selection** — RecommendedWatcher is hard-coded to
   inotify on Linux even when the path is DrvFs/9p/CIFS where polling
   would be cheaper.

B. **Rescan loop control** — overflow → full rescan has no cooldown / no
   backoff / no rate limit. Steady-state spin is possible.

C. **Bigram index rebuild cost** — `build_bigram_index` in `scan.rs:306`
   rebuilds from scratch on every rescan. No incremental path.

D. **Startup safety / guard rails** — `Repository::discover` fallback
   indexes the bare cwd; nothing prevents `cwd=/c/users/oystein` or
   `cwd=/c/WORK/desktop` from scanning hundreds of GB. No max-files cap.
   Shared default log file. No machine-readable error on a bad root.

E. **Multi-tenant single daemon** — currently 1 process per repo.
   Architecture could move to one daemon with `HashMap<PathBuf, FilePicker>`
   keyed by repo root, lazy-init on first MCP call. Tools would gain a
   `project_path` field. Reuses existing SharedFilePicker primitives.

F. **Per-process resource budgets** — no CLI knob for max watches, max
   events/sec, max indexable files, max RSS. The two unbounded instances
   (15443, 28274) exposed the absence of any guardrail.

G. **Adversarial review** — what could each proposed change break?
   Regressions, edge cases, neovim plugin compatibility, MCP protocol
   compatibility, behavior under cancel/shutdown, performance regressions
   on the happy path (small Linux repos, native ext4).

## Constraints for any proposal

- Must remain a drop-in MCP server for Claude Code (stdio transport,
  same tool names: find_files / grep / multi_grep).
- Must not break the Neovim plugin (`lua/` directory + `fff-c` crate).
- Must keep the existing `Shared*` primitive design — don't redesign
  the core for the sake of a fix.
- Patches will be carried on the `oysteinkrog/fff` fork (default branch
  to be created on top of upstream `main`). Aim for changes that are
  clean enough to upstream when ready.
- Author: Øystein Krog. Co-author tag for AI assistance is acceptable.

## What each agent produces

A single markdown file at
`/c/work/fff.nvim/.planning/round-1/<letter>-<topic>.md` containing:

1. **Problem statement** — restate the specific issue in 3-5 sentences.
2. **Code references** — file:line citations for the relevant existing
   code. Read it; don't speculate.
3. **Root cause analysis** — what specifically causes the symptom; what
   assumption in the current code breaks under the user's environment.
4. **Proposed change** — concrete design: types/functions touched, new
   CLI flags, behavior change. Include before/after pseudocode for the
   hot path.
5. **Compatibility / risk** — what could regress; what tests are needed;
   does this require a new dependency.
6. **Rollout plan** — can it ship behind a flag; default on or off; can
   it land independently of the other axes; estimated diff size in LOC.
7. **Open questions** — anything you'd want Oystein to decide before
   implementation.

Length budget: 600-1200 words per agent. Be concrete, cite line numbers,
and write the proposal at the level where another engineer could
implement it from your report without re-reading the codebase.
