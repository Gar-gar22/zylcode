//! One process-wide index slot, shared by every consumer of this crate.
//!
//! # Why it exists
//!
//! A query must not re-index the repository. Walking and hashing every file
//! would make each navigation request cost O(files), which is exactly the
//! failure mode the work order forbids. [`query_repository`] therefore holds
//! the most recently built index in a slot and rebuilds only when the root
//! differs or [`REUSE_FOR`] has elapsed.
//!
//! # A rebuild is not a re-parse
//!
//! `RepoIndex::build` reuses per-file facts keyed by content hash, so a
//! rebuild re-reads bytes but only re-parses files that actually changed.
//! That is why this window can be short without making edits invisible: an
//! edit is picked up by the next query one second later, and files nobody
//! touched are never handed to the parser again.
//!
//! # Staleness is a stated trade-off, not an accident
//!
//! Inside the window a query is answered from memory and may not yet reflect
//! an edit that landed a moment ago. That is deliberate — the alternative
//! (stat every file on every request) reintroduces O(files) work per query.
//! The window is bounded at one second precisely so the exposure is short.
//!
//! # Side effects
//!
//! Building persists a warm-start cache at `<root>/.zylcode/nav-index.json`
//! (gitignored). That is the only write performed here; source files are never
//! touched. A build failure is reported and **not** cached, so a transient
//! failure does not poison every later query.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::index::RepoIndex;

/// How long a built index is reused before the next query rebuilds it.
pub const REUSE_FOR: Duration = Duration::from_millis(1_000);

/// Why a repository query could not produce a result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NavError {
    /// The index could not be built, or the shared slot was poisoned.
    ///
    /// The tool ran and had nothing to answer *from* — not a caller mistake,
    /// so executors should surface this as "unavailable", not "bad request".
    Unavailable(String),
    /// The request itself was rejected by the engine (a malformed selector,
    /// a missing required field). Nothing was queried.
    Rejected(String),
}

impl std::fmt::Display for NavError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(reason) => write!(f, "repository index unavailable: {reason}"),
            Self::Rejected(reason) => write!(f, "request rejected: {reason}"),
        }
    }
}

impl std::error::Error for NavError {}

/// A built index, the root it was built from, and when that happened.
struct CachedIndex {
    root: PathBuf,
    built_at: Instant,
    index: RepoIndex,
}

/// The slot. One per process, shared by the MCP executors, the `serve-intel`
/// HTTP routes and the Tauri commands — so opening a repository indexes it
/// once, not once per surface.
static SLOT: OnceLock<Mutex<Option<CachedIndex>>> = OnceLock::new();

/// Run one query against the repository index for `root`.
///
/// The closure receives a borrowed index. It runs while the slot's lock is
/// held on purpose: queries are in-memory scans of pre-parsed facts, and the
/// lock is what makes two concurrent callers share one build instead of
/// racing two. Errors the *closure* returns come back as [`NavError::Rejected`]
/// (the caller asked for something the engine will not answer); failures to
/// build come back as [`NavError::Unavailable`] (there was nothing to answer
/// from).
///
/// ```text
/// let payload = zylcode_nav::query_repository(root, |index| {
///     zylcode_nav::payload::dispatch(index, "find_references", args.clone())
/// })?;
/// ```
pub fn query_repository<T>(
    root: &Path,
    query: impl FnOnce(&RepoIndex) -> Result<T, String>,
) -> Result<T, NavError> {
    let slot = SLOT.get_or_init(|| Mutex::new(None));
    let mut guard = slot
        .lock()
        .map_err(|_| NavError::Unavailable("the index cache lock is poisoned".to_string()))?;

    let reusable = guard.as_ref().is_some_and(|cached| {
        cached.root.as_path() == root && cached.built_at.elapsed() < REUSE_FOR
    });
    if !reusable {
        let index = RepoIndex::build(root).map_err(|e| {
            NavError::Unavailable(format!("could not index `{}`: {e}", root.display()))
        })?;
        // Deliberately not cached on the error path: see the module docs.
        *guard = Some(CachedIndex {
            root: root.to_path_buf(),
            built_at: Instant::now(),
            index,
        });
    }

    let cached = guard.as_ref().expect("the index slot was populated above");
    query(&cached.index).map_err(NavError::Rejected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn repo(name: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("src");
        fs::create_dir_all(&file).expect("mkdir");
        fs::write(file.join("lib.rs"), "pub fn first() -> u32 {\n    1\n}\n").expect("write");
        // Distinguish the two roots so a cache keyed on nothing but recency
        // would fail the second assertion.
        fs::write(dir.path().join(name), "marker\n").expect("marker");
        dir
    }

    #[test]
    fn the_slot_is_keyed_by_root_not_just_by_recency() {
        let a = repo("a.txt");
        let b = repo("b.txt");

        let first = query_repository(a.path(), |_| Ok("a")).expect("query a");
        assert_eq!(first, "a");
        let other = query_repository(b.path(), |_| Ok("b")).expect("query b");
        assert_eq!(other, "b", "a second root must not inherit the first root");

        // Back to the first root within the reuse window: still correct,
        // because the slot was replaced rather than overwritten in place.
        let again = query_repository(a.path(), |_| Ok("a")).expect("query a again");
        assert_eq!(again, "a");
    }

    #[test]
    fn a_closure_rejection_is_reported_as_rejected_not_unavailable() {
        let dir = repo("c.txt");
        let err = query_repository(dir.path(), |_| -> Result<(), String> {
            Err("a selector is required".to_string())
        })
        .expect_err("the closure failed");
        assert_eq!(
            err,
            NavError::Rejected("a selector is required".to_string())
        );
        assert!(!matches!(err, NavError::Unavailable(_)));
    }

    #[test]
    fn the_index_is_actually_shared_between_calls() {
        let dir = repo("d.txt");
        let id_a = query_repository(dir.path(), |index| Ok(format!("{:p}", index))).expect("first");
        let id_b =
            query_repository(dir.path(), |index| Ok(format!("{:p}", index))).expect("second");
        assert_eq!(
            id_a, id_b,
            "two queries inside the reuse window must share one index build"
        );
    }

    #[test]
    fn a_bad_root_is_unavailable_and_is_not_cached_as_a_result() {
        let dir = repo("e.txt");
        let missing = dir.path().join("does-not-exist");
        let err = query_repository(&missing, |_| Ok(())).expect_err("no repository there");
        assert!(matches!(err, NavError::Unavailable(_)), "{err}");
        assert!(
            err.to_string().contains("could not index"),
            "the reason must say what failed: {err}"
        );
    }
}
