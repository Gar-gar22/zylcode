//! JSON payloads: one dispatcher for the HTTP routes, the MCP tools and the
//! Tauri commands.
//!
//! A single entry point means the CLI, the desktop app and the agent tools can
//! never disagree about what a query means — they differ only in transport.
//! Every payload is a plain `serde_json::Value` so it survives both `serde_json`
//! and Tauri's `InvokeBody` without a second type layer.

use crate::index::{IndexStats, RepoIndex};
use crate::model::GraphQuery;
use crate::queries::{
    file_dependencies, file_dependents, file_impact, find_callees, find_callers, find_references,
    repository_graph_query, symbol_impact, CallQuery, DefinitionQuery, DefinitionResult,
    ImpactQuery, ReferenceRequest, SymbolSelector,
};
use serde::Serialize;
use serde_json::{json, Value};

/// The tools this crate implements. `zylcode-mcp` binds each id to
/// [`dispatch`] — the ids in `mcp.tools.yaml` must be exactly these.
pub const NAV_TOOL_IDS: [&str; 8] = [
    "find_definition",
    "find_references",
    "find_callers",
    "find_callees",
    "file_dependencies",
    "file_dependents",
    "symbol_impact",
    "repository_graph_query",
];

/// Everything the index can say about itself, for `/healthz` and the status bar.
#[derive(Debug, Clone, Serialize)]
pub struct IndexSummary {
    pub schema_version: u32,
    pub root: String,
    pub stats: IndexStats,
    pub languages: Vec<(String, usize)>,
    pub packages: Vec<(String, String)>,
    /// Query-shaped counters the UI can show without running a query.
    pub symbol_count: usize,
    pub import_edges: usize,
    pub unresolved_imports: usize,
    pub external_imports: usize,
}

pub fn index_summary(index: &RepoIndex) -> IndexSummary {
    let mut languages: std::collections::BTreeMap<String, usize> = Default::default();
    let mut import_edges = 0usize;
    let mut unresolved_imports = 0usize;
    let mut external_imports = 0usize;

    for facts in index.facts.values() {
        *languages.entry(facts.language.as_str()).or_default() += 1;
        for decl in &facts.imports {
            match &decl.resolution {
                crate::model::ImportResolution::Resolved { .. } => import_edges += 1,
                crate::model::ImportResolution::External { .. } => external_imports += 1,
                crate::model::ImportResolution::Unresolved { .. } => unresolved_imports += 1,
            }
        }
    }

    let mut packages: Vec<(String, String)> = index
        .resolver
        .layout
        .packages
        .iter()
        .map(|p| match p.kind {
            crate::resolve::PackageKind::Cargo => (format!("cargo:{}", p.name), p.dir.clone()),
            crate::resolve::PackageKind::Npm => (format!("npm:{}", p.name), p.dir.clone()),
        })
        .collect();
    packages.sort();

    IndexSummary {
        schema_version: crate::index::SCHEMA_VERSION,
        root: index.root.display().to_string(),
        stats: index.stats.clone(),
        languages: languages.into_iter().collect(),
        packages,
        symbol_count: index.symbols.len(),
        import_edges,
        unresolved_imports,
        external_imports,
    }
}

/// Marker included in every response so a caller can prove which engine
/// answered (and that it answered at all).
pub const ENGINE: &str = "zylcode-nav";

// ---------------------------------------------------------------------------
// Argument decoding
// ---------------------------------------------------------------------------

/// Accept either `{"target": {...}}` or the flat convenience form
/// `{"file": .., "line": ..}`, `{"name": ..}`, `{"qualified_name": ..}`,
/// `{"id": ..}` — so a tool call can be short without a second schema.
pub fn parse_selector(args: &Value) -> Result<SymbolSelector, String> {
    if let Some(target) = args.get("target") {
        return serde_json::from_value(target.clone())
            .map_err(|e| format!("invalid `target`: {e}"));
    }
    if let Some(id) = args.get("id").and_then(Value::as_str) {
        return Ok(SymbolSelector::Id { id: id.to_string() });
    }
    if let (Some(file), Some(line)) = (
        args.get("file").and_then(Value::as_str),
        args.get("line").and_then(Value::as_u64),
    ) {
        return Ok(SymbolSelector::At {
            file: file.to_string(),
            line: line as u32,
        });
    }
    if let Some(qualified_name) = args.get("qualified_name").and_then(Value::as_str) {
        return Ok(SymbolSelector::Qualified {
            qualified_name: qualified_name.to_string(),
            crate_key: args
                .get("crate_key")
                .and_then(Value::as_str)
                .map(str::to_string),
            file: args.get("file").and_then(Value::as_str).map(str::to_string),
        });
    }
    if let Some(name) = args.get("name").and_then(Value::as_str) {
        return Ok(SymbolSelector::Name {
            name: name.to_string(),
            file: args.get("file").and_then(Value::as_str).map(str::to_string),
        });
    }
    Err(
        "a selector is required: `target`, `id`, `qualified_name`, `name`, or `file` + `line`"
            .to_string(),
    )
}

fn required_file(args: &Value) -> Result<String, String> {
    args.get("file")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "`file` is required (repository-relative path)".to_string())
}

fn num(args: &Value, key: &str, default: usize) -> usize {
    args.get(key)
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .unwrap_or(default)
}

fn bool_or(args: &Value, key: &str, default: bool) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(default)
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// Run one navigation tool. Read-only: nothing here writes to the repository
/// other than the index cache.
pub fn dispatch(index: &RepoIndex, tool: &str, args: Value) -> Result<Value, String> {
    let mut payload = match tool {
        "find_definition" => {
            let query = DefinitionQuery {
                target: parse_selector(&args)?,
            };
            let result: DefinitionResult = index.resolve(&query.target);
            to_value(&result)?
        }
        "find_references" => {
            let request = ReferenceRequest {
                target: parse_selector(&args)?,
                include_definition: bool_or(&args, "include_definition", true),
                include_possible: bool_or(&args, "include_possible", true),
                limit: num(&args, "limit", 500),
            };
            to_value(&find_references(index, &request))?
        }
        "find_callers" => {
            let request = CallQuery {
                target: parse_selector(&args)?,
                limit: num(&args, "limit", 500),
            };
            to_value(&find_callers(index, &request))?
        }
        "find_callees" => {
            let request = CallQuery {
                target: parse_selector(&args)?,
                limit: num(&args, "limit", 500),
            };
            to_value(&find_callees(index, &request))?
        }
        "file_dependencies" => to_value(&file_dependencies(index, &required_file(&args)?))?,
        "file_dependents" => to_value(&file_dependents(index, &required_file(&args)?))?,
        "symbol_impact" => dispatch_impact(index, &args)?,
        "repository_graph_query" => {
            let parsed: GraphQuery =
                serde_json::from_value(args).map_err(|e| format!("invalid graph query: {e}"))?;
            let query = parsed.normalised();
            to_value(&repository_graph_query(index, &query))?
        }
        "index_summary" => to_value(&index_summary(index))?,
        other => {
            return Err(format!(
                "unknown navigation tool `{other}`; known tools: {}",
                NAV_TOOL_IDS.join(", ")
            ))
        }
    };
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("engine".to_string(), json!(ENGINE));
    }
    Ok(payload)
}

fn dispatch_impact(index: &RepoIndex, args: &Value) -> Result<Value, String> {
    let depth = num(args, "depth", 3);
    let limit = num(args, "limit", 500);
    // `subject: "file"` (or a bare `file` with no selector) analyses a file;
    // anything else analyses a symbol.
    let wants_file = args.get("subject").and_then(Value::as_str) == Some("file")
        || (args.get("target").is_none()
            && args.get("id").is_none()
            && args.get("name").is_none()
            && args.get("qualified_name").is_none()
            && args.get("line").is_none());
    if wants_file {
        let file = required_file(args)?;
        return to_value(&file_impact(index, &file, depth, limit));
    }
    let query = ImpactQuery {
        target: parse_selector(args)?,
        depth: Some(depth),
        limit,
    };
    let (report, symbol) = symbol_impact(index, &query);
    let mut value = to_value(&report)?;
    if let Some(symbol) = symbol {
        if let Some(obj) = value.as_object_mut() {
            obj.insert("symbol".to_string(), to_value(&symbol)?);
        }
    }
    Ok(value)
}

fn to_value<T: Serialize>(value: &T) -> Result<Value, String> {
    serde_json::to_value(value).map_err(|e| format!("serialising navigation payload: {e}"))
}

// ---------------------------------------------------------------------------
// Citable evidence for agent answers
// ---------------------------------------------------------------------------

/// One line an agent can quote to back a claim about a symbol.
#[derive(Debug, Clone, Serialize)]
pub struct Citation {
    pub symbol: String,
    pub location: String,
    pub relation: String,
    pub evidence: crate::model::Evidence,
    pub excerpt: String,
}

/// Produce citations for a symbol: its declaration plus up to `limit`
/// deterministic references. Deliberately excludes heuristic matches so an
/// agent quoting these is quoting evidence, not a guess.
pub fn citations(index: &RepoIndex, selector: &SymbolSelector, limit: usize) -> Vec<Citation> {
    let resolved = index.resolve(selector);
    let Some(target) = resolved.definition else {
        return Vec::new();
    };
    if !resolved.candidates.is_empty() {
        return Vec::new();
    }
    let mut out = vec![Citation {
        symbol: target.qualified_name.clone(),
        location: format!("{}:{}", target.file, target.name_range.start_line),
        relation: "declared".to_string(),
        evidence: crate::model::Evidence::Deterministic,
        excerpt: target.signature.clone().unwrap_or_default(),
    }];
    let refs = find_references(
        index,
        &ReferenceRequest {
            target: SymbolSelector::Id {
                id: target.id.clone(),
            },
            include_definition: false,
            include_possible: false,
            limit: limit.max(1),
        },
    );
    for r in refs.references.iter().take(limit.saturating_sub(1)) {
        out.push(Citation {
            symbol: target.qualified_name.clone(),
            location: format!("{}:{}", r.file, r.range.start_line),
            relation: format!("{} via {}", r.kind.as_str(), r.via),
            evidence: r.evidence,
            excerpt: r.excerpt.clone(),
        });
    }
    out
}

/// Identifier-shaped tokens in `goal`, first-seen order, deduplicated.
///
/// Used only to decide *which* symbols to look up. Matching a word from a
/// sentence to a symbol name is retrieval, not evidence — see
/// [`goal_citations`] for the line this draws.
fn goal_tokens(goal: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    for ch in goal.chars() {
        if ch.is_alphanumeric() || ch == '_' {
            current.push(ch);
        } else {
            push_token(&mut out, &mut current);
        }
    }
    push_token(&mut out, &mut current);
    out
}

fn push_token(out: &mut Vec<String>, current: &mut String) {
    if current.chars().count() >= 3 && !out.iter().any(|t| t == current) {
        out.push(current.clone());
    }
    current.clear();
}

/// Citations for every symbol `goal` mentions by name.
///
/// # The line this draws
///
/// The *lookup* — "the goal said `helper`, is there a symbol called `helper`"
/// — is a retrieval heuristic and is never presented as a fact. Everything it
/// returns comes from [`citations`], which resolves through the index, drops
/// ambiguous selectors outright, and excludes heuristic references. So the
/// list an agent is handed may be incomplete, but every line in it is a
/// deterministic graph fact.
///
/// `limit` caps the total number of citations across all tokens.
pub fn goal_citations(index: &RepoIndex, goal: &str, limit: usize) -> Vec<Citation> {
    let mut out: Vec<Citation> = Vec::new();
    let mut seen: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    for token in goal_tokens(goal) {
        if out.len() >= limit {
            break;
        }
        let selector = SymbolSelector::Name {
            name: token,
            file: None,
        };
        for citation in citations(index, &selector, 3) {
            if seen.insert((citation.symbol.clone(), citation.location.clone())) {
                out.push(citation);
                if out.len() >= limit {
                    break;
                }
            }
        }
    }
    out
}

/// [`goal_citations`], rendered one fact per line for an agent prompt.
///
/// Returns exactly one explanatory line when nothing matched, so a caller can
/// never mistake "the index found no symbol by that name" for an empty list
/// of facts it should have quoted.
pub fn goal_citation_lines(index: &RepoIndex, goal: &str, limit: usize) -> Vec<String> {
    let found = goal_citations(index, goal, limit);
    if found.is_empty() {
        return vec![
            "(no symbol in this repository is named in the goal — there is no graph evidence to cite for it)"
                .to_string(),
        ];
    }
    found
        .iter()
        .map(|c| {
            format!(
                "{} · {} @ {} — {} — {}",
                c.evidence.as_str(),
                c.symbol,
                c.location,
                c.relation,
                c.excerpt
            )
        })
        .collect()
}

/// Re-exported so callers that hold only a `RepoIndex` can label an
/// occurrence's attribution without importing `queries` directly.
pub use crate::queries::attribute as attribute_occurrence;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_ids_are_eight_and_named_exactly() {
        assert_eq!(NAV_TOOL_IDS.len(), 8);
        assert!(NAV_TOOL_IDS.contains(&"find_definition"));
        assert!(NAV_TOOL_IDS.contains(&"repository_graph_query"));
    }

    #[test]
    fn selector_accepts_flat_and_nested_forms() {
        let flat = json!({"file": "src/a.rs", "line": 12});
        assert_eq!(
            parse_selector(&flat).expect("flat"),
            SymbolSelector::At {
                file: "src/a.rs".into(),
                line: 12
            }
        );
        let nested = json!({"target": {"kind": "name", "name": "foo"}});
        assert_eq!(
            parse_selector(&nested).expect("nested"),
            SymbolSelector::Name {
                name: "foo".into(),
                file: None
            }
        );
        assert!(parse_selector(&json!({})).is_err(), "empty args must fail");
    }

    #[test]
    fn unknown_tool_is_an_error_not_a_fake_success() {
        let index = RepoIndex::default();
        let err = dispatch(&index, "definitely_not_a_tool", json!({}))
            .expect_err("unknown tool must fail");
        assert!(err.contains("unknown navigation tool"));
    }

    #[test]
    fn goal_tokens_are_identifier_shaped_deduplicated_and_minimally_sized() {
        assert_eq!(
            goal_tokens("Which files mention helper and helper() in the repo?"),
            vec!["Which", "files", "mention", "helper", "and", "the", "repo"]
        );
        assert_eq!(goal_tokens("a an fn helper"), vec!["helper"]);
        assert!(goal_tokens("").is_empty());
    }

    #[test]
    fn an_index_with_no_match_says_so_instead_of_citing_nothing() {
        let index = RepoIndex::default();
        assert!(
            goal_citations(&index, "find the helper function", 10).is_empty(),
            "nothing to cite from an empty index"
        );

        let lines = goal_citation_lines(&index, "find the helper function", 10);
        assert_eq!(
            lines.len(),
            1,
            "an empty citation set must still produce exactly one explanatory line: {lines:?}"
        );
        assert!(
            lines[0].contains("no symbol in this repository is named in the goal"),
            "{}",
            lines[0]
        );
    }

    #[test]
    fn a_goal_that_names_nothing_cites_nothing_and_says_so_too() {
        let index = RepoIndex::default();
        let lines = goal_citation_lines(&index, "", 10);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("no symbol"), "{}", lines[0]);
    }
}
