use crate::Args;
use git2::Repository;

fn check(label: &str, ok: bool, detail: &str) -> bool {
    let marker = if ok { "+" } else { "x" };
    println!("  [{marker}] {label}: {detail}");
    ok
}

fn warn(label: &str, detail: &str) {
    println!("  [!] {label}: {detail}");
}

pub fn run_healthcheck(args: &Args) -> Result<(), Box<dyn std::error::Error>> {
    let version = concat!(env!("CARGO_PKG_VERSION"), " (", env!("FFF_GIT_HASH"), ")");
    println!("fff-mcp {version}\n");

    let mut all_ok = true;

    // 1. Base path
    let base_path = args.base_path.clone().unwrap_or_else(|| {
        std::env::current_dir()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string()
    });

    let path_exists = std::path::Path::new(&base_path).is_dir();
    all_ok &= check(
        "Base path",
        path_exists,
        if path_exists {
            &base_path
        } else {
            "directory does not exist"
        },
    );

    // 2. Git repository — `--require-git-root` promotes "not found" to a
    //    hard failure so `--healthcheck --require-git-root` accurately
    //    predicts whether the real startup would refuse to run.
    match Repository::discover(&base_path) {
        Ok(repo) => {
            if let Some(workdir) = repo.workdir() {
                all_ok &= check("Git repository", true, &format!("{}", workdir.display()));
            } else {
                all_ok &= check("Git repository", true, "bare repository");
            }
        }
        Err(_) => {
            if args.require_git_root {
                all_ok &= check(
                    "Git repository",
                    false,
                    "not found and --require-git-root is set (startup would refuse)",
                );
            } else {
                warn(
                    "Git repository",
                    "not found (fff-mcp will still work, but git-status features are disabled)",
                );
            }
        }
    }

    // 2b. Root allowlist — when `--root-must-be-under` is configured the
    //     resolved base path must be under at least one prefix; mirror the
    //     check the binary will run at startup.
    if !args.root_must_be_under.is_empty() {
        let base = std::path::Path::new(&base_path);
        let allowed = args
            .root_must_be_under
            .iter()
            .any(|prefix| base.starts_with(prefix));
        all_ok &= check(
            "Root allowlist",
            allowed,
            if allowed {
                "base path is under a configured prefix"
            } else {
                "base path is not under any configured prefix (startup would refuse)"
            },
        );
    }

    // 3. Frecency database
    if let Some(ref db_path) = args.frecency_db_path {
        let parent_ok = std::path::Path::new(db_path)
            .parent()
            .is_some_and(|p| p.is_dir());
        all_ok &= check(
            "Frecency DB",
            parent_ok,
            if parent_ok {
                db_path
            } else {
                "parent directory does not exist"
            },
        );
    } else {
        check("Frecency DB", false, "path not resolved");
    }

    // 4. Query history database
    if let Some(ref db_path) = args.history_db_path {
        let parent_ok = std::path::Path::new(db_path)
            .parent()
            .is_some_and(|p| p.is_dir());
        all_ok &= check(
            "History DB",
            parent_ok,
            if parent_ok {
                db_path
            } else {
                "parent directory does not exist"
            },
        );
    } else {
        check("History DB", false, "path not resolved");
    }

    // 5. Log file
    if let Some(ref log_path) = args.log_file {
        let parent_ok = std::path::Path::new(log_path)
            .parent()
            .is_some_and(|p| p.is_dir());
        all_ok &= check(
            "Log file",
            parent_ok,
            if parent_ok {
                log_path
            } else {
                "parent directory does not exist"
            },
        );
    } else {
        check("Log file", false, "path not resolved");
    }

    if all_ok {
        println!("All checks passed.");
        Ok(())
    } else {
        Err("Some checks failed — review the items marked [x] above.".into())
    }
}
