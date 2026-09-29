//! Deterministic file inventory for navigation.
//!
//! Uses the `ignore` crate's walker so `.gitignore` semantics match what
//! ripgrep and the existing `intelligence::scanner` already do — a navigation
//! index that disagrees with what the team can actually see is worse than none.

use crate::model::{FileFacts, Language};
use anyhow::Result;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Maximum size we will parse. Larger files are inventoried but marked
/// unsupported-by-policy rather than silently skipped, so the UI can say why.
const MAX_PARSE_BYTES: u64 = 2 * 1024 * 1024;

/// One inventoried file: its identity, content hash and raw bytes (when read).
#[derive(Debug, Clone)]
pub struct ScannedFile {
    pub repo_relative: String,
    pub absolute: PathBuf,
    pub language: Language,
    pub content_hash: String,
    pub bytes: Option<Vec<u8>>,
    /// Why bytes are absent (size policy, read error).
    pub read_error: Option<String>,
}

/// Convert an absolute path to a repository-relative, `/`-separated id.
pub fn repo_relative(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let mut out = String::new();
    for comp in rel.components() {
        match comp {
            std::path::Component::Normal(s) => {
                if !out.is_empty() {
                    out.push('/');
                }
                out.push_str(&s.to_string_lossy());
            }
            _ => return None,
        }
    }
    Some(out)
}

/// Walk the repository and return every source file we have a grammar for.
///
/// Files with no grammar are not returned: reporting them would only produce
/// `UNSUPPORTED` noise. The caller surfaces unsupported *languages* separately
/// via [`supported_languages`].
pub fn scan(root: &Path, read_bytes: bool) -> Result<Vec<ScannedFile>> {
    let mut out = Vec::new();
    let walker = ignore::WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .parents(true)
        .build();

    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let path = entry.path();
        let rel = match repo_relative(root, path) {
            Some(r) => r,
            None => continue,
        };
        // Never index the index.
        if rel.starts_with(".zylcode/") || rel.contains("/.zylcode/") {
            continue;
        }
        let language = Language::from_path(&rel);
        if !language.is_supported() {
            continue;
        }
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.len() > MAX_PARSE_BYTES {
            out.push(ScannedFile {
                repo_relative: rel,
                absolute: path.to_path_buf(),
                language,
                content_hash: String::new(),
                bytes: None,
                read_error: Some(format!(
                    "file is {} bytes; navigation parses files up to {} bytes",
                    meta.len(),
                    MAX_PARSE_BYTES
                )),
            });
            continue;
        }
        let bytes = std::fs::read(path).ok();
        let (content_hash, read_error) = match &bytes {
            Some(b) => (hash_bytes(b), None),
            None => (String::new(), Some("could not read file".to_string())),
        };
        out.push(ScannedFile {
            repo_relative: rel,
            absolute: path.to_path_buf(),
            language,
            content_hash,
            bytes: if read_bytes { bytes } else { None },
            read_error,
        });
    }
    out.sort_by(|a, b| a.repo_relative.cmp(&b.repo_relative));
    Ok(out)
}

/// SHA-256 of raw bytes, lowercase hex.
pub fn hash_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// Decode a scanned file's bytes, reporting why that is impossible.
pub fn read_source(file: &ScannedFile) -> Result<String, String> {
    match std::fs::read(&file.absolute) {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(s) => Ok(s),
            Err(_) => Err("file is not valid UTF-8".to_string()),
        },
        Err(e) => Err(format!("could not read file: {e}")),
    }
}

/// Inventory wrapper handed back to callers that need both facts and policy
/// notes (unsupported-language counts, oversized files).
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct Inventory {
    pub facts: Vec<FileFacts>,
    /// Files whose extension has no grammar, counted by extension.
    pub unsupported_by_extension: Vec<(String, usize)>,
    /// Files skipped by policy, with the reason.
    pub skipped: Vec<(String, String)>,
}

/// Language labels currently materialised by this wave.
pub fn supported_languages() -> Vec<&'static str> {
    vec!["rust", "typescript", "javascript"]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_from_path() {
        assert_eq!(Language::from_path("src/main.rs"), Language::Rust);
        assert_eq!(Language::from_path("a/b.tsx"), Language::TypeScript);
        assert_eq!(Language::from_path("a\\b.tsx"), Language::TypeScript);
        assert_eq!(Language::from_path("x.jsx"), Language::JavaScript);
        assert!(matches!(
            Language::from_path("docs/readme.md"),
            Language::Unsupported(_)
        ));
        assert!(!Language::from_path("src/main").is_supported());
    }

    #[test]
    fn hash_is_stable() {
        assert_eq!(hash_bytes(b"abc"), hash_bytes(b"abc"));
        assert_ne!(hash_bytes(b"abc"), hash_bytes(b"abd"));
        assert_eq!(hash_bytes(b"").len(), 64);
    }

    #[test]
    fn relative_paths_are_posix() {
        let root = Path::new("/tmp/repo");
        assert_eq!(
            repo_relative(root, Path::new("/tmp/repo/src/lib.rs")).as_deref(),
            Some("src/lib.rs")
        );
    }
}
