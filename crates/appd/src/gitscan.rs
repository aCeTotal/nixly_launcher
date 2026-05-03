use std::path::{Path, PathBuf};
use std::thread;
use std::time::SystemTime;

use calloop::channel::Sender;
use walkdir::WalkDir;

use crate::mounts;

#[derive(Debug, Clone)]
pub struct GitProject {
    pub path: PathBuf,
    pub mtime: u64,
}

const MAX_DEPTH: usize = 10;

// Same philosophy as fileindex: hidden dirs are no longer blanket-skipped so
// dotfile repositories (`~/.dotfiles`, `~/.config/some-repo`, etc.) are found.
// Two classes of skip:
//   - Build / cache / app-data dirs (heavy + uninteresting)
//   - Vendored-collection dirs (parents that hold cloned-but-not-developed
//     repos — RAG/dataset checkouts, vendored deps, archives, backups). The
//     individual repos inside aren't structurally nested in another git, so
//     `strip_nested` can't filter them; their parent's NAME is the only
//     reliable heuristic we have.
const SKIP_DIRS: &[&str] = &[
    // Build / dependency caches
    "node_modules",
    "target",
    "build",
    "dist",
    "Trash",
    ".cache",
    ".cargo",
    ".rustup",
    ".npm",
    ".gradle",
    ".m2",
    ".gem",
    ".pnpm-store",
    ".yarn",
    ".bundle",
    ".cabal",
    ".stack",
    ".tox",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".venv",
    "venv",
    "__pycache__",
    // Heavy app data
    ".local",
    ".mozilla",
    ".thunderbird",
    ".steam",
    ".vscode",
    ".vscode-server",
    ".cursor-server",
    ".thumbnails",
    // Vendored-collection parent dirs — repos inside these are typically
    // cloned-for-reference, not actively developed.
    "curated",
    "vendor",
    "vendored",
    "third_party",
    "third-party",
    "deps",
    "submodules",
    "subprojects",
    "external",
    "externals",
    "archive",
    "archives",
    "backup",
    "backups",
    "clones",
];

pub fn spawn(tx: Sender<Vec<GitProject>>) {
    thread::Builder::new()
        .name("gitscan".into())
        .spawn(move || {
            let roots = mounts::walk_roots();
            log::info!("gitscan: roots = {:?}", roots);
            let mut accumulated: Vec<GitProject> = Vec::new();
            for root in &roots {
                log::info!("gitscan: walking {}", root.display());
                let chunk = scan_one(root);
                log::info!("gitscan: +{} raw from {}", chunk.len(), root.display());
                accumulated.extend(chunk);
                let filtered = strip_nested(&accumulated);
                log::info!(
                    "gitscan: {} top-level after nested-filter (was {})",
                    filtered.len(),
                    accumulated.len()
                );
                if tx.send(filtered).is_err() {
                    return;
                }
            }
            log::info!("gitscan: done");
        })
        .ok();
}

// Drops nested git projects so vendored deps / submodules / archived
// backup copies don't pollute the list — only the outermost project at
// each path prefix wins. Keep a sibling's checkout in-tact, only prune
// when one project's path is inside another's.
fn strip_nested(projects: &[GitProject]) -> Vec<GitProject> {
    let mut by_depth: Vec<&GitProject> = projects.iter().collect();
    by_depth.sort_by_key(|p| p.path.components().count());
    let mut keep: Vec<GitProject> = Vec::new();
    for p in by_depth {
        if !keep.iter().any(|k| p.path.starts_with(&k.path)) {
            keep.push(p.clone());
        }
    }
    keep.sort_by(|a, b| b.mtime.cmp(&a.mtime));
    keep
}

fn scan_one(root: &PathBuf) -> Vec<GitProject> {
    let mut projects: Vec<GitProject> = Vec::new();
    let mut iter = WalkDir::new(root)
        .max_depth(MAX_DEPTH)
        .follow_links(false)
        .into_iter();

    loop {
        let entry = match iter.next() {
            None => break,
            Some(Err(_)) => continue,
            Some(Ok(e)) => e,
        };
        if !entry.file_type().is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if entry.depth() > 0 {
            if name.starts_with(".Trash-")
                || SKIP_DIRS.iter().any(|d| **d == name)
            {
                iter.skip_current_dir();
                continue;
            }
        }
        if name == ".git" {
            if let Some(parent) = entry.path().parent() {
                let mtime = git_mtime(entry.path());
                projects.push(GitProject {
                    path: parent.to_path_buf(),
                    mtime,
                });
            }
            // Don't descend into the .git internals — there's nothing
            // recursable inside, and old-style submodules with
            // `.git/modules/<name>/` could otherwise look like nested repos.
            iter.skip_current_dir();
        }
    }
    projects
}

fn git_mtime(git_dir: &Path) -> u64 {
    let head = git_dir.join("HEAD");
    let fetch = git_dir.join("FETCH_HEAD");
    let candidates = [Some(head), Some(fetch), None];
    let dir_meta = std::fs::metadata(git_dir).ok();
    candidates
        .into_iter()
        .filter_map(|p| match p {
            Some(p) => std::fs::metadata(&p).ok(),
            None => dir_meta.clone(),
        })
        .filter_map(|m| m.modified().ok())
        .filter_map(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .max()
        .unwrap_or(0)
}
