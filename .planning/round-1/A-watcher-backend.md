# A — Watcher backend selection

## Problem statement

`BackgroundWatcher` unconditionally uses `notify::RecommendedWatcher`, which
resolves to inotify on Linux. On WSL DrvFs (`C:\` mounted at `/c`), inotify is
functional but relays all Windows-side filesystem activity — Defender,
OneDrive, the Search indexer, MSBuild watchers, ReSharper — as synthetic
inotify events. This creates a continuous event storm. Per the diagnosis, a
single session sustains 2163 debounce overflows per 2-second window, each
triggering a 37-second file walk plus a 77-second bigram rebuild. The root fix
for axes B and C depends on this event volume being reduced; without switching
the backend, those fixes just defer the inevitable.

On network filesystems (NFS, CIFS/SMB) and other pseudo-filesystems (9p,
sshfs), inotify either does not deliver events at all or does so unreliably. A
periodic poll is both cheaper and more correct for these mounts.

## Code references

| Symbol | Location |
|---|---|
| `type Debouncer` alias | `background_watcher.rs:17` — `Debouncer<notify::RecommendedWatcher, NoCache>` |
| `DEBOUNCE_TIMEOUT` | `background_watcher.rs:27` — 50 ms |
| `BackgroundWatcher::new` signature | `background_watcher.rs:38-44` |
| `Self::create_debouncer(...)` call | `background_watcher.rs:88-96` |
| `create_debouncer` function | `background_watcher.rs:165-339` |
| `new_debouncer_opt` call | `background_watcher.rs:195-235` — creates `RecommendedWatcher` debouncer |
| `use_recursive` decision | `background_watcher.rs:79` — `cfg!(any(target_os = "macos", target_os = "windows"))` |
| `FilePickerOptions` struct | `file_picker.rs:434-449` |
| `FilePicker::spawn_background_watcher` | `file_picker.rs:844-860` — passes `self.mode` but no watcher config |
| C FFI `fff_create_instance2` | `crates/fff-c/src/lib.rs:183-287` — constructs `FilePickerOptions{watch, ...}` |
| `--no-watch` flag | `fff-mcp/src/main.rs:149-151` |
| `FilePickerOptions` construction (MCP) | `fff-mcp/src/main.rs:263-274` |
| `notify::Config::with_poll_interval` | notify 9.0.0-rc.4 — `Config` builder method, accepts `Duration` |
| `PollWatcher::new` | notify 9.0.0-rc.4 — `pub fn new<F: EventHandler>(handler: F, config: Config) -> Result<PollWatcher>` |
| `new_debouncer_opt` generics | fff-notify-debouncer-full 0.9.4 — `fn new_debouncer_opt<F, T: Watcher, C: FileIdCache>(...)` — watcher type is fully generic |

## Root cause analysis

The type alias at `background_watcher.rs:17` hard-codes the watcher to
`RecommendedWatcher`. `create_debouncer` calls `new_debouncer_opt`, which is
fully generic over `T: Watcher`, but the return type is pinned to
`Debouncer<RecommendedWatcher, NoCache>`. Nothing in the call stack carries any
hint about the filesystem type — `BackgroundWatcher::new` receives only
`base_path`, `git_workdir`, `SharedFilePicker`, `SharedFrecency`, and
`FFFMode`. There is no mechanism for the caller to request a different backend.

On WSL1, DrvFs is mounted as `drvfs` type. Its `statfs().f_type` value is
`0x53464846` (four-byte magic; the diagnosis document identifies this as the
WSL DrvFs magic). On WSL2 the DrvFs share is exposed over 9p; `statfs()` would
return `V9FS_MAGIC = 0x01021997` (from `linux/magic.h`). CIFS/SMB mounts
return `CIFS_SUPER_MAGIC = 0xFF534D42`; NFS returns `NFS_SUPER_MAGIC = 0x6969`.
None of these should use inotify as the primary change-detection mechanism.

## Proposed change

### 1. New enum: `WatcherBackend`

Add to `crates/fff-core/src/file_picker.rs` (alongside `FFFMode`):

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WatcherBackend {
    /// Automatically choose based on filesystem type (default).
    #[default]
    Auto,
    /// Force the platform's native event-driven watcher (inotify on Linux,
    /// FSEvents on macOS, ReadDirectoryChangesW on Windows).
    Native,
    /// Force periodic polling regardless of filesystem type.
    Poll,
}
```

### 2. Extend `FilePickerOptions`

Add two fields to `FilePickerOptions` (`file_picker.rs:434`):

```rust
pub struct FilePickerOptions {
    // ... existing fields ...
    /// Which watcher backend to use. Defaults to `WatcherBackend::Auto`.
    pub watcher_backend: WatcherBackend,
    /// Poll interval used when backend is Poll or auto-detected as polling.
    /// Defaults to 10 seconds.
    pub poll_interval: Option<Duration>,
}

impl Default for FilePickerOptions {
    fn default() -> Self {
        Self {
            // ... existing ...
            watcher_backend: WatcherBackend::Auto,
            poll_interval: None,  // resolved to DEFAULT_POLL_INTERVAL at use
        }
    }
}
```

Add `const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(10)` near the top of `background_watcher.rs`.

### 3. Filesystem detection helper (Linux-only)

Add a private function to `background_watcher.rs`:

```rust
#[cfg(target_os = "linux")]
fn is_polling_preferred_fs(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    use std::ffi::CString;

    let c_path = match CString::new(path.as_os_str().as_bytes()) {
        Ok(p) => p,
        Err(_) => return false,
    };

    // SAFETY: statfs(2) is async-signal-safe and only reads kernel metadata.
    let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c_path.as_ptr(), &mut buf) } != 0 {
        return false;
    }

    // Magic numbers that indicate inotify is unreliable or absent:
    //   0x53464846 — WSL1 DrvFs (Windows NTFS exposed via Plan 9 shim)
    //   0x01021997 — V9FS_MAGIC (WSL2 DrvFs over 9p, and other 9p mounts)
    //   0xFF534D42 — CIFS_SUPER_MAGIC (Samba / SMB shares)
    //   0x6969     — NFS_SUPER_MAGIC
    matches!(
        buf.f_type as u64,
        0x53464846 | 0x01021997 | 0xFF534D42 | 0x6969
    )
}

#[cfg(not(target_os = "linux"))]
fn is_polling_preferred_fs(_path: &Path) -> bool {
    false  // FSEvents and ReadDirectoryChangesW are reliable on macOS/Windows
}
```

This requires adding `libc` to `fff-core/Cargo.toml`. `libc` is already a
transitive dependency via `git2`; confirm it can be listed as a direct
`[target.'cfg(target_os = "linux")'.dependencies]` dependency.

### 4. Thread the backend choice through `BackgroundWatcher`

Extend `BackgroundWatcher::new` signature:

```rust
pub fn new(
    base_path: PathBuf,
    git_workdir: Option<PathBuf>,
    shared_picker: SharedFilePicker,
    shared_frecency: SharedFrecency,
    mode: FFFMode,
    watcher_backend: WatcherBackend,    // NEW
    poll_interval: Duration,            // NEW
) -> Result<Self, Error>
```

At the top of `new`, resolve the effective backend before branching:

```rust
let effective_backend = match watcher_backend {
    WatcherBackend::Poll => WatcherBackend::Poll,
    WatcherBackend::Native => WatcherBackend::Native,
    WatcherBackend::Auto => {
        if is_polling_preferred_fs(&base_path) {
            info!("Auto-detected polling-preferred filesystem; switching to PollWatcher");
            WatcherBackend::Poll
        } else {
            WatcherBackend::Native
        }
    }
};
```

Pass `effective_backend` and `poll_interval` to `create_debouncer`.

### 5. Split `create_debouncer` into two constructors

The current `type Debouncer` alias at `background_watcher.rs:17` is
`Debouncer<RecommendedWatcher, NoCache>`. Because `Debouncer<PollWatcher,
NoCache>` is a different type, they cannot be stored in the same field without
boxing or an enum. The cleanest approach is a lightweight enum wrapper:

```rust
enum AnyDebouncer {
    Native(notify_debouncer_full::Debouncer<notify::RecommendedWatcher, NoCache>),
    Poll(notify_debouncer_full::Debouncer<notify::PollWatcher, NoCache>),
}
```

Update `BackgroundWatcher` to hold `Arc<Mutex<Option<AnyDebouncer>>>`.

Implement forwarding methods on `AnyDebouncer`:

```rust
impl AnyDebouncer {
    fn watch(&mut self, path: &Path, mode: RecursiveMode) -> notify::Result<()> {
        match self {
            Self::Native(d) => d.watch(path, mode),
            Self::Poll(d) => d.watch(path, mode),
        }
    }
    fn stop_nonblocking(self) {
        match self {
            Self::Native(d) => d.stop_nonblocking(),
            Self::Poll(d) => d.stop_nonblocking(),
        }
    }
}
```

`create_debouncer` becomes a function returning `Result<AnyDebouncer, Error>`:

**Before (pseudo):**
```rust
fn create_debouncer(...) -> Result<Debouncer, Error> {
    let config = Config::default()...;
    let mut debouncer = new_debouncer_opt(DEBOUNCE_TIMEOUT, Some(DEBOUNCE_TIMEOUT/2),
        handler, NoCache::new(), config)?;
    // watch dirs ...
    Ok(debouncer)
}
```

**After (pseudo):**
```rust
fn create_debouncer(
    ...,
    effective_backend: WatcherBackend,
    poll_interval: Duration,
) -> Result<AnyDebouncer, Error> {
    let base_config = Config::default()
        .with_follow_symlinks(false)
        .with_event_kinds(EventKindMask::CORE);

    match effective_backend {
        WatcherBackend::Poll => {
            // PollWatcher ignores DEBOUNCE_TIMEOUT for the debouncer; use a
            // longer debounce (poll_interval / 2) to avoid rapid refire.
            let poll_config = base_config.with_poll_interval(poll_interval);
            let mut debouncer = new_debouncer_opt::<_, notify::PollWatcher, _>(
                poll_interval / 2,
                Some(poll_interval / 4),
                handler,
                NoCache::new(),
                poll_config,
            )?;
            setup_watches(&mut debouncer, &base_path, use_recursive,
                          &shared_picker_for_watching, &watch_tx)?;
            Ok(AnyDebouncer::Poll(debouncer))
        }
        _ => {
            let mut debouncer = new_debouncer_opt::<_, notify::RecommendedWatcher, _>(
                DEBOUNCE_TIMEOUT,
                Some(DEBOUNCE_TIMEOUT / 2),
                handler,
                NoCache::new(),
                base_config,
            )?;
            setup_watches(&mut debouncer, &base_path, use_recursive,
                          &shared_picker_for_watching, &watch_tx)?;
            Ok(AnyDebouncer::Native(debouncer))
        }
    }
}
```

The per-directory watch loop (currently `background_watcher.rs:267-328`) and
the git-status path watch (`background_watcher.rs:337`) should be extracted
into `setup_watches` to avoid duplication between branches.

### 6. New CLI flags in `fff-mcp/src/main.rs`

Following the pattern of `--no-watch` (`main.rs:149`):

```rust
/// Watcher backend: auto (default), native, poll, off.
/// "auto" uses polling when the indexed root is on DrvFs, 9p, NFS, or CIFS.
#[arg(long = "watcher-backend", default_value = "auto")]
watcher_backend: WatcherBackendArg,

/// Poll interval in seconds when using the poll backend (default 10).
#[arg(long = "poll-interval-secs", default_value = "10")]
poll_interval_secs: u64,
```

Where `WatcherBackendArg` is a small clap-compatible enum (`auto | native |
poll | off`) that maps to `WatcherBackend` (with `off` mapping to
`watch: false`). Thread these into `FilePickerOptions` at `main.rs:263`.

### 7. `FilePicker::spawn_background_watcher` update

At `file_picker.rs:844`, `spawn_background_watcher` currently passes only
`self.mode`. It must also pass `self.watcher_backend` and the resolved
`self.poll_interval` (defaulting to `DEFAULT_POLL_INTERVAL` when `None`).
Store both as fields on `FilePicker` (alongside the existing `watch: bool`
field at `file_picker.rs:476`).

## Compatibility / risk

**C FFI (`fff-c`):** `fff_create_instance2` at `lib.rs:183` and `fff_restart_index`
at `lib.rs:918` construct `FilePickerOptions` with positional field syntax.
Adding two fields with `Default` values means both call sites must be updated
to include the new fields or switch to `..Default::default()`. The ABI of the
exported C symbols is unchanged — no new C parameters are needed; Neovim can
continue using `fff_create_instance2` as-is. When Neovim runs on a DrvFs path,
`auto` detection will kick in automatically without any Lua-side changes.

**Debounce timing on Poll backend:** `PollWatcher` already has an internal
scan interval (`Config::with_poll_interval`). The debouncer's `timeout`
parameter then adds a second layer of delay. For a 10-second poll interval,
set debounce timeout to `poll_interval / 2` (5 s) and tick to `poll_interval /
4` (2.5 s). This means changes are visible within ~15 s worst-case — acceptable
for a background index that is only used for search, not for build systems.

**`use_recursive` on Poll backend:** `PollWatcher` performs a recursive scan
internally when registered with `RecursiveMode::Recursive`, regardless of
platform. On the poll backend the existing `use_recursive` logic can be
simplified: always use `Recursive` (since `PollWatcher` has no FD-per-dir
overhead), skipping the `picker.for_each_dir` loop entirely.

**`fff-watcher-own` thread:** The owner thread only calls `debouncer.watch()`
for newly discovered directories (`background_watcher.rs:134`). With
`AnyDebouncer` this requires forwarding through the enum's `watch()` method —
a two-line change.

**Native path regression:** The native (inotify) path is structurally unchanged.
The only new code executed on a native-filesystem repo is the `statfs()` call
in `BackgroundWatcher::new` — one syscall per startup, negligible cost.

**No new required dependency:** `libc` is already in the dependency graph via
`git2`. It just needs to be made a direct `[target.'cfg(target_os = "linux")'.dependencies]`
entry in `crates/fff-core/Cargo.toml`.

## Rollout plan

This change can land independently of axes B–F. It directly reduces event
volume entering the pipeline that axes B and C attempt to rate-limit, so it
should land first or in parallel.

1. Add `WatcherBackend` enum and extend `FilePickerOptions` — no behavior
   change yet.
2. Add `is_polling_preferred_fs` behind `#[cfg(target_os = "linux")]` with
   unit tests using a mock path (test asserts `false` on `/tmp`).
3. Implement `AnyDebouncer` enum and update `BackgroundWatcher`.
4. Update `spawn_background_watcher` and `FilePicker` field storage.
5. Add CLI flags to `fff-mcp`; update both `fff-c` call sites.
6. Test on a DrvFs path: `fff-mcp /c/work/some-repo --watcher-backend poll`.
   Observe that `fff-watcher-own` no longer pegs CPU.

Estimated diff: ~150 LOC new + ~60 LOC changed. The `AnyDebouncer` enum and
its forwarding methods account for most of the new lines.

Ship behind the existing flag surface: `--watcher-backend auto` is the default.
Users on ext4/tmpfs notice no change. Users on DrvFs get polling automatically.

## Open questions

1. **WSL1 DrvFs magic number**: The diagnosis doc gives `0x53464846` as the
   DrvFs `f_type`. This needs to be confirmed by running `python3 -c "import
   ctypes; ..."` or a small C snippet inside the user's WSL1 environment. If
   the actual value differs, the constant in `is_polling_preferred_fs` must be
   corrected before shipping.

2. **Poll interval default**: 10 seconds feels right for a developer workstation
   (changes are visible within ~15 s). Should it be shorter (5 s) at the cost of
   slightly more CPU? Oystein should decide based on perceived latency tolerance
   in Claude Code sessions.

3. **`libc` as direct dep**: Confirm that adding `libc` as a direct
   `cfg(target_os = "linux")` dependency in `fff-core` is acceptable, or
   whether the `statfs` call should be factored into a small `sys` module that
   only appears in the Linux build.

4. **`fff_restart_index` FFI**: That function recreates a `FilePickerOptions`
   from the picker's current state (`lib.rs:905-913`). It currently drops
   watcher configuration on restart (always uses `watch=picker.has_watcher()`
   but no backend info). Should it also preserve `watcher_backend` and
   `poll_interval`? This requires `FilePicker` to expose a `watcher_backend()`
   accessor, or store the resolved backend in the struct.

5. **`compare_contents` on poll**: `notify`'s `PollWatcher` can use
   `Config::with_compare_contents(true)` to detect changes by file content hash
   rather than mtime. This is more reliable on filesystems where mtime is coarse
   or unreliable, but adds CPU. Should this be exposed as a separate flag, or
   always left off?
