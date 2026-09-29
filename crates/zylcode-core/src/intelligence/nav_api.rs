//! Repository-intelligence navigation payload — one function, three consumers.
//!
//! Mirrors [`super::api`]: the `serve-intel` HTTP routes, the Tauri
//! `nav_query` command and direct callers all go through [`nav_payload`], so
//! every surface answers from the same engine (`zylcode-nav`) and the same
//! process-wide index slot. There is no second, divergent implementation of
//! "find references" anywhere in the product.
//!
//! Honesty rules inherited from the engine and deliberately *not* softened
//! here:
//!
//! * an unresolvable selector comes back as a payload with `unresolved` set,
//!   never as an invented hit;
//! * every relationship carries `evidence` — `DETERMINISTIC`, `HEURISTIC` or
//!   `UNSUPPORTED` — and heuristic results are returned *alongside*
//!   deterministic ones, never promoted;
//! * impact output states that it reports relationships, not predictions.
//!
//! Failure to *run* the query (an index that could not be built, a malformed
//! selector) is an [`Err`], not a payload: these callers must never render an
//! error body as if it were a navigation answer.

use serde_json::Value;
use std::path::Path;
use zylcode_nav::payload;
use zylcode_nav::NavError;

/// The navigation tools this function accepts, as bare engine ids.
///
/// The `nav.`-prefixed MCP form is also accepted (and stripped) so a caller
/// can pass either an HTTP/Tauri id or an MCP tool id without branching.
pub const NAV_TOOL_IDS: &[&str] = &payload::NAV_TOOL_IDS;

/// Run one navigation query against `root`.
///
/// `tool` may be a bare engine id (`"find_references"`) or the MCP form
/// (`"nav.find_references"`).
pub fn nav_payload(root: &Path, tool: &str, args: Value) -> Result<Value, NavError> {
    let tool = tool.strip_prefix("nav.").unwrap_or(tool);
    zylcode_nav::query_repository(root, |index| payload::dispatch(index, tool, args.clone()))
}

/// Deterministic graph citations for `goal`, one fact per line, for an agent
/// prompt.
///
/// Deliberately returns `Err` when the index cannot be built instead of an
/// empty list: "no symbol in the goal matched anything" (a fact about the
/// goal) and "we could not read the repository" (a fact about us) are
/// different things, and a prompt that renders both as "no citations" would
/// teach the model that silence means *no relationships exist*.
pub fn agent_citation_block(
    root: &Path,
    goal: &str,
    limit: usize,
) -> Result<Vec<String>, NavError> {
    zylcode_nav::query_repository(root, |index| {
        Ok(payload::goal_citation_lines(index, goal, limit))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let src = dir.path().join("src");
        fs::create_dir_all(&src).expect("mkdir");
        fs::write(
            src.join("lib.rs"),
            "pub mod util;\n\npub fn entry() -> u32 {\n    util::helper()\n}\n",
        )
        .expect("lib");
        fs::write(src.join("util.rs"), "pub fn helper() -> u32 {\n    1\n}\n").expect("util");
        fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"demo\"\n",
        )
        .expect("manifest");
        dir
    }

    #[test]
    fn a_definition_resolves_through_the_shared_slot() {
        let dir = repo();
        let value = nav_payload(
            dir.path(),
            "find_definition",
            serde_json::json!({"file": "src/util.rs", "name": "helper"}),
        )
        .expect("query");
        assert_eq!(value["engine"], "zylcode-nav");
        assert_eq!(value["definition"]["name"], "helper");
        assert!(value["unresolved"].is_null(), "{value}");
    }

    #[test]
    fn the_mcp_prefixed_id_is_accepted_too() {
        let dir = repo();
        let bare = nav_payload(
            dir.path(),
            "find_definition",
            serde_json::json!({"file": "src/util.rs", "name": "helper"}),
        )
        .expect("bare id");
        let prefixed = nav_payload(
            dir.path(),
            "nav.find_definition",
            serde_json::json!({"file": "src/util.rs", "name": "helper"}),
        )
        .expect("nav-prefixed id");
        assert_eq!(bare, prefixed, "both spellings must reach the same engine");
    }

    #[test]
    fn a_rejected_request_is_an_error_not_a_payload() {
        let dir = repo();
        let err = nav_payload(dir.path(), "find_definition", serde_json::json!({}))
            .expect_err("no selector was given");
        assert!(
            matches!(err, NavError::Rejected(_)),
            "a bad request must not be reported as an unavailable index: {err}"
        );
    }

    #[test]
    fn an_unknown_tool_is_rejected_rather_than_answered() {
        let dir = repo();
        let err = nav_payload(dir.path(), "find_everything", serde_json::json!({}))
            .expect_err("no such tool");
        assert!(matches!(err, NavError::Rejected(_)), "{err}");
    }

    #[test]
    fn a_deleted_root_is_unavailable_not_empty() {
        let dir = repo();
        let missing = dir.path().join("gone");
        let err = nav_payload(&missing, "find_definition", serde_json::json!({}))
            .expect_err("there is no repository there");
        assert!(
            matches!(err, NavError::Unavailable(_)),
            "an unindexable root must not answer `unresolved` as if it were a real result: {err}"
        );
    }

    #[test]
    fn the_public_id_list_matches_the_engine() {
        assert_eq!(NAV_TOOL_IDS.len(), 8);
        assert!(NAV_TOOL_IDS.contains(&"find_references"));
        assert!(NAV_TOOL_IDS.contains(&"symbol_impact"));
    }

    #[test]
    fn agent_citations_are_deterministic_facts_about_named_symbols() {
        let dir = repo();
        let lines = agent_citation_block(dir.path(), "call helper from entry", 12)
            .expect("the fixture index builds");

        assert!(
            !lines.is_empty(),
            "the fixture names real symbols: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("helper")),
            "the named symbol must be cited: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("src/util.rs:")),
            "every citation must be a location an agent can quote: {lines:?}"
        );
        assert!(
            lines.iter().all(|l| l.starts_with("DETERMINISTIC")),
            "citations are deterministic-only; an HEURISTIC line would let an agent \
             quote a guess as a fact: {lines:?}"
        );
    }

    #[test]
    fn a_goal_naming_nothing_is_distinguished_from_an_unreadable_repository() {
        let dir = repo();

        let no_match = agent_citation_block(dir.path(), "zzz qqq", 12).expect("index builds");
        assert_eq!(no_match.len(), 1, "{no_match:?}");
        assert!(no_match[0].contains("no symbol"), "{}", no_match[0]);

        let missing = dir.path().join("gone");
        let err = agent_citation_block(&missing, "helper", 12).expect_err("no repository there");
        assert!(
            matches!(err, NavError::Unavailable(_)),
            "an unindexable root must be an error, not an empty citation list: {err}"
        );
    }
}
