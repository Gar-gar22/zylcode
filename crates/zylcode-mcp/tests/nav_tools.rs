//! End-to-end evidence that the eight `nav.*` tools actually execute.
//!
//! Every assertion here travels through `real_tools::dispatch` — the same
//! resolve → gate → execute path the product uses — against a repository built
//! on disk. That is what the catalogue's `tested: true` and rung `R2` for
//! these tools refer to: without this file they would be claims, not
//! measurements.
//!
//! The gate is deliberately the **restrictive** one. These tools are allowed
//! because they are classified `Read`, not because a test relaxed policy.

use std::path::Path;

use serde_json::{json, Value};
use zylcode_mcp::nav_tools::{is_nav_tool, NAV_TOOL_IDS};
use zylcode_mcp::real_tools::{dispatch, get_real_tool, RiskLevel, ToolContext};
use zylcode_mcp::ToolRuntime;

// ---------------------------------------------------------------------------
// Fixture: a three-file Rust crate with one cross-module call and one call
// through an import binding.
// ---------------------------------------------------------------------------

const CARGO_TOML: &str = "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n";

const LIB_RS: &str = "\
pub mod util;
pub mod api;

pub fn entry() -> u32 {
    util::helper()
}
";

const UTIL_RS: &str = "\
pub fn helper() -> u32 {
    1
}

pub fn never_called() -> u32 {
    2
}
";

const API_RS: &str = "\
use crate::util::helper;

pub fn make() -> u32 {
    helper()
}
";

const LIB: &str = "crates/demo/src/lib.rs";
const UTIL: &str = "crates/demo/src/util.rs";
const API: &str = "crates/demo/src/api.rs";

fn repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("create fixture directory");
    for (rel, body) in [
        ("crates/demo/Cargo.toml", CARGO_TOML),
        (LIB, LIB_RS),
        (UTIL, UTIL_RS),
        (API, API_RS),
    ] {
        let path = dir.path().join(rel);
        let parent = path.parent().expect("path has a parent");
        std::fs::create_dir_all(parent).expect("create fixture directory");
        std::fs::write(&path, body).expect("write fixture file");
    }
    dir
}

fn context(root: &Path) -> ToolContext {
    ToolContext {
        working_directory: root.to_path_buf(),
        environment: std::collections::HashMap::new(),
        timeout: std::time::Duration::from_secs(30),
        session_id: Some("nav-tools-test".to_string()),
        actor: Some("agent:nav-tools-test".to_string()),
        approval_required: false,
    }
}

/// Restrictive policy, no evidence file: the tests must not write logs, and
/// they must not be handed a permissive gate.
fn runtime() -> ToolRuntime {
    ToolRuntime::restrictive().without_evidence()
}

/// Dispatch one tool and return its payload, asserting the gate allowed it.
async fn call(root: &Path, id: &str, args: Value) -> Value {
    let outcome = dispatch(id, args, &context(root), &runtime()).await;
    assert!(
        outcome.decision.is_allow(),
        "`{id}` must be allowed as a Read tool; gate said {:?}",
        outcome.decision
    );
    let result = outcome
        .result
        .unwrap_or_else(|e| panic!("`{id}` did not execute: {e:#}"));
    assert!(result.success, "`{id}` reported failure: {}", result.output);
    result.output
}

/// Dispatch one tool that must fail — no fabricated success, ever.
async fn call_expect_err(root: &Path, id: &str, args: Value) -> String {
    let outcome = dispatch(id, args, &context(root), &runtime()).await;
    match outcome.result {
        Ok(r) => panic!("`{id}` must not succeed for that request: {}", r.output),
        Err(e) => format!("{e:#}"),
    }
}

fn arr<'a>(value: &'a Value, key: &str) -> &'a Vec<Value> {
    value[key]
        .as_array()
        .unwrap_or_else(|| panic!("`{key}` must be an array: {value}"))
}

// ---------------------------------------------------------------------------
// Shape of the capability
// ---------------------------------------------------------------------------

#[test]
fn all_eight_nav_tools_have_a_real_read_only_executor() {
    assert_eq!(NAV_TOOL_IDS.len(), 8);
    for id in NAV_TOOL_IDS {
        assert!(is_nav_tool(id), "`{id}` must be claimed by nav_tools");
        let tool = get_real_tool(id).unwrap_or_else(|| panic!("`{id}` has no executor behind it"));
        assert_eq!(
            tool.risk_level(),
            RiskLevel::Read,
            "`{id}` must be read-only"
        );
        let p = tool.permissions();
        assert!(
            p.read && !p.write && !p.execute && !p.network && !p.destructive,
            "`{id}` must be read-only: {p:?}"
        );
        assert!(
            tool.bound_operation().is_some(),
            "`{id}` must declare the operation it is bound to"
        );
        assert!(
            !tool.description().is_empty(),
            "`{id}` must carry the catalogue's description"
        );
    }
}

#[test]
fn nav_ids_without_a_namespace_have_no_executor() {
    // The bare engine names are not MCP tool ids; only the namespaced form is.
    for bare in [
        "find_definition",
        "find_references",
        "nav.something_else",
        "nav.index_summary",
    ] {
        assert!(
            get_real_tool(bare).is_none(),
            "`{bare}` must not resolve to an executor"
        );
    }
}

// ---------------------------------------------------------------------------
// The eight queries, each through dispatch
// ---------------------------------------------------------------------------

#[tokio::test]
async fn find_definition_resolves_a_symbol_to_its_declaration() {
    let dir = repo();
    let out = call(
        dir.path(),
        "nav.find_definition",
        json!({"file": UTIL, "name": "helper"}),
    )
    .await;

    assert_eq!(out["engine"], "zylcode-nav");
    assert!(out["unresolved"].is_null(), "resolved: {out}");
    let definition = &out["definition"];
    assert_eq!(definition["name"], "helper");
    assert_eq!(definition["file"], UTIL);
    assert_eq!(definition["range"]["start_line"], 1);
    assert!(definition["signature"].is_string());
    assert_eq!(definition["kind"], "function");
}

#[tokio::test]
async fn find_references_is_evidence_labelled_and_never_fabricated() {
    let dir = repo();
    let out = call(
        dir.path(),
        "nav.find_references",
        json!({"file": UTIL, "name": "helper"}),
    )
    .await;

    let references = arr(&out, "references");
    assert!(!references.is_empty(), "the two call sites must be found");
    for reference in references {
        assert_eq!(
            reference["evidence"], "DETERMINISTIC",
            "an index-resolved reference must be graded: {reference}"
        );
        assert!(reference["file"].is_string());
        assert!(reference["excerpt"].is_string());
        assert!(reference["range"]["start_line"].is_u64());
    }
    let files: Vec<&str> = references
        .iter()
        .map(|r| r["file"].as_str().expect("file is a string"))
        .collect();
    assert!(files.contains(&LIB), "qualified call: {files:?}");
    assert!(
        files.contains(&API),
        "call through the import binding: {files:?}"
    );
    assert!(
        out["deterministic_count"].as_u64().expect("count") >= 2,
        "at least the two call sites: {out}"
    );

    // A symbol nothing refers to reports an empty list rather than guesses.
    let quiet = call(
        dir.path(),
        "nav.find_references",
        json!({"file": UTIL, "name": "never_called", "include_definition": false}),
    )
    .await;
    assert!(
        arr(&quiet, "references").is_empty(),
        "nothing refers to `never_called`: {quiet}"
    );
    assert!(
        arr(&quiet, "possible").is_empty(),
        "no textual correspondence may be invented either: {quiet}"
    );
    assert!(quiet["unresolved"].is_null(), "{quiet}");

    // An unresolvable selector states the failure instead of returning hits.
    let unknown = call(
        dir.path(),
        "nav.find_references",
        json!({"file": UTIL, "name": "definitely_not_here"}),
    )
    .await;
    assert!(unknown["unresolved"].is_string(), "{unknown}");
    assert!(arr(&unknown, "references").is_empty());
}

#[tokio::test]
async fn callers_and_callees_are_labelled_not_text_matched() {
    let dir = repo();
    let callers = call(
        dir.path(),
        "nav.find_callers",
        json!({"file": UTIL, "name": "helper"}),
    )
    .await;

    assert!(callers["unresolved"].is_null(), "{callers}");
    assert_eq!(callers["heuristic_count"], 0, "{callers}");
    assert_eq!(callers["unsupported_count"], 0, "{callers}");
    assert_eq!(
        callers["deterministic_count"].as_u64().expect("count"),
        2,
        "`entry` and `make` both call `helper`: {callers}"
    );
    for relation in arr(&callers, "relations") {
        assert_eq!(relation["evidence"], "DETERMINISTIC", "{relation}");
        assert!(
            relation["note"].is_string() && !relation["note"].as_str().unwrap().is_empty(),
            "every relation carries its reasoning: {relation}"
        );
        assert!(relation["site"]["enclosing"].is_string(), "{relation}");
        assert_eq!(relation["resolved"]["name"], "helper", "{relation}");
    }

    let callees = call(
        dir.path(),
        "nav.find_callees",
        json!({"file": LIB, "name": "entry"}),
    )
    .await;
    assert_eq!(callees["deterministic_count"].as_u64().expect("count"), 1);
    assert_eq!(
        callees["relations"][0]["resolved"]["name"], "helper",
        "{callees}"
    );
}

#[tokio::test]
async fn file_dependencies_and_dependents_carry_provenance() {
    let dir = repo();

    let out = call(dir.path(), "nav.file_dependencies", json!({"file": LIB})).await;
    assert_eq!(out["file"], LIB);
    assert_eq!(
        out["resolved_count"].as_u64().expect("count"),
        2,
        "`mod util;` and `mod api;` both resolve: {out}"
    );
    for dependency in arr(&out, "dependencies") {
        assert!(
            dependency["detail"]
                .as_str()
                .expect("detail")
                .contains("--imports-->"),
            "every edge must be human-readable: {dependency}"
        );
        assert_eq!(dependency["evidence"], "DETERMINISTIC");
        assert!(dependency["excerpt"].is_string(), "{dependency}");
    }

    let reverse = call(dir.path(), "nav.file_dependents", json!({"file": UTIL})).await;
    let dependents = arr(&reverse, "dependencies");
    assert!(
        dependents.len() >= 2,
        "lib.rs declares the module and api.rs imports from it: {reverse}"
    );
    assert!(
        dependents.iter().all(|d| d["evidence"] == "DETERMINISTIC"),
        "{reverse}"
    );

    // A file that is not in the repository reports nothing rather than
    // inventing neighbours.
    let missing = call(
        dir.path(),
        "nav.file_dependencies",
        json!({"file": "crates/demo/src/nope.rs"}),
    )
    .await;
    assert!(arr(&missing, "dependencies").is_empty(), "{missing}");
}

#[tokio::test]
async fn symbol_impact_reports_relationships_not_predictions() {
    let dir = repo();
    let out = call(
        dir.path(),
        "nav.symbol_impact",
        json!({"file": UTIL, "name": "helper", "depth": 3}),
    )
    .await;

    assert_eq!(out["subject"]["kind"], "symbol", "{out}");
    assert!(out["unresolved_subject"].is_null(), "{out}");
    let summary = out["summary"].as_str().expect("summary");
    assert!(
        summary.contains("not predictions"),
        "the non-prediction caveat must be stated: {summary}"
    );
    assert!(
        out["direct_count"].as_u64().expect("count") >= 1,
        "`entry` and `make` depend on `helper`: {out}"
    );
    for item in arr(&out, "items") {
        assert!(
            [
                "DIRECT_DEPENDENT",
                "TRANSITIVE_DEPENDENT",
                "POSSIBLE_TEXTUAL_REFERENCE",
                "UNRESOLVED"
            ]
            .contains(&item["class"].as_str().expect("class")),
            "unknown impact class: {item}"
        );
        assert!(item["provenance"].is_string(), "{item}");
    }

    // An unresolvable subject is stated, not answered.
    let unknown = call(
        dir.path(),
        "nav.symbol_impact",
        json!({"file": UTIL, "name": "definitely_not_here"}),
    )
    .await;
    assert!(unknown["unresolved_subject"].is_string(), "{unknown}");
    assert!(arr(&unknown, "items").is_empty(), "{unknown}");
}

#[tokio::test]
async fn graph_query_filters_by_edge_kind_with_traceable_edges() {
    let dir = repo();

    let imports = call(
        dir.path(),
        "nav.repository_graph_query",
        json!({"edge_kinds": ["imports"], "limit": 50}),
    )
    .await;
    let edges = arr(&imports, "edges");
    assert!(!edges.is_empty(), "{imports}");
    for edge in edges {
        assert_eq!(edge["kind"], "imports", "{edge}");
        assert!(edge["detail"]
            .as_str()
            .expect("detail")
            .contains("--imports-->"));
        assert_eq!(edge["evidence"], "DETERMINISTIC", "{edge}");
        assert!(
            edge["source_file"].is_string(),
            "an edge must be traceable to a file: {edge}"
        );
    }
    assert!(!arr(&imports, "nodes").is_empty());

    let calls = call(
        dir.path(),
        "nav.repository_graph_query",
        json!({"edge_kinds": ["calls"], "limit": 50}),
    )
    .await;
    for edge in arr(&calls, "edges") {
        assert_eq!(edge["kind"], "calls", "the filter must hold: {edge}");
    }
    assert!(
        arr(&calls, "edges").as_slice() != arr(&imports, "edges").as_slice(),
        "the two filters must not return the same thing"
    );

    // Scoping narrows the result rather than silently returning everything.
    let scoped = call(
        dir.path(),
        "nav.repository_graph_query",
        json!({"focus": LIB, "depth": 1, "edge_kinds": ["imports"], "limit": 50}),
    )
    .await;
    assert!(
        arr(&scoped, "edges").len() <= arr(&imports, "edges").len(),
        "{scoped}"
    );
    assert!(
        arr(&scoped, "edges")
            .iter()
            .all(|e| { e["from"]["id"] == LIB || e["to"]["id"] == LIB }),
        "depth 1 from a focus must only return edges touching it: {scoped}"
    );
}

// ---------------------------------------------------------------------------
// Failure behaviour
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_selectorless_request_fails_loudly_instead_of_guessing() {
    let dir = repo();
    let error = call_expect_err(dir.path(), "nav.find_definition", json!({})).await;
    assert!(
        error.contains("selector") || error.contains("invalid request"),
        "the reason must say what was missing: {error}"
    );

    let error = call_expect_err(dir.path(), "nav.find_references", json!({"limit": 5})).await;
    assert!(error.contains("selector"), "…and the same again: {error}");

    let error = call_expect_err(dir.path(), "nav.file_dependencies", json!({})).await;
    assert!(
        error.contains("file"),
        "…and file tools need a file: {error}"
    );
}

#[tokio::test]
async fn an_id_without_an_executor_is_not_a_success() {
    let dir = repo();
    let error = call_expect_err(dir.path(), "nav.not_a_tool", json!({"file": UTIL})).await;
    assert!(
        error.contains("no executor"),
        "an unsupported id must fail closed: {error}"
    );
}

#[tokio::test]
async fn repeated_queries_are_consistent_and_scoped_to_their_root() {
    let a = repo();
    let b = repo();

    let first = call(
        a.path(),
        "nav.find_references",
        json!({"file": UTIL, "name": "helper"}),
    )
    .await;
    let second = call(
        a.path(),
        "nav.find_references",
        json!({"file": UTIL, "name": "helper"}),
    )
    .await;
    assert_eq!(
        first["references"], second["references"],
        "a cached index must answer exactly like a fresh one"
    );

    // A different root must not inherit the first repository's facts.
    let other = call(
        b.path(),
        "nav.find_definition",
        json!({"file": UTIL, "name": "helper"}),
    )
    .await;
    assert_eq!(other["definition"]["file"], UTIL);

    let wrong_root = call(
        a.path(),
        "nav.find_definition",
        json!({"file": API, "name": "make"}),
    )
    .await;
    assert_eq!(wrong_root["definition"]["file"], API);
}
