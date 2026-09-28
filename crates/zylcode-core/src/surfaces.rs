//! Search, file-tree, and evidence payload APIs.
//!
//! Same contract as [`crate::gitops`] and [`crate::intelligence::api`]: one
//! function per surface, three consumers (CLI `serve-intel` HTTP routes,
//! Tauri commands, direct callers). Every field comes from real data:
//!
//! * **search** runs the real ranked retriever over the persisted index and
//!   additionally matches file paths (the retriever works on indexed
//!   resources; a path hit with zero symbols must still be findable).
//! * **file tree** is the scanner's actual output — the same tree the
//!   intelligence pipeline indexes, `.gitignore` respected, hidden dirs
//!   skipped.
//! * **evidence** reads the SQLite evidence ledger directly. An absent or
//!   empty ledger is an honest `empty` state, never a fabricated timeline.

use crate::intelligence::persisted::PersistedIndex;
use crate::intelligence::scanner::{scan_repository, ScannerConfig};
use anyhow::Result;
use rusqlite::Connection;
use serde_json::{json, Value};
use std::path::Path;

/// Maximum search results returned.
pub const MAX_SEARCH_RESULTS: usize = 30;

/// Maximum file-tree rows returned (flat, client builds the hierarchy).
pub const MAX_TREE_ROWS: usize = 2000;

/// Maximum evidence entries returned per response.
pub const MAX_EVIDENCE_ENTRIES: usize = 200;

// ---------------------------------------------------------------------------
// Search
// ---------------------------------------------------------------------------

/// Ranked search over the real pipeline: the retriever's indexed resources
/// plus direct file-path matches, deduplicated by resource.
pub fn search_payload(root: &Path, query: &str) -> Result<Value> {
    let query = query.trim();
    anyhow::ensure!(!query.is_empty(), "search query must not be empty");

    let started = std::time::Instant::now();
    let repo_query = PersistedIndex::new(root).build()?;

    let mut results: Vec<Value> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    // 1. The retriever's own ranking (symbols, files, packages).
    for r in repo_query.relevant_context(query) {
        if !seen.insert(r.resource.clone()) {
            continue;
        }
        results.push(json!({
            "resource": r.resource,
            "resource_type": r.resource_type,
            "relevance": r.relevance,
            "reason": r.reason,
        }));
        if results.len() >= MAX_SEARCH_RESULTS {
            break;
        }
    }

    // 2. File-path matches the symbol-oriented retriever may rank below the
    //    cut — a filename hit must still surface.
    let q_lower = query.to_lowercase();
    let tokens: Vec<&str> = q_lower.split_whitespace().collect();
    if results.len() < MAX_SEARCH_RESULTS {
        for f in repo_query.files() {
            let display = display_path(root, &f.path);
            let path_str = display.to_lowercase();
            let name_match = path_str.contains(&q_lower);
            let token_match = !tokens.is_empty() && tokens.iter().all(|t| path_str.contains(t));
            if name_match || token_match {
                let resource = display;
                if seen.insert(format!("file:{resource}")) {
                    results.push(json!({
                        "resource": resource,
                        "resource_type": "file",
                        "relevance": if name_match { 0.75 } else { 0.55 },
                        "reason": if name_match {
                            "path contains the query".to_string()
                        } else {
                            "path contains all query tokens".to_string()
                        },
                    }));
                    if results.len() >= MAX_SEARCH_RESULTS {
                        break;
                    }
                }
            }
        }
    }

    // Deterministic order: relevance desc, then resource asc.
    results.sort_by(|a, b| {
        let ra = a["relevance"].as_f64().unwrap_or(0.0);
        let rb = b["relevance"].as_f64().unwrap_or(0.0);
        rb.partial_cmp(&ra)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                a["resource"]
                    .as_str()
                    .unwrap_or("")
                    .cmp(b["resource"].as_str().unwrap_or(""))
            })
    });

    Ok(json!({
        "query": query,
        "elapsed_ms": started.elapsed().as_secs_f64() * 1000.0,
        "indexed_files": repo_query.file_count(),
        "results": results.into_iter().take(MAX_SEARCH_RESULTS).collect::<Vec<_>>(),
    }))
}

// ---------------------------------------------------------------------------
// File tree
// ---------------------------------------------------------------------------

/// Clean, display-friendly path: strip Windows extended-length prefixes from
/// both the root and the path, then relativize against the root.
fn display_path(root: &Path, path: &Path) -> String {
    let norm = |p: &Path| {
        let s = p.to_string_lossy();
        s.strip_prefix("\\\\?\\").unwrap_or(&s).to_string()
    };
    let root_str = norm(root);
    let path_str = norm(path);
    let root_norm = Path::new(&root_str);
    let path_norm = Path::new(&path_str);
    if let Ok(rel) = path_norm.strip_prefix(root_norm) {
        return rel.to_string_lossy().replace('\\', "/");
    }
    path_str.replace('\\', "/")
}

/// The scanner's real output as a flat row list (the client nests it).
pub fn file_tree_payload(root: &Path) -> Result<Value> {
    anyhow::ensure!(
        root.is_dir(),
        "repository root '{}' is not a directory",
        root.display()
    );
    let started = std::time::Instant::now();
    let scan = scan_repository(root, &ScannerConfig::default())?;

    let mut rows: Vec<Value> = scan
        .files
        .iter()
        .take(MAX_TREE_ROWS)
        .map(|f| {
            json!({
                "path": display_path(root, &f.path),
                "language": f.language.label(),
            })
        })
        .collect();
    rows.sort_by(|a, b| {
        a["path"]
            .as_str()
            .unwrap_or("")
            .cmp(b["path"].as_str().unwrap_or(""))
    });

    Ok(json!({
        "total_scanned": scan.files.len(),
        "truncated": scan.files.len() > MAX_TREE_ROWS,
        "elapsed_ms": started.elapsed().as_secs_f64() * 1000.0,
        "rows": rows,
    }))
}

// ---------------------------------------------------------------------------
// File content (central editor)
// ---------------------------------------------------------------------------

/// Maximum file size served to the editor; larger files are refused honestly.
pub const MAX_FILE_BYTES: u64 = 512 * 1024;

/// Resolve a workspace-relative path to a real file inside `root`.
/// Rejects absolute paths, traversal escapes, and missing files — the
/// editor must never become a filesystem read primitive for arbitrary paths.
fn resolve_in_root(root: &Path, rel: &str) -> Result<std::path::PathBuf> {
    anyhow::ensure!(!rel.trim().is_empty(), "file path must not be empty");
    let rel_path = std::path::Path::new(rel);
    anyhow::ensure!(
        !rel_path.is_absolute(),
        "file path must be relative to the workspace"
    );
    let normalized = rel.replace('\\', "/");
    anyhow::ensure!(
        !normalized.starts_with(".git/"),
        "'.git/*' paths are not openable in the editor"
    );
    let canon_root = root.canonicalize()?;
    // canonicalize errors if the file is missing and resolves symlinks/..,
    // so the containment check below is exact.
    let canon = root.join(rel_path).canonicalize()?;
    anyhow::ensure!(
        canon.starts_with(&canon_root),
        "file path escapes the workspace"
    );
    Ok(canon)
}

/// Real file content for the central editor, with honest limits: the file
/// must exist inside the workspace, be a regular file, and fit the size cap.
/// Non-UTF-8 content is served lossily with `lossy: true` rather than failing.
pub fn file_content_payload(root: &Path, rel_path: &str) -> Result<Value> {
    anyhow::ensure!(
        root.is_dir(),
        "repository root '{}' is not a directory",
        root.display()
    );
    let canon = resolve_in_root(root, rel_path)?;
    let meta = std::fs::metadata(&canon)?;
    anyhow::ensure!(meta.is_file(), "'{}' is a directory, not a file", rel_path);
    anyhow::ensure!(
        meta.len() <= MAX_FILE_BYTES,
        "file is {} bytes; the editor serves at most {} bytes",
        meta.len(),
        MAX_FILE_BYTES
    );
    let bytes = std::fs::read(&canon)?;
    let (content, lossy) = match String::from_utf8(bytes.clone()) {
        Ok(s) => (s, false),
        Err(_) => (String::from_utf8_lossy(&bytes).into_owned(), true),
    };
    Ok(json!({
        "path": rel_path.replace('\\', "/"),
        "size": meta.len(),
        "lines": content.lines().count(),
        "lossy": lossy,
        "content": content,
    }))
}

// ---------------------------------------------------------------------------
// Editor write path (G-03): save + apply-patch backends
// ---------------------------------------------------------------------------

/// Write edited editor content back into the workspace (the desktop editor's
/// save command backend).
///
/// Same containment rules as [`file_content_payload`] plus write-specific
/// hardening:
/// - refuses `.git/*`, absolute paths and `..` *before* touching the fs;
/// - resolves the parent directory (which must exist) and re-checks
///   containment, so the final target path is canonical;
/// - never writes *through* a symlink that resolves outside the workspace;
/// - refuses payloads above [`MAX_FILE_BYTES`] instead of truncating.
pub fn save_file_payload(root: &Path, rel_path: &str, content: &str) -> Result<Value> {
    anyhow::ensure!(
        root.is_dir(),
        "repository root '{}' is not a directory",
        root.display()
    );
    anyhow::ensure!(!rel_path.trim().is_empty(), "file path must not be empty");
    anyhow::ensure!(
        content.len() <= MAX_FILE_BYTES as usize,
        "content is {} bytes; the editor refuses writes above {} bytes",
        content.len(),
        MAX_FILE_BYTES
    );
    let rel = std::path::Path::new(rel_path);
    anyhow::ensure!(
        !rel.is_absolute(),
        "file path must be relative to the workspace"
    );
    anyhow::ensure!(
        !rel.components()
            .any(|c| matches!(c, std::path::Component::ParentDir)),
        "file path must not contain '..'"
    );
    let normalized = rel_path.replace('\\', "/");
    anyhow::ensure!(
        !normalized.starts_with(".git/"),
        "'.git/*' paths are not writable from the editor"
    );

    let canon_root = root.canonicalize()?;
    let dest = root.join(rel);
    let parent = dest
        .parent()
        .ok_or_else(|| anyhow::anyhow!("path '{}' has no parent directory", rel_path))?;
    let canon_parent = parent.canonicalize()?;
    anyhow::ensure!(
        canon_parent.starts_with(&canon_root),
        "file path escapes the workspace"
    );
    let name = dest
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("path '{}' has no file name", rel_path))?;
    let mut target = canon_parent.join(name);
    if let Ok(meta) = std::fs::symlink_metadata(&target) {
        let ft = meta.file_type();
        if ft.is_symlink() {
            // resolve (errors honestly on a dangling link), then re-check
            target = target.canonicalize()?;
            anyhow::ensure!(
                target.starts_with(&canon_root),
                "file path escapes the workspace"
            );
        } else {
            anyhow::ensure!(ft.is_file(), "'{}' is not a regular file", rel_path);
        }
    }
    std::fs::write(&target, content)?;
    let written = std::fs::metadata(&target)?.len();
    Ok(json!({
        "path": normalized,
        "size": written,
        "lines": content.lines().count(),
        "bytes": content.len(),
    }))
}

/// Apply a unified diff (the artifact stream's Ctrl+Enter patch) with
/// `git apply` in the workspace root.
///
/// Every target path is validated *before* git runs: git 2.45 silently
/// rewrites absolute and `../` headers to workspace-relative paths and
/// refuses `.git/` targets itself (all three verified empirically), so
/// ZylCode refuses them up front instead of depending on version-specific
/// git behaviour. The patch is materialised in a temp file *outside* the
/// workspace and applied with `git apply`, which fails closed on any
/// malformed hunk and reports the real stderr — no fuzzy patching, no
/// partial success.
///
/// Line endings are **byte-faithful** (`-c core.autocrlf=false`, verified
/// against git 2.45): an LF file + LF patch stays LF, a CRLF file + CRLF
/// patch stays CRLF, and a cross-EOL patch is refused with git's real
/// error instead of being "fixed" — the default autocrlf path on Windows
/// would otherwise rewrite every untouched line of an LF file to CRLF, and
/// `--ignore-whitespace` leaves mixed-EOL corruption behind. The user's git
/// config can never silently rewrite content the patch did not touch.
pub fn apply_patch_payload(root: &Path, patch: &str) -> Result<Value> {
    anyhow::ensure!(
        root.is_dir(),
        "repository root '{}' is not a directory",
        root.display()
    );
    anyhow::ensure!(!patch.trim().is_empty(), "patch is empty");

    let mut paths: Vec<String> = Vec::new();
    let mut new_paths: Vec<String> = Vec::new();
    for line in patch.lines() {
        let header = if let Some(rest) = line.strip_prefix("+++ ") {
            Some((rest, true))
        } else {
            line.strip_prefix("--- ").map(|rest| (rest, false))
        };
        let Some((rest, is_new)) = header else {
            continue;
        };
        // strip an optional tab-separated timestamp, then the a/ b/ prefix
        let raw = rest.split('\t').next().unwrap_or("").trim();
        if raw.is_empty() || raw == "/dev/null" {
            continue;
        }
        let path = raw
            .strip_prefix("a/")
            .or_else(|| raw.strip_prefix("b/"))
            .unwrap_or(raw);
        let p = std::path::Path::new(path);
        anyhow::ensure!(
            !p.is_absolute() && !path.starts_with('/'),
            "patch targets an absolute path: {path}"
        );
        anyhow::ensure!(
            !p.components()
                .any(|c| matches!(c, std::path::Component::ParentDir)),
            "patch escapes the workspace: {path}"
        );
        let normalized = path.replace('\\', "/");
        anyhow::ensure!(
            !normalized.starts_with(".git/"),
            "patch touches .git/*: refused"
        );
        paths.push(normalized.clone());
        if is_new {
            new_paths.push(normalized);
        }
    }
    anyhow::ensure!(!paths.is_empty(), "patch names no files");
    // report post-image names; pure deletions have only pre-image names
    let targets = if new_paths.is_empty() {
        paths
    } else {
        new_paths
    };

    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let tmp =
        std::env::temp_dir().join(format!("zylcode-patch-{}-{stamp}.diff", std::process::id()));
    std::fs::write(&tmp, patch)?;
    let out = std::process::Command::new("git")
        .arg("-c")
        .arg("core.autocrlf=false")
        .arg("apply")
        .arg("--whitespace=nowarn")
        .arg("--recount")
        .arg(&tmp)
        .current_dir(root)
        .output();
    let _ = std::fs::remove_file(&tmp);
    let out = out.map_err(|e| anyhow::anyhow!("git apply could not start: {e}"))?;
    anyhow::ensure!(
        out.status.success(),
        "git apply failed: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(json!({
        "applied": true,
        "files": targets,
        "bytes": patch.len(),
    }))
}

// ---------------------------------------------------------------------------
// Evidence ledger
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct EvidenceRow {
    id: String,
    session_id: String,
    action_id: String,
    state: String,
    prev_hash: String,
    timestamp: String,
    payload: Option<Value>,
    error: Option<String>,
}

fn read_evidence_rows(db_path: &Path, limit: usize) -> Result<Vec<EvidenceRow>> {
    anyhow::ensure!(
        db_path.exists(),
        "no evidence ledger at {}",
        db_path.display()
    );
    let conn = Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut stmt = conn.prepare(
        "SELECT id, session_id, action_id, state, prev_hash, timestamp, payload, error
         FROM ledger_entries
         ORDER BY timestamp DESC, rowid DESC
         LIMIT ?1",
    )?;
    let rows = stmt
        .query_map(rusqlite::params![limit as i64], |row| {
            Ok(EvidenceRow {
                id: row.get(0)?,
                session_id: row.get(1)?,
                action_id: row.get(2)?,
                // The state column stores JSON (`"Recorded"`); unwrap the
                // string so the UI shows `Recorded`, not `"Recorded\"`.
                // Unparseable values pass through unchanged — never fabricated.
                state: {
                    let raw: String = row.get(3)?;
                    serde_json::from_str::<String>(&raw).unwrap_or(raw)
                },
                prev_hash: row.get(4)?,
                timestamp: row.get(5)?,
                payload: row
                    .get::<_, Option<String>>(6)?
                    .and_then(|s| serde_json::from_str(&s).ok()),
                error: row.get(7)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Verify the hash chain of chronologically ordered rows (oldest first),
/// **per session**. Same replay formula as the agent loop: hash of
/// `(prev_hash, action_id)`.
///
/// Sessions are independent chains — each writer starts its session from the
/// genesis value (`""`), so a listing that spans sessions must never be
/// verified as one concatenated chain. A session whose first visible row
/// carries a non-genesis `prev_hash` had its head cut off by the read window
/// (`MAX_EVIDENCE_ENTRIES`); its remaining visible links are still verified
/// from that anchor, but the missing head cannot invalidate the verdict.
fn chain_is_intact(rows_oldest_first: &[EvidenceRow]) -> bool {
    use sha2::Digest;
    use std::collections::HashMap;

    // session_id -> expected prev_hash of the session's next row
    let mut anchors: HashMap<&str, String> = HashMap::new();
    for row in rows_oldest_first {
        let expected = anchors.entry(row.session_id.as_str()).or_default(); // genesis for a fresh session
        if row.prev_hash != *expected {
            return false;
        }
        *expected = format!(
            "{:x}",
            sha2::Sha256::digest(format!("{}:{}", row.prev_hash, row.action_id).as_bytes())
        );
    }
    true
}

/// The evidence ledger state for the frontend.
///
/// `Ok` payloads: `empty` (no ledger yet — honest), `ready` (entries +
/// chain verdict + session count), `error` (ledger unreadable — the error
/// is surfaced, not swallowed).
pub fn evidence_payload(root: &Path) -> Value {
    let db_path = root.join(".zylcode").join("ledger.db");
    if !db_path.exists() {
        return json!({
            "kind": "empty",
            "reason": "no evidence ledger yet — run a mission or `zylcode best-of-n` to record evidence",
        });
    }

    let rows = match read_evidence_rows(&db_path, MAX_EVIDENCE_ENTRIES) {
        Ok(rows) => rows,
        Err(e) => {
            return json!({
                "kind": "error",
                "reason": format!("ledger unreadable: {e:#}"),
            });
        }
    };

    if rows.is_empty() {
        return json!({
            "kind": "empty",
            "reason": "the evidence ledger exists but has no entries yet",
        });
    }

    // Chain verification needs chronological order; the API returns newest
    // first for display, so verify on a reversed copy.
    let mut chrono_rows = rows.clone();
    chrono_rows.reverse();
    let intact = chain_is_intact(&chrono_rows);

    let sessions: std::collections::HashSet<&str> =
        rows.iter().map(|r| r.session_id.as_str()).collect();

    let entries: Vec<Value> = rows
        .iter()
        .take(MAX_EVIDENCE_ENTRIES)
        .map(|r| {
            json!({
                "id": r.id,
                "session_id": r.session_id,
                "action_id": r.action_id,
                "state": r.state,
                "timestamp": r.timestamp,
                "error": r.error,
                "payload": r.payload,
            })
        })
        .collect();

    json!({
        "kind": "ready",
        "chain_intact": intact,
        "session_count": sessions.len(),
        "entry_count": rows.len(),
        "entries": entries,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::{ExecutionState, LedgerEntry};
    use std::process::Command;

    fn sample_repo() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let run = |args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .current_dir(&root)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "t@example.com"]);
        run(&["config", "user.name", "t"]);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src").join("engine.rs"),
            "pub struct Engine;\nimpl Engine { pub fn run(&self) {} }\n",
        )
        .unwrap();
        std::fs::write(root.join("README.md"), "# sample\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "initial commit"]);
        (dir, root)
    }

    fn append(
        ledger: &dyn crate::ledger::LedgerStore,
        session: uuid::Uuid,
        prev: &str,
        action: &str,
    ) -> String {
        let id = uuid::Uuid::new_v4();
        let entry = LedgerEntry {
            id,
            session_id: session,
            action_id: action.to_string(),
            arguments: serde_json::json!({}),
            state: ExecutionState::Recorded,
            prev_hash: prev.to_string(),
            timestamp: chrono::Utc::now(),
            payload: Some(serde_json::json!({ "note": action })),
            error: None,
        };
        futures_block_on(ledger.append(entry)).expect("ledger append must succeed");
        format!(
            "{:x}",
            sha2::Sha256::digest(format!("{prev}:{action}").as_bytes())
        )
    }

    // Tiny local executor: the ledger trait is async but the tests are sync.
    fn futures_block_on<F: std::future::Future>(fut: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(fut)
    }

    use sha2::Digest;

    #[test]
    fn search_finds_symbols_and_file_paths_and_ranks_deterministically() {
        let (_dir, root) = sample_repo();
        let payload = search_payload(&root, "engine run").unwrap();
        assert_eq!(payload["query"], "engine run");
        let results = payload["results"].as_array().unwrap();
        assert!(!results.is_empty(), "{payload:?}");
        // Both classes can appear; ordering must be non-increasing relevance.
        let rels: Vec<f64> = results
            .iter()
            .map(|r| r["relevance"].as_f64().unwrap())
            .collect();
        for w in rels.windows(2) {
            assert!(
                w[0] >= w[1],
                "results must be relevance-descending: {rels:?}"
            );
        }
        // A pure filename query finds the file even without symbol hits.
        let by_name = search_payload(&root, "README").unwrap();
        let rows = by_name["results"].as_array().unwrap();
        assert!(
            rows.iter()
                .any(|r| r["resource"].as_str().unwrap().contains("README.md")),
            "{by_name:?}"
        );
    }

    #[test]
    fn empty_query_is_rejected_not_silently_widened() {
        let (_dir, root) = sample_repo();
        assert!(search_payload(&root, "   ").is_err());
    }

    #[test]
    fn file_tree_lists_the_scanned_tree() {
        let (_dir, root) = sample_repo();
        let payload = file_tree_payload(&root).unwrap();
        assert!(payload["total_scanned"].as_u64().unwrap() >= 2);
        let rows = payload["rows"].as_array().unwrap();
        assert!(rows
            .iter()
            .any(|r| r["path"].as_str().unwrap().contains("engine.rs")));
        assert!(rows
            .iter()
            .any(|r| r["path"].as_str().unwrap().contains("README.md")));
        // Sorted by path for a stable tree.
        let paths: Vec<&str> = rows.iter().map(|r| r["path"].as_str().unwrap()).collect();
        let mut sorted = paths.clone();
        sorted.sort();
        assert_eq!(paths, sorted);
    }

    #[test]
    fn evidence_reports_an_honest_empty_state_without_a_ledger() {
        let (_dir, root) = sample_repo();
        let payload = evidence_payload(&root);
        assert_eq!(payload["kind"], "empty");
        assert!(payload["reason"]
            .as_str()
            .unwrap()
            .contains("no evidence ledger"));
    }

    #[test]
    fn evidence_reads_real_entries_and_verifies_the_chain() {
        let (_dir, root) = sample_repo();
        std::fs::create_dir_all(root.join(".zylcode")).unwrap();
        let ledger = crate::sqlite_ledger::SqliteLedgerStore::new(
            root.join(".zylcode")
                .join("ledger.db")
                .to_string_lossy()
                .as_ref(),
        )
        .unwrap();
        let session = uuid::Uuid::new_v4();
        let h1 = append(&ledger, session, "", "mission.step[0]");
        let h2 = append(&ledger, session, &h1, "mission.step[1]");

        let payload = evidence_payload(&root);
        assert_eq!(payload["kind"], "ready", "{payload:?}");
        assert_eq!(payload["chain_intact"], true);
        assert_eq!(payload["session_count"], 1);
        assert_eq!(payload["entry_count"], 2);
        let entries = payload["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries[0]["action_id"]
            .as_str()
            .unwrap()
            .starts_with("mission.step"));

        // Tampering with one link must be detected.
        let conn = Connection::open(root.join(".zylcode").join("ledger.db")).unwrap();
        conn.execute(
            "UPDATE ledger_entries SET prev_hash = 'tampered' WHERE action_id = 'mission.step[1]'",
            [],
        )
        .unwrap();
        let tampered = evidence_payload(&root);
        assert_eq!(tampered["chain_intact"], false, "{tampered:?}");
        let _ = h2;
    }

    #[test]
    fn evidence_chain_is_verified_per_session_not_across_the_listing() {
        // Two sessions: the second starts its own chain from genesis, which
        // must NOT be read as a broken link between sessions. (This exact
        // shape fired in the real ledger after a second best-of-n run.)
        let (_dir, root) = sample_repo();
        std::fs::create_dir_all(root.join(".zylcode")).unwrap();
        let ledger = crate::sqlite_ledger::SqliteLedgerStore::new(
            root.join(".zylcode")
                .join("ledger.db")
                .to_string_lossy()
                .as_ref(),
        )
        .unwrap();
        let s1 = uuid::Uuid::new_v4();
        let s2 = uuid::Uuid::new_v4();
        let _h1 = append(&ledger, s1, "", "best_of_n.candidate[0]");
        let _h2 = append(&ledger, s2, "", "best_of_n.selection"); // genesis again: new session

        let payload = evidence_payload(&root);
        assert_eq!(payload["kind"], "ready", "{payload:?}");
        assert_eq!(payload["session_count"], 2);
        assert_eq!(
            payload["chain_intact"], true,
            "each session's chain starts from genesis; session boundaries are not broken links: {payload:?}"
        );
    }

    #[test]
    fn evidence_still_detects_a_tampered_link_inside_one_session() {
        let (_dir, root) = sample_repo();
        std::fs::create_dir_all(root.join(".zylcode")).unwrap();
        let ledger = crate::sqlite_ledger::SqliteLedgerStore::new(
            root.join(".zylcode")
                .join("ledger.db")
                .to_string_lossy()
                .as_ref(),
        )
        .unwrap();
        let s1 = uuid::Uuid::new_v4();
        let s2 = uuid::Uuid::new_v4();
        let _ = append(&ledger, s1, "", "a");
        let _ = append(&ledger, s2, "", "b");

        let conn = Connection::open(root.join(".zylcode").join("ledger.db")).unwrap();
        conn.execute(
            "UPDATE ledger_entries SET prev_hash = 'forged' WHERE session_id = ?1",
            [s1.to_string()],
        )
        .unwrap();
        let payload = evidence_payload(&root);
        assert_eq!(payload["chain_intact"], false, "{payload:?}");
    }

    #[test]
    fn evidence_with_an_existing_but_empty_ledger_is_empty_not_ready() {
        let (_dir, root) = sample_repo();
        std::fs::create_dir_all(root.join(".zylcode")).unwrap();
        let _ledger = crate::sqlite_ledger::SqliteLedgerStore::new(
            root.join(".zylcode")
                .join("ledger.db")
                .to_string_lossy()
                .as_ref(),
        )
        .unwrap();
        let payload = evidence_payload(&root);
        assert_eq!(payload["kind"], "empty");
        assert!(payload["reason"].as_str().unwrap().contains("no entries"));
    }

    #[test]
    fn file_content_serves_real_content_with_metadata() {
        let (_dir, root) = sample_repo();
        let payload = file_content_payload(&root, "src/engine.rs").unwrap();
        assert_eq!(payload["path"], "src/engine.rs");
        assert!(payload["content"]
            .as_str()
            .unwrap()
            .contains("pub struct Engine;"));
        assert_eq!(payload["lines"], 2);
        assert_eq!(payload["lossy"], false);
        assert!(payload["size"].as_u64().unwrap() > 0);
    }

    #[test]
    fn file_content_rejects_escape_and_missing_and_directory() {
        let (_dir, root) = sample_repo();
        for bad in ["../outside.rs", "no/such/file.rs", "src"] {
            assert!(
                file_content_payload(&root, bad).is_err(),
                "must refuse {bad}"
            );
        }
        // absolute path refused too
        assert!(file_content_payload(&root, "C:/Windows/win.ini").is_err());
    }

    #[test]
    fn file_content_marks_non_utf8_lossy() {
        let (_dir, root) = sample_repo();
        std::fs::write(root.join("blob.bin"), [0x68, 0x69, 0xFF, 0xFE]).unwrap();
        let payload = file_content_payload(&root, "blob.bin").unwrap();
        assert_eq!(payload["lossy"], true, "{payload:?}");
        assert!(payload["content"].as_str().unwrap().contains("hi"));
    }
}

#[cfg(test)]
mod editor_write_tests {
    use super::*;

    /// A plain directory — deliberately *not* a git repository: `git apply`
    /// must work on any workspace the editor can open (verified empirically).
    fn workspace() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("target.txt"), "line1\nline2\nline3\n").unwrap();
        (dir, root)
    }

    fn patch_for(new_line: &str) -> String {
        format!(
            "--- a/target.txt\n+++ b/target.txt\n@@ -1,3 +1,3 @@\n line1\n-line2\n+{new_line}\n line3\n"
        )
    }

    #[test]
    fn save_writes_existing_file_inside_workspace() {
        let (_dir, root) = workspace();
        let payload = save_file_payload(&root, "target.txt", "edited\n").unwrap();
        assert_eq!(payload["path"], "target.txt");
        assert_eq!(payload["bytes"], 7);
        assert_eq!(
            std::fs::read_to_string(root.join("target.txt")).unwrap(),
            "edited\n"
        );
    }

    #[test]
    fn save_creates_new_file_in_existing_directory() {
        let (_dir, root) = workspace();
        std::fs::create_dir(root.join("sub")).unwrap();
        save_file_payload(&root, "sub/new.txt", "hello").unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("sub/new.txt")).unwrap(),
            "hello"
        );
    }

    #[test]
    fn save_refuses_escape_git_and_absolute_paths() {
        let (_dir, root) = workspace();
        for bad in [
            "../evil.txt",
            "sub/../../evil.txt",
            "C:/Windows/win.ini",
            "/etc/hosts",
            ".git/hooks/planted",
            "",
        ] {
            assert!(
                save_file_payload(&root, bad, "x").is_err(),
                "must refuse {bad:?}"
            );
        }
        // nothing outside or hidden was written
        assert!(!root.parent().unwrap().join("evil.txt").exists());
        assert!(!root.join(".git").exists());
    }

    #[test]
    fn save_refuses_directory_target_and_oversize_payload() {
        let (_dir, root) = workspace();
        std::fs::create_dir(root.join("adir")).unwrap();
        assert!(save_file_payload(&root, "adir", "x").is_err());
        let big = "a".repeat(MAX_FILE_BYTES as usize + 1);
        assert!(save_file_payload(&root, "target.txt", &big).is_err());
        // refused write left the original untouched
        assert_eq!(
            std::fs::read_to_string(root.join("target.txt")).unwrap(),
            "line1\nline2\nline3\n"
        );
    }

    #[test]
    fn save_refuses_symlink_resolving_outside() {
        let (_dir, root) = workspace();
        let outside = tempfile::tempdir().unwrap();
        let victim = outside.path().join("victim.txt");
        std::fs::write(&victim, "original").unwrap();
        let link = root.join("link.txt");
        #[cfg(windows)]
        if std::os::windows::fs::symlink_file(&victim, &link).is_err() {
            eprintln!("skip: symlink creation not permitted on this host");
            return;
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(&victim, &link).unwrap();
        let err = save_file_payload(&root, "link.txt", "PWNED").unwrap_err();
        assert!(err.to_string().contains("escapes"), "{err}");
        assert_eq!(
            std::fs::read_to_string(&victim).unwrap(),
            "original",
            "outside file must be untouched"
        );
    }

    #[test]
    fn patch_applies_real_unified_diff_outside_a_git_repo() {
        let (_dir, root) = workspace();
        let payload = apply_patch_payload(&root, &patch_for("line2-EDITED")).unwrap();
        assert_eq!(payload["applied"], true);
        assert_eq!(payload["files"][0], "target.txt");
        assert_eq!(
            std::fs::read_to_string(root.join("target.txt")).unwrap(),
            "line1\nline2-EDITED\nline3\n"
        );
    }

    #[test]
    fn patch_refuses_escaping_headers_before_git_runs() {
        let (_dir, root) = workspace();
        for (header, needle) in [
            ("+++ /etc/hosts", "absolute path"),
            ("+++ b/../evil.txt", "escapes the workspace"),
            ("+++ .git/hooks/planted", ".git/*"),
        ] {
            let patch = format!("--- /dev/null\n{header}\n@@ -0,0 +1 @@\n+x\n");
            let err = apply_patch_payload(&root, &patch).unwrap_err();
            assert!(
                err.to_string().contains(needle),
                "expected {needle:?} for {header}, got: {err}"
            );
        }
        assert!(!root.join("evil.txt").exists());
        assert!(!root.join(".git").exists());
        assert_eq!(
            std::fs::read_to_string(root.join("target.txt")).unwrap(),
            "line1\nline2\nline3\n",
            "target unchanged — refusal happened before git ran"
        );
    }

    #[test]
    fn patch_malformed_fails_closed_without_touching_the_file() {
        let (_dir, root) = workspace();
        // context mismatch: git apply must reject the whole patch
        let broken =
            "--- a/target.txt\n+++ b/target.txt\n@@ -1,3 +1,3 @@\n line1\n WRONG\n line3\n";
        let err = apply_patch_payload(&root, broken).unwrap_err();
        assert!(err.to_string().contains("git apply failed"), "{err}");
        assert_eq!(
            std::fs::read_to_string(root.join("target.txt")).unwrap(),
            "line1\nline2\nline3\n",
            "file untouched after failed apply"
        );
        // not a unified diff at all
        assert!(apply_patch_payload(&root, "just some text").is_err());
        assert!(apply_patch_payload(&root, "   ").is_err());
    }

    #[test]
    fn patch_line_endings_are_byte_faithful_and_fail_closed() {
        let (_dir, root) = workspace();

        // 1. LF file + LF patch → applied, still LF (this host has
        //    core.autocrlf=true globally; the invocation must override it).
        apply_patch_payload(&root, &patch_for("lf-edit")).unwrap();
        let bytes = std::fs::read(root.join("target.txt")).unwrap();
        assert!(
            !bytes.contains(&b'\r'),
            "untouched LF lines must not be rewritten to CRLF: {bytes:?}"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("target.txt")).unwrap(),
            "line1\nlf-edit\nline3\n"
        );

        // 2. CRLF file + CRLF patch → applied, still uniformly CRLF.
        std::fs::write(root.join("target.txt"), "line1\r\nline2\r\nline3\r\n").unwrap();
        let crlf_patch = patch_for("crlf-edit").replace('\n', "\r\n");
        apply_patch_payload(&root, &crlf_patch).unwrap();
        let bytes = std::fs::read(root.join("target.txt")).unwrap();
        assert_eq!(
            bytes,
            b"line1\r\ncrlf-edit\r\nline3\r\n".to_vec(),
            "CRLF file + CRLF patch stays uniformly CRLF"
        );

        // 3. CRLF file + LF patch → refused honestly, no mixed-EOL damage.
        std::fs::write(root.join("target.txt"), "line1\r\nline2\r\nline3\r\n").unwrap();
        let err = apply_patch_payload(&root, &patch_for("x")).unwrap_err();
        assert!(err.to_string().contains("git apply failed"), "{err}");
        assert_eq!(
            std::fs::read(root.join("target.txt")).unwrap(),
            b"line1\r\nline2\r\nline3\r\n".to_vec(),
            "refused patch left the CRLF file untouched"
        );
    }
}
