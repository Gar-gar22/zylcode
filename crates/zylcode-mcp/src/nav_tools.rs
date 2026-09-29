//! Repository-intelligence executors — the eight read-only `nav.*` tools.
//!
//! # What they answer
//!
//! Every tool in this module is answered by `zylcode-nav` from a parsed index
//! of the repository: symbol definitions, references, callers/callees,
//! import/dependency edges and impact relationships. Nothing here performs a
//! text search, and nothing here invents an answer the index does not contain.
//! When a selector cannot be resolved the payload carries an `unresolved`
//! message (and, where the engine found candidates, the list) instead of a
//! fabricated hit.
//!
//! # Index reuse — no re-index per query
//!
//! [`query`] delegates to [`zylcode_nav::query_repository`], which holds the
//! most recently built index for a repository root in a process-wide slot and
//! rebuilds it only when the root differs or [`zylcode_nav::REUSE_FOR`] has
//! elapsed. The slot is shared with the `serve-intel` HTTP routes and the
//! Tauri commands, so a repository is indexed once per process rather than
//! once per surface. A rebuild is cheap because `RepoIndex::build` reuses
//! per-file facts keyed by content hash, so only files whose bytes actually
//! changed are re-parsed; queries inside the window touch no files at all.
//!
//! # Side effects
//!
//! Building the index persists a warm-start cache at
//! `<root>/.zylcode/nav-index.json` (gitignored). That is the only write these
//! read-only tools perform; they never touch source files.
//!
//! # Failure
//!
//! An engine error ([`zylcode_nav::NavError::Rejected`]) is reported as
//! [`ToolError::InvalidRequest`]; an index that cannot be built
//! ([`zylcode_nav::NavError::Unavailable`]) is reported as
//! [`ToolError::ExecutorUnavailable`]. Neither path ever returns
//! `success: true` for work that did not happen.

use std::path::Path;
use std::time::Instant;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use crate::real_tools::{
    RealTool, RiskLevel, ToolContext, ToolError, ToolEvidence, ToolPermissions, ToolResult,
};
use zylcode_nav::payload;

/// The ids this module executes, in catalogue order.
///
/// The `nav.` prefix is what keeps them namespace-clean beside `fs.read` and
/// `git.status`. The engine dispatches the bare name (`find_definition`); see
/// [`payload_id`].
pub const NAV_TOOL_IDS: [&str; 8] = [
    "nav.find_definition",
    "nav.find_references",
    "nav.find_callers",
    "nav.find_callees",
    "nav.file_dependencies",
    "nav.file_dependents",
    "nav.symbol_impact",
    "nav.repository_graph_query",
];

/// The engine-side name for a `nav.*` id.
///
/// `nav.find_references` → `find_references`. Unknown ids fall through
/// unchanged so the engine's "unknown navigation tool" error stays informative.
pub fn payload_id(tool_id: &str) -> &str {
    tool_id.strip_prefix("nav.").unwrap_or(tool_id)
}

/// True for every id this module owns. Used by the catalogue and the registry
/// risk map so a `nav.` tool is never classified by accident.
pub fn is_nav_tool(tool_id: &str) -> bool {
    tool_id.starts_with("nav.") && NAV_TOOL_IDS.contains(&tool_id)
}

/// Answer one navigation tool against `root`.
///
/// Returns the engine payload verbatim, including its `engine` marker. The
/// index itself lives in [`zylcode_nav::query_repository`]'s process-wide slot,
/// shared with the `serve-intel` routes and the Tauri commands — a repository
/// is indexed once per process, not once per surface.
fn query(root: &Path, tool: &str, args: &Value) -> Result<Value, zylcode_nav::NavError> {
    zylcode_nav::query_repository(root, |index| payload::dispatch(index, tool, args.clone()))
}

/// One read-only repository-intelligence query, bound to exactly one tool id.
///
/// Like every executor in this crate the binding is enforced rather than
/// documented: [`payload_id`] strips the namespace, and the engine only knows
/// the eight bare names, so a `nav.*` id means exactly one operation.
#[derive(Debug)]
pub struct NavTool {
    id: String,
    description: String,
}

impl NavTool {
    pub fn new(id: &str) -> Self {
        Self {
            id: id.to_string(),
            // The catalogue owns the wording; the executor borrows it so the
            // two can never drift.
            description: crate::tool_catalogue::executable_description(id).to_string(),
        }
    }
}

#[async_trait]
impl RealTool for NavTool {
    fn id(&self) -> &str {
        &self.id
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn risk_level(&self) -> RiskLevel {
        RiskLevel::Read
    }

    fn bound_operation(&self) -> Option<String> {
        Some(format!(
            "repository intelligence `{}`",
            payload_id(&self.id)
        ))
    }

    fn permissions(&self) -> ToolPermissions {
        ToolPermissions {
            read: true,
            write: false,
            execute: false,
            network: false,
            destructive: false,
        }
    }

    async fn execute(&self, params: Value, context: &ToolContext) -> Result<ToolResult> {
        let start = Instant::now();
        let mut evidence = ToolEvidence::begin(
            &self.id,
            self.risk_level(),
            self.bound_operation(),
            &params,
            context,
        );

        match query(&context.working_directory, payload_id(&self.id), &params) {
            Ok(output) => {
                evidence.end_time = Some(chrono::Utc::now());
                evidence.stdout = Some(output.to_string());
                Ok(ToolResult {
                    success: true,
                    output,
                    evidence,
                    changed_files: Vec::new(),
                    duration: start.elapsed(),
                })
            }
            Err(zylcode_nav::NavError::Rejected(reason)) => Err(ToolError::InvalidRequest {
                tool_id: self.id.clone(),
                reason,
            }
            .into()),
            Err(zylcode_nav::NavError::Unavailable(reason)) => {
                Err(ToolError::ExecutorUnavailable(format!("{}: {reason}", self.id)).into())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_ids_are_eight_and_prefixed() {
        assert_eq!(NAV_TOOL_IDS.len(), 8);
        assert!(NAV_TOOL_IDS.contains(&"nav.find_definition"));
        assert!(NAV_TOOL_IDS.contains(&"nav.repository_graph_query"));
        assert!(NAV_TOOL_IDS.iter().all(|id| id.starts_with("nav.")));
    }

    #[test]
    fn payload_id_strips_only_the_namespace() {
        assert_eq!(payload_id("nav.find_references"), "find_references");
        // An unexpected id falls through so the engine's own error names it.
        assert_eq!(payload_id("find_references"), "find_references");
        assert_eq!(payload_id("fs.read"), "fs.read");
    }

    #[test]
    fn is_nav_tool_rejects_lookalikes() {
        assert!(is_nav_tool("nav.find_definition"));
        assert!(!is_nav_tool("nav.index_summary"));
        assert!(!is_nav_tool("find_definition"));
        assert!(!is_nav_tool("git.status"));
    }

    #[test]
    fn every_id_maps_to_a_known_engine_tool() {
        for id in NAV_TOOL_IDS {
            let bare = payload_id(id);
            assert!(
                zylcode_nav::payload::NAV_TOOL_IDS.contains(&bare),
                "`{id}` has no engine tool behind it"
            );
        }
    }
}
