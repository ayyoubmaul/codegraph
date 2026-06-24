//! Gitignore-aware file discovery.

use std::path::{Path, PathBuf};

use ignore::WalkBuilder;

use crate::lang::Lang;

/// Directories pruned regardless of `.gitignore` (dependency/build/output dirs
/// that bloat an index, especially when indexing a parent folder of repos).
/// Hidden dirs (`.git`, `.venv`, …) are already skipped by the walker's default.
const SKIP_DIRS: &[&str] = &[
    "node_modules",
    "target",
    "dist",
    "build",
    "out",
    "vendor",
    "venv",
    "__pycache__",
    "coverage",
    "bin",
    "obj",
    "Pods",
    "DerivedData",
];

/// Files larger than this are skipped (minified bundles, generated code, blobs).
const MAX_FILE_BYTES: u64 = 512 * 1024;

/// A discovered source file we know how to parse.
pub struct SourceFile {
    pub path: PathBuf,
    /// Repo-relative path with forward slashes.
    pub rel: String,
    pub lang: Lang,
}

/// The workspace prefix for a repo path: its (canonicalized) directory name.
pub fn repo_name(path: &Path) -> String {
    path.canonicalize()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "repo".to_string())
}

/// Walk `root`, honoring `.gitignore`, returning every supported-language file.
/// Paths are qualified with `repo/` so ids stay unique across repos in a
/// multi-repo workspace.
pub fn collect_files(root: &Path, repo: &str) -> Vec<SourceFile> {
    let mut files = Vec::new();
    let walker = WalkBuilder::new(root)
        // Prune denylisted directories (don't even descend into them).
        .filter_entry(|e| {
            if e.depth() > 0 && e.file_type().is_some_and(|t| t.is_dir()) {
                if let Some(name) = e.file_name().to_str() {
                    if SKIP_DIRS.contains(&name) {
                        return false;
                    }
                }
            }
            true
        })
        .build();

    for result in walker {
        let entry = match result {
            Ok(e) => e,
            Err(_) => continue,
        };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.path();
        let Some(lang) = Lang::from_path(path) else {
            continue;
        };
        // Skip minified/bundled files (everything on a few lines → garbage symbols).
        if is_minified(path) {
            continue;
        }
        // Skip oversized files (generated code, blobs).
        if entry.metadata().is_ok_and(|m| m.len() > MAX_FILE_BYTES) {
            continue;
        }
        let stripped = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        let rel = format!("{repo}/{stripped}");
        files.push(SourceFile {
            path: path.to_path_buf(),
            rel,
            lang,
        });
    }
    files
}

/// Minified/bundled files (`*.min.js`, `*.bundle.js`, …) put everything on a few
/// lines, producing garbage symbols — skip them.
fn is_minified(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|name| name.contains(".min.") || name.ends_with(".bundle.js"))
}
