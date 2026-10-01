use crate::error::Error;
use crate::file_picker::{FFFMode, MAX_OVERFLOW_FILES};
use crate::git::GitStatusCache;
use crate::shared::{SharedFilePicker, SharedFrecency};
use crate::sort_buffer::sort_with_buffer;
use git2::Repository;
use notify::event::{AccessKind, AccessMode};
use notify::{
    Config, EventKind, EventKindMask, PollWatcher, RecommendedWatcher, RecursiveMode, Watcher,
};
use notify_debouncer_full::{DebounceEventResult, DebouncedEvent, NoCache, new_debouncer_opt};
use parking_lot::Mutex;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::mpsc;
use std::time::Duration;
use tracing::{Level, debug, error, info, warn};

/// Concrete debouncer for the platform's native event-driven watcher
/// (inotify / FSEvents / ReadDirectoryChangesW).
type NativeDebouncer = notify_debouncer_full::Debouncer<RecommendedWatcher, NoCache>;
/// Concrete debouncer for the periodic polling watcher.
type PollDebouncer = notify_debouncer_full::Debouncer<PollWatcher, NoCache>;

/// Default polling interval used when the poll backend is active and no
/// explicit interval was configured. 10 s keeps CPU negligible while making
/// edits visible to search within ~15 s worst-case (poll + debounce).
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(10);

/// Which filesystem-watching strategy [`BackgroundWatcher`] uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WatcherBackend {
    /// Pick automatically from the indexed root's filesystem type: polling on
    /// DrvFs/9p/CIFS/NFS (where inotify is noisy or unreliable), the native
    /// event-driven watcher everywhere else. This is the default.
    #[default]
    Auto,
    /// Force the platform-native watcher (inotify / FSEvents / RDCW).
    Native,
    /// Force periodic polling regardless of filesystem type.
    Poll,
}

impl WatcherBackend {
    /// Parse a CLI / config string. Unknown values fall back to `Auto`.
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "native" => Self::Native,
            "poll" => Self::Poll,
            _ => Self::Auto,
        }
    }
}

/// Process-global watcher configuration.
///
/// fff-mcp runs one process per repo, so a single set-once config is
/// sufficient and avoids threading two override knobs through
/// `FilePickerOptions` and its ~30 construction sites. Defaults to `Auto` +
/// [`DEFAULT_POLL_INTERVAL`]; consumers that never set it (e.g. the Neovim
/// plugin) get auto-detection for free.
#[derive(Debug, Clone, Copy)]
pub struct WatcherConfig {
    pub backend: WatcherBackend,
    pub poll_interval: Duration,
}

impl Default for WatcherConfig {
    fn default() -> Self {
        Self {
            backend: WatcherBackend::Auto,
            poll_interval: DEFAULT_POLL_INTERVAL,
        }
    }
}

static WATCHER_CONFIG: OnceLock<WatcherConfig> = OnceLock::new();

/// Install the process-global [`WatcherConfig`]. Call once at startup before
/// any [`FilePicker`](crate::file_picker::FilePicker) spawns its watcher;
/// later calls are ignored.
pub fn set_watcher_config(config: WatcherConfig) {
    if WATCHER_CONFIG.set(config).is_err() {
        warn!("Watcher config already set; ignoring later override");
    }
}

fn watcher_config() -> WatcherConfig {
    if let Some(cfg) = WATCHER_CONFIG.get() {
        return *cfg;
    }

    // Fallback to environment when no explicit config was installed (e.g. unit
    // tests, the Neovim plugin). Lets `FFF_WATCHER_BACKEND=native` keep the
    // event-driven watcher under test on WSL, where the rootfs is wslfs and
    // `Auto` would otherwise resolve to the slow poll backend.
    let mut cfg = WatcherConfig::default();
    if let Ok(backend) = std::env::var("FFF_WATCHER_BACKEND") {
        cfg.backend = WatcherBackend::parse(&backend);
    }
    if let Ok(secs) = std::env::var("FFF_POLL_INTERVAL_SECS")
        && let Ok(secs) = secs.parse::<u64>()
        && secs > 0
    {
        cfg.poll_interval = Duration::from_secs(secs);
    }
    cfg
}

/// Type-erased debouncer so [`BackgroundWatcher`] can hold either backend in a
/// single field. `Debouncer<RecommendedWatcher, _>` and `Debouncer<PollWatcher,
/// _>` are distinct concrete types.
enum AnyDebouncer {
    Native(NativeDebouncer),
    Poll(PollDebouncer),
}

impl AnyDebouncer {
    /// Register a directory discovered at runtime.
    ///
    /// Only meaningful for the native inotify backend, which watches each
    /// directory non-recursively. The poll backend watches the base path
    /// recursively, so new subdirectories are already covered — adding a
    /// per-dir poll entry would only schedule another full-subtree stat cycle.
    fn watch(&mut self, path: &Path, mode: RecursiveMode) -> notify::Result<()> {
        match self {
            Self::Native(d) => d.watch(path, mode),
            Self::Poll(_) => Ok(()),
        }
    }

    fn stop_nonblocking(self) {
        match self {
            Self::Native(d) => d.stop_nonblocking(),
            Self::Poll(d) => d.stop_nonblocking(),
        }
    }
}

/// True when `path` lives on a filesystem where inotify/FSEvents are noisy,
/// unreliable, or absent and a periodic poll is cheaper and more correct.
///
/// On WSL1 the entire rootfs is `wslfs`, so this returns true box-wide — which
/// is exactly right: WSL1 surfaces all Windows-side filesystem activity
/// (Defender, OneDrive, the Search indexer, MSBuild watchers) as synthetic
/// inotify events. On WSL2 the Linux rootfs is real ext4 (native inotify
/// works); only `/mnt/c` is 9p and gets polled.
#[cfg(target_os = "linux")]
fn is_polling_preferred_fs(path: &Path) -> bool {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let Ok(c_path) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };

    // SAFETY: `statfs(2)` only reads kernel metadata into a caller-owned
    // buffer; `c_path` is a valid NUL-terminated path for the duration of the
    // call. A non-zero return means the path is unreadable — treat as "native".
    let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c_path.as_ptr(), &mut buf) } != 0 {
        return false;
    }

    matches!(
        buf.f_type as u64,
        0x5346_4846       // WSL1 DrvFs / wslfs (Windows NTFS via the WSL shim)
            | 0x0102_1997 // V9FS  — WSL2 /mnt/c over 9p, and other 9p mounts
            | 0xFF53_4D42 // CIFS_SUPER_MAGIC — SMB / Samba shares
            | 0x6969      // NFS_SUPER_MAGIC
    )
}

#[cfg(not(target_os = "linux"))]
fn is_polling_preferred_fs(_path: &Path) -> bool {
    // FSEvents (macOS) and ReadDirectoryChangesW (Windows) are reliable.
    false
}

/// Resolve an `Auto` backend against the indexed root's filesystem type.
fn resolve_backend(backend: WatcherBackend, base_path: &Path) -> WatcherBackend {
    match backend {
        WatcherBackend::Auto => {
            if is_polling_preferred_fs(base_path) {
                info!(
                    "Auto-detected polling-preferred filesystem at {}; using PollWatcher",
                    base_path.display()
                );
                WatcherBackend::Poll
            } else {
                WatcherBackend::Native
            }
        }
        explicit => explicit,
    }
}

/// Cache for the libgit2 `Repository` handle used by event-processing helpers.
///
/// Opening a Repository is surprisingly expensive: every call re-reads
/// `/etc/gitconfig`, the user's `~/.gitconfig`, and the repo's own
/// `.git/config`, then walks the worktree to resolve `.git` indirection. On a
/// shared `.git/` (e.g. multiple git worktrees fed from one common dir), the
/// debouncer batches fire many times per second, and the per-batch open
/// dominated watcher CPU in profiling (~16 opens/sec/watcher on Linux with
/// libgit2 0.20.x).
///
/// The handle is kept alive across batches and shared between the debouncer's
/// callback thread and the owner thread via the parking_lot mutex. The mutex
/// is held only while the helper actually uses the repository (one debounced
/// batch or one `track_files_from_new_directories` call), which keeps
/// contention negligible.
#[derive(Clone)]
struct GitRepoCache {
    workdir: Option<PathBuf>,
    repo: Arc<Mutex<Option<Repository>>>,
}

impl GitRepoCache {
    fn new(workdir: Option<PathBuf>) -> Self {
        let repo = workdir.as_deref().and_then(|p| Repository::open(p).ok());
        Self {
            workdir,
            repo: Arc::new(Mutex::new(repo)),
        }
    }

    /// Run `f` with a borrow of the cached repository.
    ///
    /// If the cache slot is empty (initial open failed, or a previous caller
    /// invalidated it), this attempts to re-open from `workdir` while holding
    /// the lock so concurrent callers don't race on the open.
    fn with_repo<R>(&self, f: impl FnOnce(&Option<Repository>) -> R) -> R {
        let mut guard = self.repo.lock();
        if guard.is_none()
            && let Some(workdir) = self.workdir.as_deref()
        {
            *guard = Repository::open(workdir).ok();
        }
        f(&*guard)
    }
}

/// Owns the file-system watcher and guarantees that all background threads
/// are fully joined before `stop()` / `Drop` returns.
pub struct BackgroundWatcher {
    debouncer: Arc<Mutex<Option<AnyDebouncer>>>,
    watch_tx: Option<mpsc::Sender<PathBuf>>,
    owner_thread: Option<std::thread::JoinHandle<()>>,
}

const DEBOUNCE_TIMEOUT: Duration = Duration::from_millis(50);
const MAX_PATHS_THRESHOLD: usize = 1024;
/// Minimum seconds between frecency tracks of the same file in AI mode.
/// Prevents score inflation from rapid burst edits by AI agents.
const AI_MODE_COOLDOWN_SECS: u64 = 5 * 60;

impl BackgroundWatcher {
    pub fn new(
        base_path: PathBuf,
        git_workdir: Option<PathBuf>,
        shared_picker: SharedFilePicker,
        shared_frecency: SharedFrecency,
        mode: FFFMode,
    ) -> Result<Self, Error> {
        info!(
            "Initializing background watcher for path: {}, mode: {:?}",
            base_path.display(),
            mode,
        );

        // Refuse to watch the filesystem root or the user's home directory.
        // These are prone to high-volume event churn (editor temp files,
        // browser caches, log rotations) which inflates the overflow arena
        // and, on macOS, can exhaust the per-process FSEvents stream limit.
        if base_path.parent().is_none()
            || Some(base_path.as_os_str()) == dirs::home_dir().as_ref().map(|p| p.as_os_str())
        {
            return Err(Error::FilesystemRoot(base_path));
        }

        // macOS: always use a single recursive FSEvent stream.
        //
        // Per-dir NonRecursive watches create one FSEvent stream per dir.
        // The per-process FSEvent cap is lower than expected in practice
        // (4096 per process, but FFF usually is running within code editors),
        // and each failed `watch()` after the cap blocks ~40 ms on kernel retry.
        // Yes we pay for filtering events on handler phase but it is usable
        //
        // macOS and Windows use a single recursive watch. FSEvents and
        // ReadDirectoryChangesW both support true kernel-level recursion
        // on one handle — per-dir NonRecursive watches burn streams/handles
        // for no benefit and, on Windows, have been observed to silently
        // drop Modify events for nested paths.
        //
        // Linux keeps the per-dir NonRecursive strategy: inotify has no
        // kernel-level recursion, so Recursive here would still register
        // one watch per subdir but without the ignored-dir filtering we
        // get by iterating `picker.for_each_dir` ourselves.
        let use_recursive = cfg!(any(target_os = "macos", target_os = "windows"));

        // Resolve the process-global watcher config against this root's
        // filesystem. On DrvFs/9p/CIFS/NFS this flips to PollWatcher, which
        // makes the inotify event storm — and the rescan/git-status spin it
        // drives — physically impossible.
        let WatcherConfig {
            backend,
            poll_interval,
        } = watcher_config();
        let effective_backend = resolve_backend(backend, &base_path);

        let (watch_tx, watch_rx) = mpsc::channel::<PathBuf>();
        let watch_tx_for_debouncer = watch_tx.clone();

        let owner_weak_picker = shared_picker.weaken();
        let owner_frecency = shared_frecency.clone();

        // Shared, lazily-opened Repository handle. Two consumers — the
        // debouncer's event handler and the owner thread — would otherwise
        // each call `Repository::open` per batch / per new directory.
        let repo_cache = GitRepoCache::new(git_workdir.clone());
        let owner_repo_cache = repo_cache.clone();

        let debouncer = Self::create_debouncer(
            base_path,
            git_workdir,
            shared_picker,
            shared_frecency,
            mode,
            use_recursive,
            watch_tx_for_debouncer,
            repo_cache,
            effective_backend,
            poll_interval,
        )?;

        info!("Background file watcher initialized successfully");

        // debouncer is shared with the owner thread, once it's dropped the thread is closed
        let debouncer = Arc::new(Mutex::new(Some(debouncer)));
        // Only the Linux per-dir-watch branch needs this clone; on other
        // platforms the owner thread never touches the debouncer.
        #[cfg(target_os = "linux")]
        let owner_debouncer = Arc::clone(&debouncer);

        let owner_thread = std::thread::Builder::new()
            .name("fff-watcher-own".into())
            .spawn(move || {
                // Dedup re-emitted Create(Folder) events. notify-rs occasionally
                // re-reports a directory it already streamed (observed on
                // wslfs/inotify in particular), and registering a duplicate
                // watch + re-walking the directory contents is the second
                // biggest source of owner-thread CPU after `Repository::open`.
                let mut watched_dirs: HashSet<PathBuf> = HashSet::new();

                while let Ok(dir) = watch_rx.recv() {
                    if !watched_dirs.insert(dir.clone()) {
                        debug!(
                            dir = %dir.display(),
                            "Skipping already-watched directory"
                        );
                        continue;
                    }

                    // if the picker is dropped we do need to exit the loop
                    let Some(strong_picker) = owner_weak_picker.upgrade() else {
                        break;
                    };

                    // Only inotify (Linux) has no kernel-level recursion, so
                    // it's the only platform that needs a per-subdir watch to
                    // be registered at runtime. macOS FSEvents and Windows
                    // ReadDirectoryChangesW are already watching recursively
                    // from the base path (see `create_debouncer`), and
                    // registering a second overlapping stream there produces
                    // duplicate/out-of-order events.
                    #[cfg(target_os = "linux")]
                    {
                        // Register the new directory with the debouncer, then
                        // drop the mutex BEFORE doing picker-side work — see
                        // the comment on `BackgroundWatcher::stop` for the
                        // lock-ordering rationale.
                        let mut guard = owner_debouncer.lock();
                        let Some(debouncer) = guard.as_mut() else {
                            break;
                        };

                        if let Err(e) = debouncer.watch(&dir, RecursiveMode::NonRecursive) {
                            warn!(
                                ?e,
                                dir = %dir.display(),
                                "Failed to init watcher for new directory"
                            );
                        }
                    }

                    track_files_from_new_directories(
                        &dir,
                        &strong_picker,
                        &owner_frecency,
                        &owner_repo_cache,
                    );

                    // Transient strong ref drops here, back
                    // to weak-only before the next `recv()`.
                }

                tracing::info!("Background watcher is stopped");
            })
            .expect("failed to spawn fff-watcher-owner thread");

        Ok(Self {
            debouncer,
            watch_tx: Some(watch_tx),
            owner_thread: Some(owner_thread),
        })
    }

    // Construction needs the full watcher context; bundling into a struct
    // would add indirection without clarity for a single private call site.
    #[allow(clippy::too_many_arguments)]
    fn create_debouncer(
        base_path: PathBuf,
        git_workdir: Option<PathBuf>,
        shared_picker: SharedFilePicker,
        shared_frecency: SharedFrecency,
        mode: FFFMode,
        use_recursive: bool,
        watch_tx: mpsc::Sender<PathBuf>,
        repo_cache: GitRepoCache,
        effective_backend: WatcherBackend,
        poll_interval: Duration,
    ) -> Result<AnyDebouncer, Error> {
        let base_config = Config::default()
            // do not follow symlinks as then notifiers spawns a bunch of events for symlinked
            // files that could be git ignored, we have to property differentiate those and if
            // the file was edited through a
            .with_follow_symlinks(false)
            // only the actual modification events, ignore the open syscals that we can generate by
            // our own grep calls and preview window rendering
            .with_event_kinds(EventKindMask::CORE);

        // `use_recursive` was decided by the caller from a cheap size hint,
        // so the event-handler closure can capture it directly.
        //
        // The closure lives on the debouncer's internal event thread
        // for as long as the debouncer exists — i.e. the full
        // lifetime of `BackgroundWatcher`. Capturing a strong
        // `SharedFilePicker` here would re-introduce the Arc cycle
        // we just broke with `owner_picker`'s `downgrade()` above.
        // Capture a weak handle instead and upgrade per-batch.
        let shared_picker_for_watching = shared_picker.clone();
        let event_picker = shared_picker.weaken();
        let handler_repo_cache = repo_cache;
        // The event-handler closure lives on the debouncer's internal event
        // thread for the full lifetime of `BackgroundWatcher`. It upgrades the
        // *weak* `SharedFilePicker` per batch, so it never forms an Arc cycle
        // with the picker that owns this watcher.
        //
        // Built once and moved into whichever backend is constructed below. The
        // two match arms are mutually exclusive, so moving `handler` — and
        // `base_config` — in each arm is sound.
        let handler = move |result: DebounceEventResult| match result {
            Ok(events) => {
                // Upgrade just long enough to drive one debounced batch.
                // Failure means every external `SharedFilePicker` has already
                // dropped and teardown is underway.
                let Some(strong_picker) = event_picker.upgrade() else {
                    return;
                };

                let new_dirs = handle_debounced_events(
                    events,
                    &strong_picker,
                    &shared_frecency,
                    mode,
                    &handler_repo_cache,
                );

                // Every newly created directory must be reflected in the picker.
                for dir in new_dirs {
                    if let Err(e) = watch_tx.send(dir) {
                        warn!(?e, "Failed to send directory update error");
                    }
                }
            }
            Err(errors) => {
                error!("File watcher errors: {:?}", errors);
            }
        };

        // The poll backend does a single recursive stat sweep of the base path,
        // so one recursive watch covers the whole subtree with no per-dir
        // overhead — it must never fan out into per-directory poll entries.
        let recursive = use_recursive || matches!(effective_backend, WatcherBackend::Poll);

        // `NoCache`: the debouncer's recommended file-id cache would track every
        // watched file for no benefit — any rename already forces an index
        // remove + re-add under the new name.
        let debouncer = match effective_backend {
            WatcherBackend::Poll => {
                // PollWatcher rate-limits via `poll_interval`; the debounce on
                // top (timeout = interval/2) just coalesces one sweep's events.
                let config = base_config.with_poll_interval(poll_interval);
                let mut debouncer = new_debouncer_opt::<_, PollWatcher, _>(
                    poll_interval / 2,
                    Some(poll_interval / 4),
                    handler,
                    NoCache::new(),
                    config,
                )?;
                info!(
                    "Background watcher using PollWatcher (interval {:?}) for {}",
                    poll_interval,
                    base_path.display(),
                );
                setup_watches(&mut debouncer, &base_path, recursive, &shared_picker_for_watching)?;
                watch_git_status_paths(&mut debouncer, git_workdir.as_ref());
                AnyDebouncer::Poll(debouncer)
            }
            _ => {
                let mut debouncer = new_debouncer_opt::<_, RecommendedWatcher, _>(
                    DEBOUNCE_TIMEOUT,
                    Some(DEBOUNCE_TIMEOUT / 2), // tick rate for the event span
                    handler,
                    NoCache::new(),
                    base_config,
                )?;
                setup_watches(&mut debouncer, &base_path, recursive, &shared_picker_for_watching)?;
                watch_git_status_paths(&mut debouncer, git_workdir.as_ref());
                AnyDebouncer::Native(debouncer)
            }
        };

        Ok(debouncer)
    }

    /// Signal the watcher to shut down without blocking on its worker
    /// threads. Safe to call from any context, including while holding
    /// the [`SharedFilePicker`] write lock.
    ///
    /// Both the debouncer's internal event loop and our owner thread
    /// may call `SharedFilePicker::write()` inside their handlers. A
    /// blocking join here would deadlock against a caller that already
    /// holds that lock (e.g. `stop_background_monitor` under a
    /// `shared_picker.write()` guard). Instead we:
    ///
    ///   * drop the `watch_tx` Sender — the owner thread's
    ///     `watch_rx.recv()` returns `Err` and the thread exits at
    ///     its next `recv`.
    ///   * call `debouncer.stop_nonblocking()` — signals the debouncer
    ///     event loop to exit on its next tick and drops the watcher,
    ///     closing the FSEvent / inotify / ReadDirectoryChangesW stream.
    ///   * detach both `JoinHandle`s.
    ///
    /// In-flight handler invocations finish on their own (at most one
    /// more batch) once the caller releases any locks they hold.
    pub fn stop(&mut self) {
        self.watch_tx.take();
        if let Some(debouncer) = self.debouncer.lock().take() {
            debouncer.stop_nonblocking();
        }

        self.owner_thread.take();

        info!("Background file watcher stop signaled");
    }

    /// Queue a non-recursive watch registration on `dir`.
    ///
    /// The owner thread is always blocked on `watch_rx.recv()`, so
    /// the `send()` here wakes it immediately via the channel's
    /// condvar — no external unpark needed.
    ///
    /// Returns `false` once `stop()` has dropped our `Sender` — any
    /// further request is silently discarded.
    pub(crate) fn request_watch_dir(&self, dir: PathBuf) -> bool {
        match self.watch_tx.as_ref() {
            Some(tx) => tx.send(dir).is_ok(),
            None => false,
        }
    }
}

impl Drop for BackgroundWatcher {
    fn drop(&mut self) {
        self.stop();
    }
}

#[tracing::instrument(
    name = "fs_events",
    skip(events, shared_picker, shared_frecency, repo_cache),
    level = Level::DEBUG,
)]
fn handle_debounced_events(
    events: Vec<DebouncedEvent>,
    shared_picker: &SharedFilePicker,
    shared_frecency: &SharedFrecency,
    mode: FFFMode,
    repo_cache: &GitRepoCache,
) -> Vec<PathBuf> {
    // This will be called very often. The libgit2 `Repository` handle is
    // cached for the lifetime of the watcher — see `GitRepoCache` — so we
    // hold the lock for the duration of this batch and feed `&Option<Repository>`
    // to the existing helpers unchanged.
    repo_cache.with_repo(|repo| {
        handle_debounced_events_inner(events, shared_picker, shared_frecency, mode, repo)
    })
}

fn handle_debounced_events_inner(
    events: Vec<DebouncedEvent>,
    shared_picker: &SharedFilePicker,
    shared_frecency: &SharedFrecency,
    mode: FFFMode,
    repo: &Option<Repository>,
) -> Vec<PathBuf> {
    let mut need_full_rescan = false;
    let mut need_full_git_rescan = false;
    let mut paths_to_remove = Vec::new();
    let mut dirs_to_remove: Vec<PathBuf> = Vec::new();
    let mut paths_to_add_or_modify = Vec::new();
    let mut new_dirs_to_watch = Vec::new();
    let mut affected_paths_count = 0usize;

    for debounced_event in &events {
        // It is very important to not react to the access errors because we inevitably
        // gonna trigger the sync by our own preview or other unnecessary noise
        if matches!(
            debounced_event.event.kind,
            EventKind::Access(
                AccessKind::Read
                    | AccessKind::Open(_)
                    | AccessKind::Close(AccessMode::Read | AccessMode::Execute)
            )
        ) {
            continue;
        }

        // When macOS FSEvents (or other backends) overflow their event buffer, the kernel
        // drops individual events and emits a Rescan flag telling us to re-scan the subtree.
        // Without handling this, modified source files can be silently missed.
        if debounced_event.event.need_rescan() {
            warn!(
                "Received rescan event for paths {:?}, triggering full rescan",
                debounced_event.event.paths
            );
            need_full_rescan = true;
            break;
        }

        tracing::debug!(event = ?debounced_event.event, "Processing FS event");
        for path in &debounced_event.event.paths {
            if is_ignore_definition_path(path) {
                info!(
                    "Detected change in ignore definition file: {}",
                    path.display()
                );
                need_full_rescan = true;
                break;
            }

            if is_dotgit_change_affecting_status(path, &repo) {
                need_full_git_rescan = true;
            }

            if is_git_file(path) {
                continue;
            }

            // Use a combination of event kind and filesystem state to decide
            // whether a path is an addition/modification or a removal.
            //
            // We cannot rely on `path.exists()` alone because:
            //   - A freshly created file might not be visible yet (race).
            //   - macOS FSEvents uses Modify(Name(Any)) for both rename-in
            //     and rename-out, so we must stat the path to disambiguate.
            //
            // We cannot rely on event kind alone because:
            //   - Remove events are not always emitted (macOS often sends
            //     Modify(Name(Any)) instead of Remove).
            let is_removal = matches!(debounced_event.event.kind, EventKind::Remove(_));

            // Directory-level remove: both fsevents and inotify delivers a single
            // `Remove(Folder)` event for a whole directory tree (e.g.
            // after `git reset --hard` wipes a dir full of staged-but-
            // uncommitted files).
            let is_folder_removal = matches!(
                debounced_event.event.kind,
                EventKind::Remove(notify::event::RemoveKind::Folder)
            );

            if is_folder_removal {
                dirs_to_remove.push(path.to_path_buf());
            } else if is_removal || !path.exists() {
                paths_to_remove.push(path.as_path());
            } else if path.is_dir() {
                // New directory — collect it so the caller can register a
                // watcher. No filesystem scanning: files that arrive later
                // will be handled by the newly registered watch.
                if !is_path_ignored(path, &repo) {
                    new_dirs_to_watch.push(path.to_path_buf());
                }
            } else {
                // For additions/modifications, still filter gitignored files.
                if should_include_file(path, &repo) {
                    paths_to_add_or_modify.push(path.as_path());
                }
            }
        }

        affected_paths_count += debounced_event.event.paths.len();
        if affected_paths_count > MAX_PATHS_THRESHOLD {
            warn!(
                "Too many affected paths ({}) in a single batch, triggering full rescan",
                affected_paths_count
            );

            need_full_rescan = true;
            break;
        }

        if need_full_rescan {
            break;
        }
    }

    if need_full_rescan {
        info!(?affected_paths_count, "Triggering full rescan");
        if let Err(e) = shared_picker.trigger_full_rescan_async(shared_frecency) {
            error!("Failed to trigger full rescan: {:?}", e);
        }
        return Vec::new();
    }

    // It's important to get the allocated sort
    sort_with_buffer(paths_to_add_or_modify.as_mut_slice(), |a, b| {
        a.as_os_str().cmp(b.as_os_str())
    });
    paths_to_add_or_modify.dedup_by(|a, b| a.as_os_str().eq(b.as_os_str()));

    info!(
        "Event processing summary: {} to remove, {} dirs to remove, {} to add/modify, {} new dirs",
        paths_to_remove.len(),
        dirs_to_remove.len(),
        paths_to_add_or_modify.len(),
        new_dirs_to_watch.len()
    );

    if paths_to_remove.is_empty()
        && dirs_to_remove.is_empty()
        && paths_to_add_or_modify.is_empty()
        && !need_full_git_rescan
    {
        debug!("No file index changes to apply");
        return new_dirs_to_watch;
    }

    let mut files_to_update_git_status = Vec::new();
    let mut need_full_rescan = false;
    let mut overflow_count = 0;

    if !paths_to_remove.is_empty()
        || !dirs_to_remove.is_empty()
        || !paths_to_add_or_modify.is_empty()
    {
        debug!(
            "Applying file index changes: {} to remove, {} dirs to remove, {} to add/modify",
            paths_to_remove.len(),
            dirs_to_remove.len(),
            paths_to_add_or_modify.len(),
        );

        let Ok(mut guard) = shared_picker.write() else {
            error!("Failed to acquire file picker write lock");
            return new_dirs_to_watch;
        };
        let Some(ref mut picker) = *guard else {
            error!("File picker not initialized");
            return new_dirs_to_watch;
        };

        for dir in &dirs_to_remove {
            let count = picker.remove_all_files_in_dir(dir);
            debug!("remove_all_files_in_dir({:?}) -> {} files", dir, count);
        }

        for path in &paths_to_remove {
            let removed = picker.remove_file_by_path(path);
            debug!("remove_file_by_path({:?}) -> {}", path, removed);
        }

        files_to_update_git_status.reserve(paths_to_add_or_modify.len());
        for path in &paths_to_add_or_modify {
            if picker.handle_create_or_modify(path).is_some() {
                files_to_update_git_status.push(path.to_path_buf());
            } else {
                need_full_rescan = true;
            }
        }

        overflow_count = picker.get_overflow_files().len();
    }

    info!(
        files_updated = files_to_update_git_status.len(),
        overflow_count, "File index changes applied",
    );
    if need_full_rescan || overflow_count > MAX_OVERFLOW_FILES {
        info!("Watcher faced limit of index overflow. Triggering rescan");
        if let Err(e) = shared_picker.trigger_full_rescan_async(shared_frecency) {
            error!("Failed to trigger full rescan: {:?}", e);
        }
    }

    // AI mode: auto-track frecency for all modified/created files.
    // Uses a 5-minute cooldown per file to prevent score inflation from rapid
    // burst edits (AI agents often edit the same file many times in minutes).
    // This runs after apply_changes so the picker write lock is released.
    if mode.is_ai() && !paths_to_add_or_modify.is_empty() {
        let mut tracked_count = 0usize;
        if let Ok(frecency_guard) = shared_frecency.read()
            && let Some(ref frecency) = *frecency_guard
        {
            for path in &paths_to_add_or_modify {
                // Skip if this file was tracked less than 5 minutes ago
                let should_track = match frecency.seconds_since_last_access(path) {
                    Ok(Some(secs)) => secs >= AI_MODE_COOLDOWN_SECS,
                    Ok(None) => true, // Never tracked before
                    Err(_) => true,   // DB error, track anyway
                };
                if !should_track {
                    continue;
                }

                if let Err(e) = frecency.track_access(path) {
                    error!("Failed to track frecency for {:?}: {:?}", path, e);
                } else {
                    tracked_count += 1;
                }
            }
            if tracked_count > 0 {
                info!("AI mode: tracked frecency for {} files", tracked_count);
            }
        }

        // Update in-memory frecency scores for tracked files
        if tracked_count > 0
            && let Ok(mut picker_guard) = shared_picker.write()
            && let Some(ref mut picker) = *picker_guard
            && let Ok(frecency_guard) = shared_frecency.read()
            && let Some(ref frecency) = *frecency_guard
        {
            for path in &paths_to_add_or_modify {
                let _ = picker.update_single_file_frecency(path, frecency);
            }
        }
    }

    // do not try to update the paths if we anyway going to rescan everything from scratch
    if !need_full_rescan && (need_full_git_rescan || !files_to_update_git_status.is_empty()) {
        let git_workdir = repo
            .as_ref()
            .map(|r| r.workdir().unwrap_or_else(|| r.path()).to_path_buf());

        let shared_picker = shared_picker.clone();
        let shared_frecency = shared_frecency.clone();

        // git status query even with a pathspec could be really slow, if we do this syncrhronously
        // within the event handler, we actually risk of forming a snow ball of conflicting events
        crate::file_picker::BACKGROUND_THREAD_POOL.spawn(move || {
            let Some(git_path) = git_workdir else { return };
            let Ok(repo) = Repository::open(&git_path) else {
                error!("Failed to open git repo for async status update");
                return;
            };

            if need_full_git_rescan && !need_full_rescan {
                info!("Async: triggering full git rescan");
                if let Err(e) = shared_picker.refresh_git_status(&shared_frecency) {
                    error!("Failed to refresh git status: {:?}", e);
                }
            }

            if !files_to_update_git_status.is_empty() {
                let status = match GitStatusCache::git_status_for_paths(
                    &repo,
                    &files_to_update_git_status,
                ) {
                    Ok(s) => s,
                    Err(e) => {
                        error!("Failed to query git status: {:?}", e);
                        return;
                    }
                };

                if let Ok(mut guard) = shared_picker.write()
                    && let Some(ref mut picker) = *guard
                {
                    if let Err(e) = picker.update_git_statuses(status, &shared_frecency) {
                        error!("Failed to update git statuses: {:?}", e);
                    } else {
                        info!("Async: git statuses updated");
                    }
                }
            }
        });
    }

    new_dirs_to_watch
}

/// After registering a watch on a newly created directory, list its
/// immediate children and add any files to the picker.
fn track_files_from_new_directories(
    dir: &Path,
    shared_picker: &SharedFilePicker,
    shared_frecency: &SharedFrecency,
    repo_cache: &GitRepoCache,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };

    repo_cache.with_repo(|repo| {
        track_files_from_new_directories_inner(
            dir,
            entries,
            shared_picker,
            shared_frecency,
            repo,
        )
    })
}

fn track_files_from_new_directories_inner(
    dir: &Path,
    entries: std::fs::ReadDir,
    shared_picker: &SharedFilePicker,
    shared_frecency: &SharedFrecency,
    repo: &Option<Repository>,
) {
    let mut files_to_add = Vec::new();

    for entry in entries.flatten() {
        if entry.file_type().is_ok_and(|ft| ft.is_file()) {
            let path = entry.path();
            if should_include_file(&path, &repo) {
                files_to_add.push(path);
            }
        }
    }

    if files_to_add.is_empty() {
        return;
    }

    {
        let Ok(mut guard) = shared_picker.write() else {
            return;
        };

        let Some(ref mut picker) = *guard else {
            return;
        };

        for path in &files_to_add {
            picker.handle_create_or_modify(path);
        }
    }

    if let Some(repo) = repo.as_ref() {
        let status = match GitStatusCache::git_status_for_paths(repo, &files_to_add) {
            Ok(status) => status,
            Err(e) => {
                tracing::error!(?e, "inject_existing_files: git status query failed");
                return;
            }
        };

        if let Ok(mut guard) = shared_picker.write()
            && let Some(ref mut picker) = *guard
            && let Err(e) = picker.update_git_statuses(status, shared_frecency)
        {
            error!("inject_existing_files: failed to update git statuses: {e:?}");
        }
    }

    debug!(
        "Injected {} existing files from new directory {}",
        files_to_add.len(),
        dir.display(),
    );
}

fn should_include_file(path: &Path, repo: &Option<Repository>) -> bool {
    // Directories are not indexed — only regular files (and symlinks to files).
    if path.is_dir() {
        return false;
    }

    match repo.as_ref() {
        Some(repo) => repo.is_path_ignored(path) != Ok(true),
        None => {
            // No git repo — apply basic sanity filters.
            // Hidden directories are skipped by the watcher setup (hidden(true)),
            // but events can still arrive for files in known non-code directories.
            !is_non_code_directory(path)
        }
    }
}

fn is_non_code_directory(path: &Path) -> bool {
    crate::ignore::is_non_code_directory(path)
}

#[inline]
fn is_path_ignored(path: &Path, repo: &Option<Repository>) -> bool {
    match repo.as_ref() {
        Some(repo) => repo.is_path_ignored(path) == Ok(true),
        None => is_non_code_directory(path),
    }
}

#[inline]
pub(crate) fn is_git_file(path: &Path) -> bool {
    path.components()
        .any(|component| component.as_os_str() == ".git")
}

fn is_dotgit_change_affecting_status(changed: &Path, repo: &Option<Repository>) -> bool {
    let Some(repo) = repo.as_ref() else {
        return false;
    };

    let git_dir = repo.path();

    if let Ok(path_in_git_dir) = changed.strip_prefix(git_dir) {
        // Only react to changes that rewrite the worktree state: commits,
        // staging, checkouts, merges, conflict resolution. Ref-only updates
        // under refs/ (fetch, push, tag writes, pack-refs) do not change
        // which files are modified/untracked, so we deliberately skip them —
        // watching refs/ recursively would cost one inotify watch per ref
        // namespace on repos with many branches/remotes.
        if path_in_git_dir == Path::new("index") || path_in_git_dir == Path::new("index.lock") {
            return true;
        }
        if path_in_git_dir == Path::new("HEAD") {
            return true;
        }
        if path_in_git_dir == Path::new("info/exclude")
            || path_in_git_dir == Path::new("info/sparse-checkout")
        {
            return true;
        }

        if let Some(fname) = path_in_git_dir.file_name().and_then(|f| f.to_str())
            && matches!(fname, "MERGE_HEAD" | "CHERRY_PICK_HEAD" | "REVERT_HEAD")
        {
            return true;
        }
    }

    false
}

fn is_ignore_definition_path(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(|f| f.to_str()),
        Some(".ignore") | Some(".gitignore")
    )
}

/// Register the initial set of watches on `debouncer`.
///
/// When `recursive` is true (macOS/Windows native streams, or any poll
/// backend) a single recursive watch on `base_path` covers the whole subtree.
/// Otherwise (Linux inotify) we register one NonRecursive watch per indexed
/// directory: inotify has no kernel-level recursion, and per-dir watches let
/// us skip gitignored subtrees and keep the kernel watch count bounded.
///
/// New directories created at runtime are picked up via Create events on the
/// parent and added by the owner thread (native backend only).
fn setup_watches<T: Watcher>(
    debouncer: &mut notify_debouncer_full::Debouncer<T, NoCache>,
    base_path: &Path,
    recursive: bool,
    shared_picker_for_watching: &SharedFilePicker,
) -> Result<(), Error> {
    if recursive {
        debouncer.watch(base_path, RecursiveMode::Recursive)?;
        info!(
            "File watcher initialized with single recursive watch on {}",
            base_path.display(),
        );
        return Ok(());
    }

    debouncer.watch(base_path, RecursiveMode::NonRecursive)?;

    // Stream watch-dir registration directly under the picker read lock.
    // `inotify_add_watch()` is fast-fail: on ENOSPC it returns immediately with
    // no kernel retry loop, so holding the read lock across the loop is O(ms)
    // even for large repos.
    //
    // Abort after a run of failures. Once ENOSPC hits, further calls won't
    // succeed until the user raises `fs.inotify.max_user_watches`, so there's
    // no value in continuing.
    const MAX_CONSECUTIVE_WATCH_FAILURES: usize = 16;

    let mut watched = 0usize;
    let mut consecutive_failures = 0usize;
    let mut aborted_early = false;

    if let Some(guard) = shared_picker_for_watching.read().ok()
        && let Some(picker) = guard.as_ref()
    {
        use std::ops::ControlFlow;
        picker.for_each_dir(|dir| {
            match debouncer.watch(dir, RecursiveMode::NonRecursive) {
                Ok(()) => {
                    watched += 1;
                    consecutive_failures = 0;
                    ControlFlow::Continue(())
                }
                Err(e) => {
                    consecutive_failures += 1;
                    if consecutive_failures <= 4 {
                        warn!("Failed to watch directory {}: {}", dir.display(), e);
                    }

                    if consecutive_failures >= MAX_CONSECUTIVE_WATCH_FAILURES {
                        warn!(
                            consecutive_failures,
                            watched,
                            "Aborting NonRecursive watch loop — per-process \
                             watch cap exhausted, further dirs would just burn \
                             kernel time for no coverage"
                        );
                        aborted_early = true;
                        ControlFlow::Break(())
                    } else {
                        ControlFlow::Continue(())
                    }
                }
            }
        });
    }

    info!(
        "File watcher initialized for {} directories (NonRecursive) under {} (aborted_early={})",
        watched,
        base_path.display(),
        aborted_early,
    );

    Ok(())
}

fn watch_git_status_paths<T: Watcher>(
    debouncer: &mut notify_debouncer_full::Debouncer<T, NoCache>,
    git_workdir: Option<&PathBuf>,
) {
    let Some(workdir) = git_workdir else {
        return;
    };

    let git_dir = workdir.join(".git");
    if !git_dir.is_dir() {
        return;
    }

    // Watch .git/ non-recursively to catch top-level files:
    // index, index.lock, HEAD, MERGE_HEAD, CHERRY_PICK_HEAD, REVERT_HEAD.
    // We intentionally do NOT watch refs/ — individual ref updates don't
    // affect worktree status, and a recursive watch there blows up inotify
    // watch counts on repos with many branches/remotes/tags.
    if let Err(e) = debouncer.watch(&git_dir, RecursiveMode::NonRecursive) {
        warn!("Failed to watch .git directory: {}", e);
        return;
    }

    // Watch info/ non-recursively for exclude and sparse-checkout
    let info_dir = git_dir.join("info");
    if info_dir.is_dir()
        && let Err(e) = debouncer.watch(&info_dir, RecursiveMode::NonRecursive)
    {
        warn!("Failed to watch .git/info: {}", e);
    }
}
