# D. Startup Safety / Guardrails

## 1. Problem Statement

`main.rs:215-233` calls `Repository::discover(&base_path)` and, when that
returns `Err`, silently falls back to indexing the raw `cwd`. This means any
Claude Code session opened from `/c/users/oystein` (no git root) or
`/c/WORK/desktop` (umbrella over sibling worktrees, also no git root) causes
fff-mcp to walk the entire subtree: hundreds of gigabytes, millions of files,
sustained CPU spin and 12-14 GB VSZ per process. There is no file-count cap
that would have aborted the walk early, no machine-readable error emitted on
stderr when a bad root is detected, and no per-PID log path, so all 16
concurrent instances stomp on the same `~/.cache/fff_mcp.log` file causing
tracing-appender backpressure (last log entry 2.5 hours stale at observation
time).

## 2. Code References

| Location | Role |
|---|---|
| `fff-mcp/src/main.rs:177-185` | `resolve_defaults` — builds the shared default log path (`~/.cache/fff_mcp.log` on Linux) |
| `fff-mcp/src/main.rs:208-233` | `base_path` resolution: `current_dir()` + `Repository::discover` + silent fallback |
| `fff-mcp/src/main.rs:260-275` | `FilePicker::new_with_shared_state(...)` — first point where `Result::Err` could bubble back |
| `fff-core/src/file_picker.rs:696-730` | `FilePicker::new` — validates path exists and is not filesystem root, then returns `Ok(picker)` without a file-count check |
| `fff-core/src/file_picker.rs:734-786` | `FilePicker::new_with_shared_state` — spawns the background scan thread; no abort path from the caller after this returns `Ok` |
| `fff-core/src/file_picker.rs:1721-1902` | `FileSync::walk_filesystem` — parallel `ignore::WalkBuilder` walk; no early-exit once threshold is crossed; the only abort point visible is `cancelled` on `ScanSignals`, which is checked inside `ScanJob::run` at `scan.rs:175` but not by the walk closure itself |
| `fff-core/src/scan.rs:156-169` | `ScanJob::run` calls `walk_filesystem`; errors log-and-return but no external signal is sent |
| `fff-mcp/src/healthcheck.rs:40-55` | Existing `--healthcheck` flag; warns on missing git root but does NOT treat it as fatal |

## 3. Root Cause Analysis

Two independent assumptions break on the observed system:

**Assumption A — git discovery always succeeds or is harmless.** In a
mono-repo worktree workflow it nearly always does. But when Claude Code is
launched from a non-repo directory the fallback at `main.rs:226-232`
(`tracing::info!("No git repository found …"); base_path` unchanged) simply
proceeds. Nothing downstream has a size limit.

**Assumption B — the walk is bounded by .gitignore.** `walk_filesystem` at
line 1740-1746 sets `git_ignore(true)`, `git_exclude(true)`, and `ignore(true)`.
These filters only work when a `.git` directory is present for the walker to
read. For a non-git root the walker falls back to its built-in heuristics,
which are much weaker. The comment at line 1740 (`// this is a very important
guard for the user opening ~/ or other root non-git dir`) acknowledges the
problem but the only guard is `.hidden(!is_git_repo)` — not enough to
prevent walking 690 K+ files under a non-git parent of multiple worktrees.

**Log collision.** `resolve_defaults` at `main.rs:177-185` has no
process-specific component. All 16 instances compute the same string. The
tracing-appender serialises writes with a mutex and a bounded channel; under 16
concurrent writers the channel fills and tracing silently drops records rather
than blocking, which explains the stale timestamp.

## 4. Proposed Changes

### 4a. Per-PID default log path

**Before** (`main.rs:177-185`):
```rust
args.log_file = Some(if is_windows {
    format!("{}\\AppData\\Local\\fff_mcp.log", home)
} else {
    format!("{}/.cache/fff_mcp.log", home)
});
```

**After**:
```rust
let pid = std::process::id();
args.log_file = Some(if is_windows {
    format!("{}\\AppData\\Local\\fff_mcp-{pid}.log", home)
} else {
    format!("{}/.cache/fff_mcp-{pid}.log", home)
});
```

This is a one-line mechanical change. Log rotation is deferred — the files
accumulate until the user prunes them. A cron job or a `--log-rotate-keep N`
flag (keep the N most-recent PID logs) can be added later; it is not blocking.
The open question is whether the MCP host (Claude Code) exposes the log path
to the user; currently it does not, so stale per-PID files may go unnoticed.
Consider writing a symlink `fff_mcp-latest.log -> fff_mcp-<pid>.log` at
startup to give a stable "current" name for monitoring tools.

### 4b. `--require-git-root` flag

Add to `Args` in `main.rs`:

```rust
/// Refuse to start if the resolved base path is not a git working directory.
/// Recommended for MCP server use to prevent accidental indexing of home
/// directories or umbrella directories.
#[arg(long = "require-git-root", env = "FFF_REQUIRE_GIT_ROOT")]
require_git_root: bool,
```

After `main.rs:215-233` (the `Repository::discover` block), insert:

```rust
if args.require_git_root
    && Repository::discover(&base_path)
        .ok()
        .and_then(|r| r.workdir().map(Path::to_path_buf))
        .is_none()
{
    let msg = StartupError {
        code: "NO_GIT_ROOT",
        message: format!(
            "fff-mcp: refusing to start — no git repository found at or above '{}'. \
             Pass --no-require-git-root to index without a git root.",
            base_path,
        ),
        base_path: base_path.clone(),
    };
    eprintln!("{}", serde_json::to_string(&msg)?);
    std::process::exit(1);
}
```

`StartupError` is a small `#[derive(Serialize)]` struct (see §4e). Default for
this flag: **off** in the binary, **on** in the MCP launch wrapper / Claude
Code MCP config (see §6).

### 4c. `--max-indexable-files` flag with early-abort walk

Add to `Args`:

```rust
/// Refuse to start if the walk discovers more than this many files.
/// Default 500 000. Set to 0 to disable. Also settable via
/// FFF_MAX_INDEXABLE_FILES.
#[arg(long = "max-indexable-files", env = "FFF_MAX_INDEXABLE_FILES",
      default_value_t = 500_000)]
max_indexable_files: usize,
```

**Default threshold rationale.** The observed normal worktrees contain 690 K
files. That means 500 K would fire on the normal case too — the right default
for the guard is therefore higher: **1 000 000** files. At that point the
process is clearly walking a non-project tree. For contexts where the operator
knows the repo is large (Chromium-scale), they set `--max-indexable-files 0`
or `2000000` explicitly. 500 K is a reasonable per-session soft limit for
most projects; 1 M is the hard upper bound before the guardrail fires by
default.

**Early-abort feasibility.** `walk_filesystem` (`file_picker.rs:1762-1794`)
uses `ignore::WalkBuilder::build_parallel()` whose closure returns
`ignore::WalkState`. The walker already checks `WalkState::Continue` vs
`WalkState::Quit` (`WalkState::Quit` stops all threads immediately). The
counter `synced_files_count` (`Arc<AtomicUsize>`) is incremented per-file at
line 1790. The closure already has a clone of this `Arc`, so the check is
zero-cost:

```rust
// inside the walk closure, after counter.fetch_add(1, Ordering::Relaxed):
let limit = max_indexable_files; // captured by value
if limit > 0 && counter.load(Ordering::Relaxed) > limit {
    return ignore::WalkState::Quit;
}
```

`WalkState::Quit` aborts the parallel walk immediately — no surgery to
`walk_filesystem`'s return type is needed. After the walk, check the counter:

```rust
// in ScanJob::run (scan.rs:156-169), after walk_filesystem returns:
let final_count = scanned_files_counter.load(Ordering::Relaxed);
if max_indexable_files > 0 && final_count > max_indexable_files {
    // emit structured error and propagate Err
    return Err(Error::TooManyFiles { count: final_count, limit: max_indexable_files });
}
```

The `Error` enum in `fff-core/src/error.rs` needs one new variant:
`TooManyFiles { count: usize, limit: usize }`.

The propagation path is: `walk_filesystem` returns a truncated `FileSync` →
`ScanJob::run` detects the overflow → calls a new
`ScanSignals::abort_with_error(&str)` helper that stores a `String` in
`Arc<Mutex<Option<String>>>` on `ScanSignals` → `main.rs` reads it during the
"wait for initial scan" polling loop (`main.rs:286-301`) and exits non-zero.

Alternatively (simpler): pass `max_indexable_files` down to
`FilePicker::new_with_shared_state` via a new field on `FilePickerOptions`,
and promote the walk-abort to a hard `Err` from `walk_filesystem` directly.
This avoids touching `ScanSignals` and keeps the error on the synchronous call
path. The downside is that `FilePicker::new` and
`FilePicker::new_with_shared_state` both need the new field.

**Preferred design**: add `max_indexable_files: usize` to `FilePickerOptions`
(`file_picker.rs:434-449`). Pass it into `FileSync::walk_filesystem`. When the
counter exceeds the limit, return `Err(Error::TooManyFiles{…})` immediately.
`FilePicker::new_with_shared_state` already maps walk errors to `Err` at
`scan.rs:165`. That `Err` propagates to `main.rs:275` and exits with the error
message below.

### 4d. `--root-must-be-under <path>` allowlist

```rust
/// Only allow indexing if the resolved base path is a descendant of one of
/// these prefixes. Repeatable. Example: --root-must-be-under /c/WORK
#[arg(long = "root-must-be-under")]
root_must_be_under: Vec<String>,
```

Check after base_path is fully resolved (`main.rs:233`, after the
`Repository::discover` block):

```rust
if !args.root_must_be_under.is_empty() {
    let allowed = args.root_must_be_under.iter()
        .any(|prefix| Path::new(&base_path).starts_with(prefix));
    if !allowed {
        emit_startup_error("ROOT_NOT_ALLOWED", &base_path, ...);
        std::process::exit(1);
    }
}
```

This flag is entirely optional; the expected usage is in the MCP config:
`"--root-must-be-under", "/c/WORK"` to ensure no session launched outside the
work tree ever indexes the home directory, even if `--require-git-root` is
somehow bypassed.

### 4e. Structured stderr error format

Every hard-failure exit emits one JSON line on stderr before `std::process::exit(1)`:

```json
{"event":"fff_startup_error","code":"TOO_MANY_FILES","base_path":"/c/users/oystein","detail":"walk found 1432891 files, limit is 1000000"}
{"event":"fff_startup_error","code":"NO_GIT_ROOT","base_path":"/c/WORK/desktop","detail":"no git repository found"}
{"event":"fff_startup_error","code":"ROOT_NOT_ALLOWED","base_path":"/c/tmp","detail":"not under any allowed prefix"}
```

The MCP host (Claude Code) captures stderr on process failure and can display
`detail` directly. A human-readable version of the same message goes to stdout
as well (for terminal use). The `code` field is a stable machine identifier;
the `detail` field is for display only and may change.

### 4f. `--validate-root` subcommand

This reuses the existing `--healthcheck` infrastructure in
`fff-mcp/src/healthcheck.rs`. A new `--validate-root` flag (or subcommand)
performs only the cheap pre-scan checks:

1. `base_path` exists and is a directory — already in `healthcheck.rs:28-37`
2. `Repository::discover` succeeds (if `--require-git-root`) — already in
   `healthcheck.rs:40-55`, but currently only warns; make it `Err` when
   `require_git_root` is set
3. `base_path` passes the allowlist (if `--root-must-be-under`) — new check
4. Estimate file count via a short-circuit walk capped at
   `max_indexable_files * 2` — new check using the same `WalkState::Quit`
   mechanism, terminating in at most a few seconds rather than full 37 s

Exit 0 if all checks pass, exit 1 with a JSON line (§4e format) if any fail.
A wrapper script can call `fff-mcp --validate-root [all other flags]` before
the real launch and abort cleanly if the exit code is non-zero.

## 5. Compatibility / Risk

- **Happy path performance**: the file-count check inside the walk closure is
  one atomic load per file — unmeasurable overhead on the 690 K normal case.
- **Non-git projects**: `--require-git-root` defaults off; non-git users are
  unaffected unless they opt in.
- **Neovim plugin**: `fff-c` and `lua/` do not pass CLI flags; they call the
  Rust API directly via `FilePicker::new_with_shared_state`. The new
  `max_indexable_files` field on `FilePickerOptions` defaults to `0` (disabled)
  so existing callers are unaffected.
- **MCP protocol**: the change is pre-MCP handshake — the server never starts,
  so no protocol compatibility concern.
- **Log file naming change**: any monitoring script that tails
  `~/.cache/fff_mcp.log` breaks. This is a one-time migration; document in
  release notes.
- **New dependency**: `serde_json` is likely already a transitive dependency
  (via rmcp or tracing-subscriber JSON format); verify before adding it
  explicitly to `fff-mcp/Cargo.toml`.

## 6. Rollout Plan

All four guardrails can be merged as a single diff because they are all
additive (new CLI flags, new `FilePickerOptions` field, new `Error` variant,
changed default log path). No existing behavior changes unless the new flags
are passed.

**Phase 1 (flag-gated, opt-in):** Land the diff. Default for
`--require-git-root` is off. Default for `--max-indexable-files` is 1 000 000
(fires only on pathological cases). Default log path gains the PID suffix.
Estimated diff size: ~150-200 LOC across `main.rs`, `healthcheck.rs`,
`file_picker.rs` (FilePickerOptions), `scan.rs` (walk closure +
early-return), and `error.rs`.

**Phase 2 (MCP config):** Update the Claude Code MCP configuration
(`~/.claude.json` or project `.mcp.json`) to pass
`--require-git-root --max-indexable-files 1000000`. This is a configuration
change, not a code change.

**Phase 3 (make default on):** After one release cycle with no regressions,
flip `require_git_root` default to `true` for `FFFMode::Ai`. Non-AI users
(Neovim plugin) use the API directly and are not affected.

The changes to axes A, B, C are independent; this axis D can ship first.

## 7. Open Questions

1. **Threshold for `--max-indexable-files`**: 1 000 000 is proposed. The
   observed normal worktrees are 690 K. Should the default be lowered to
   750 000 (catches the `desktop` umbrella case at ~16 × 690 K ≈ 11 M) or
   kept at 1 M to avoid false positives on legitimately large repos?

2. **Log rotation**: per-PID files accumulate. Should fff-mcp prune files
   older than N days at startup? Or should the operator handle this with
   `logrotate`? The latter is simpler but requires user setup.

3. **`--validate-root` as a flag vs subcommand**: the existing `--healthcheck`
   is a flag (`args.healthcheck`). Consistency suggests `--validate-root` is
   also a flag. But a subcommand (`fff-mcp validate-root`) is more shell-script
   friendly. Decide before implementation.

4. **`ScanSignals` error propagation vs synchronous `Err` from
   `walk_filesystem`**: the synchronous path (abort `walk_filesystem` → `Err`
   → surface at `main.rs:275`) is simpler but means the MCP server never
   starts accepting connections. The async path (abort mid-walk → post error to
   a shared atomic → poll in startup loop) would allow the server to emit an
   MCP-level error response to the first tool call instead of dying. The
   synchronous path is strongly preferred for a guardrail.

5. **Symlink for "latest" log**: `fff_mcp-latest.log → fff_mcp-<pid>.log` is
   useful for `tail -f` but requires an atomic symlink swap and cleanup on
   exit. Is this worth the complexity or should the operator use
   `ls -t ~/.cache/fff_mcp-*.log | head -1` instead?
