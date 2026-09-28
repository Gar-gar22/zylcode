//! Evidence Graph — Master Transformation Prompt §5.
//!
//! A first-class, restart-surviving graph of typed nodes tracing one chain:
//!
//! ```text
//! INTENT → SPECIFICATION → PLAN → DECISION → ACTION → TOOL_CALL →
//! RESULT → ARTIFACT → VERIFICATION → CLAIM
//! ```
//!
//! Every node carries the provenance §5 demands (who initiated, which model,
//! which tool, inputs, outputs, files changed, environment, commit, what was
//! verified, what conclusion was derived, what remains unknown) plus explicit
//! references into the *existing* evidence stores — execution ledger entries,
//! proof records, tool-evidence JSONL lines. The graph never duplicates that
//! data; it links to it.
//!
//! Persistence is a single hash-chained JSON file (`.zylcode/evidence_graph.json`)
//! with atomic writes, mirroring `ProofEngine`/`ClaimStore` conventions: an
//! interrupted write can truncate at worst a `.tmp` file, never the graph.

use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use uuid::Uuid;

use crate::claim::EvidenceRef;

/// Node types of the §5 chain, in canonical order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    Intent,
    Specification,
    Plan,
    Decision,
    Action,
    ToolCall,
    Result,
    Artifact,
    Verification,
    Claim,
}

impl NodeKind {
    /// Position in the canonical INTENT→CLAIM chain (for ordering/diagnostics).
    pub fn stage(self) -> u8 {
        match self {
            NodeKind::Intent => 0,
            NodeKind::Specification => 1,
            NodeKind::Plan => 2,
            NodeKind::Decision => 3,
            NodeKind::Action => 4,
            NodeKind::ToolCall => 5,
            NodeKind::Result => 6,
            NodeKind::Artifact => 7,
            NodeKind::Verification => 8,
            NodeKind::Claim => 9,
        }
    }
}

/// Relationship between two nodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    /// `to` rests on `to`'s predecessor rationale (spec→plan, plan→action…).
    DerivesFrom,
    /// `to` was produced by `from` (action→result, tool_call→result, result→artifact).
    Produces,
    /// `from` verifies `to` (verification→artifact / verification→result).
    Verifies,
    /// `from` contradicts `to`.
    Contradicts,
    /// Generic traceable link where no sharper relation applies.
    Links,
}

/// One node of the evidence graph with full §5 provenance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvidenceNode {
    pub id: String,
    pub schema_version: u32,
    pub kind: NodeKind,
    /// Short human-readable statement ("what happened").
    pub summary: String,
    /// Who/what initiated this node (actor identity from `mcp/actor.rs` semantics).
    pub actor: String,
    /// Which model participated, if any (§5: models propose, never establish).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Which tool executed, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// Inputs provided (command argv, parameters, queries…).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inputs: Option<serde_json::Value>,
    /// Outputs produced (captured stdout tail, result payload…).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outputs: Option<serde_json::Value>,
    /// Files this node touched (workspace-relative).
    #[serde(default)]
    pub files_changed: Vec<String>,
    /// Environment snapshot (selected vars only — never secrets).
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
    /// Source revision active when the node was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// What verification was performed (command / procedure).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification: Option<String>,
    /// What conclusion was derived, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conclusion: Option<String>,
    /// What remains unknown — recorded, never hidden (§18).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unknowns: Option<String>,
    /// References into existing evidence stores (ledger/proofs/tool JSONL/artifacts/claims).
    #[serde(default)]
    pub evidence_refs: Vec<EvidenceRef>,
    pub created_at: DateTime<Utc>,
    /// Chain: hex hash of the previous stored node, or zeros for genesis.
    pub prev_hash: String,
    /// Hex hash over this node's outcome fields (tamper evidence).
    pub entry_hash: String,
}

impl EvidenceNode {
    /// Create a node; hashes are stamped by `EvidenceGraph::add_node`.
    pub fn new(kind: NodeKind, summary: impl Into<String>, actor: impl Into<String>) -> Self {
        EvidenceNode {
            id: Uuid::new_v4().to_string(),
            schema_version: 1,
            kind,
            summary: summary.into(),
            actor: actor.into(),
            model: None,
            tool: None,
            inputs: None,
            outputs: None,
            files_changed: Vec::new(),
            environment: BTreeMap::new(),
            commit: None,
            verification: None,
            conclusion: None,
            unknowns: None,
            evidence_refs: Vec::new(),
            created_at: Utc::now(),
            prev_hash: "0".repeat(64),
            entry_hash: String::new(),
        }
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    pub fn with_tool(mut self, tool: impl Into<String>) -> Self {
        self.tool = Some(tool.into());
        self
    }

    pub fn with_commit(mut self, commit: impl Into<String>) -> Self {
        self.commit = Some(commit.into());
        self
    }

    pub fn with_evidence(
        mut self,
        kind: crate::claim::EvidenceKind,
        id: impl Into<String>,
    ) -> Self {
        self.evidence_refs.push(EvidenceRef {
            kind,
            id: id.into(),
        });
        self
    }

    /// Hash over every field a future reader must be able to trust.
    pub fn outcome_hash(&self) -> String {
        let mut h = Sha256::new();
        h.update(self.id.as_bytes());
        h.update(self.summary.as_bytes());
        h.update(serde_json::to_string(&self.kind).unwrap_or_default());
        h.update(self.actor.as_bytes());
        h.update(self.model.clone().unwrap_or_default().as_bytes());
        h.update(self.tool.clone().unwrap_or_default().as_bytes());
        h.update(serde_json::to_string(&self.inputs).unwrap_or_default());
        h.update(serde_json::to_string(&self.outputs).unwrap_or_default());
        h.update(serde_json::to_string(&self.files_changed).unwrap_or_default());
        h.update(serde_json::to_string(&self.environment).unwrap_or_default());
        h.update(self.commit.clone().unwrap_or_default().as_bytes());
        h.update(self.verification.clone().unwrap_or_default().as_bytes());
        h.update(self.conclusion.clone().unwrap_or_default().as_bytes());
        h.update(self.unknowns.clone().unwrap_or_default().as_bytes());
        h.update(serde_json::to_string(&self.evidence_refs).unwrap_or_default());
        h.update(self.created_at.to_rfc3339().as_bytes());
        h.update(self.prev_hash.as_bytes());
        hex(h.finalize())
    }
}

/// A directed edge; both endpoints must already exist (no dangling claims).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceEdge {
    pub from: String,
    pub to: String,
    pub relation: EdgeKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// Persisted graph document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphDocument {
    pub schema_version: u32,
    pub nodes: Vec<EvidenceNode>,
    pub edges: Vec<EvidenceEdge>,
}

fn hex(bytes: impl AsRef<[u8]>) -> String {
    bytes.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

/// Where the graph is persisted (env override → `<root>/.zylcode/evidence_graph.json`).
pub fn graph_path(root: &Path) -> PathBuf {
    if let Ok(p) = std::env::var("EVIDENCE_GRAPH_PATH") {
        return PathBuf::from(p);
    }
    root.join(".zylcode").join("evidence_graph.json")
}

/// File-backed evidence graph with hash-chained nodes.
#[derive(Debug)]
pub struct EvidenceGraph {
    path: PathBuf,
    lock: Mutex<()>,
}

impl EvidenceGraph {
    pub fn open(root: &Path) -> Result<Self> {
        Ok(Self::with_path(graph_path(root)))
    }

    pub fn with_path(path: impl Into<PathBuf>) -> Self {
        EvidenceGraph {
            path: path.into(),
            lock: Mutex::new(()),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn load(&self) -> Result<GraphDocument> {
        if !self.path.exists() {
            return Ok(GraphDocument {
                schema_version: 1,
                nodes: Vec::new(),
                edges: Vec::new(),
            });
        }
        let raw = std::fs::read_to_string(&self.path)?;
        if raw.trim().is_empty() {
            bail!("evidence graph file is empty — refusing to present an empty graph as valid");
        }
        Ok(serde_json::from_str(&raw)?)
    }

    fn persist(&self, doc: &GraphDocument) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(doc)?)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    /// Validate, chain, and append a node. Returns the stored (stamped) node.
    pub fn add_node(&self, mut node: EvidenceNode) -> Result<EvidenceNode> {
        if node.summary.trim().is_empty() {
            bail!("evidence node {}: summary must not be empty", node.id);
        }
        if node.actor.trim().is_empty() {
            bail!(
                "evidence node {}: actor must not be empty (§5: who initiated?)",
                node.id
            );
        }
        let _g = self.lock.lock().unwrap();
        let mut doc = self.load()?;
        if doc.nodes.iter().any(|n| n.id == node.id) {
            bail!("evidence node {}: duplicate id", node.id);
        }
        let prev = doc.nodes.last().map(|n| n.entry_hash.clone());
        node.prev_hash = prev.unwrap_or_else(|| "0".repeat(64));
        node.entry_hash = node.outcome_hash();
        doc.nodes.push(node.clone());
        self.persist(&doc)?;
        Ok(node)
    }

    /// Append an edge; refuses unknown endpoints (integrity over convenience).
    pub fn add_edge(
        &self,
        from: impl Into<String>,
        to: impl Into<String>,
        relation: EdgeKind,
        note: Option<String>,
    ) -> Result<EvidenceEdge> {
        let (from, to) = (from.into(), to.into());
        let _g = self.lock.lock().unwrap();
        let mut doc = self.load()?;
        let has = |id: &str| doc.nodes.iter().any(|n| n.id == id);
        if !has(&from) {
            bail!("evidence edge: unknown `from` node {from}");
        }
        if !has(&to) {
            bail!("evidence edge: unknown `to` node {to}");
        }
        if from == to {
            bail!("evidence edge: self-loop rejected");
        }
        if doc
            .edges
            .iter()
            .any(|e| e.from == from && e.to == to && e.relation == relation)
        {
            bail!("evidence edge: duplicate {from} -{relation:?}-> {to}");
        }
        let edge = EvidenceEdge {
            from,
            to,
            relation,
            note,
            created_at: Utc::now(),
        };
        doc.edges.push(edge.clone());
        self.persist(&doc)?;
        Ok(edge)
    }

    pub fn document(&self) -> Result<GraphDocument> {
        let _g = self.lock.lock().unwrap();
        self.load()
    }

    /// Verify the node hash chain, per-node outcome hashes, and that every
    /// edge endpoint resolves. `false` means tampering or corruption.
    pub fn verify_integrity(&self) -> Result<bool> {
        let doc = self.document()?;
        let mut expected_prev = "0".repeat(64);
        for n in &doc.nodes {
            if n.prev_hash != expected_prev || n.entry_hash != n.outcome_hash() {
                return Ok(false);
            }
            expected_prev = n.entry_hash.clone();
        }
        let ids: std::collections::HashSet<_> = doc.nodes.iter().map(|n| n.id.as_str()).collect();
        for e in &doc.edges {
            if !ids.contains(e.from.as_str()) || !ids.contains(e.to.as_str()) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Walk edges backward from a node (direct parents), e.g. to trace a
    /// claim back to the intent that produced it.
    pub fn parents_of(&self, id: &str) -> Result<Vec<EvidenceEdge>> {
        let doc = self.document()?;
        Ok(doc.edges.into_iter().filter(|e| e.to == id).collect())
    }

    /// Trace an unbroken ancestry chain from a node back to an `Intent`
    /// node, if one exists. Returns nodes ordered descendant → ancestor.
    pub fn ancestry(&self, id: &str) -> Result<Vec<EvidenceNode>> {
        let doc = self.document()?;
        let by_id: BTreeMap<&str, &EvidenceNode> =
            doc.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
        let mut chain = Vec::new();
        let mut current = id.to_string();
        let mut guard = 0;
        while let Some(node) = by_id.get(current.as_str()) {
            chain.push((*node).clone());
            if node.kind == NodeKind::Intent {
                break;
            }
            match doc
                .edges
                .iter()
                .find(|e| e.to == current && e.relation != EdgeKind::Contradicts)
            {
                Some(e) => current = e.from.clone(),
                None => break,
            }
            guard += 1;
            if guard > doc.nodes.len() + 1 {
                bail!("evidence graph: ancestry cycle detected at {id}");
            }
        }
        Ok(chain)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claim::EvidenceKind;

    fn tmp_graph(tag: &str) -> EvidenceGraph {
        let dir = std::env::temp_dir().join(format!("zylcode-graph-test-{tag}-{}", Uuid::new_v4()));
        EvidenceGraph::with_path(dir.join("evidence_graph.json"))
    }

    #[test]
    fn node_chain_reload_roundtrip_and_integrity() {
        let g = tmp_graph("roundtrip");
        let mut prev = String::new();
        for kind in [
            NodeKind::Intent,
            NodeKind::Specification,
            NodeKind::Plan,
            NodeKind::Action,
            NodeKind::Verification,
            NodeKind::Claim,
        ] {
            let n = g
                .add_node(EvidenceNode::new(kind, format!("step {kind:?}"), "agent"))
                .unwrap();
            if !prev.is_empty() {
                g.add_edge(&prev, &n.id, EdgeKind::DerivesFrom, None)
                    .unwrap();
            }
            prev = n.id.clone();
        }
        let doc = g.document().unwrap();
        assert_eq!(doc.nodes.len(), 6);
        assert_eq!(doc.edges.len(), 5);
        assert_eq!(doc.nodes[0].prev_hash, "0".repeat(64));
        assert!(g.verify_integrity().unwrap());
        assert_eq!(g.document().unwrap().nodes, doc.nodes, "reload is stable");
    }

    #[test]
    fn full_section5_chain_is_traceable_to_intent() {
        let g = tmp_graph("fullchain");
        let kinds = [
            NodeKind::Intent,
            NodeKind::Specification,
            NodeKind::Plan,
            NodeKind::Decision,
            NodeKind::Action,
            NodeKind::ToolCall,
            NodeKind::Result,
            NodeKind::Artifact,
            NodeKind::Verification,
            NodeKind::Claim,
        ];
        let mut ids = Vec::new();
        for (i, kind) in kinds.iter().enumerate() {
            let n = g
                .add_node(
                    EvidenceNode::new(*kind, format!("n{i}"), "agent")
                        .with_tool("cargo")
                        .with_commit("abc123")
                        .with_evidence(EvidenceKind::ProofRecord, format!("proof-{i}")),
                )
                .unwrap();
            ids.push(n.id);
        }
        for w in ids.windows(2) {
            g.add_edge(&w[0], &w[1], EdgeKind::DerivesFrom, None)
                .unwrap();
        }
        let chain = g.ancestry(&ids[9]).unwrap();
        assert_eq!(chain.len(), 10, "claim traces back through all stages");
        assert_eq!(chain[0].kind, NodeKind::Claim);
        assert_eq!(chain[9].kind, NodeKind::Intent);
        assert!(g.verify_integrity().unwrap());
    }

    #[test]
    fn dangling_edge_refused() {
        let g = tmp_graph("dangling");
        let n = g
            .add_node(EvidenceNode::new(NodeKind::Intent, "want it", "human"))
            .unwrap();
        let r = g.add_edge(&n.id, "missing-node", EdgeKind::Produces, None);
        assert!(r.is_err());
        assert!(g.document().unwrap().edges.is_empty());
    }

    #[test]
    fn self_loop_and_duplicate_refused() {
        let g = tmp_graph("loopy");
        let n = g
            .add_node(EvidenceNode::new(NodeKind::Plan, "p", "agent"))
            .unwrap();
        assert!(g.add_edge(&n.id, &n.id, EdgeKind::Links, None).is_err());
        g.add_edge(n.id.clone(), n.id.clone(), EdgeKind::Links, None)
            .ok();
        // duplicate id node refused
        assert!(g.add_node(n.clone()).is_err());
    }

    #[test]
    fn empty_summary_or_actor_refused() {
        let g = tmp_graph("empty");
        assert!(g
            .add_node(EvidenceNode::new(NodeKind::Action, "  ", "agent"))
            .is_err());
        assert!(g
            .add_node(EvidenceNode::new(NodeKind::Action, "x", ""))
            .is_err());
    }

    #[test]
    fn tamper_detection() {
        let g = tmp_graph("tamper");
        let n = g
            .add_node(EvidenceNode::new(NodeKind::Result, "exit 0", "cargo"))
            .unwrap();
        assert!(g.verify_integrity().unwrap());
        let raw = std::fs::read_to_string(g.path()).unwrap();
        let tampered = raw.replace("exit 0", "exit 1");
        std::fs::write(g.path(), tampered).unwrap();
        assert!(
            !g.verify_integrity().unwrap(),
            "tampered graph must not verify"
        );
        let _ = n;
    }

    #[test]
    fn corrupt_file_is_error_not_silent_empty_graph() {
        let g = tmp_graph("corrupt");
        std::fs::create_dir_all(g.path().parent().unwrap()).unwrap();
        std::fs::write(g.path(), "{ nope").unwrap();
        assert!(g.document().is_err());
        assert!(g.verify_integrity().is_err());
        std::fs::write(g.path(), "   ").unwrap();
        assert!(
            g.document().is_err(),
            "blank file must not read as an empty valid graph"
        );
    }
}
