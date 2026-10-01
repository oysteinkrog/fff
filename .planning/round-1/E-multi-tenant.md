# Axis E — Multi-tenant single daemon

## 1. Problem statement

Currently every Claude Code session that opens `fff-mcp` spawns a dedicated
process via stdio MCP. With 16 sessions rooted at sibling worktrees of the
same monorepo, 16 independent processes each scan ~690 K files, each build a
bigram index, each maintain 107 K inotify watches, and each open their own
LMDB environments. The resulting steady-state is ~1.7 M kernel inotify
watches, 16 × 0.3–1.2 GB RSS, and 16 parallel watcher/rescan loops that
contend for DrvFs I/O concurrently (`diagnosis.md` §"Observed symptom").

The `SharedFilePicker` primitive (`crates/fff-core/src/shared.rs:57`) is
already an `Arc<SharedPickerInner>` whose inner `parking_lot::RwLock` lets
many concurrent readers share a single `FilePicker` with no copies. What is
missing is a registry that maps repo roots to `SharedFilePicker` instances
and a way for an MCP tool call to name the project it is querying.

## 2. Code references

| Location | Relevant detail |
|---|---|
| `crates/fff-mcp/src/main.rs:208–233` | Single `base_path` resolved at startup; `Repository::discover` fallback |
| `crates/fff-mcp/src/main.rs:260–275` | `FilePicker::new_with_shared_state` — single picker created once |
| `crates/fff-mcp/src/main.rs:303–306` | `server.serve(stdio())` — single stdio transport |
| `crates/fff-mcp/src/server.rs:182–188` | `FffServer` struct — holds one `SharedFilePicker` and one `SharedFrecency` |
| `crates/fff-mcp/src/server.rs:85–179` | `FindFilesParams`, `GrepParams`, `MultiGrepParams` — no project field today |
| `crates/fff-core/src/shared.rs:57–106` | `SharedFilePicker` — `Arc<SharedPickerInner>` clone-able handle |
| `crates/fff-core/src/shared.rs:257–333` | `SharedFrecency` — separate `Arc<RwLock<Option<FrecencyTracker>>>` |
| `crates/fff-core/src/file_picker.rs:102–106` | `FFFMode::Ai` flag used at picker construction |
| `crates/fff-c/src/lib.rs:44–51` | `FffInstance` — plugin embeds core via FFI, independent of MCP path |

## 3. Root cause analysis

The architecture assumes 1 MCP process = 1 indexed repo. That was fine when a
developer ran one session; it breaks at 16 sessions on a Chromium-scale repo.
The `SharedFilePicker` abstraction was designed for *intra-process* sharing
(multiple Tokio tasks reading a picker concurrently); it is not wired for
*inter-session* sharing. Nothing in the MCP protocol itself prevents a single
process from serving multiple clients: HTTP/SSE-based MCP transports allow
many clients to connect to one daemon, each sending tool calls that carry
project identity. The gap is purely in fff-mcp.

## 4. Proposed change

### 4a. New CLI flag and startup mode

Add `--multi-tenant` (`-M`) to `Args` (`main.rs:106–162`):

```rust
/// Run as a multi-tenant HTTP daemon. The optional PATH argument becomes the
/// allowed root prefix; all project_path values in tool calls must be
/// descendants of it.
#[arg(long = "multi-tenant", short = 'M')]
multi_tenant: bool,

/// HTTP listen address for multi-tenant mode (default 127.0.0.1:9876).
#[arg(long = "listen", default_value = "127.0.0.1:9876")]
listen_addr: String,
```

When `--multi-tenant` is present:
- `base_path` becomes an **allowed root prefix** validated at registration time,
  not an indexed path. If omitted, any canonical path is accepted (unsafe for
  shared hosts; fine for Oystein's single-user laptop).
- The process binds an HTTP listener instead of reading stdio.

### 4b. Project registry

New module `crates/fff-mcp/src/registry.rs`:

```rust
pub struct ProjectEntry {
    pub picker: SharedFilePicker,
    pub frecency: SharedFrecency,
    pub last_access: Instant,     // for LRU eviction
}

pub struct ProjectRegistry {
    inner: Arc<RwLock<HashMap<PathBuf, ProjectEntry>>>,
    allowed_prefix: Option<PathBuf>,  // from --multi-tenant PATH
    max_projects: usize,              // CLI flag, default 12
    evict_after: Duration,            // CLI flag, default 30 min
}
```

`ProjectRegistry::get_or_create(path: &Path)`:
1. Canonicalize `path` with `std::fs::canonicalize`.
2. If `allowed_prefix` is set, reject any path not starting with it (return
   `ErrorData::invalid_params`).
3. Read-lock the map and return the existing entry if present (common case,
   no writer contention).
4. Write-lock; check again (double-checked locking). If at capacity, evict
   the LRU entry by calling `picker.write()?.as_mut()?.stop_background_monitor()`
   then removing the map entry. If all N entries have been accessed recently and
   the cap is hit, return an error rather than evicting — evicting a live
   project mid-query is confusing. Default cap: **12** (one fewer than the user's
   16 sessions, so cold-start doesn't immediately evict; users can tune via
   `--max-projects`).
5. Create `SharedFilePicker::default()`, `SharedFrecency::default()`, call
   `FilePicker::new_with_shared_state(...)` (`main.rs:260–275` logic extracted
   into a helper), insert the entry.

**Eviction note:** the watcher for a project must keep running between queries.
The per-project `BackgroundWatcher` is owned by `FilePicker`, which is owned
by the `SharedFilePicker` entry. As long as the `ProjectEntry` lives, the
watcher lives. Eviction = drop the `ProjectEntry` after calling
`stop_background_monitor()`. A project that goes stale for 30+ minutes has
likely been closed by the user; evicting it frees watches and RAM.

### 4c. Tool param extension

Each of `FindFilesParams`, `GrepParams`, `MultiGrepParams` in `server.rs:84–179`
gains one optional field:

```rust
/// Absolute path to the project root. Required in multi-tenant mode;
/// ignored in single-tenant mode (defaults to the process base_path).
#[serde(rename = "projectPath")]
pub project_path: Option<String>,
```

`FffServer` grows a field:

```rust
registry: Option<Arc<ProjectRegistry>>,  // None = single-tenant mode
default_picker: Option<SharedFilePicker>, // the single-tenant picker
```

At the top of every tool handler, a `resolve_picker(project_path: Option<&str>)` helper:

```rust
fn resolve_picker(&self, project_path: Option<&str>) -> Result<SharedFilePicker, ErrorData> {
    match (&self.registry, project_path) {
        (Some(reg), Some(path)) => reg.get_or_create(Path::new(path)),
        (Some(_), None) => Err(ErrorData::invalid_params(
            "multi-tenant mode requires projectPath in every tool call", None)),
        (None, _) => Ok(self.default_picker.clone().unwrap()),
    }
}
```

**Backward-compat choice: error when absent in multi-tenant mode, not silent
fallback.** Rationale: a silent fallback to the last-used project would silently
return results from the wrong repo when an LLM omits the field. Failing loudly
makes the missing field obvious immediately. The error message should name the
expected field and give an example so the LLM can self-correct on the next call.

### 4d. HTTP transport

**Recommendation: keep using supergateway-rs, do not grow native HTTP/SSE.**

Adding an HTTP/SSE server means adding a dependency (axum or warp), managing
keep-alive, authentication tokens, TLS configuration, and CORS — all
infrastructure supergateway already handles correctly. supergateway-rs is
already running for the user's other MCPs; the marginal cost of routing
fff-mcp through it as well is near zero. The stdio→HTTP wrapping adds ~1 ms
per call, which is negligible against the dozens-of-milliseconds search
latency. The only scenario where native HTTP wins is if fff-mcp needs to
multiplex thousands of concurrent connections; for 16 sessions that is not
the case.

If a future version wants native HTTP, `rmcp` supports streamable-HTTP
transport (it is in the crate's feature matrix). That path would be: add
`rmcp = { features = ["transport-sse-server"] }` and replace `stdio()` with
`StreamableHttpServerTransport::new(listener)`. This can be done independently
of the registry work above.

### 4e. Frecency / LMDB per-project

Each `ProjectEntry` holds its own `SharedFrecency` pointing to a
per-project LMDB env. This is correct: frecency data is repo-specific
(which files you recently opened in repo A tells you nothing about repo B).

LMDB limitation: the default per-process open-file limit on Linux is 1024 fd.
Each LMDB env holds 2 fds (data + lock). With 12 projects that is 24 fds for
LMDB, well within budget. The existing 16-process model already has 16 LMDB
envs; the daemon with 12 projects has fewer. No ulimit change needed.

### 4f. Interaction with the Neovim plugin (fff-c)

`crates/fff-c/src/lib.rs:44–51` shows `FffInstance` — the plugin creates its
own picker via C FFI (`fff_create_instance`), bypassing MCP entirely. It has
its own base_path, its own LMDB, its own watcher. Multi-tenant mode in
`fff-mcp` is completely orthogonal to this path: nothing in `fff-c` changes,
no FFI signatures change. If a user runs both the Neovim plugin and a
multi-tenant daemon, they get one plugin picker + N daemon pickers, each
independent. No regression.

## 5. Compatibility and risk

| Risk | Mitigation |
|---|---|
| LLM omits `projectPath` | Return a clear error with example — LLMs recover on next call |
| Concurrent lazy-init race | Double-checked locking under write guard in `get_or_create` |
| Evicting a picker mid-query | `stop_background_monitor()` only; ongoing read guards unaffected (parking_lot RwLock) |
| LMDB open-file limit | 12 projects × 2 fds = 24; far below the 1024 fd default |
| Bigram index memory × N | Each project still builds its own bigram index. For 12 copies of the same 690 K file tree this is 12× the RAM. The user must configure `--max-projects` and `--max-cached-files` lower than the current per-process defaults. |
| Backward compat (stdio clients) | Single-tenant `--no-multi-tenant` path must remain the default startup mode |

## 6. Rollout plan

- **Phase 1 (flag-gated):** add `--multi-tenant` flag, `registry.rs`, param extension, `resolve_picker`. All behind the flag; default startup is unchanged. Estimated diff: ~400 LOC (registry.rs ~150, server.rs delta ~100, main.rs delta ~150).
- **Phase 2:** expose `--max-projects`, `--evict-after`, `--listen` in `Args`. Add eviction background task (Tokio interval timer, 60 s tick).
- **Phase 3:** update the supergateway-rs integration docs and the user's `.claude.json` to point 16 sessions at one daemon endpoint.

Can land fully independently of axes A–D and F. If axis F (per-process resource budgets) lands first, its `--max-files` cap should be respected by each per-project picker inside the daemon as well — no conflict.

## 7. Is this worth doing?

**Probably not as a first move.** The core pathology (diagnosis §"Self-perpetuating loop") is the watcher rescan spin on DrvFs. Whether there are 16 processes or 1 daemon with 16 projects, the bigram rebuild and DrvFs inotify storms occur equally — they are just consolidated into one process. Multi-tenant does not fix any of axes A–C; it trades 16 × RSS for 1 × N × RSS while adding complexity.

The correct sequencing is:
1. **Axes A + B first** (watcher backend + rescan cooldown) — stops the spin.
2. **Axis C** (incremental bigram) — cuts the 77 s post-scan cost.
3. **Axis D** (startup guard rails) — prevents the two unbounded instances.
4. **Multi-tenant after** — once per-project cost is acceptable, consolidation into one daemon becomes a genuine win for memory and kernel watches.

If axes A–C land but users still run 16+ sessions, a multi-tenant daemon reduces kernel watches from 16 × 107 K = 1.7 M to ~107 K for sibling worktrees (they share the same git root, so they map to the same project entry). That is the scenario where this axis pays for itself.

## 8. Open questions

1. **Project deduplication for worktrees.** Sibling worktrees share a `.git` (via `.git` file pointing to the main repo's `worktrees/` directory). `Repository::discover` resolves to the same `workdir()` for each of them. Should the registry key on `workdir()` (deduplicate all worktrees) or on the supplied `project_path` (one entry per worktree)? Deduplication saves memory but means all worktrees share one file index, which may miss worktree-specific files. Oystein should decide.
2. **Default `--max-projects`.** 12 is a guess. Oystein's typical working set is 16 sessions on 12 sibling worktrees. If worktrees are deduplicated (question 1), 12 projects would cover them all; if not, the cap would evict the 13th+ session.
3. **Auth for the HTTP endpoint.** The daemon binds to `127.0.0.1` so localhost-only. Is that sufficient, or should it accept a bearer token to prevent other local processes from querying it?
4. **Frecency DB placement.** Currently `--frecency-db` is an explicit path. In multi-tenant mode, where should per-project frecency DBs live? Proposal: `~/.cache/fff/<canonical-hash-of-path>/frecency/`. Oystein to confirm.
