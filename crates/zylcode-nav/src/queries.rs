//! Deterministic queries over a [`RepoIndex`].
//!
//! Every answer carries [`Evidence`]: `DETERMINISTIC`, `HEURISTIC` or
//! `UNSUPPORTED`. Heuristic answers are returned in their own bucket and are
//! never merged into the deterministic one. Where an occurrence cannot be
//! attributed, nothing is reported for it — the answer is "not found", never
//! a plausible guess.

use crate::index::{BindingTarget, RepoIndex};
use crate::model::*;
use crate::resolve::{self, ModuleTarget};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::path::Path;

// ---------------------------------------------------------------------------
// Selectors
// ---------------------------------------------------------------------------

/// How a query names the symbol it is about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SymbolSelector {
    /// Exact symbol id from a previous result.
    Id { id: String },
    /// Qualified name, optionally scoped to a file or crate to break ties
    /// between two packages that share a module path.
    Qualified {
        qualified_name: String,
        #[serde(default)]
        crate_key: Option<String>,
        #[serde(default)]
        file: Option<String>,
    },
    /// Bare name, optionally restricted to a file.
    Name {
        name: String,
        #[serde(default)]
        file: Option<String>,
    },
    /// Go-to-definition at a 1-based line of a file.
    At { file: String, line: u32 },
}

/// Request/response for `find_definition`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DefinitionQuery {
    pub target: SymbolSelector,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DefinitionResult {
    pub definition: Option<Symbol>,
    /// Every symbol the selector could mean when it is ambiguous. Returned
    /// instead of guessing.
    pub candidates: Vec<Symbol>,
    pub unresolved: Option<String>,
}

/// Request/response for `find_references`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReferenceRequest {
    pub target: SymbolSelector,
    /// Include the declaration itself as a reference.
    #[serde(default = "yes")]
    pub include_definition: bool,
    /// Also return name-only correspondences in `possible`.
    #[serde(default = "yes")]
    pub include_possible: bool,
    /// Hard cap per bucket.
    #[serde(default = "default_limit")]
    pub limit: usize,
}

fn yes() -> bool {
    true
}
fn default_limit() -> usize {
    500
}

impl Default for ReferenceRequest {
    fn default() -> Self {
        ReferenceRequest {
            target: SymbolSelector::Name {
                name: String::new(),
                file: None,
            },
            include_definition: true,
            include_possible: true,
            limit: default_limit(),
        }
    }
}

/// Request/response for `find_callers` / `find_callees`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallQuery {
    pub target: SymbolSelector,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

impl Default for CallQuery {
    fn default() -> Self {
        CallQuery {
            target: SymbolSelector::Name {
                name: String::new(),
                file: None,
            },
            limit: default_limit(),
        }
    }
}

/// Request/response for `symbol_impact`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImpactQuery {
    pub target: SymbolSelector,
    /// Import hops followed for `TRANSITIVE_DEPENDENT`. Default 3, max 6.
    #[serde(default)]
    pub depth: Option<usize>,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

impl Default for ImpactQuery {
    fn default() -> Self {
        ImpactQuery {
            target: SymbolSelector::Name {
                name: String::new(),
                file: None,
            },
            depth: None,
            limit: default_limit(),
        }
    }
}

// ---------------------------------------------------------------------------
// Scope algebra
// ---------------------------------------------------------------------------

/// All `::`-prefixes of a qualified name, **excluding** the name itself.
/// `crate::a::f` -> `["crate", "crate::a"]`.
///
/// Slices the input rather than building a buffer, so the returned strings
/// borrow from `qualified` and stay valid for the caller's lifetime.
fn ancestors_of(qualified: &str) -> Vec<&str> {
    let bytes = qualified.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b':' && bytes[i + 1] == b':' {
            out.push(&qualified[..i]);
            i += 2;
        } else {
            i += 1;
        }
    }
    out
}

/// Every `::`-prefix of a scope chain, **including** the scope itself.
/// `crate::a::f` -> `["crate", "crate::a", "crate::a::f"]`.
fn chain_of(scope: &str) -> Vec<&str> {
    let mut out = ancestors_of(scope);
    if !scope.is_empty() {
        out.push(scope);
    }
    out
}

/// True when `anc` is a prefix of `chain`.
fn is_prefix_of(anc: &[&str], chain: &[&str]) -> bool {
    anc.len() <= chain.len() && anc.iter().zip(chain.iter()).all(|(a, b)| a == b)
}

// ---------------------------------------------------------------------------
// Attribution: occurrence -> symbol
// ---------------------------------------------------------------------------

/// How one occurrence was tied to a symbol (or why it was not).
#[derive(Debug, Clone)]
pub struct Attribution {
    pub symbol: Option<String>,
    pub evidence: Evidence,
    pub via: String,
}

impl Attribution {
    fn det(symbol: Option<String>, via: impl Into<String>) -> Self {
        Attribution {
            symbol,
            evidence: Evidence::Deterministic,
            via: via.into(),
        }
    }
    fn heuristic(symbol: Option<String>, via: impl Into<String>) -> Self {
        Attribution {
            symbol,
            evidence: Evidence::Heuristic,
            via: via.into(),
        }
    }
    fn unsupported(via: impl Into<String>) -> Self {
        Attribution {
            symbol: None,
            evidence: Evidence::Unsupported,
            via: via.into(),
        }
    }
}

/// Attribute a single occurrence to a symbol.
///
/// The rules are tried in strict order of strength. A rule that does not apply
/// falls through; a rule that applies and fails produces an explicit negative
/// ("unresolved", "shadowed") rather than a lower-confidence answer.
pub fn attribute(index: &RepoIndex, file: &str, occ: &Occurrence) -> Attribution {
    let Some(facts) = index.file(file) else {
        return Attribution::unsupported(format!("`{file}` is not in the index"));
    };

    // 1. The occurrence *is* a declaration.
    if occ.ctx == OccContext::Definition {
        if let Some(sym) = facts.symbols.iter().find(|s| s.name_range == occ.range) {
            return Attribution::det(Some(sym.id.clone()), "declaration");
        }
    }

    // 2. An import/export statement: the statement itself says where the name
    //    comes from, so this is stronger than any scope guess.
    if occ.ctx == OccContext::Import {
        if let Some(a) = attribute_via_import(index, facts, occ) {
            return a;
        }
    }

    // 3. A module path prefix resolves through the module table.
    if let Some(prefix) = &occ.path_prefix {
        return match attribute_scoped(index, facts, occ, prefix) {
            Some(a) => a,
            None => Attribution::det(
                None,
                format!("path `{prefix}::{}` did not resolve to a symbol", occ.name),
            ),
        };
    }

    // 4. A member receiver carries no static type here: name match only.
    if let Some(recv) = &occ.receiver {
        let candidate = index
            .symbol_id_by_name(file, &occ.name)
            .or_else(|| scope_candidate(index, facts, occ));
        return Attribution::heuristic(
            candidate,
            format!(
                "member access `{recv}.{}` — name correspondence only, \
                 no receiver type is available to the parser",
                occ.name
            ),
        );
    }

    // 5. An import binding in this file.
    if let Some(binding) = index.bindings.get(file).and_then(|m| m.get(&occ.name)) {
        return match &binding.target {
            BindingTarget::Symbol(id) => Attribution::det(
                Some(id.clone()),
                format!("import binding `{}`", binding.specifier),
            ),
            BindingTarget::File(target) => match index.symbol_id_by_name(target, &occ.name) {
                Some(id) => {
                    Attribution::det(Some(id), format!("glob/namespace import from `{target}`"))
                }
                None => Attribution::det(
                    None,
                    format!(
                        "import resolves to `{target}` but it exports no symbol named `{}`",
                        occ.name
                    ),
                ),
            },
            BindingTarget::External(package) => Attribution::det(
                None,
                format!("`{}` is provided by external package `{package}`", occ.name),
            ),
            BindingTarget::Unresolved(reason) => {
                Attribution::det(None, format!("import unresolved: {reason}"))
            }
        };
    }

    // 6. An inner declaration owns this name; the outer symbol is *not* it.
    if occ.shadowed {
        return Attribution::det(
            None,
            format!("shadowed by an inner declaration of `{}`", occ.name),
        );
    }

    // 7. Module scope of the reference.
    if let Some(id) = scope_candidate(index, facts, occ) {
        return Attribution::det(Some(id), "in scope at the reference site");
    }

    // 7b. A glob/namespace import can bring in a module the scope chain cannot
    //     see. Checked after local scope, because a local declaration always
    //     wins over a glob.
    if let Some(list) = index.globs.get(file) {
        let mut targets: Vec<&String> = list.iter().collect();
        targets.sort();
        for target in targets {
            if let Some(id) = index.symbol_id_by_name(target, &occ.name) {
                return Attribution::det(
                    Some(id),
                    format!("glob/namespace import from `{target}`"),
                );
            }
        }
    }

    // 8. Nothing in scope — report the name-only correspondence honestly.
    if index.symbols.values().any(|s| s.name == occ.name) {
        return Attribution::heuristic(
            index.symbol_id_by_name(file, &occ.name),
            format!(
                "no import binding and no in-scope declaration for `{}`",
                occ.name
            ),
        );
    }
    Attribution::det(None, format!("no symbol named `{}` exists", occ.name))
}

/// `use`/`import` statements resolve through the entry the name belongs to.
fn attribute_via_import(
    index: &RepoIndex,
    facts: &FileFacts,
    occ: &Occurrence,
) -> Option<Attribution> {
    let decl = facts
        .imports
        .iter()
        .find(|d| d.range.contains(occ.range.start_byte))?;
    let entry = decl
        .entries
        .iter()
        .find(|e| e.local == occ.name)
        .or_else(|| decl.entries.iter().find(|e| e.imported == occ.name))?;
    match index.entry_target(facts, decl, entry) {
        BindingTarget::Symbol(id) => Some(Attribution::det(
            Some(id),
            format!("import binding `{}`", decl.specifier),
        )),
        BindingTarget::File(target) => match index.symbol_id_by_name(&target, &occ.name) {
            Some(id) => Some(Attribution::det(
                Some(id),
                format!("import from `{}` -> `{target}`", decl.specifier),
            )),
            None => Some(Attribution::det(
                None,
                format!(
                    "`{}` resolves to `{target}`, which exports no `{}`",
                    decl.specifier, occ.name
                ),
            )),
        },
        BindingTarget::External(package) => Some(Attribution::det(
            None,
            format!("`{}` is provided by external package `{package}`", occ.name),
        )),
        BindingTarget::Unresolved(reason) => Some(Attribution::det(
            None,
            format!("import unresolved: {reason}"),
        )),
    }
}

/// `Prefix::name`: resolve `Prefix` as a module or as a symbol, then find
/// `name` inside it.
fn attribute_scoped(
    index: &RepoIndex,
    facts: &FileFacts,
    occ: &Occurrence,
    prefix: &str,
) -> Option<Attribution> {
    if !matches!(facts.language, Language::Rust) {
        return None;
    }
    let scope = facts
        .scopes
        .get(occ.scope_idx as usize)
        .cloned()
        .unwrap_or_else(|| facts.module.clone());

    // (a) The prefix is a module path: `name` is a member of that module.
    if let ModuleTarget::File(target) =
        index
            .resolver
            .resolve_rust(&facts.crate_key, &scope, prefix)
    {
        if let Some(id) = index.symbol_id_by_name(&target, &occ.name) {
            return Some(Attribution::det(
                Some(id),
                format!("module path `{prefix}` -> `{target}`"),
            ));
        }
        return Some(Attribution::det(
            None,
            format!(
                "module path `{prefix}` -> `{target}` exports no `{}`",
                occ.name
            ),
        ));
    }

    // (b) The prefix is a symbol (a type, a module item); `name` is associated
    //     with it. `container` is recorded at parse time, so this is an exact
    //     structural match rather than a name guess.
    let segs = resolve::split_path(prefix);
    let (rest, last) = segs.split_at(segs.len().saturating_sub(1));
    let last = last.first()?;
    let parent = if rest.is_empty() {
        bare_in_scope(index, facts, occ, last)
    } else {
        let joined = rest.join("::");
        match index
            .resolver
            .resolve_rust(&facts.crate_key, &scope, &joined)
        {
            ModuleTarget::File(f) => index.symbol_id_by_name(&f, last),
            _ => None,
        }
    }?;
    let parent_symbol = index.symbol(&parent)?;

    if let Some(id) = index.find_contained(&parent_symbol.qualified_name, &occ.name) {
        return Some(Attribution::det(
            Some(id),
            format!("associated item `{}` of `{prefix}`", occ.name),
        ));
    }
    if parent_symbol.kind == SymbolKind::Module || parent_symbol.kind == SymbolKind::Trait {
        if let Some(id) = index.symbol_id_by_name(&parent_symbol.file, &occ.name) {
            return Some(Attribution::det(Some(id), format!("member of `{prefix}`")));
        }
    }
    None
}

/// Bare-name resolution against the occurrence's own scope chain.
fn scope_candidate(index: &RepoIndex, facts: &FileFacts, occ: &Occurrence) -> Option<String> {
    if occ.shadowed {
        return None;
    }
    bare_in_scope(index, facts, occ, &occ.name)
}

fn bare_in_scope(
    index: &RepoIndex,
    facts: &FileFacts,
    occ: &Occurrence,
    name: &str,
) -> Option<String> {
    let scope = facts
        .scopes
        .get(occ.scope_idx as usize)
        .map(|s| s.as_str())
        .unwrap_or(facts.module.as_str());
    let chain = chain_of(scope);
    let here = facts.file.as_str();
    // A bare name may only reach items in an *enclosing* module, never a
    // shallower one: `crate::util::helper` is not in scope inside `crate::api`,
    // and neither is a root-level `crate::helper` from a sub-module (Rust 2018
    // needs an explicit path; ESM needs an import). The last chain element is
    // usually a function/impl block rather than a module, so one level of
    // slack is allowed there — and no more.
    let module_depth = chain_of(facts.module.as_str()).len();
    let min_depth = module_depth.max(chain.len().saturating_sub(1));

    let mut best: Option<(usize, &Symbol)> = None;
    for sym in index.symbols.values() {
        if sym.name != name {
            continue;
        }
        let anc = ancestors_of(&sym.qualified_name);
        if anc.len() < min_depth || !is_prefix_of(&anc, &chain) {
            continue;
        }
        let better = match best {
            None => true,
            Some((len, existing)) => {
                if anc.len() != len {
                    // The most specific enclosing scope wins: a declaration
                    // nested deeper inside the same scope chain shadows the
                    // outer one.
                    anc.len() > len
                } else {
                    // Equal depth means two sibling modules both declare this
                    // name. Prefer the occurrence's own file — which is also
                    // what Rust and TypeScript do when a glob or a global
                    // would otherwise be ambiguous — then break on id so the
                    // answer never depends on hash order.
                    let mine = sym.file == here;
                    let theirs = existing.file == here;
                    if mine != theirs {
                        mine
                    } else {
                        sym.id < existing.id
                    }
                }
            }
        };
        if better {
            best = Some((anc.len(), sym));
        }
    }
    best.map(|(_, s)| s.id.clone())
}

// ---------------------------------------------------------------------------
// Symbol selection
// ---------------------------------------------------------------------------

impl RepoIndex {
    /// Resolve a selector to exactly one symbol, or explain why it is not one.
    pub fn resolve(&self, selector: &SymbolSelector) -> DefinitionResult {
        match selector {
            SymbolSelector::Id { id } => match self.symbols.get(id) {
                Some(s) => DefinitionResult {
                    definition: Some(s.clone()),
                    candidates: vec![],
                    unresolved: None,
                },
                None => DefinitionResult {
                    definition: None,
                    candidates: vec![],
                    unresolved: Some(format!("no symbol with id `{id}`")),
                },
            },
            SymbolSelector::Qualified {
                qualified_name,
                crate_key,
                file,
            } => {
                let mut hits: Vec<Symbol> = self
                    .symbols
                    .values()
                    .filter(|s| s.qualified_name == *qualified_name)
                    .filter(|s| file.as_deref().map(|f| s.file == f).unwrap_or(true))
                    .filter(|s| {
                        crate_key
                            .as_deref()
                            .map(|c| s.crate_key == c)
                            .unwrap_or(true)
                    })
                    .cloned()
                    .collect();
                hits.sort_by(|a, b| a.id.cmp(&b.id));
                finish(hits)
            }
            SymbolSelector::Name { name, file } => {
                let mut hits: Vec<Symbol> = match file {
                    Some(f) => self
                        .symbols_by_file
                        .get(f)
                        .map(|ids| {
                            ids.iter()
                                .filter_map(|id| self.symbols.get(id))
                                .filter(|s| s.name == *name)
                                .cloned()
                                .collect()
                        })
                        .unwrap_or_default(),
                    None => self
                        .symbols
                        .values()
                        .filter(|s| s.name == *name)
                        .cloned()
                        .collect(),
                };
                hits.sort_by(|a, b| a.id.cmp(&b.id));
                if hits.is_empty() {
                    // `use a::B as C;` introduces `C` as a local name that is
                    // not a declaration of its own. Go-to-definition on it must
                    // land on the symbol it was imported as, not on "nothing".
                    if let Some(sym) = file.as_deref().and_then(|f| self.alias_target(f, name)) {
                        hits.push(sym);
                    }
                }
                finish(hits)
            }
            SymbolSelector::At { file, line } => self.definition_at(file, *line),
        }
    }

    /// Go-to-definition at a line: the declaration on that line, or whatever
    /// the identifier on that line refers to.
    pub fn definition_at(&self, file: &str, line: u32) -> DefinitionResult {
        let Some(facts) = self.file(file) else {
            return DefinitionResult {
                definition: None,
                candidates: vec![],
                unresolved: Some(format!("`{file}` is not in the index")),
            };
        };
        if let Some(sym) = facts
            .symbols
            .iter()
            .find(|s| s.name_range.start_line == line)
        {
            return DefinitionResult {
                definition: Some(sym.clone()),
                candidates: vec![],
                unresolved: None,
            };
        }
        for occ in &facts.occurrences {
            if occ.range.start_line != line {
                continue;
            }
            let attr = attribute(self, file, occ);
            if let Some(id) = attr.symbol {
                if let Some(sym) = self.symbols.get(&id) {
                    return DefinitionResult {
                        definition: Some(sym.clone()),
                        candidates: vec![],
                        unresolved: None,
                    };
                }
            }
        }
        DefinitionResult {
            definition: None,
            candidates: vec![],
            unresolved: Some(format!("nothing resolves at `{file}:{line}`")),
        }
    }

    /// Symbol whose `container` is `parent` and whose name is `name`.
    pub fn find_contained(&self, parent: &str, name: &str) -> Option<String> {
        let mut hits: Vec<&Symbol> = self
            .symbols
            .values()
            .filter(|s| s.container.as_deref() == Some(parent) && s.name == name)
            .collect();
        hits.sort_by(|a, b| a.id.cmp(&b.id));
        hits.first().map(|s| s.id.clone())
    }

    /// The symbol a *local import name* stands for: `use a::B as C;` in `f.rs`
    /// binds `C`, but `C` is not a declaration of its own. Resolving it has to
    /// follow the exact binding, never a name search across the repository.
    pub fn alias_target(&self, file: &str, local: &str) -> Option<Symbol> {
        let binding = self.bindings.get(file)?.get(local)?;
        match binding.target {
            BindingTarget::Symbol(ref id) => self.symbols.get(id).cloned(),
            _ => None,
        }
    }
}

fn finish(mut hits: Vec<Symbol>) -> DefinitionResult {
    match hits.len() {
        0 => DefinitionResult {
            definition: None,
            candidates: vec![],
            unresolved: Some("no symbol matches that selector".to_string()),
        },
        1 => DefinitionResult {
            definition: hits.pop(),
            candidates: vec![],
            unresolved: None,
        },
        _ => {
            let unresolved = Some(format!(
                "{} symbols match; specify a file or crate key",
                hits.len()
            ));
            let first = hits.first().cloned();
            DefinitionResult {
                definition: first,
                candidates: hits,
                unresolved,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Line excerpts (display only)
// ---------------------------------------------------------------------------

struct LineCache<'a> {
    root: &'a Path,
    files: HashMap<String, Option<Vec<String>>>,
}

impl<'a> LineCache<'a> {
    fn new(root: &'a Path) -> Self {
        LineCache {
            root,
            files: HashMap::new(),
        }
    }

    fn line(&mut self, file: &str, line: u32) -> String {
        let entry = self.files.entry(file.to_string()).or_insert_with(|| {
            std::fs::read_to_string(
                self.root
                    .join(file.replace('/', std::path::MAIN_SEPARATOR_STR)),
            )
            .ok()
            .map(|t| t.lines().map(|l| l.to_string()).collect())
        });
        entry
            .as_ref()
            .and_then(|lines| lines.get(line.saturating_sub(1) as usize))
            .map(|l| l.trim().to_string())
            .unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------
// find_references
// ---------------------------------------------------------------------------

/// Find every reference to a symbol.
///
/// Occurrences that resolve to the symbol land in `references`; occurrences
/// that only share the spelling land in `possible`, labelled `HEURISTIC` and
/// never merged.
pub fn find_references(index: &RepoIndex, request: &ReferenceRequest) -> ReferenceResult {
    let resolved = index.resolve(&request.target);
    let Some(target) = resolved.definition else {
        return ReferenceResult {
            definition: None,
            references: vec![],
            possible: vec![],
            unresolved: resolved
                .unresolved
                .or_else(|| Some("target did not resolve".to_string())),
            deterministic_count: 0,
            heuristic_count: 0,
        };
    };
    if !resolved.candidates.is_empty() {
        return ReferenceResult {
            definition: Some(target),
            references: vec![],
            possible: vec![],
            unresolved: Some(format!(
                "{} symbols match the selector; disambiguate before searching",
                resolved.candidates.len()
            )),
            deterministic_count: 0,
            heuristic_count: 0,
        };
    }

    let limit = request.limit.max(1);
    let mut lines = LineCache::new(&index.root);
    let mut references: Vec<Reference> = Vec::new();
    let mut possible: Vec<Reference> = Vec::new();
    let mut deterministic_count = 0usize;
    let mut heuristic_count = 0usize;

    if request.include_definition && target.name_range.start_line > 0 {
        references.push(Reference {
            file: target.file.clone(),
            range: target.name_range,
            kind: ReferenceKind::Definition,
            evidence: Evidence::Deterministic,
            excerpt: lines.line(&target.file, target.name_range.start_line),
            via: "declaration".to_string(),
        });
        deterministic_count += 1;
    }

    // A `use a::B as C;` rename means the target is also referred to by the
    // local name. Following an *exact* symbol binding to that local name stays
    // deterministic — this is not a text search for `C`.
    let mut aliases: BTreeSet<&str> = BTreeSet::new();
    for map in index.bindings.values() {
        for (local, binding) in map {
            if local != &target.name && binding.target == BindingTarget::Symbol(target.id.clone()) {
                aliases.insert(local.as_str());
            }
        }
    }

    let mut files: Vec<&String> = index.facts.keys().collect();
    files.sort();
    for file in files {
        let facts = &index.facts[file];
        for occ in &facts.occurrences {
            let attr = if occ.name == target.name {
                if occ.ctx == OccContext::Definition && occ.range == target.name_range {
                    continue; // already reported as the definition
                }
                attribute(index, file, occ)
            } else if aliases.contains(occ.name.as_str()) {
                match index.bindings.get(file).and_then(|m| m.get(&occ.name)) {
                    Some(b) if b.target == BindingTarget::Symbol(target.id.clone()) => {
                        Attribution::det(
                            Some(target.id.clone()),
                            format!(
                                "import binding `{}` renames `{}` to `{}`",
                                b.specifier, target.name, occ.name
                            ),
                        )
                    }
                    _ => continue,
                }
            } else {
                continue;
            };
            match (attr.symbol.as_deref(), attr.evidence) {
                (Some(id), Evidence::Deterministic) if id == target.id => {
                    deterministic_count += 1;
                    if references.len() >= limit {
                        continue;
                    }
                    references.push(Reference {
                        file: file.clone(),
                        range: occ.range,
                        kind: reference_kind(occ),
                        evidence: Evidence::Deterministic,
                        excerpt: lines.line(file, occ.range.start_line),
                        via: attr.via,
                    });
                }
                (Some(id), Evidence::Heuristic) if id == target.id => {
                    heuristic_count += 1;
                    if !request.include_possible || possible.len() >= limit {
                        continue;
                    }
                    possible.push(Reference {
                        file: file.clone(),
                        range: occ.range,
                        kind: reference_kind(occ),
                        evidence: Evidence::Heuristic,
                        excerpt: lines.line(file, occ.range.start_line),
                        via: attr.via,
                    });
                }
                // The same text with no attributable symbol: a textual
                // correspondence only. Surfaced in `possible`, never promoted
                // into `references`.
                (None, Evidence::Heuristic) => {
                    heuristic_count += 1;
                    if !request.include_possible || possible.len() >= limit {
                        continue;
                    }
                    possible.push(Reference {
                        file: file.clone(),
                        range: occ.range,
                        kind: reference_kind(occ),
                        evidence: Evidence::Heuristic,
                        excerpt: lines.line(file, occ.range.start_line),
                        via: attr.via,
                    });
                }
                // Deterministic *misses* are not references at all, and a
                // heuristic match on a different symbol is a different symbol.
                _ => {}
            }
        }
    }

    references.sort_by(|a, b| {
        (a.file.as_str(), a.range.start_byte).cmp(&(b.file.as_str(), b.range.start_byte))
    });
    possible.sort_by(|a, b| {
        (a.file.as_str(), a.range.start_byte).cmp(&(b.file.as_str(), b.range.start_byte))
    });

    ReferenceResult {
        definition: Some(target),
        references,
        possible,
        unresolved: None,
        deterministic_count,
        heuristic_count,
    }
}

fn reference_kind(occ: &Occurrence) -> ReferenceKind {
    match occ.ctx {
        OccContext::Definition => ReferenceKind::Definition,
        OccContext::Import => ReferenceKind::Import,
        OccContext::Call => ReferenceKind::Call,
        OccContext::Type => ReferenceKind::Type,
        OccContext::Write => ReferenceKind::Write,
        OccContext::Read => ReferenceKind::Read,
    }
}

// ---------------------------------------------------------------------------
// Callers / callees
// ---------------------------------------------------------------------------

/// The occurrence that names `site`'s callee.
///
/// `callee_range` covers the whole callee expression, so in `a::b::f()` it
/// starts at `a` — but the symbol being called is `f`. Take the *last*
/// identifier with the callee's name that starts inside that range: that is
/// the segment the call resolves through.
fn callee_occurrence<'a>(occurrences: &'a [Occurrence], site: &CallSite) -> Option<&'a Occurrence> {
    occurrences
        .iter()
        .filter(|o| o.name == site.callee_name && site.callee_range.contains(o.range.start_byte))
        .max_by_key(|o| o.range.start_byte)
}

/// Every call site whose resolved callee is `target`.
pub fn find_callers(index: &RepoIndex, request: &CallQuery) -> CallHierarchyResult {
    let resolved = index.resolve(&request.target);
    let Some(target) = resolved.definition else {
        return CallHierarchyResult {
            target: None,
            relations: vec![],
            unresolved: resolved.unresolved,
            deterministic_count: 0,
            heuristic_count: 0,
            unsupported_count: 0,
        };
    };

    let limit = request.limit.max(1);
    let mut relations = Vec::new();
    let mut deterministic_count = 0;
    let mut heuristic_count = 0;
    let mut unsupported_count = 0;

    let mut files: Vec<&String> = index.facts.keys().collect();
    files.sort();
    for file in files {
        let facts = &index.facts[file];
        for site in &facts.call_sites {
            if site.callee_name != target.name {
                continue;
            }
            let attr = match callee_occurrence(&facts.occurrences, site) {
                Some(occ) => attribute(index, file, occ),
                None => Attribution::unsupported(
                    "callee is a compound expression; no identifier to attribute",
                ),
            };
            match attr.evidence {
                Evidence::Deterministic if attr.symbol.as_deref() == Some(target.id.as_str()) => {
                    deterministic_count += 1;
                    if relations.len() < limit {
                        relations.push(CallRelation {
                            site: site.clone(),
                            resolved: Some(target.clone()),
                            evidence: Evidence::Deterministic,
                            note: attr.via,
                        });
                    }
                }
                Evidence::Deterministic => {}
                Evidence::Heuristic => {
                    if attr.symbol.as_deref() == Some(target.id.as_str()) {
                        heuristic_count += 1;
                        if relations.len() < limit {
                            relations.push(CallRelation {
                                site: site.clone(),
                                resolved: Some(target.clone()),
                                evidence: Evidence::Heuristic,
                                note: attr.via,
                            });
                        }
                    }
                }
                Evidence::Unsupported => {
                    unsupported_count += 1;
                    if relations.len() < limit {
                        relations.push(CallRelation {
                            site: site.clone(),
                            resolved: None,
                            evidence: Evidence::Unsupported,
                            note: attr.via,
                        });
                    }
                }
            }
        }
    }

    CallHierarchyResult {
        target: Some(target),
        relations,
        unresolved: None,
        deterministic_count,
        heuristic_count,
        unsupported_count,
    }
}

/// Everything `target` calls, from the call sites inside its declaration.
pub fn find_callees(index: &RepoIndex, request: &CallQuery) -> CallHierarchyResult {
    let resolved = index.resolve(&request.target);
    let Some(target) = resolved.definition else {
        return CallHierarchyResult {
            target: None,
            relations: vec![],
            unresolved: resolved.unresolved,
            deterministic_count: 0,
            heuristic_count: 0,
            unsupported_count: 0,
        };
    };

    let limit = request.limit.max(1);
    let mut relations = Vec::new();
    let mut deterministic_count = 0;
    let mut heuristic_count = 0;
    let mut unsupported_count = 0;

    let facts = match index.file(&target.file) {
        Some(f) => f,
        None => {
            return CallHierarchyResult {
                target: Some(target),
                relations: vec![],
                unresolved: Some("defining file is not in the index".to_string()),
                deterministic_count: 0,
                heuristic_count: 0,
                unsupported_count: 0,
            }
        }
    };
    for site in &facts.call_sites {
        if site.enclosing.as_deref() != Some(target.qualified_name.as_str()) {
            continue;
        }
        let attr = match callee_occurrence(&facts.occurrences, site) {
            Some(occ) => attribute(index, &target.file, occ),
            None => Attribution::unsupported(
                "callee is a compound expression; no identifier to attribute",
            ),
        };
        match attr.evidence {
            Evidence::Deterministic => {
                if attr.symbol.is_some() {
                    deterministic_count += 1;
                }
            }
            Evidence::Heuristic => {
                if attr.symbol.is_some() {
                    heuristic_count += 1;
                }
            }
            Evidence::Unsupported => unsupported_count += 1,
        }
        if relations.len() >= limit {
            continue;
        }
        relations.push(CallRelation {
            site: site.clone(),
            resolved: attr.symbol.and_then(|id| self_symbol(index, &id)),
            evidence: attr.evidence,
            note: attr.via,
        });
    }

    CallHierarchyResult {
        target: Some(target),
        relations,
        unresolved: None,
        deterministic_count,
        heuristic_count,
        unsupported_count,
    }
}

fn self_symbol(index: &RepoIndex, id: &str) -> Option<Symbol> {
    index.symbols.get(id).cloned()
}

// ---------------------------------------------------------------------------
// File dependencies
// ---------------------------------------------------------------------------

/// One import/export/module edge leaving (or entering) a file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileDependency {
    /// For `file_dependencies`: the other file. For `file_dependents`: the
    /// importing file.
    pub file: String,
    pub kind: ImportKind,
    pub specifier: String,
    pub resolution: ImportResolution,
    pub range: TextRange,
    pub excerpt: String,
    pub evidence: Evidence,
    /// Human-readable relation, e.g. `src/a.ts --imports--> src/b.ts`.
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileDependenciesResult {
    pub file: String,
    pub dependencies: Vec<FileDependency>,
    pub resolved_count: usize,
    pub external_count: usize,
    pub unresolved_count: usize,
}

/// Everything `file` imports, with how each specifier landed.
pub fn file_dependencies(index: &RepoIndex, file: &str) -> FileDependenciesResult {
    let empty = FileDependenciesResult {
        file: file.to_string(),
        dependencies: vec![],
        resolved_count: 0,
        external_count: 0,
        unresolved_count: 0,
    };
    let Some(facts) = index.file(file) else {
        return empty;
    };
    let mut out = Vec::new();
    let (mut resolved, mut external, mut unresolved) = (0, 0, 0);
    for decl in &facts.imports {
        let (detail, bucket) = match &decl.resolution {
            ImportResolution::Resolved { file: target } => {
                resolved += 1;
                (
                    format!("{file} --imports--> {target}"),
                    Evidence::Deterministic,
                )
            }
            ImportResolution::External { package } => {
                external += 1;
                (
                    format!("{file} --imports--> {package} (external package)"),
                    Evidence::Deterministic,
                )
            }
            ImportResolution::Unresolved { reason } => {
                unresolved += 1;
                (
                    format!(
                        "{file} --imports--> {} (unresolved: {reason})",
                        decl.specifier
                    ),
                    Evidence::Deterministic,
                )
            }
        };
        out.push(FileDependency {
            file: file.to_string(),
            kind: decl.kind,
            specifier: decl.specifier.clone(),
            resolution: decl.resolution.clone(),
            range: decl.range,
            excerpt: decl.excerpt.clone(),
            evidence: bucket,
            detail,
        });
    }
    FileDependenciesResult {
        file: file.to_string(),
        dependencies: out,
        resolved_count: resolved,
        external_count: external,
        unresolved_count: unresolved,
    }
}

/// Every file that imports `file`.
pub fn file_dependents(index: &RepoIndex, file: &str) -> FileDependenciesResult {
    let mut out = Vec::new();
    let (mut resolved, mut external, mut unresolved) = (0, 0, 0);
    let mut files: Vec<&String> = index.facts.keys().collect();
    files.sort();
    for source in files {
        let facts = &index.facts[source];
        for decl in &facts.imports {
            let ImportResolution::Resolved { file: target } = &decl.resolution else {
                continue;
            };
            if target != file {
                continue;
            }
            resolved += 1;
            out.push(FileDependency {
                file: source.clone(),
                kind: decl.kind,
                specifier: decl.specifier.clone(),
                resolution: decl.resolution.clone(),
                range: decl.range,
                excerpt: decl.excerpt.clone(),
                evidence: Evidence::Deterministic,
                detail: format!("{source} --imports--> {file}"),
            });
        }
    }
    let _ = (&mut external, &mut unresolved);
    FileDependenciesResult {
        file: file.to_string(),
        dependencies: out,
        resolved_count: resolved,
        external_count: external,
        unresolved_count: unresolved,
    }
}

// ---------------------------------------------------------------------------
// Impact
// ---------------------------------------------------------------------------

/// Who depends on `subject`, classified by how the relationship was found.
///
/// Reports relationships only. Nothing in the result asserts that a change
/// would break anything.
pub fn symbol_impact(index: &RepoIndex, query: &ImpactQuery) -> (ImpactReport, Option<Symbol>) {
    let depth = query.depth.unwrap_or(3).clamp(1, 6);
    let limit = query.limit.max(1);

    let resolved = index.resolve(&query.target);
    let Some(target) = resolved.definition else {
        let report = ImpactReport {
            subject: ImpactSubject::Symbol {
                id: format!("{:?}", query.target),
            },
            items: vec![],
            direct_count: 0,
            transitive_count: 0,
            possible_count: 0,
            unresolved_count: 0,
            summary: "The subject did not resolve, so no relationships are claimed.".to_string(),
            unresolved_subject: resolved
                .unresolved
                .or_else(|| Some("target did not resolve".to_string())),
        };
        return (report, None);
    };

    let mut items: Vec<ImpactItem> = Vec::new();
    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();

    // -- direct: deterministic references from other files, and callers -----
    let refs = find_references(
        index,
        &ReferenceRequest {
            target: SymbolSelector::Id {
                id: target.id.clone(),
            },
            include_definition: false,
            include_possible: false,
            limit: limit.max(200),
        },
    );
    for r in &refs.references {
        if r.file == target.file {
            continue;
        }
        let label = format!("{}:{}", r.file, r.range.start_line);
        if !seen.insert(("symbol".into(), label.clone())) {
            continue;
        }
        items.push(ImpactItem {
            class: ImpactClass::DirectDependent,
            target_kind: ImpactTargetKind::Symbol,
            id: target.id.clone(),
            label: format!(
                "{} — referenced at {}:{}",
                r.file, r.range.start_line, r.range.start_col
            ),
            evidence: r.evidence,
            provenance: Provenance::Derived,
            via: vec![
                format!("{} --references--> {}", r.file, target.qualified_name),
                r.via.clone(),
            ],
            excerpt: if r.excerpt.is_empty() {
                None
            } else {
                Some(r.excerpt.clone())
            },
        });
    }

    let callers = find_callers(
        index,
        &CallQuery {
            target: SymbolSelector::Id {
                id: target.id.clone(),
            },
            limit: limit.max(200),
        },
    );
    for rel in &callers.relations {
        if rel.evidence != Evidence::Deterministic {
            continue;
        }
        if rel.site.file == target.file {
            continue;
        }
        let label = format!("call:{}", rel.site.range.start_byte);
        if !seen.insert(("call".into(), label.clone())) {
            continue;
        }
        items.push(ImpactItem {
            class: ImpactClass::DirectDependent,
            target_kind: ImpactTargetKind::Symbol,
            id: target.id.clone(),
            label: format!(
                "{} — called from {}",
                rel.site.file,
                rel.site.enclosing.clone().unwrap_or_default()
            ),
            evidence: rel.evidence,
            provenance: Provenance::Derived,
            via: vec![format!(
                "{} --calls--> {}",
                rel.site.file, target.qualified_name
            )],
            excerpt: if rel.site.excerpt.is_empty() {
                None
            } else {
                Some(rel.site.excerpt.clone())
            },
        });
    }

    let direct_files: BTreeSet<String> = index
        .referencing_files(&target)
        .into_iter()
        .filter(|f| *f != target.file)
        .collect();

    // -- transitive: import-graph reverse closure ---------------------------
    let mut frontier: VecDeque<(String, usize)> = VecDeque::new();
    let mut visited: BTreeSet<String> = BTreeSet::new();
    for f in &direct_files {
        frontier.push_back((f.clone(), 1));
        visited.insert(f.clone());
    }
    visited.insert(target.file.clone());
    let mut transitive: Vec<(String, String, usize)> = Vec::new();
    while let Some((file, d)) = frontier.pop_front() {
        if d > depth {
            continue;
        }
        for importer in index.importers_of(&file) {
            if !visited.insert(importer.clone()) {
                continue;
            }
            transitive.push((importer.clone(), file.clone(), d + 1));
            frontier.push_back((importer.clone(), d + 1));
        }
    }
    transitive.sort();
    for (importer, via, d) in transitive.into_iter().take(limit) {
        items.push(ImpactItem {
            class: ImpactClass::TransitiveDependent,
            target_kind: ImpactTargetKind::File,
            id: importer.clone(),
            label: format!("{importer} — imports {via} (hop {d})"),
            evidence: Evidence::Deterministic,
            provenance: Provenance::Derived,
            via: vec![
                format!("{importer} --imports--> {via}"),
                format!("via {via}"),
            ],
            excerpt: None,
        });
    }

    // -- possible textual references ----------------------------------------
    let mut possible_files: BTreeSet<String> = BTreeSet::new();
    let mut files: Vec<&String> = index.facts.keys().collect();
    files.sort();
    for file in files {
        let facts = &index.facts[file];
        if file == &target.file {
            continue;
        }
        for occ in &facts.occurrences {
            if occ.name != target.name {
                continue;
            }
            if matches!(
                attribute(index, file, occ).evidence,
                Evidence::Heuristic | Evidence::Unsupported
            ) {
                possible_files.insert(file.clone());
                break;
            }
        }
    }
    for file in possible_files.into_iter().take(limit) {
        items.push(ImpactItem {
            class: ImpactClass::PossibleTextualReference,
            target_kind: ImpactTargetKind::File,
            id: file.clone(),
            label: format!(
                "{file} — mentions `{}` without a resolved binding",
                target.name
            ),
            evidence: Evidence::Heuristic,
            provenance: Provenance::Inferred,
            via: vec![format!("{file} --mentions--> {}", target.name)],
            excerpt: None,
        });
    }

    // -- unresolved imports touching the subject's file ---------------------
    let mut unresolved_items: Vec<ImpactItem> = Vec::new();
    let mut affected: BTreeSet<String> = direct_files.clone();
    affected.insert(target.file.clone());
    for file in &affected {
        let Some(facts) = index.file(file) else {
            continue;
        };
        for decl in &facts.imports {
            let ImportResolution::Unresolved { reason } = &decl.resolution else {
                continue;
            };
            unresolved_items.push(ImpactItem {
                class: ImpactClass::Unresolved,
                target_kind: ImpactTargetKind::File,
                id: file.clone(),
                label: format!("{file} — `{}` did not resolve", decl.specifier),
                evidence: Evidence::Deterministic,
                provenance: Provenance::Derived,
                via: vec![
                    format!("{file} --imports--> {}", decl.specifier),
                    reason.clone(),
                ],
                excerpt: if decl.excerpt.is_empty() {
                    None
                } else {
                    Some(decl.excerpt.clone())
                },
            });
        }
    }
    unresolved_items.sort_by(|a, b| a.label.cmp(&b.label));
    items.extend(unresolved_items.into_iter().take(limit));

    let direct_count = items
        .iter()
        .filter(|i| i.class == ImpactClass::DirectDependent)
        .count();
    let transitive_count = items
        .iter()
        .filter(|i| i.class == ImpactClass::TransitiveDependent)
        .count();
    let possible_count = items
        .iter()
        .filter(|i| i.class == ImpactClass::PossibleTextualReference)
        .count();
    let unresolved_count = items
        .iter()
        .filter(|i| i.class == ImpactClass::Unresolved)
        .count();

    let report = ImpactReport {
        subject: ImpactSubject::Symbol {
            id: target.id.clone(),
        },
        items,
        direct_count,
        transitive_count,
        possible_count,
        unresolved_count,
        summary: format!(
            "{direct_count} direct dependents, {transitive_count} files within \
             {depth} import hops, {possible_count} possible textual references \
             and {unresolved_count} unresolved imports relate to `{}`. These are \
             relationships observed in the repository, not predictions: nothing \
             here states that a change would break any of them.",
            target.qualified_name
        ),
        unresolved_subject: None,
    };
    (report, Some(target))
}

/// Impact for a whole file: who imports it, transitively, plus unresolved
/// specifiers on both sides.
pub fn file_impact(index: &RepoIndex, file: &str, depth: usize, limit: usize) -> ImpactReport {
    let depth = depth.clamp(1, 6);
    let limit = limit.max(1);
    let subject = ImpactSubject::File {
        path: file.to_string(),
    };
    if index.file(file).is_none() {
        return ImpactReport {
            subject,
            items: vec![],
            direct_count: 0,
            transitive_count: 0,
            possible_count: 0,
            unresolved_count: 0,
            summary: format!("`{file}` is not in the index; no relationships are claimed."),
            unresolved_subject: Some(format!("`{file}` is not in the index")),
        };
    }

    let mut items = Vec::new();
    let direct: Vec<String> = index.importers_of(file);
    for importer in direct.iter().take(limit) {
        items.push(ImpactItem {
            class: ImpactClass::DirectDependent,
            target_kind: ImpactTargetKind::File,
            id: importer.clone(),
            label: format!("{importer} — imports {file}"),
            evidence: Evidence::Deterministic,
            provenance: Provenance::Derived,
            via: vec![format!("{importer} --imports--> {file}")],
            excerpt: None,
        });
    }

    let mut visited: BTreeSet<String> = direct.iter().cloned().collect();
    visited.insert(file.to_string());
    let mut frontier: VecDeque<(String, usize)> = direct.into_iter().map(|f| (f, 1)).collect();
    let mut hops: Vec<(String, String, usize)> = Vec::new();
    while let Some((cur, d)) = frontier.pop_front() {
        if d > depth {
            continue;
        }
        for next in index.importers_of(&cur) {
            if !visited.insert(next.clone()) {
                continue;
            }
            hops.push((next.clone(), cur.clone(), d + 1));
            frontier.push_back((next.clone(), d + 1));
        }
    }
    hops.sort();
    for (importer, via, d) in hops.into_iter().take(limit) {
        items.push(ImpactItem {
            class: ImpactClass::TransitiveDependent,
            target_kind: ImpactTargetKind::File,
            id: importer.clone(),
            label: format!("{importer} — imports {via} (hop {d})"),
            evidence: Evidence::Deterministic,
            provenance: Provenance::Derived,
            via: vec![format!("{importer} --imports--> {via}")],
            excerpt: None,
        });
    }

    let deps = file_dependencies(index, file);
    let mut unresolved_items = Vec::new();
    for d in deps
        .dependencies
        .iter()
        .filter(|d| matches!(d.resolution, ImportResolution::Unresolved { .. }))
    {
        unresolved_items.push(ImpactItem {
            class: ImpactClass::Unresolved,
            target_kind: ImpactTargetKind::File,
            id: file.to_string(),
            label: format!("{file} — `{}` did not resolve", d.specifier),
            evidence: Evidence::Deterministic,
            provenance: Provenance::Derived,
            via: vec![d.detail.clone()],
            excerpt: if d.excerpt.is_empty() {
                None
            } else {
                Some(d.excerpt.clone())
            },
        });
    }
    unresolved_items.sort_by(|a, b| a.label.cmp(&b.label));
    items.extend(unresolved_items.into_iter().take(limit));

    let direct_count = items
        .iter()
        .filter(|i| i.class == ImpactClass::DirectDependent)
        .count();
    let transitive_count = items
        .iter()
        .filter(|i| i.class == ImpactClass::TransitiveDependent)
        .count();
    let unresolved_count = items
        .iter()
        .filter(|i| i.class == ImpactClass::Unresolved)
        .count();

    ImpactReport {
        subject,
        items,
        direct_count,
        transitive_count,
        possible_count: 0,
        unresolved_count,
        summary: format!(
            "{direct_count} files import `{file}` directly and {transitive_count} \
             more do so within {depth} hops; {unresolved_count} of its own imports \
             are unresolved. These are relationships observed in the repository, \
             not predictions: nothing here states that a change would break any \
             of them."
        ),
        unresolved_subject: None,
    }
}

impl RepoIndex {
    /// Files with a deterministic reference to `target`, never `target`'s own
    /// file: a definition declaring itself is not evidence that anybody else
    /// depends on it.
    pub fn referencing_files(&self, target: &Symbol) -> Vec<String> {
        let mut out: BTreeSet<String> = BTreeSet::new();
        let mut files: Vec<&String> = self.facts.keys().collect();
        files.sort();
        for file in files {
            if *file == target.file {
                continue;
            }
            let facts = &self.facts[file];
            for occ in &facts.occurrences {
                if occ.name != target.name {
                    continue;
                }
                if attribute(self, file, occ).symbol.as_deref() == Some(target.id.as_str()) {
                    out.insert(file.clone());
                    break;
                }
            }
        }
        out.into_iter().collect()
    }

    /// Files whose import statements name `file`, never `file` itself.
    ///
    /// The self case is a real edge but a useless one for impact: a
    /// `use super::*;` inside a file's own `#[cfg(test)] mod tests` resolves
    /// back to that file, and without this filter the subject showed up as one
    /// of its own direct dependents — a true line that reads as a false claim.
    pub fn importers_of(&self, file: &str) -> Vec<String> {
        let mut out: BTreeSet<String> = BTreeSet::new();
        let mut files: Vec<&String> = self.facts.keys().collect();
        files.sort();
        for source in files {
            if source == file {
                continue;
            }
            for decl in &self.facts[source].imports {
                if let ImportResolution::Resolved { file: target } = &decl.resolution {
                    if target == file {
                        out.insert(source.clone());
                        break;
                    }
                }
            }
        }
        out.into_iter().collect()
    }
}

// ---------------------------------------------------------------------------
// Repository graph
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct RawEdge {
    from: GraphNode,
    to: GraphNode,
    kind: EdgeKind,
    provenance: Provenance,
    evidence: Evidence,
    detail: String,
    source_file: Option<String>,
    range: Option<TextRange>,
    excerpt: Option<String>,
}

/// Query the repository graph with provenance on every edge.
pub fn repository_graph_query(index: &RepoIndex, query: &GraphQuery) -> GraphResult {
    let q = query.normalised();
    let wants = |k: EdgeKind| q.edge_kinds.is_empty() || q.edge_kinds.contains(&k);
    let wants_nodes = |k: NodeKind| q.node_kinds.is_empty() || q.node_kinds.contains(&k);

    let mut edges: Vec<RawEdge> = Vec::new();
    let mut seen: BTreeSet<(String, String, &'static str)> = BTreeSet::new();

    let mut files: Vec<&String> = index.facts.keys().collect();
    files.sort();

    // -- imports -----------------------------------------------------------
    if wants(EdgeKind::Imports) && wants_nodes(NodeKind::File) {
        for file in &files {
            let facts = &index.facts[*file];
            for decl in &facts.imports {
                let (to, label) = match &decl.resolution {
                    ImportResolution::Resolved { file: target } => (target.clone(), target.clone()),
                    ImportResolution::External { package } => (
                        format!("package::{package}"),
                        format!("{package} (external)"),
                    ),
                    ImportResolution::Unresolved { reason } => (
                        format!("unresolved::{}", decl.specifier),
                        format!("{} (unresolved: {reason})", decl.specifier),
                    ),
                };
                let to_node = if to.starts_with("package::") || to.starts_with("unresolved::") {
                    GraphNode {
                        kind: NodeKind::Package,
                        id: to.clone(),
                        label,
                        language: None,
                    }
                } else {
                    GraphNode {
                        kind: NodeKind::File,
                        id: to.clone(),
                        label,
                        language: index.file(&to).map(|f| f.language.as_str().to_string()),
                    }
                };
                let detail = format!("{file} --imports--> {}", to_node.label);
                let key = (file.to_string(), to.clone(), "imports");
                if !seen.insert(key) {
                    continue;
                }
                edges.push(RawEdge {
                    from: GraphNode {
                        kind: NodeKind::File,
                        id: (*file).clone(),
                        label: (*file).clone(),
                        language: facts.language.as_str().to_string().into(),
                    },
                    to: to_node,
                    kind: EdgeKind::Imports,
                    provenance: Provenance::Parsed,
                    evidence: Evidence::Deterministic,
                    detail,
                    source_file: Some((*file).clone()),
                    range: Some(decl.range),
                    excerpt: if decl.excerpt.is_empty() {
                        None
                    } else {
                        Some(decl.excerpt.clone())
                    },
                });
            }
        }
    }

    // -- package dependencies ---------------------------------------------
    if wants(EdgeKind::DependsOn) && wants_nodes(NodeKind::Package) {
        for pkg in &index.resolver.layout.packages {
            for dep in &pkg.deps {
                let Some(other) = index.resolver.layout.cargo_named(dep) else {
                    continue;
                };
                if other.dir == pkg.dir {
                    continue;
                }
                let key = (pkg.name.clone(), other.name.clone(), "depends_on");
                if !seen.insert(key) {
                    continue;
                }
                let source = if pkg.manifest.is_empty() {
                    None
                } else {
                    Some(pkg.manifest.clone())
                };
                edges.push(RawEdge {
                    from: GraphNode {
                        kind: NodeKind::Package,
                        id: format!("package::{}", pkg.name),
                        label: pkg.name.clone(),
                        language: None,
                    },
                    to: GraphNode {
                        kind: NodeKind::Package,
                        id: format!("package::{}", other.name),
                        label: other.name.clone(),
                        language: None,
                    },
                    kind: EdgeKind::DependsOn,
                    provenance: Provenance::Parsed,
                    evidence: Evidence::Deterministic,
                    detail: format!(
                        "package {} --depends_on--> package {}",
                        pkg.name, other.name
                    ),
                    source_file: source,
                    range: None,
                    excerpt: None,
                });
            }
        }
    }

    // -- symbol-level edges (only when explicitly asked for) ---------------
    let wants_symbols = wants_nodes(NodeKind::Symbol)
        && (wants(EdgeKind::Contains)
            || wants(EdgeKind::Exports)
            || wants(EdgeKind::References)
            || wants(EdgeKind::Calls));
    if wants_symbols {
        for file in &files {
            let facts = &index.facts[*file];
            if wants(EdgeKind::Contains) {
                for sym in &facts.symbols {
                    edges.push(RawEdge {
                        from: GraphNode {
                            kind: NodeKind::File,
                            id: (*file).clone(),
                            label: (*file).clone(),
                            language: facts.language.as_str().to_string().into(),
                        },
                        to: GraphNode {
                            kind: NodeKind::Symbol,
                            id: sym.id.clone(),
                            label: sym.qualified_name.clone(),
                            language: facts.language.as_str().to_string().into(),
                        },
                        kind: EdgeKind::Contains,
                        provenance: Provenance::Parsed,
                        evidence: Evidence::Deterministic,
                        detail: format!("{file} --contains--> {}", sym.qualified_name),
                        source_file: Some((*file).clone()),
                        range: Some(sym.range),
                        excerpt: sym.signature.clone(),
                    });
                }
            }
            if wants(EdgeKind::Exports) {
                if let Some(map) = index.exports.get(*file) {
                    let mut names: Vec<&String> = map.keys().collect();
                    names.sort();
                    for name in names {
                        let Some(sym) = index.symbols.get(&map[name]) else {
                            continue;
                        };
                        edges.push(RawEdge {
                            from: GraphNode {
                                kind: NodeKind::File,
                                id: (*file).clone(),
                                label: (*file).clone(),
                                language: facts.language.as_str().to_string().into(),
                            },
                            to: GraphNode {
                                kind: NodeKind::Symbol,
                                id: sym.id.clone(),
                                label: sym.qualified_name.clone(),
                                language: facts.language.as_str().to_string().into(),
                            },
                            kind: EdgeKind::Exports,
                            provenance: Provenance::Parsed,
                            evidence: Evidence::Deterministic,
                            detail: format!("{file} --exports--> {name}"),
                            source_file: Some((*file).clone()),
                            range: Some(sym.name_range),
                            excerpt: None,
                        });
                    }
                }
            }
        }
    }

    // -- cycles ------------------------------------------------------------
    let cycles = if q.include_cycles {
        import_cycles(index)
    } else {
        Vec::new()
    };

    // -- scope, expand, cap ------------------------------------------------
    let adjacency = build_adjacency(&edges);
    let selected = select_edges(&edges, &adjacency, &q.focus, q.depth);

    let mut nodes: BTreeMap<String, GraphNode> = BTreeMap::new();
    let mut dropped = 0usize;
    for (i, e) in selected.iter().enumerate() {
        if i >= q.limit {
            dropped += 1;
            continue;
        }
        nodes
            .entry(e.from.id.clone())
            .or_insert_with(|| e.from.clone());
        nodes.entry(e.to.id.clone()).or_insert_with(|| e.to.clone());
    }
    let total = selected.len();
    let edges_out: Vec<GraphEdge> = selected
        .into_iter()
        .take(q.limit)
        .map(|e| GraphEdge {
            from: e.from,
            to: e.to,
            kind: e.kind,
            provenance: e.provenance,
            evidence: e.evidence,
            detail: e.detail,
            source_file: e.source_file,
            range: e.range,
            excerpt: e.excerpt,
        })
        .collect();

    GraphResult {
        nodes: nodes.into_values().collect(),
        edges: edges_out,
        cycles,
        truncated: dropped > 0,
        total_edges_matched: total,
    }
}

fn build_adjacency(edges: &[RawEdge]) -> HashMap<String, Vec<usize>> {
    let mut map: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, e) in edges.iter().enumerate() {
        map.entry(e.from.id.clone()).or_default().push(i);
        map.entry(e.to.id.clone()).or_default().push(i);
    }
    map
}

/// Breadth-first expansion from `focus` (or everything when unscoped).
///
/// `depth` counts *hops*: depth 1 keeps only the edges touching `focus`,
/// depth 2 adds the edges touching those neighbours, and so on.
fn select_edges(
    edges: &[RawEdge],
    adjacency: &HashMap<String, Vec<usize>>,
    focus: &Option<String>,
    depth: usize,
) -> Vec<RawEdge> {
    let mut keep: BTreeSet<usize> = BTreeSet::new();
    match focus {
        Some(f) => {
            let mut seen: BTreeSet<String> = BTreeSet::new();
            let mut frontier: VecDeque<(String, usize)> = VecDeque::new();
            frontier.push_back((f.clone(), 0));
            seen.insert(f.clone());
            while let Some((node, d)) = frontier.pop_front() {
                if d >= depth {
                    continue;
                }
                if let Some(idxs) = adjacency.get(&node) {
                    for &i in idxs {
                        keep.insert(i);
                        let e = &edges[i];
                        for next in [&e.from.id, &e.to.id] {
                            if seen.insert(next.clone()) {
                                frontier.push_back((next.clone(), d + 1));
                            }
                        }
                    }
                }
            }
        }
        None => {
            for i in 0..edges.len() {
                keep.insert(i);
            }
        }
    }
    keep.into_iter()
        .map(|i| edges[i].clone())
        .collect::<Vec<_>>()
}

/// Strongly connected components of the file import graph with more than one
/// member (a real cycle). Iterative Kosaraju: no recursion, so a deep graph
/// cannot blow the stack, and the result is sorted for determinism.
fn import_cycles(index: &RepoIndex) -> Vec<Vec<String>> {
    let mut files: Vec<String> = index.facts.keys().cloned().collect();
    files.sort();

    let mut out_edges: HashMap<String, Vec<String>> = HashMap::new();
    let mut in_edges: HashMap<String, Vec<String>> = HashMap::new();
    for file in &files {
        let mut targets: Vec<String> = Vec::new();
        for decl in &index.facts[file].imports {
            if let ImportResolution::Resolved { file: target } = &decl.resolution {
                // Only self-edges and edges between indexed files can cycle.
                if target != file && index.facts.contains_key(target) {
                    targets.push(target.clone());
                }
            }
        }
        targets.sort();
        targets.dedup();
        for target in &targets {
            in_edges
                .entry(target.clone())
                .or_default()
                .push(file.clone());
        }
        out_edges.insert(file.clone(), targets);
    }

    // Pass 1 — post-order over the forward graph.
    let mut visited: HashSet<String> = HashSet::new();
    let mut order: Vec<String> = Vec::new();
    for root in &files {
        if !visited.insert(root.clone()) {
            continue;
        }
        let mut stack: Vec<(String, usize)> = vec![(root.clone(), 0)];
        while let Some((node, i)) = stack.pop() {
            let children = out_edges.get(&node).cloned().unwrap_or_default();
            if i < children.len() {
                stack.push((node, i + 1));
                let child = &children[i];
                if visited.insert(child.clone()) {
                    stack.push((child.clone(), 0));
                }
            } else {
                order.push(node);
            }
        }
    }

    // Pass 2 — collect components in decreasing finish time.
    let mut seen: HashSet<String> = HashSet::new();
    let mut components: Vec<Vec<String>> = Vec::new();
    for root in order.iter().rev() {
        if seen.contains(root) {
            continue;
        }
        let mut comp: Vec<String> = Vec::new();
        let mut stack: Vec<String> = vec![root.clone()];
        seen.insert(root.clone());
        while let Some(node) = stack.pop() {
            comp.push(node.clone());
            for parent in in_edges.get(&node).cloned().unwrap_or_default() {
                if seen.insert(parent.clone()) {
                    stack.push(parent);
                }
            }
        }
        comp.sort();
        if comp.len() > 1 {
            components.push(comp);
        }
    }
    components.sort();
    components.dedup();
    components
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ancestors_and_chains_are_consistent() {
        assert_eq!(ancestors_of("crate::a::f"), vec!["crate", "crate::a"]);
        assert_eq!(
            chain_of("crate::a::f"),
            vec!["crate", "crate::a", "crate::a::f"]
        );
        assert!(is_prefix_of(
            &["crate", "crate::a"],
            &chain_of("crate::a::f")
        ));
        assert!(!is_prefix_of(
            &["crate", "crate::b"],
            &chain_of("crate::a::f")
        ));
        assert!(ancestors_of("crate").is_empty());
        assert!(chain_of("crate").len() == 1);
    }

    #[test]
    fn scope_chain_picks_the_most_specific_symbol() {
        // Simulated: a module-level `x` and a function-local `x`.
        let chain = chain_of("crate::a::main");
        let outer = ancestors_of("crate::a::x");
        let inner = ancestors_of("crate::a::main::x");
        assert!(is_prefix_of(&outer, &chain));
        assert!(is_prefix_of(&inner, &chain));
        assert!(
            inner.len() > outer.len(),
            "the local declaration is more specific"
        );
    }
}
