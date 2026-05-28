//! FFF MCP Server — high-performance file finder for AI code assistants.
//!
//! Drop-in replacement for AI code assistant file search tools (Glob/Grep).
//! Provides frecency-ranked, fuzzy-matched, git-aware file finding and
//! code search via the Model Context Protocol (MCP).
//!
//! Uses `fff-core` directly (zero FFI overhead) for all search operations.

mod cursor;
mod healthcheck;
mod output;
mod server;
mod update_check;

use clap::Parser;
use fff::file_picker::FilePicker;
use fff::frecency::FrecencyTracker;
use fff::{FFFMode, SharedFilePicker, SharedFrecency};
use git2::Repository;
use mimalloc::MiMalloc;
use rmcp::{ServiceExt, transport::stdio};
use server::FffServer;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

pub const MCP_INSTRUCTIONS: &str = concat!(
    "FFF is a fast file finder with frecency-ranked results (frequent/recent files first, git-dirty files boosted).\n",
    "\n",
    "## Which Tool Should I Use?\n",
    "\n",
    "- **grep**: DEFAULT tool. Searches file CONTENTS -- definitions, usage, patterns. Use when you have a specific name or pattern.\n",
    "- **find_files**: Explores which files/modules exist for a topic. Use when you DON'T have a specific identifier or LOOKING FOR A FILE.\n",
    "- **multi_grep**: OR logic across multiple patterns. Use for case variants (e.g. ['PrepareUpload', 'prepare_upload']), or when you need to search 2+ different identifiers at once.\n",
    "\n",
    "## Core Rules\n",
    "\n",
    "### 1. Search BARE IDENTIFIERS only\n",
    "Grep matches single lines. Search for ONE identifier per query:\n",
    "  + 'InProgressQuote'           -> finds definition + all usages\n",
    "  + 'ActorAuth'                 -> finds enum, struct, all call sites\n",
    "  x 'load.*metadata.*InProgressQuote' -> regex spanning multiple tokens, 0 results\n",
    "  x 'ctx.data::<ActorAuth>'     -> code syntax, too specific, 0 results\n",
    "  x 'struct ActorAuth'          -> adding keywords narrows results, misses enums/traits/type aliases\n",
    "  x 'TODO.*#\\d+'               -> complex regex, use simple 'TODO' then filter visually\n",
    "\n",
    "### 2. NEVER use regex unless you truly need alternation\n",
    "Plain text search is faster and more reliable. Regex patterns like `.*`, `\\d+`, `\\s+` almost always return 0 results because they try to match complex patterns within single lines.\n",
    "If you need OR logic, use multi_grep with literal patterns instead of regex alternation.\n",
    "\n",
    "### 3. Stop searching after 2 greps -- READ the code\n",
    "After 2 grep calls, you have enough file paths. Read the top result to understand the code.\n",
    "Do NOT keep grepping with variations. More greps != better understanding.\n",
    "\n",
    "### 4. Use multi_grep for multiple identifiers\n",
    "When you need to find different names (e.g. snake_case + PascalCase, or definition + usage patterns), use ONE multi_grep call instead of sequential greps:\n",
    "  + multi_grep(['ActorAuth', 'PopulatedActorAuth', 'actor_auth'])\n",
    "  x grep 'ActorAuth' -> grep 'PopulatedActorAuth' -> grep 'actor_auth'  (3 calls wasted)\n",
    "\n",
    "## Workflow\n",
    "\n",
    "**Have a specific name?** -> grep the bare identifier.\n",
    "**Need multiple name variants?** -> multi_grep with all variants in one call.\n",
    "**Exploring a topic / finding files?** -> find_files.\n",
    "**Got results?** -> Read the top file. Don't grep again.\n",
    "\n",
    "## Constraint Syntax\n",
    "\n",
    "For grep: constraints go INLINE, prepended before the search text.\n",
    "For multi_grep: constraints go in the separate 'constraints' parameter.\n",
    "\n",
    "Constraints MUST match one of these formats:\n",
    "  Extension: '*.rs', '*.{ts,tsx}'\n",
    "  Directory: 'src/', 'quotes/'\n",
    "  Filename: 'schema.rs', 'src/main.rs'\n",
    "  Exclude: '!test/', '!*.spec.ts'\n",
    "\n",
    "! Bare words without extensions are NOT constraints. 'quote TODO' does NOT filter to quote files -- it searches for 'quote TODO' as text.\n",
    "  + 'schema.rs TODO'   -> searches for 'TODO' in files schema.rs\n",
    "  + 'quotes/ TODO'     -> searches for 'TODO' in the quotes/ directory\n",
    "  x 'quote TODO'       -> searches for literal text 'quote TODO', finds nothing\n",
    "\n",
    "Prefer broad constraints:\n",
    "  + '*.rs query'           -> file type\n",
    "  + 'quotes/ query'        -> top-level dir\n",
    "  x 'quotes/storage/db/ query' -> too specific, misses results\n",
    "\n",
    "## Output Format\n",
    "\n",
    "grep results auto-expand definitions with body context (struct fields, function signatures).\n",
    "This often provides enough information WITHOUT a follow-up Read call.\n",
    "Lines marked with | are definition body context. [def] marks definition files.\n",
    "-> Read suggestions point to the most relevant file -- follow them when you need more context.\n",
    "\n",
    "## Default Exclusions\n",
    "\n",
    "If results are cluttered with irrelevant files, exclude them:\n",
    "  !tests/ - exclude tests directory\n",
    "  !*.spec.ts - exclude test files\n",
    "  !generated/ - exclude generated code",
);

/// FFF MCP Server — high-performance file finder for AI code assistants.
#[derive(Parser)]
#[command(name = "fff-mcp", version = concat!(env!("CARGO_PKG_VERSION"), " (", env!("FFF_GIT_HASH"), ")"))]
pub(crate) struct Args {
    /// Base directory to index. Defaults to the current working directory.
    #[arg(value_name = "PATH")]
    base_path: Option<String>,

    /// Path to the frecency database.
    #[arg(long = "frecency-db")]
    frecency_db_path: Option<String>,

    /// Path to the query history database.
    #[arg(long = "history-db")]
    #[allow(dead_code)]
    history_db_path: Option<String>,

    /// Path to the log file.
    #[arg(long = "log-file")]
    log_file: Option<String>,

    /// Log level (e.g. trace, debug, info, warn, error).
    #[arg(long = "log-level")]
    log_level: Option<String>,

    /// Disable automatic update checks on startup.
    #[arg(long = "no-update-check")]
    no_update_check: bool,

    /// Disable eager mmap warmup after the initial scan. Grep results will
    /// still work (files are mmap'd lazily on first access), but the first
    /// search may be slightly slower. Useful on very large repos where the
    /// warmup would consume too many kernel resources.
    #[arg(long = "no-warmup")]
    no_warmup: bool,

    /// Disable the content index built after the initial scan.
    /// This makes grep calls slower but consumes less RAM (recommended to not turn off)
    // Pre-existing bug fix (discovered by axis-D tests): without an explicit
    // `long` attribute clap treats this as a positional argument and fails a
    // debug_assert in test builds. The CLI surface continues to advertise
    // `--no-content-indexing`; release builds happened to silently accept it
    // as an unrecognised flag rather than honouring it.
    #[arg(long = "no-content-indexing")]
    no_content_indexing: bool,

    /// Explicitly enable content indexing even when `--no-warmup` is set.
    #[arg(long = "content-indexing")]
    content_indexing: bool,

    /// Disable the background file-system watcher. Files are scanned once
    /// at startup but not monitored for changes.
    #[arg(long = "no-watch")]
    no_watch: bool,

    /// Maximum number of files whose content is kept persistently in memory.
    /// Files beyond this limit are still searchable via temporary mmaps that
    /// are released after each grep. Defaults to 30 000.
    /// Also settable via the FFF_MAX_CACHED_FILES environment variable.
    #[arg(long = "max-cached-files", env = "FFF_MAX_CACHED_FILES")]
    max_cached_files: Option<usize>,

    /// Refuse to start if the initial filesystem walk discovers more than this
    /// many files. Defaults to 1 000 000, which catches accidental indexing of
    /// home directories or umbrella directories without firing on legitimately
    /// large repositories (typical IF Desktop worktree ≈ 690 K files). Set to
    /// 0 to disable. Also settable via FFF_MAX_INDEXABLE_FILES.
    #[arg(
        long = "max-indexable-files",
        env = "FFF_MAX_INDEXABLE_FILES",
        default_value_t = 1_000_000
    )]
    max_indexable_files: usize,

    /// Refuse to start if the resolved base path is not a git working
    /// directory. Recommended for MCP launches so a Claude Code session
    /// opened in a non-repo directory does not silently start indexing the
    /// user's home tree. Default off in the binary; turn on via MCP config.
    /// Also settable via FFF_REQUIRE_GIT_ROOT=1.
    #[arg(long = "require-git-root", env = "FFF_REQUIRE_GIT_ROOT")]
    require_git_root: bool,

    /// Only allow indexing if the resolved base path is a descendant of one
    /// of these prefixes. Repeatable. Example:
    /// `--root-must-be-under /c/WORK --root-must-be-under /home/user/repos`.
    #[arg(long = "root-must-be-under")]
    root_must_be_under: Vec<String>,

    /// Validate the resolved base path against `--require-git-root` /
    /// `--root-must-be-under` rules, then exit 0 (or exit 1 with a JSON line
    /// on stderr if any check fails). Does not run the file walk; intended
    /// for wrapper scripts that probe a candidate base path before launching
    /// the real server.
    #[arg(long = "validate-root")]
    validate_root: bool,

    /// Run a health check and print diagnostic information, then exit.
    #[arg(long = "healthcheck")]
    pub(crate) healthcheck: bool,
}

/// Machine-parseable startup error written to stderr (one JSON object per
/// line) before `std::process::exit(1)`. The MCP host (Claude Code) captures
/// stderr on process failure and can display `detail` directly. The `code`
/// field is a stable identifier; `detail` is for human display and may change.
#[derive(serde::Serialize)]
struct StartupError<'a> {
    event: &'a str,
    code: &'a str,
    base_path: &'a str,
    detail: &'a str,
}

/// Emit a structured startup error and exit non-zero. Writes JSON to stderr
/// followed by a human-readable line so both machine and human readers see the
/// failure. Called BEFORE `init_tracing` so the JSON line is never interleaved
/// with tracing-subscriber output.
fn emit_startup_error_and_exit(code: &str, base_path: &str, detail: &str) -> ! {
    let err = StartupError {
        event: "fff_startup_error",
        code,
        base_path,
        detail,
    };
    if let Ok(json) = serde_json::to_string(&err) {
        eprintln!("{json}");
    }
    eprintln!("fff-mcp: {code}: {detail} (base_path={base_path})");
    std::process::exit(1);
}

/// Resolve default paths for the log file.
/// Database paths (frecency, history) must be explicitly provided via flags.
fn resolve_defaults(args: &mut Args) {
    // Ensure parent directories exist for database paths when provided
    for path in [&args.frecency_db_path, &args.history_db_path]
        .into_iter()
        .flatten()
    {
        if let Some(parent) = std::path::Path::new(path).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
    }

    if args.log_file.is_none() {
        let home = dirs_home();
        let pid = std::process::id();
        let is_windows = cfg!(target_os = "windows");
        let path = if is_windows {
            format!("{}\\AppData\\Local\\fff_mcp-{pid}.log", home)
        } else {
            format!("{}/.cache/fff_mcp-{pid}.log", home)
        };

        // Maintain a `fff_mcp-latest.log` symlink for `tail -F` monitoring.
        // Atomic swap via `rename(2)` so concurrent readers never see a
        // broken symlink. Best-effort: silent failures on platforms without
        // unix symlinks or when the cache dir is not writable.
        #[cfg(unix)]
        {
            let latest = format!("{}/.cache/fff_mcp-latest.log", home);
            let tmp = format!("{}/.cache/.fff_mcp-latest.log.{pid}.tmp", home);
            let _ = std::fs::remove_file(&tmp);
            if std::os::unix::fs::symlink(&path, &tmp).is_ok() {
                let _ = std::fs::rename(&tmp, &latest);
            }
        }

        args.log_file = Some(path);
    }
}

fn dirs_home() -> String {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| "/tmp".to_string())
}

/// Resolve the user-supplied base path against the optional git workdir.
/// Returns `(resolved_base_path, has_git_root)`. Pure function (no logging /
/// no global state) so it can run BEFORE `init_tracing` for the startup-error
/// JSON path.
fn resolve_base_path(raw: &str) -> (String, bool) {
    match Repository::discover(raw) {
        Ok(repo) => match repo.workdir() {
            Some(workdir) => (workdir.to_string_lossy().to_string(), true),
            // Bare repo — treat as "has git" but use the original path since
            // there is no workdir to index.
            None => (raw.to_string(), true),
        },
        Err(_) => (raw.to_string(), false),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = Args::parse();
    resolve_defaults(&mut args);

    if args.healthcheck {
        return healthcheck::run_healthcheck(&args);
    }

    // Compute base_path BEFORE init_tracing so a structured startup error
    // can be emitted on stderr without tracing-subscriber output being
    // interleaved with the JSON line (per PLAN-review NB3).
    let raw_base_path = args.base_path.clone().unwrap_or_else(|| {
        std::env::current_dir()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string()
    });
    let (base_path, has_git_root) = resolve_base_path(&raw_base_path);

    // Guard 1: --require-git-root refuses to start when no git workdir is
    // resolvable from the base path. This prevents accidentally indexing a
    // user's home directory or a non-repo umbrella directory.
    if args.require_git_root && !has_git_root {
        emit_startup_error_and_exit(
            "NO_GIT_ROOT",
            &base_path,
            "no git repository found at or above the base path; \
             pass --no-require-git-root or unset FFF_REQUIRE_GIT_ROOT to index without a git root",
        );
    }

    // Guard 2: --root-must-be-under <prefix> allowlist. If any prefix is
    // configured, the resolved base path MUST be a descendant of at least
    // one of them.
    if !args.root_must_be_under.is_empty() {
        let base = std::path::Path::new(&base_path);
        let allowed = args
            .root_must_be_under
            .iter()
            .any(|prefix| base.starts_with(prefix));
        if !allowed {
            let detail = format!(
                "base path is not under any of the configured allowlist prefixes: {:?}",
                args.root_must_be_under
            );
            emit_startup_error_and_exit("ROOT_NOT_ALLOWED", &base_path, &detail);
        }
    }

    // --validate-root: run only the cheap pre-scan checks above and exit
    // cleanly. Wrapper scripts use this to probe a candidate base path
    // without paying for the full walk.
    if args.validate_root {
        println!(
            "OK: base_path={base_path}, has_git_root={has_git_root}, \
             max_indexable_files={}",
            args.max_indexable_files
        );
        return Ok(());
    }

    let log_file = args.log_file.as_deref().unwrap_or("");
    if let Err(e) = fff::log::init_tracing(log_file, args.log_level.as_deref()) {
        eprintln!("Warning: Failed to init tracing: {}", e);
    }

    if has_git_root {
        tracing::info!("Discovered git root: {}", base_path);
    } else {
        tracing::info!(
            "No git repository found, indexing from base path: {}",
            base_path
        );
    }

    let shared_picker = SharedFilePicker::default();
    let shared_frecency = SharedFrecency::default();
    if let Some(frecency_db_path) = args.frecency_db_path {
        match FrecencyTracker::open(&frecency_db_path) {
            Ok(tracker) => {
                let _ = shared_frecency.init(tracker);
            }
            Err(e) => {
                eprintln!("Warning: Failed to init frecency db: {}", e);
            }
        }
    }

    // Content indexing follows warmup by default (backward compat), unless
    // the user explicitly opts in via --content-indexing or out via
    // --no-content-indexing.
    let enable_content_indexing = if args.content_indexing {
        true
    } else if args.no_content_indexing {
        false
    } else {
        !args.no_warmup
    };

    // Capture base_path for use after the move into FilePickerOptions.
    let base_path_for_error = base_path.clone();
    let max_indexable_files = args.max_indexable_files;

    // Initialize file picker (spawns background scan + watcher)
    FilePicker::new_with_shared_state(
        shared_picker.clone(),
        shared_frecency.clone(),
        fff::FilePickerOptions {
            base_path,
            enable_mmap_cache: !args.no_warmup,
            enable_content_indexing,
            watch: !args.no_watch,
            mode: FFFMode::Ai,
            cache_budget: args
                .max_cached_files
                .map(fff::ContentCacheBudget::new_for_repo),
            follow_symlinks: false,
            max_indexable_files,
        },
    )
    .map_err(|e| format!("Failed to init file picker: {}", e))?;

    if !args.no_update_check {
        update_check::spawn_update_check();
    }

    // Create and start the MCP server
    let server = FffServer::new(shared_picker.clone(), shared_frecency.clone());

    // Wait for initial scan in background. When the scan completes we also
    // check whether it failed fatally (e.g. `Error::TooManyFiles`); on
    // failure we emit a structured stderr error and exit non-zero so the MCP
    // host (Claude Code) sees the cause rather than a silent empty index.
    let picker_clone_for_scan = shared_picker.clone();
    let base_path_for_error_clone = base_path_for_error.clone();
    tokio::task::spawn_blocking(move || {
        let start = std::time::Instant::now();
        loop {
            let (is_scanning, scan_error) = picker_clone_for_scan
                .read()
                .ok()
                .and_then(|g| {
                    g.as_ref().map(|p| (p.is_scan_active(), p.scan_error()))
                })
                .unwrap_or((true, None));

            if !is_scanning {
                if let Some(detail) = scan_error {
                    // The scan thread already logged via tracing; surface a
                    // machine-parseable error and terminate the process.
                    let code = if detail.starts_with("Too many files") {
                        "TOO_MANY_FILES"
                    } else {
                        "SCAN_FAILED"
                    };
                    emit_startup_error_and_exit(
                        code,
                        &base_path_for_error_clone,
                        &detail,
                    );
                }
                tracing::info!("Initial scan completed in {:?}", start.elapsed());
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    });

    let service = server
        .serve(stdio())
        .await
        .map_err(|e| format!("Failed to start MCP server: {}", e))?;

    let picker_for_shutdown = shared_picker.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        if let Ok(mut guard) = picker_for_shutdown.write()
            && let Some(ref mut picker) = *guard
        {
            picker.stop_background_monitor();
        }
        std::process::exit(0);
    });

    service.waiting().await?;

    if let Ok(mut guard) = shared_picker.write()
        && let Some(ref mut picker) = *guard
    {
        picker.stop_background_monitor();
    }

    Ok(())
}
