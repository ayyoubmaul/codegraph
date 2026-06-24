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

/// Walk `root`, honoring `.gitignore`, returning every file with a supported
/// language.
pub fn collect_files(root: &Path) -> Vec<SourceFile> {
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
        // Skip oversized files (minified/generated/blobs).
        if entry.metadata().is_ok_and(|m| m.len() > MAX_FILE_BYTES) {
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        files.push(SourceFile {
            path: path.to_path_buf(),
            rel,
            lang,
        });
    }
    files
}
