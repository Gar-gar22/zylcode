//! Canonical types for deterministic repository navigation.
//!
//! Every fact in this crate is produced by parsing source text with a real
//! grammar ([`tree_sitter`]) or by reading a manifest. Two labels travel with
//! every claim and must never be collapsed:
//!
//! * [`Provenance`] — *where the fact came from* (parsed source, package
//!   metadata, filesystem observation, derivation, inference).
//! * [`Evidence`] — *how strongly the relationship was established*
//!   (deterministic resolution, heuristic name match, or not supported).
//!
//! A heuristic result is always carried next to a deterministic one, never in
//! place of it, and never silently promoted.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Language
// ---------------------------------------------------------------------------

/// Languages this navigation wave materialises.
///
/// Everything else is [`Language::Unsupported`] and is reported as such rather
/// than guessed at.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Language {
    Rust,
    TypeScript,
    JavaScript,
    Unsupported(String),
}

impl Language {
    pub fn from_path(path: &str) -> Self {
        let lower = path.replace('\\', "/");
        let ext = lower.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
        // Directory names such as `.github` produce an "extension" that is the
        // whole path; only accept a real final suffix.
        let has_ext = lower.rsplit('/').next().map(|s| s.contains('.')) == Some(true);
        if !has_ext {
            return Language::Unsupported(String::new());
        }
        match ext.as_str() {
            "rs" => Language::Rust,
            "ts" | "tsx" | "mts" | "cts" => Language::TypeScript,
            "js" | "jsx" | "mjs" | "cjs" => Language::JavaScript,
            other => Language::Unsupported(other.to_string()),
        }
    }

    pub fn as_str(&self) -> String {
        match self {
            Language::Rust => "rust".into(),
            Language::TypeScript => "typescript".into(),
            Language::JavaScript => "javascript".into(),
            Language::Unsupported(ext) => format!("unsupported:{ext}"),
        }
    }

    /// Whether a real grammar exists for this language.
    pub fn is_supported(&self) -> bool {
        matches!(
            self,
            Language::Rust | Language::TypeScript | Language::JavaScript
        )
    }
}

// ---------------------------------------------------------------------------
// Provenance and evidence
// ---------------------------------------------------------------------------

/// How a repository fact was established.
///
/// Deliberately the same vocabulary as `zylcode_core::intelligence::types::
/// Provenance` so navigation results surface through the existing evidence
/// payloads without inventing a second classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    /// Directly observed from the filesystem (file exists, size, content hash).
    Observed,
    /// Extracted by parsing structured content (manifest, AST).
    Parsed,
    /// Computed from other facts (transitive closure, cycle detection).
    Derived,
    /// Inferred heuristically; never presented as deterministic evidence.
    Inferred,
}

impl Provenance {
    pub fn as_str(&self) -> &'static str {
        match self {
            Provenance::Observed => "observed",
            Provenance::Parsed => "parsed",
            Provenance::Derived => "derived",
            Provenance::Inferred => "inferred",
        }
    }
}

/// How strongly a relationship was established.
///
/// This is the label the work order requires on callers/callees and on impact
/// results: `DETERMINISTIC` / `HEURISTIC` / `UNSUPPORTED`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Evidence {
    /// Resolved through the grammar plus module/import scope resolution.
    Deterministic,
    /// Name-only correspondence. Never presented as a semantic relation.
    Heuristic,
    /// No grammar or resolution rule exists for this construct.
    Unsupported,
}

impl Evidence {
    pub fn as_str(&self) -> &'static str {
        match self {
            Evidence::Deterministic => "DETERMINISTIC",
            Evidence::Heuristic => "HEURISTIC",
            Evidence::Unsupported => "UNSUPPORTED",
        }
    }
}

// ---------------------------------------------------------------------------
// Source ranges
// ---------------------------------------------------------------------------

/// A half-open source range. Byte offsets are authoritative; line/column are
/// 1-based and stored so UI surfaces can navigate without re-deriving them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TextRange {
    pub start_byte: u32,
    pub end_byte: u32,
    pub start_line: u32,
    pub start_col: u32,
    pub end_line: u32,
    pub end_col: u32,
}

impl TextRange {
    pub fn contains(&self, byte: u32) -> bool {
        byte >= self.start_byte && byte < self.end_byte
    }

    /// Smallest range covering both inputs.
    pub fn span(&self, other: &TextRange) -> TextRange {
        TextRange {
            start_byte: self.start_byte.min(other.start_byte),
            end_byte: self.end_byte.max(other.end_byte),
            start_line: self.start_line.min(other.start_line),
            start_col: if self.start_byte <= other.start_byte {
                self.start_col
            } else {
                other.start_col
            },
            end_line: self.end_line.max(other.end_line),
            end_col: if self.end_byte >= other.end_byte {
                self.end_col
            } else {
                other.end_col
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Symbols
// ---------------------------------------------------------------------------

/// Kind of a symbol.
///
/// The variant set matches `zylcode_core::intelligence::types::SymbolKind` so
/// the core layer can map a navigation symbol onto the existing intelligence
/// model without a lossy translation table.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SymbolKind {
    // Rust
    Module,
    Struct,
    Enum,
    Trait,
    Impl,
    Function,
    Method,
    Constant,
    TypeAlias,
    Macro,
    // TypeScript/JavaScript
    Class,
    Interface,
    Type,
    Component,
    Variable,
    // Generic
    Other(String),
}

impl SymbolKind {
    pub fn as_str(&self) -> String {
        match self {
            SymbolKind::Other(s) => s.clone(),
            SymbolKind::Module => "module".into(),
            SymbolKind::Struct => "struct".into(),
            SymbolKind::Enum => "enum".into(),
            SymbolKind::Trait => "trait".into(),
            SymbolKind::Impl => "impl".into(),
            SymbolKind::Function => "function".into(),
            SymbolKind::Method => "method".into(),
            SymbolKind::Constant => "constant".into(),
            SymbolKind::TypeAlias => "type_alias".into(),
            SymbolKind::Macro => "macro".into(),
            SymbolKind::Class => "class".into(),
            SymbolKind::Interface => "interface".into(),
            SymbolKind::Type => "type".into(),
            SymbolKind::Component => "component".into(),
            SymbolKind::Variable => "variable".into(),
        }
    }
}

/// Visibility of a symbol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    Public,
    PubCrate,
    PubSuper,
    Private,
    Unknown,
}

impl Visibility {
    pub fn as_str(&self) -> &'static str {
        match self {
            Visibility::Public => "public",
            Visibility::PubCrate => "pub_crate",
            Visibility::PubSuper => "pub_super",
            Visibility::Private => "private",
            Visibility::Unknown => "unknown",
        }
    }
}

/// A symbol definition.
///
/// Identity is `(file, qualified_name)`, not the bare name: two functions called
/// `helper` in different modules — or in different `impl` blocks of the same
/// file — are distinct symbols with distinct ids.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Symbol {
    /// Stable id: `<file>#<qualified_name>` (a `@<byte>` suffix is appended by
    /// the index only in the rare case that two symbols share both fields).
    pub id: String,
    /// Repository-relative path of the defining file, `/`-separated.
    pub file: String,
    pub language: Language,
    /// Canonical path, e.g. `crate::intelligence::nav::RepoIndex` for Rust or
    /// `src::components::shell::SurfaceHost::render` for TypeScript.
    pub qualified_name: String,
    /// Bare name, e.g. `render`.
    pub name: String,
    pub kind: SymbolKind,
    /// Full extent of the declaration.
    pub range: TextRange,
    /// Extent of just the identifier (for go-to-definition highlight).
    pub name_range: TextRange,
    /// Qualified name of the immediately enclosing container (`impl`, class,
    /// module, trait, function), if any.
    pub container: Option<String>,
    /// Declaration signature with the body removed (`fn foo(a: u8) -> u8`).
    pub signature: Option<String>,
    pub visibility: Visibility,
    /// Whether the module's consumers can name this symbol.
    pub exported: bool,
    /// Module path segments this symbol belongs to, without the `crate::`
    /// prefix for Rust and without the file extension for TS/JS.
    pub module: String,
    /// File identity of the enclosing crate (Rust crate root, or the package
    /// manifest directory for TS/JS). Used to scope `crate::` / `self::`.
    pub crate_key: String,
}

// ---------------------------------------------------------------------------
// Imports and exports
// ---------------------------------------------------------------------------

/// How an import statement is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportKind {
    /// Rust `use a::b::C;`
    Use,
    /// Rust `mod x;` — a file dependency, not a use-import.
    ModDecl,
    /// Rust `extern crate x;`
    ExternCrate,
    /// ES module `import ... from "spec"` / `export ... from "spec"`.
    Esm,
    /// CommonJS `require("spec")`.
    Require,
}

/// One name bound by an import.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportEntry {
    /// Name usable *inside the importing file* (after any `as` alias).
    pub local: String,
    /// Name in the source module (`""` for glob/namespace imports, `"default"`
    /// for an ESM default import).
    pub imported: String,
    /// Module this entry is imported *from*: `crate::ledger`, `./foo`,
    /// `react`. Rust `self::`/`super::` segments are kept verbatim here and
    /// rewritten against [`ImportDecl::scope`] during resolution, so the same
    /// fact serves display and lookup. Resolution is per-entry, so a single
    /// statement importing from several nested modules still resolves exactly.
    pub module: String,
    /// True when `local != imported` because of an explicit rename.
    pub alias: bool,
    /// True for `use a::*` / `import * as ns` / `export *`.
    pub glob: bool,
}

/// Why an import could not be tied to a repository file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportResolution {
    /// Resolved to a file in this repository.
    Resolved { file: String },
    /// Points at a registry crate / npm package that is not in this repository.
    External { package: String },
    /// Could not be resolved; recorded so impact analysis can report it.
    Unresolved { reason: String },
}

/// An import or module declaration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportDecl {
    /// Importing file (repository-relative).
    pub file: String,
    pub range: TextRange,
    pub kind: ImportKind,
    /// Raw specifier exactly as written: `crate::agent`, `./foo`, `react`.
    pub specifier: String,
    pub entries: Vec<ImportEntry>,
    /// True for `pub use ...` — the binding is also an export of this module.
    pub public: bool,
    /// Qualified scope the statement appears in (`crate::a::inline_mod`). Used
    /// to resolve `self::`/`super::` in Rust, which are relative to the
    /// enclosing module rather than to the file's path-derived module.
    pub scope: String,
    pub resolution: ImportResolution,
    /// Line text of the statement, trimmed, for display and evidence.
    pub excerpt: String,
}

/// One name a module exports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportEntry {
    /// Name consumers import.
    pub exported: String,
    /// Name in this module (differs for `export { a as b }`).
    pub local: String,
    pub alias: bool,
    pub glob: bool,
}

/// An export declaration (`pub use` in Rust, `export` in TS/JS).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportDecl {
    pub file: String,
    pub range: TextRange,
    pub entries: Vec<ExportEntry>,
    /// `export default` — the local binding is exposed as `default`.
    pub default: bool,
    /// Re-export source specifier, when this export pulls from another module.
    pub from: Option<String>,
    pub excerpt: String,
}

// ---------------------------------------------------------------------------
// Calls
// ---------------------------------------------------------------------------

/// Syntactic form of a callee expression.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallShape {
    /// `foo(...)`
    Plain,
    /// `a::b::foo(...)` / `crate::x::foo(...)` / `Type::foo(...)`
    Scoped,
    /// `obj.foo(...)`
    Member,
}

/// A call site found in the AST.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallSite {
    pub file: String,
    /// Extent of the whole call expression.
    pub range: TextRange,
    /// Extent of just the callee expression.
    pub callee_range: TextRange,
    /// Last segment: `foo` in both `a::b::foo` and `obj.foo`.
    pub callee_name: String,
    /// Full callee text when it is a path/member expression.
    pub callee_path: Option<String>,
    pub shape: CallShape,
    /// Qualified name of the innermost symbol containing this call, when known.
    pub enclosing: Option<String>,
    /// Source line, trimmed, for display.
    pub excerpt: String,
}

// ---------------------------------------------------------------------------
// Occurrences (reference search material)
// ---------------------------------------------------------------------------

/// What syntactic role an identifier occurrence plays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OccContext {
    /// The identifier *is* a declaration's name.
    Definition,
    /// Appears in an import/export binding.
    Import,
    /// Callee position of a call expression.
    Call,
    /// Type position.
    Type,
    /// Left-hand side of an assignment.
    Write,
    /// Anything else (read).
    Read,
}

impl OccContext {
    pub fn as_str(&self) -> &'static str {
        match self {
            OccContext::Definition => "definition",
            OccContext::Import => "import",
            OccContext::Call => "call",
            OccContext::Type => "type",
            OccContext::Write => "write",
            OccContext::Read => "read",
        }
    }
}

/// One identifier occurrence in a file.
///
/// Built from the parse tree, never from a regex sweep over raw text: comments,
/// string literals and attribute bodies are not identifier occurrences.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Occurrence {
    pub name: String,
    pub range: TextRange,
    pub ctx: OccContext,
    /// True when an inner scope in the same file declares this name, which
    /// makes the occurrence shadowed and therefore *not* attributable to an
    /// outer symbol of the same name.
    pub shadowed: bool,
    /// Module-path prefix for scoped references (`crate::agent` before
    /// `AgentLoop`, `Foo` before `Bar::assoc`). `None` for a bare name. A
    /// module path can be resolved deterministically; an [`Occurrence::receiver`]
    /// cannot.
    pub path_prefix: Option<String>,
    /// Scope (innermost qualified name) this occurrence sits in. Index into
    /// [`FileFacts::scopes`] — the ancestor chain of a qualified name is its
    /// list of `::`-prefixes, so a reference is attributable to a symbol when
    /// the symbol's ancestors are a prefix of this chain.
    pub scope_idx: u32,
    /// Receiver of a member access (`obj` in `obj.method()`). A member access
    /// carries no static type information here, so an occurrence with a
    /// receiver is only ever reported as `HEURISTIC`.
    pub receiver: Option<String>,
}

// ---------------------------------------------------------------------------
// References
// ---------------------------------------------------------------------------

/// What kind of use a reference is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReferenceKind {
    Definition,
    Import,
    Export,
    Call,
    Type,
    Read,
    Write,
}

impl ReferenceKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ReferenceKind::Definition => "definition",
            ReferenceKind::Import => "import",
            ReferenceKind::Export => "export",
            ReferenceKind::Call => "call",
            ReferenceKind::Type => "type",
            ReferenceKind::Read => "read",
            ReferenceKind::Write => "write",
        }
    }
}

/// A single reference to a symbol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reference {
    pub file: String,
    pub range: TextRange,
    pub kind: ReferenceKind,
    pub evidence: Evidence,
    /// Trimmed source line at `range.start_line`, for display.
    pub excerpt: String,
    /// How the reference was attributed: an import binding, a module path, or
    /// same-module scope. Surfaced so a reviewer can audit the claim.
    pub via: String,
}

/// Result of a find-references query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReferenceResult {
    /// The symbol the query resolved to, when it resolved at all.
    pub definition: Option<Symbol>,
    pub references: Vec<Reference>,
    /// Occurrences of the same text that could *not* be attributed to the
    /// symbol. Returned separately and never merged into `references`.
    pub possible: Vec<Reference>,
    /// Set when the query target could not be resolved to any symbol. The
    /// reference list stays empty in that case — nothing is fabricated.
    pub unresolved: Option<String>,
    pub deterministic_count: usize,
    pub heuristic_count: usize,
}

// ---------------------------------------------------------------------------
// Call hierarchy
// ---------------------------------------------------------------------------

/// One resolved caller/callee relation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallRelation {
    pub site: CallSite,
    /// Resolved target symbol (callers) or enclosing caller (callees).
    pub resolved: Option<Symbol>,
    pub evidence: Evidence,
    /// Why this grade was assigned; shown next to every heuristic result.
    pub note: String,
}

/// Result of a callers/callees query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallHierarchyResult {
    pub target: Option<Symbol>,
    pub relations: Vec<CallRelation>,
    pub unresolved: Option<String>,
    pub deterministic_count: usize,
    pub heuristic_count: usize,
    pub unsupported_count: usize,
}

// ---------------------------------------------------------------------------
// Graph
// ---------------------------------------------------------------------------

/// Kind of graph node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    File,
    Symbol,
    Package,
}

/// A graph node reference.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GraphNode {
    pub kind: NodeKind,
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
}

/// Kind of graph edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    /// `A.ts --imports--> B.ts`
    Imports,
    /// `foo() --calls--> bar()`
    Calls,
    /// crate `A --depends_on-->` crate `B`
    DependsOn,
    /// Symbol/file contains a symbol.
    Contains,
    /// A file exports a symbol.
    Exports,
    /// A file references a symbol.
    References,
}

impl EdgeKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            EdgeKind::Imports => "imports",
            EdgeKind::Calls => "calls",
            EdgeKind::DependsOn => "depends_on",
            EdgeKind::Contains => "contains",
            EdgeKind::Exports => "exports",
            EdgeKind::References => "references",
        }
    }
}

/// An edge between two nodes, carrying where it came from and how sure we are.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphEdge {
    pub from: GraphNode,
    pub to: GraphNode,
    pub kind: EdgeKind,
    pub provenance: Provenance,
    pub evidence: Evidence,
    /// Human-readable relation, e.g. `A.ts --imports--> B.ts`.
    pub detail: String,
    /// Repository-relative file where the edge can be verified.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<TextRange>,
    /// Verbatim source excerpt backing the edge (the `use`/`import` line, or
    /// the manifest line for package dependencies).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excerpt: Option<String>,
}

/// A graph query.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct GraphQuery {
    /// Restrict to specific edge kinds (default: all).
    pub edge_kinds: Vec<EdgeKind>,
    /// Restrict to specific node kinds (default: all).
    pub node_kinds: Vec<NodeKind>,
    /// Only edges touching this node id (scoped exploration).
    pub focus: Option<String>,
    /// Depth of expansion from `focus`. Default 1 (direct neighbours).
    pub depth: usize,
    /// Hard cap on returned edges. Default 400, max 4000.
    pub limit: usize,
    /// Include cycles detected in the import graph.
    pub include_cycles: bool,
}

impl GraphQuery {
    pub fn normalised(&self) -> GraphQuery {
        GraphQuery {
            edge_kinds: self.edge_kinds.clone(),
            node_kinds: self.node_kinds.clone(),
            focus: self.focus.clone(),
            depth: self.depth.clamp(1, 6),
            limit: if self.limit == 0 {
                400
            } else {
                self.limit.clamp(1, 4000)
            },
            include_cycles: self.include_cycles,
        }
    }
}

/// Result of a graph query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphResult {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
    pub cycles: Vec<Vec<String>>,
    pub truncated: bool,
    pub total_edges_matched: usize,
}

// ---------------------------------------------------------------------------
// Impact
// ---------------------------------------------------------------------------

/// How a dependent relates to the subject.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ImpactClass {
    DirectDependent,
    TransitiveDependent,
    PossibleTextualReference,
    Unresolved,
}

/// What a dependent *is*.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImpactTargetKind {
    File,
    Symbol,
    Package,
}

/// One item in an impact report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImpactItem {
    pub class: ImpactClass,
    pub target_kind: ImpactTargetKind,
    pub id: String,
    pub label: String,
    pub evidence: Evidence,
    pub provenance: Provenance,
    /// Chain of edges establishing this item, root-first.
    pub via: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excerpt: Option<String>,
}

/// What the impact query was about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ImpactSubject {
    Symbol { id: String },
    File { path: String },
    Package { name: String },
}

/// Result of an impact analysis.
///
/// Deliberately reports *relationships*, never consequences: nothing here says
/// a change "will break" anything.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImpactReport {
    pub subject: ImpactSubject,
    pub items: Vec<ImpactItem>,
    pub direct_count: usize,
    pub transitive_count: usize,
    pub possible_count: usize,
    pub unresolved_count: usize,
    /// Plain-language scope statement plus the non-prediction caveat.
    pub summary: String,
    /// Set when the subject itself could not be resolved.
    pub unresolved_subject: Option<String>,
}

// ---------------------------------------------------------------------------
// Files and parse status
// ---------------------------------------------------------------------------

/// How a file fared with the parser.
/// How a file fared with the parser.
///
/// `Default` exists only so [`ParsedFile`] can be built with a struct-update
/// literal; every construction site in `parse` sets `status` explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParseStatus {
    Parsed,
    /// Grammar exists but tree-sitter reported errors; partial facts kept and
    /// labelled.
    Partial,
    /// No grammar for this language.
    #[default]
    Unsupported,
    /// Grammar exists but parsing failed entirely.
    Failed,
}

/// Everything the index knows about one file.
///
/// This is the unit of incremental invalidation: it is cached keyed by content
/// hash, so editing one file re-parses one file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileFacts {
    /// Repository-relative path, `/`-separated.
    pub file: String,
    pub language: Language,
    /// SHA-256 of the raw file bytes, lowercase hex.
    pub content_hash: String,
    pub status: ParseStatus,
    /// Set when parsing failed; the file stays in the inventory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Path-derived module path for this file: `crate::intelligence::nav` for
    /// Rust (`crate` for a crate root), `src::components::shell::SurfaceHost`
    /// for TS/JS (package-relative, extension stripped).
    pub module: String,
    /// Crate/package key used to scope `crate::` and bare-specifier lookups.
    pub crate_key: String,
    /// Extra qualified module names that resolve to this file (Rust inline
    /// `mod x { .. }`). Appended to the module table after [`FileFacts::module`],
    /// so an exact path-derived file wins any collision.
    #[serde(default)]
    pub module_aliases: Vec<String>,
    pub symbols: Vec<Symbol>,
    pub imports: Vec<ImportDecl>,
    pub exports: Vec<ExportDecl>,
    pub call_sites: Vec<CallSite>,
    pub occurrences: Vec<Occurrence>,
    /// Deduplicated innermost-scope names, indexed by
    /// [`Occurrence::scope_idx`].
    pub scopes: Vec<String>,
}
