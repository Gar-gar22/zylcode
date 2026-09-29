//! End-to-end navigation tests against deterministic fixture repositories.
//!
//! Every assertion is about *evidence* — which symbol a reference was
//! attributed to, how sure the engine says it is, and what it refused to
//! claim — rather than merely that something came back.

use std::fs;
use std::path::Path;

use serde_json::json;
use zylcode_nav::index::RepoIndex;
use zylcode_nav::model::*;
use zylcode_nav::payload;
use zylcode_nav::queries::{
    file_dependencies, file_dependents, find_callees, find_callers, find_references,
    repository_graph_query, symbol_impact, CallQuery, ImpactQuery, ReferenceRequest,
    SymbolSelector,
};

// ---------------------------------------------------------------------------
// Fixture plumbing
// ---------------------------------------------------------------------------

fn put(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().expect("fixture parent")).expect("mkdir");
    fs::write(path, body).expect("write fixture");
}

fn repo(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temp dir");
    for (rel, body) in files {
        put(dir.path(), rel, body);
    }
    dir
}

fn qualified(name: &str) -> SymbolSelector {
    SymbolSelector::Qualified {
        qualified_name: name.to_string(),
        crate_key: None,
        file: None,
    }
}

fn at(file: &str, line: u32) -> SymbolSelector {
    SymbolSelector::At {
        file: file.to_string(),
        line,
    }
}

// ---------------------------------------------------------------------------
// Fixture A — a small Rust package with deliberate edge cases
// ---------------------------------------------------------------------------

const CARGO_TOML: &str = "\
[package]
name = \"demo\"
version = \"0.1.0\"
edition = \"2021\"
";

const LIB_RS: &str = "\
pub mod ledger;
pub mod util;
pub mod api;
pub mod other;

pub fn entry() {
    util::helper();
}
";

const UTIL_RS: &str = "\
pub fn helper() -> u32 {
    1
}

pub fn never_used() -> u32 {
    2
}
";

const LEDGER_RS: &str = "\
pub struct LedgerEntry {
    pub id: u32,
}

impl LedgerEntry {
    pub fn total(&self) -> u32 {
        self.id
    }
}

pub fn add(entry: &LedgerEntry) -> u32 {
    entry.total()
}
";

const API_RS: &str = "\
use crate::ledger::LedgerEntry as Entry;

pub fn make(id: u32) -> Entry {
    Entry { id }
}

/// Not imported here on purpose: a bare name that is out of scope must come
/// back as a *possible* textual reference, never as a real one.
pub fn not_really() -> u32 {
    helper()
}
";

const OTHER_RS: &str = "\
pub fn helper() -> u32 {
    3
}

pub fn call_local() -> u32 {
    helper()
}
";

const LIB: &str = "crates/demo/src/lib.rs";
const UTIL: &str = "crates/demo/src/util.rs";
const LEDGER: &str = "crates/demo/src/ledger.rs";
const API: &str = "crates/demo/src/api.rs";
const OTHER: &str = "crates/demo/src/other.rs";

fn rust_repo() -> tempfile::TempDir {
    repo(&[
        ("crates/demo/Cargo.toml", CARGO_TOML),
        (LIB, LIB_RS),
        (UTIL, UTIL_RS),
        (LEDGER, LEDGER_RS),
        (API, API_RS),
        (OTHER, OTHER_RS),
    ])
}

// ---------------------------------------------------------------------------
// Fixture B — a tiny TypeScript package with a cycle and a broken import
// ---------------------------------------------------------------------------

const X_TS: &str = "\
import { y } from \"./y\";

export function x(): number {
  return y();
}
";

const Y_TS: &str = "\
import { x } from \"./x\";

export function y(): number {
  return x();
}
";

const Z_TS: &str = "\
import { missing } from \"./does-not-exist\";

export function z(): number {
  return 0;
}
";

const X: &str = "web/src/x.ts";
const Y: &str = "web/src/y.ts";
const Z: &str = "web/src/z.ts";
const W: &str = "web/src/w.ts";

const W_TS: &str = "\
class Base {}

export class Child extends Base {
  constructor() {
    super();
  }
}
";

fn ts_repo() -> tempfile::TempDir {
    repo(&[
        (
            "web/package.json",
            "{\n  \"name\": \"web\",\n  \"version\": \"1.0.0\"\n}\n",
        ),
        (X, X_TS),
        (Y, Y_TS),
        (Z, Z_TS),
        (W, W_TS),
    ])
}

// ---------------------------------------------------------------------------
// Definitions
// ---------------------------------------------------------------------------

#[test]
fn definitions_resolve_by_name_by_qualified_name_and_by_location() {
    let dir = rust_repo();
    let index = RepoIndex::build(dir.path()).expect("index");

    let by_name = index.resolve(&SymbolSelector::Name {
        name: "helper".to_string(),
        file: Some(UTIL.to_string()),
    });
    assert!(by_name.unresolved.is_none(), "{:?}", by_name.unresolved);
    assert!(by_name.candidates.is_empty());
    let def = by_name.definition.expect("`helper` resolves in util.rs");
    assert_eq!(def.qualified_name, "crate::util::helper");
    assert_eq!(def.file, UTIL);
    assert_eq!(def.kind, SymbolKind::Function);
    assert!(def.name_range.start_line > 0);
    assert!(def.range.contains(def.name_range.start_byte));

    let by_qualified = index.resolve(&qualified("crate::ledger::LedgerEntry"));
    assert_eq!(
        by_qualified.definition.map(|s| (s.qualified_name, s.file)),
        Some(("crate::ledger::LedgerEntry".to_string(), LEDGER.to_string()))
    );

    // Go-to-definition by line: the declaration on line 1 of util.rs.
    assert_eq!(
        index.resolve(&at(UTIL, 1)).definition.map(|s| s.name),
        Some("helper".to_string())
    );

    // Two `helper` declarations exist; a bare name must be offered, not guessed.
    let ambiguous = index.resolve(&SymbolSelector::Name {
        name: "helper".to_string(),
        file: None,
    });
    assert_eq!(
        ambiguous.candidates.len(),
        2,
        "both declarations must be offered: {:?}",
        ambiguous
            .candidates
            .iter()
            .map(|s| &s.id)
            .collect::<Vec<_>>()
    );
    assert!(ambiguous.candidates.iter().any(|s| s.file == UTIL));
    assert!(ambiguous.candidates.iter().any(|s| s.file == OTHER));
}

#[test]
fn an_ambiguous_selector_refuses_to_search() {
    let dir = rust_repo();
    let index = RepoIndex::build(dir.path()).expect("index");

    let res = find_references(
        &index,
        &ReferenceRequest {
            target: SymbolSelector::Name {
                name: "helper".to_string(),
                file: None,
            },
            ..Default::default()
        },
    );
    assert!(res.references.is_empty(), "no guessing on ambiguity");
    assert!(res.possible.is_empty());
    assert!(res.unresolved.is_some(), "must say why it stopped");
    assert_eq!(res.deterministic_count, 0);
}

// ---------------------------------------------------------------------------
// References
// ---------------------------------------------------------------------------

#[test]
fn cross_file_references_are_deterministic_and_complete() {
    let dir = rust_repo();
    let index = RepoIndex::build(dir.path()).expect("index");

    let res = find_references(
        &index,
        &ReferenceRequest {
            target: qualified("crate::util::helper"),
            ..Default::default()
        },
    );
    assert!(res.unresolved.is_none(), "{:?}", res.unresolved);

    let files: Vec<&str> = res.references.iter().map(|r| r.file.as_str()).collect();
    assert!(files.contains(&UTIL), "the declaration: {files:?}");
    assert!(files.contains(&LIB), "the cross-file call site: {files:?}");
    assert_eq!(
        res.deterministic_count,
        res.references.len(),
        "the count must match what was returned"
    );
    for r in &res.references {
        assert_eq!(r.evidence, Evidence::Deterministic);
        assert!(!r.excerpt.is_empty(), "every reference is clickable");
        assert!(
            !r.via.is_empty(),
            "every reference says how it was attributed"
        );
    }

    // `api.rs` says `helper()` without importing it: textual only, and it is
    // kept in the separate `possible` bucket rather than promoted.
    assert_eq!(res.heuristic_count, 1, "{:?}", res.possible);
    assert_eq!(res.possible.len(), 1);
    assert_eq!(res.possible[0].file, API);
    assert_eq!(res.possible[0].evidence, Evidence::Heuristic);
}

#[test]
fn duplicate_names_do_not_bleed_across_modules() {
    let dir = rust_repo();
    let index = RepoIndex::build(dir.path()).expect("index");

    let util_refs = find_references(
        &index,
        &ReferenceRequest {
            target: qualified("crate::util::helper"),
            ..Default::default()
        },
    );
    assert!(
        !util_refs.references.iter().any(|r| r.file == OTHER),
        "`other.rs` declares its own `helper`"
    );

    let other_refs = find_references(
        &index,
        &ReferenceRequest {
            target: qualified("crate::other::helper"),
            ..Default::default()
        },
    );
    assert!(other_refs.unresolved.is_none());
    assert!(
        !other_refs.references.iter().any(|r| r.file == UTIL),
        "the sibling module must not leak in"
    );
    assert!(
        other_refs
            .references
            .iter()
            .any(|r| r.file == OTHER && r.kind == ReferenceKind::Call),
        "its own call site must be found: {:?}",
        other_refs.references
    );
    for r in &other_refs.references {
        assert_eq!(r.evidence, Evidence::Deterministic);
    }
}

#[test]
fn a_renamed_import_is_followed_to_the_original_symbol() {
    let dir = rust_repo();
    let index = RepoIndex::build(dir.path()).expect("index");

    // The alias itself resolves to the declared symbol.
    let via_alias = index.resolve(&SymbolSelector::Name {
        name: "Entry".to_string(),
        file: Some(API.to_string()),
    });
    assert_eq!(
        via_alias.definition.map(|s| s.qualified_name),
        Some("crate::ledger::LedgerEntry".to_string()),
        "the alias must not be treated as a separate symbol"
    );

    let res = find_references(
        &index,
        &ReferenceRequest {
            target: qualified("crate::ledger::LedgerEntry"),
            ..Default::default()
        },
    );
    assert!(res.unresolved.is_none());
    assert_eq!(res.heuristic_count, 0);
    assert!(
        res.references
            .iter()
            .any(|r| r.file == API && r.range.start_line == 1),
        "the `use ... as Entry` line is a reference"
    );
    assert!(
        res.references
            .iter()
            .any(|r| r.file == API && r.range.start_line == 4),
        "uses through the alias must be found: {:?}",
        res.references
    );
    assert!(
        res.references
            .iter()
            .all(|r| r.file == API || r.file == LEDGER),
        "the alias must not make the symbol appear in unrelated files"
    );
}

#[test]
fn a_symbol_with_no_references_reports_none_rather_than_guessing() {
    let dir = rust_repo();
    let index = RepoIndex::build(dir.path()).expect("index");

    let res = find_references(
        &index,
        &ReferenceRequest {
            target: SymbolSelector::Name {
                name: "never_used".to_string(),
                file: Some(UTIL.to_string()),
            },
            include_definition: false,
            ..Default::default()
        },
    );
    assert!(res.definition.is_some(), "the symbol itself exists");
    assert!(res.references.is_empty());
    assert!(res.possible.is_empty());
    assert_eq!(res.deterministic_count, 0);
    assert_eq!(res.heuristic_count, 0);
    assert!(
        res.unresolved.is_none(),
        "nothing is wrong — it simply has no use sites"
    );
}

#[test]
fn an_unknown_selector_is_unresolved_never_fabricated() {
    let dir = rust_repo();
    let index = RepoIndex::build(dir.path()).expect("index");

    let res = find_references(
        &index,
        &ReferenceRequest {
            target: SymbolSelector::Name {
                name: "definitely_missing".to_string(),
                file: None,
            },
            ..Default::default()
        },
    );
    assert!(res.definition.is_none());
    assert!(res.references.is_empty());
    assert!(res.possible.is_empty());
    assert!(res.unresolved.is_some(), "the failure must be stated");

    // A file that is not in the index returns an empty result, not an error
    // and certainly not invented edges.
    let deps = file_dependencies(&index, "no/such/file.rs");
    assert!(deps.dependencies.is_empty());
    assert_eq!(deps.resolved_count, 0);
    assert_eq!(deps.unresolved_count, 0);

    // And a reference query for a definition line that has no definition.
    let nothing = index.resolve(&at(LIB, 9_999));
    assert!(nothing.definition.is_none());
    assert!(nothing.unresolved.is_some());
}

#[test]
fn same_file_references_are_found() {
    let dir = rust_repo();
    let index = RepoIndex::build(dir.path()).expect("index");

    let res = find_references(
        &index,
        &ReferenceRequest {
            target: qualified("crate::other::helper"),
            include_definition: false,
            ..Default::default()
        },
    );
    let same_file: Vec<_> = res.references.iter().filter(|r| r.file == OTHER).collect();
    assert_eq!(same_file.len(), 1, "{:?}", res.references);
    assert_eq!(same_file[0].kind, ReferenceKind::Call);
    assert_eq!(same_file[0].evidence, Evidence::Deterministic);
    assert!(
        same_file[0].excerpt.contains("helper()"),
        "{}",
        same_file[0].excerpt
    );
}

// ---------------------------------------------------------------------------
// Call hierarchy
// ---------------------------------------------------------------------------

#[test]
fn call_hierarchy_is_labelled_and_never_text_search() {
    let dir = rust_repo();
    let index = RepoIndex::build(dir.path()).expect("index");

    let callers = find_callers(
        &index,
        &CallQuery {
            target: qualified("crate::util::helper"),
            limit: 100,
        },
    );
    assert!(callers.unresolved.is_none());
    assert_eq!(callers.deterministic_count, 1);
    assert_eq!(callers.heuristic_count, 0);
    assert_eq!(callers.relations.len(), 1);
    let rel = &callers.relations[0];
    assert_eq!(rel.evidence, Evidence::Deterministic);
    assert_eq!(rel.site.file, LIB);
    assert_eq!(
        rel.site.enclosing.as_deref(),
        Some("crate::entry"),
        "the caller's enclosing symbol must be known"
    );
    assert_eq!(
        rel.resolved.as_ref().map(|s| s.qualified_name.as_str()),
        Some("crate::util::helper")
    );
    assert!(!rel.note.is_empty(), "every relation carries its reasoning");
    assert!(
        !callers.relations.iter().any(|r| r.site.file == OTHER),
        "`other.rs` calls a *different* `helper` — text search would have added it"
    );

    let callees = find_callees(
        &index,
        &CallQuery {
            target: qualified("crate::entry"),
            limit: 100,
        },
    );
    assert_eq!(callees.deterministic_count, 1);
    assert_eq!(callees.relations.len(), 1);
    assert_eq!(
        callees.relations[0]
            .resolved
            .as_ref()
            .map(|s| s.qualified_name.as_str()),
        Some("crate::util::helper")
    );

    // A member call has no receiver type in the parse tree, so it can only be
    // a name correspondence — reported as HEURISTIC, never as a caller we know.
    let member = find_callees(
        &index,
        &CallQuery {
            target: SymbolSelector::Name {
                name: "add".to_string(),
                file: Some(LEDGER.to_string()),
            },
            limit: 100,
        },
    );
    assert_eq!(
        member.deterministic_count, 0,
        "a member call must never be sold as deterministic: {:?}",
        member.relations
    );
    assert_eq!(member.heuristic_count, 1);
    assert_eq!(member.relations.len(), 1);
    let member_rel = &member.relations[0];
    assert_eq!(member_rel.evidence, Evidence::Heuristic);
    assert!(
        member_rel.note.contains("name correspondence"),
        "{}",
        member_rel.note
    );
    assert_eq!(
        member_rel.resolved.as_ref().map(|s| s.name.as_str()),
        Some("total")
    );

    // An unresolvable target is reported as such, with no relations.
    let nobody = find_callers(
        &index,
        &CallQuery {
            target: SymbolSelector::Name {
                name: "definitely_missing".to_string(),
                file: None,
            },
            limit: 100,
        },
    );
    assert!(nobody.target.is_none());
    assert!(nobody.relations.is_empty());
    assert!(nobody.unresolved.is_some());
}

#[test]
fn a_callee_with_no_identifier_to_attribute_is_labelled_unsupported() {
    let dir = ts_repo();
    let index = RepoIndex::build(dir.path()).expect("index");

    // `super()` has no identifier node at all, so there is nothing to
    // attribute — the third label exists precisely so that a caller is never
    // invented out of the text of the callee.
    let res = find_callees(
        &index,
        &CallQuery {
            target: SymbolSelector::Name {
                name: "constructor".to_string(),
                file: Some(W.to_string()),
            },
            limit: 100,
        },
    );
    assert!(res.target.is_some(), "{:?}", res.unresolved);
    assert_eq!(
        res.relations.len(),
        1,
        "the super() call must be reported: {:?}",
        res.relations
    );
    let rel = &res.relations[0];
    assert_eq!(rel.evidence, Evidence::Unsupported);
    assert!(rel.resolved.is_none());
    assert!(!rel.note.is_empty(), "{}", rel.note);
    assert_eq!(res.unsupported_count, 1);
    assert_eq!(res.deterministic_count, 0);
    assert_eq!(res.heuristic_count, 0);
}

// ---------------------------------------------------------------------------
// Imports, dependency graph, cycles
// ---------------------------------------------------------------------------

#[test]
fn rust_module_and_import_edges_resolve() {
    let dir = rust_repo();
    let index = RepoIndex::build(dir.path()).expect("index");

    let deps = file_dependencies(&index, LIB);
    assert_eq!(deps.unresolved_count, 0, "{:?}", deps.dependencies);
    assert!(deps.resolved_count >= 4, "four `mod` declarations");
    for d in &deps.dependencies {
        assert_eq!(d.evidence, Evidence::Deterministic);
        assert!(d.detail.contains("--imports-->"), "{}", d.detail);
    }

    let deps = file_dependencies(&index, API);
    assert_eq!(deps.unresolved_count, 0, "{:?}", deps.dependencies);
    assert!(deps.dependencies.iter().any(|d| {
        matches!(&d.resolution, ImportResolution::Resolved { file } if file == LEDGER)
    }));

    let dependents = file_dependents(&index, LEDGER);
    let sources: Vec<&str> = dependents
        .dependencies
        .iter()
        .map(|d| d.file.as_str())
        .collect();
    assert!(
        sources.contains(&LIB),
        "`mod ledger;` pulls it in: {sources:?}"
    );
    assert!(
        sources.contains(&API),
        "`use crate::ledger::...` pulls it in: {sources:?}"
    );
}

#[test]
fn typescript_graph_captures_imports_cycles_and_broken_imports() {
    let dir = ts_repo();
    let index = RepoIndex::build(dir.path()).expect("index");

    let x = file_dependencies(&index, X);
    assert_eq!(x.resolved_count, 1);
    assert_eq!(x.unresolved_count, 0);
    assert!(
        matches!(&x.dependencies[0].resolution, ImportResolution::Resolved { file } if file == Y)
    );
    assert!(x.dependencies[0].detail.contains("--imports-->"));

    let dependents = file_dependents(&index, Y);
    assert!(dependents.dependencies.iter().any(|d| d.file == X));

    // A broken import is reported as broken.
    let z = file_dependencies(&index, Z);
    assert_eq!(z.unresolved_count, 1, "{:?}", z.dependencies);
    assert_eq!(z.resolved_count, 0);

    let graph = repository_graph_query(
        &index,
        &GraphQuery {
            include_cycles: true,
            ..Default::default()
        },
    );
    assert!(
        graph
            .cycles
            .iter()
            .any(|c| c.contains(&X.to_string()) && c.contains(&Y.to_string())),
        "x <-> y is a real cycle: {:?}",
        graph.cycles
    );
    assert!(
        graph
            .edges
            .iter()
            .any(|e| e.to.id.starts_with("unresolved::")),
        "the broken import must still be visible"
    );
    for e in &graph.edges {
        assert!(!e.detail.is_empty());
        assert_eq!(e.evidence, Evidence::Deterministic);
        assert!(e.source_file.is_some(), "every edge is traceable to a file");
    }
    assert!(graph.nodes.iter().any(|n| n.id == X));
    assert!(!graph.truncated);
}

#[test]
fn graph_scoping_trims_to_the_requested_neighbourhood() {
    let dir = ts_repo();
    let index = RepoIndex::build(dir.path()).expect("index");

    let unscoped = repository_graph_query(&index, &GraphQuery::default());
    assert!(unscoped.edges.len() >= 2, "no focus means everything");

    let scoped = repository_graph_query(
        &index,
        &GraphQuery {
            focus: Some(X.to_string()),
            depth: 1,
            ..Default::default()
        },
    );
    assert!(!scoped.edges.is_empty());
    assert!(
        scoped.edges.iter().all(|e| e.from.id == X || e.to.id == X),
        "depth 1 must stay on `focus`: {:?}",
        scoped.edges.iter().map(|e| &e.detail).collect::<Vec<_>>()
    );
    assert!(
        scoped.edges.len() < unscoped.edges.len(),
        "scoping must actually reduce the graph"
    );

    // Symbol-level edges are opt-in: a file graph stays a file graph.
    let files_only = repository_graph_query(
        &index,
        &GraphQuery {
            edge_kinds: vec![EdgeKind::Imports],
            node_kinds: vec![NodeKind::File],
            ..Default::default()
        },
    );
    assert!(files_only.edges.iter().all(|e| e.kind == EdgeKind::Imports));
    assert!(
        !files_only
            .edges
            .iter()
            .any(|e| e.kind == EdgeKind::Contains),
        "symbol edges stay opt-in"
    );
}

// ---------------------------------------------------------------------------
// Impact
// ---------------------------------------------------------------------------

#[test]
fn impact_reports_relationships_and_never_predictions() {
    let dir = rust_repo();
    let index = RepoIndex::build(dir.path()).expect("index");

    let (report, subject) = symbol_impact(
        &index,
        &ImpactQuery {
            target: qualified("crate::util::helper"),
            depth: Some(3),
            limit: 100,
        },
    );
    assert!(subject.is_some());
    assert!(report.unresolved_subject.is_none());
    assert!(
        report.direct_count >= 1,
        "`lib.rs` references and calls it: {:?}",
        report.items
    );
    assert!(
        report.possible_count >= 1,
        "`api.rs` mentions it without a binding: {:?}",
        report.items
    );
    assert!(report.summary.contains("not predictions"));
    assert!(
        !report.summary.to_lowercase().contains("will break"),
        "impact must never assert a consequence"
    );
    for item in &report.items {
        assert!(matches!(
            item.class,
            ImpactClass::DirectDependent
                | ImpactClass::TransitiveDependent
                | ImpactClass::PossibleTextualReference
                | ImpactClass::Unresolved
        ));
        assert!(matches!(
            item.evidence,
            Evidence::Deterministic | Evidence::Heuristic
        ));
        assert!(!item.label.is_empty());
        assert!(
            !item.via.is_empty(),
            "each item shows the edge that found it"
        );
        if let Some(excerpt) = &item.excerpt {
            assert!(!excerpt.is_empty());
        }
    }
    let possible: Vec<_> = report
        .items
        .iter()
        .filter(|i| i.class == ImpactClass::PossibleTextualReference)
        .collect();
    assert_eq!(possible[0].evidence, Evidence::Heuristic);
    assert_eq!(possible[0].provenance, Provenance::Inferred);

    // An unresolvable subject claims nothing at all.
    let (unknown, subject) = symbol_impact(
        &index,
        &ImpactQuery {
            target: SymbolSelector::Name {
                name: "not_a_symbol".to_string(),
                file: None,
            },
            depth: None,
            limit: 10,
        },
    );
    assert!(subject.is_none());
    assert!(unknown.items.is_empty());
    assert!(unknown.unresolved_subject.is_some());
    assert!(unknown.summary.contains("no relationships are claimed"));
}

#[test]
fn file_impact_reports_importers_without_predicting() {
    let dir = rust_repo();
    let index = RepoIndex::build(dir.path()).expect("index");

    let report = zylcode_nav::queries::file_impact(&index, UTIL, 3, 100);
    assert!(matches!(&report.subject, ImpactSubject::File { path } if path == UTIL));
    assert!(report.direct_count >= 1, "lib.rs declares the module");
    assert!(!report.summary.to_lowercase().contains("will break"));

    let missing = zylcode_nav::queries::file_impact(&index, "no/such/file.rs", 3, 100);
    assert!(missing.items.is_empty());
    assert!(missing.unresolved_subject.is_some());
}

#[test]
fn a_file_is_never_reported_as_its_own_direct_dependent() {
    // `use super::*;` inside a file's own `#[cfg(test)] mod tests` genuinely
    // resolves back to that file. The edge is real, so `file_dependencies`
    // must still show it — what must not happen is the impact view listing
    // the subject as one of its own dependents, a true line that reads as a
    // false claim. (Discovered by querying the live engine, where
    // `cache.rs --imports--> cache.rs` surfaced as a DIRECT_DEPENDENT.)
    let dir = repo(&[
        ("crates/demo/Cargo.toml", CARGO_TOML),
        ("crates/demo/src/lib.rs", "pub mod lonely;\n"),
        (
            "crates/demo/src/lonely.rs",
            "pub fn helper() -> u32 {\n    1\n}\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn it_works() {\n        assert_eq!(helper(), 1);\n    }\n}\n",
        ),
    ]);
    let index = RepoIndex::build(dir.path()).expect("index");
    let file = "crates/demo/src/lonely.rs";

    let deps = file_dependencies(&index, file);
    assert!(
        deps.dependencies
            .iter()
            .any(|d| d.detail == format!("{file} --imports--> {file}")),
        "the self-import is a real edge and must stay in the dependency list: {:?}",
        deps.dependencies
    );

    let report = zylcode_nav::queries::file_impact(&index, file, 3, 100);
    assert!(
        !report.items.iter().any(|item| item.id == file),
        "the subject must not appear among its own dependents: {:?}",
        report.items
    );
    assert!(
        report
            .items
            .iter()
            .any(|item| item.id == "crates/demo/src/lib.rs"),
        "lib.rs declares the module and is still a direct dependent: {:?}",
        report.items
    );
    assert!(
        !report.summary.to_lowercase().contains("will break"),
        "impact reports relationships, never consequences"
    );
}

// ---------------------------------------------------------------------------
// Incremental behaviour
// ---------------------------------------------------------------------------

#[test]
fn an_unchanged_repository_rebuilds_from_the_cache() {
    let dir = rust_repo();
    let first = RepoIndex::build(dir.path()).expect("first build");
    assert_eq!(first.stats.files_parsed, 5, "five source files");
    assert_eq!(first.stats.files_reused, 0);

    let second = RepoIndex::build(dir.path()).expect("second build");
    assert_eq!(second.stats.files_parsed, 0, "nothing changed");
    assert_eq!(second.stats.files_reused, 5, "everything is warm");
    assert_eq!(second.stats.files_removed, 0);
    assert!(second.stats.warm);
    assert_eq!(second.stats.symbols, first.stats.symbols);
}

#[test]
fn editing_one_file_reparses_exactly_one_file() {
    let dir = rust_repo();
    let first = RepoIndex::build(dir.path()).expect("first build");
    assert!(first
        .resolve(&SymbolSelector::Name {
            name: "extra".to_string(),
            file: Some(OTHER.to_string()),
        })
        .definition
        .is_none());

    put(
        dir.path(),
        OTHER,
        &format!("{OTHER_RS}\npub fn extra() -> u32 {{ 4 }}\n"),
    );

    let second = RepoIndex::build(dir.path()).expect("second build");
    assert_eq!(second.stats.files_parsed, 1, "only the edited file");
    assert_eq!(second.stats.files_reused, 4);
    assert_eq!(second.stats.files_removed, 0);
    assert_eq!(second.stats.symbols, first.stats.symbols + 1);
    assert!(second
        .resolve(&SymbolSelector::Name {
            name: "extra".to_string(),
            file: Some(OTHER.to_string()),
        })
        .definition
        .is_some());
}

#[test]
fn deleting_a_file_removes_its_symbols_and_breaks_the_declaration() {
    let dir = rust_repo();
    let _ = RepoIndex::build(dir.path()).expect("first build");
    fs::remove_file(dir.path().join(API)).expect("delete");

    let index = RepoIndex::build(dir.path()).expect("second build");
    assert_eq!(index.stats.files_removed, 1);
    assert_eq!(index.stats.files_parsed, 0);
    assert!(index
        .resolve(&SymbolSelector::Name {
            name: "make".to_string(),
            file: Some(API.to_string()),
        })
        .definition
        .is_none());
    assert!(
        !index.symbols.values().any(|s| s.file == API),
        "no ghost symbols survive the delete"
    );

    // `mod api;` no longer has a file: that is an unresolved edge, stated as such.
    let deps = file_dependencies(&index, LIB);
    assert!(deps.unresolved_count >= 1, "{:?}", deps.dependencies);
    assert!(deps
        .dependencies
        .iter()
        .any(|d| d.detail.contains("unresolved")));
}

#[test]
fn renaming_a_file_moves_its_symbols_to_the_new_path() {
    let dir = rust_repo();
    let before = RepoIndex::build(dir.path()).expect("first build");
    let old_id = before
        .resolve(&SymbolSelector::Name {
            name: "helper".to_string(),
            file: Some(UTIL.to_string()),
        })
        .definition
        .expect("helper exists")
        .id;

    fs::rename(
        dir.path().join(UTIL),
        dir.path().join("crates/demo/src/utilities.rs"),
    )
    .expect("rename");

    let after = RepoIndex::build(dir.path()).expect("second build");
    assert_eq!(after.stats.files_removed, 1, "the old path is gone");
    assert_eq!(after.stats.files_parsed, 1, "the new path is parsed");
    assert_eq!(after.stats.files_reused, 4);
    assert!(
        !after.symbols.values().any(|s| s.file == UTIL),
        "nothing keeps pointing at the old path"
    );

    let renamed = after.resolve(&SymbolSelector::Name {
        name: "helper".to_string(),
        file: Some("crates/demo/src/utilities.rs".to_string()),
    });
    let sym = renamed.definition.expect("the symbol survives the rename");
    assert_eq!(sym.qualified_name, "crate::utilities::helper");
    assert_ne!(sym.id, old_id, "identity follows the module path");

    // References are re-derived, not carried over from the old path.
    let refs = find_references(
        &after,
        &ReferenceRequest {
            target: SymbolSelector::Id { id: sym.id },
            ..Default::default()
        },
    );
    assert!(refs.unresolved.is_none());
    assert!(refs.references.iter().all(|r| r.file != UTIL));
    assert!(refs
        .references
        .iter()
        .any(|r| r.file == "crates/demo/src/utilities.rs"));
    // `mod util;` / `util::helper()` in lib.rs no longer point at a real file,
    // so the stale reference is *not* claimed. Nothing is carried across.
    assert!(
        !refs.references.iter().any(|r| r.file == LIB),
        "a broken reference must not survive the rename: {:?}",
        refs.references
    );
}

// ---------------------------------------------------------------------------
// Payload / dispatcher layer
// ---------------------------------------------------------------------------

#[test]
fn the_dispatcher_answers_every_documented_tool_with_engine_tagged_json() {
    let dir = rust_repo();
    let index = RepoIndex::build(dir.path()).expect("index");

    let cases: Vec<(&str, serde_json::Value)> = vec![
        (
            "find_definition",
            json!({"target": {"kind": "qualified", "qualified_name": "crate::util::helper"}}),
        ),
        (
            "find_references",
            json!({"target": {"kind": "qualified", "qualified_name": "crate::util::helper"}}),
        ),
        (
            "find_callers",
            json!({"target": {"kind": "qualified", "qualified_name": "crate::util::helper"}}),
        ),
        (
            "find_callees",
            json!({"target": {"kind": "qualified", "qualified_name": "crate::entry"}}),
        ),
        ("file_dependencies", json!({"file": LIB})),
        ("file_dependents", json!({"file": LEDGER})),
        (
            "symbol_impact",
            json!({"target": {"kind": "qualified", "qualified_name": "crate::util::helper"}}),
        ),
        ("repository_graph_query", json!({"depth": 1, "limit": 25})),
        ("index_summary", json!({})),
    ];

    for (tool, args) in cases {
        let value = payload::dispatch(&index, tool, args).expect(tool);
        assert_eq!(
            value.get("engine").and_then(|e| e.as_str()),
            Some("zylcode-nav"),
            "{tool} must say who answered"
        );
        assert!(value.is_object(), "{tool} must return an object");
    }

    // A wrong id is an error, never a fake success.
    let err =
        payload::dispatch(&index, "find_things", json!({})).expect_err("unknown tools must fail");
    assert!(err.contains("unknown navigation tool"), "{err}");

    // Flat convenience args work, and empty args fail loudly.
    let flat = payload::dispatch(&index, "find_definition", json!({"file": UTIL, "line": 1}))
        .expect("flat selector");
    assert!(flat.get("definition").is_some());
    assert!(payload::dispatch(&index, "find_definition", json!({})).is_err());
    assert!(payload::dispatch(&index, "file_dependencies", json!({})).is_err());

    let summary = payload::dispatch(&index, "index_summary", json!({})).expect("summary");
    assert_eq!(
        summary.get("engine").and_then(|e| e.as_str()),
        Some("zylcode-nav")
    );
    assert_eq!(
        summary.get("schema_version").and_then(|v| v.as_u64()),
        Some(zylcode_nav::index::SCHEMA_VERSION as u64)
    );

    let citations = payload::citations(&index, &qualified("crate::util::helper"), 5);
    assert!(!citations.is_empty());
    assert!(citations
        .iter()
        .all(|c| c.evidence == Evidence::Deterministic));
    assert!(citations.iter().all(|c| !c.location.is_empty()));
}

#[test]
fn tool_ids_match_the_catalogue_wording() {
    assert_eq!(
        payload::NAV_TOOL_IDS,
        [
            "find_definition",
            "find_references",
            "find_callers",
            "find_callees",
            "file_dependencies",
            "file_dependents",
            "symbol_impact",
            "repository_graph_query",
        ]
    );
}

// ---------------------------------------------------------------------------
// Facts themselves
// ---------------------------------------------------------------------------

#[test]
fn every_indexed_file_carries_a_parser_status_and_no_grammar_is_labelled() {
    let dir = rust_repo();
    let index = RepoIndex::build(dir.path()).expect("index");

    assert_eq!(index.facts.len(), 5, "manifests are not source files");
    for (file, facts) in &index.facts {
        assert!(
            matches!(facts.status, ParseStatus::Parsed),
            "{file}: {:?} ({:?})",
            facts.status,
            facts.error
        );
        assert!(facts.language.is_supported(), "{file}");
        assert!(!facts.content_hash.is_empty(), "{file}");
        assert!(!facts.module.is_empty(), "{file}");
        for sym in &facts.symbols {
            assert_eq!(sym.file, *file, "a symbol never outlives its file");
            assert!(sym.qualified_name.contains("::") || !sym.qualified_name.is_empty());
            assert!(sym.range.start_line > 0);
        }
    }

    // A file with no grammar is counted, not silently dropped.
    put(dir.path(), "crates/demo/README.md", "# not source\n");
    let after = RepoIndex::build(dir.path()).expect("second build");
    assert_eq!(after.facts.len(), 5, "markdown is still not indexed");
}
