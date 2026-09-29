//! # zylcode-nav — deterministic repository navigation
//!
//! This crate answers *where is this defined*, *what points at it*, *what does
//! it call*, *how are these files wired together* and *what would a change
//! touch* — using a real grammar ([`tree_sitter`]) and explicit module/import
//! resolution. It is a leaf crate: `zylcode-core` and `zylcode-mcp` both depend
//! on it, which is what lets the MCP tools expose the same engine the payload
//! layer serialises.
//!
//! ## Two labels every result carries
//!
//! * [`Provenance`] — where a fact came from (filesystem, parser, manifest,
//!   derivation, inference).
//! * [`Evidence`] — how strongly a *relationship* was established:
//!   `DETERMINISTIC`, `HEURISTIC` or `UNSUPPORTED`.
//!
//! Heuristic results are returned **alongside** deterministic ones, never in
//! place of them, and never promoted. Text matches are never presented as
//! semantic callers, and impact analysis never asserts that a change "will
//! break" anything.
//!
//! ## Module map
//!
//! | module | responsibility |
//! |---|---|
//! | [`model`] | the vocabulary: symbols, references, edges, impact, evidence |
//! | [`scan`] | file inventory honouring `.gitignore`, content hashing |
//! | [`parse`] | grammar-based extraction of one file |
//! | [`resolve`] | package layout, module paths, import/specifier resolution |
//! | [`index`] | [`RepoIndex`]: building, incremental invalidation, persistence |
//! | [`queries`] | find definition/references/callers/callees, deps, impact, graph |
//! | [`payload`] | JSON payloads for HTTP, MCP tools and Tauri commands |
//! | [`cache`] | one process-wide index slot; *no query re-indexes the repo* |

pub mod cache;
pub mod index;
pub mod model;
pub mod parse;
pub mod payload;
pub mod queries;
pub mod resolve;
pub mod scan;

pub use cache::{query_repository, NavError, REUSE_FOR};
pub use index::RepoIndex;
pub use model::*;
pub use queries::{CallQuery, DefinitionQuery, ImpactQuery, ReferenceRequest, SymbolSelector};
