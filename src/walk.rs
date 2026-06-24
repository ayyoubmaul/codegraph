//! Gitignore-aware file discovery.

use std::path::{Path, PathBuf};

use ignore::WalkBuilder;

use crate::lang::Lang;

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
    for result in WalkBuilder::new(root).build() {
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
