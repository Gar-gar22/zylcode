//! Formal Claim model — Master Transformation Prompt §4.
//!
//! A `Claim` is the unit ZylCode uses to represent *anything it asserts*,
//! together with the epistemic status of that assertion and the evidence that
//! backs it. The central rule (§3): **if evidence does not exist, the claim
//! cannot be represented as `VERIFIED`.** The API is fail-closed: `Verified`
//! cannot be constructed directly and cannot be promoted to without at least
//! one evidence reference and a verification level of `R2` or higher.
//!
//! ## Two ladders, one prefix — do not conflate
//!
//! * `VerificationLevel` (this module) is the **claim ladder** from Master
//!   Transformation Prompt §7: `R0 model output … R7 formally verified`.
//! * The **capability rung** `R0–R5` in `docs/governance/ZYLCODE_PROOF_GRAPH.md`
//!   describes maturity of a whole capability and is a different axis.
//!
//! Every artifact that mentions an "R" level must name its ladder.

use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use uuid::Uuid;

/// Epistemic status of a claim (§4). Never collapsed into generic "success".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimStatus {
    /// A deterministic component saw the event happen (raw output captured).
    Observed,
    /// Follows from other cited claims/evidence by stated reasoning.
    Derived,
    /// Backed by evidence at verification level R2+ (fail-closed gate).
    Verified,
    /// Reasoned prediction; no confirming evidence exists yet.
    Hypothesis,
    /// Asserted (typically by a model) with no evidence either way.
    Unverified,
    /// Cited evidence conflicts with the statement.
    Contradicted,
    /// Nobody knows; nothing has been established.
    Unknown,
}

/// Claim verification level ladder (§7). Ordered: `R0 < R1 < … < R7`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationLevel {
    /// The model generated the claim/artifact.
    R0ModelOutput,
    /// Syntax/types/basic structural validation succeeded.
    R1StructuralValidity,
    /// The artifact successfully builds/compiles.
    R2BuildVerified,
    /// Automated tests execute successfully.
    R3TestVerified,
    /// Relevant components work together.
    R4IntegrationVerified,
    /// The target execution environment has been exercised.
    R5EnvironmentVerified,
    /// The deployed production artifact demonstrated the behavior.
    R6ProductionObserved,
    /// Explicit formal properties have been mechanically checked/proved.
    R7FormallyVerified,
}

impl VerificationLevel {
    /// Canonical ladder tag for this enum (`CLAIM_LADDER`), for UI disambiguation.
    pub const LADDER: &'static str = "CLAIM_LADDER";

    pub fn as_str(self) -> &'static str {
        match self {
            VerificationLevel::R0ModelOutput => "R0",
            VerificationLevel::R1StructuralValidity => "R1",
            VerificationLevel::R2BuildVerified => "R2",
            VerificationLevel::R3TestVerified => "R3",
            VerificationLevel::R4IntegrationVerified => "R4",
            VerificationLevel::R5EnvironmentVerified => "R5",
            VerificationLevel::R6ProductionObserved => "R6",
            VerificationLevel::R7FormallyVerified => "R7",
        }
    }
}

/// What kind of statement the claim makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimKind {
    Build,
    Test,
    Integration,
    Runtime,
    Deployment,
    Requirement,
    Behavior,
    Performance,
    Security,
    Process,
    Other,
}

/// Where the claim originated. Never the sole basis for `Verified`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimSource {
    /// A model proposed it (may reason; may not establish facts, §3).
    Model,
    /// A deterministic tool produced evidence for it.
    DeterministicTool,
    /// A human stated it.
    Human,
    /// Computed from other claims by explicit derivation.
    DerivedComputation,
}

/// Which evidence store an evidence reference points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    /// Hash-chained execution ledger entry (`ledger.rs`).
    LedgerEntry,
    /// Deterministic proof record (`proof_engine.rs`).
    ProofRecord,
    /// A line in the MCP tool evidence JSONL sink (`mcp/evidence.rs`).
    ToolEvidence,
    /// Artifact Bus object.
    Artifact,
    /// Another claim (derivation parent).
    Claim,
    /// Anything else, identified by id only.
    Other,
}

/// Pointer to an evidence object produced elsewhere in the system.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceRef {
    pub kind: EvidenceKind,
    /// Identifier inside that store (uuid / record id / artifact id).
    pub id: String,
}

/// A single claim with its epistemic state (§4).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claim {
    pub id: String,
    pub schema_version: u32,
    /// The statement being asserted.
    pub statement: String,
    pub kind: ClaimKind,
    pub status: ClaimStatus,
    pub source: ClaimSource,
    /// Evidence backing the status. Required for VERIFIED/OBSERVED/DERIVED/CONTRADICTED.
    pub evidence_refs: Vec<EvidenceRef>,
    /// Subjective confidence in `0.0..=1.0` (clamped). Confidence is never evidence.
    pub confidence: f64,
    /// Claim verification ladder (§7) — *not* the capability rung ladder.
    pub verification_level: VerificationLevel,
    pub created_at: DateTime<Utc>,
    /// Set only when status reaches `verified`.
    pub verified_at: Option<DateTime<Utc>>,
    /// Stated reasoning (required for `derived`; recorded for `contradicted`).
    pub note: Option<String>,
    /// Owning mission, if any.
    pub mission_id: Option<String>,
    /// Chain: hex hash of the previous stored claim, or zeros for genesis.
    pub prev_hash: String,
    /// Hex hash over this claim's outcome fields (tamper evidence).
    pub entry_hash: String,
}

fn hex(bytes: impl AsRef<[u8]>) -> String {
    bytes.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

impl Claim {
    /// Create a claim. Always starts `unverified` — `Verified` cannot be
    /// constructed directly (§3: evidence first, then the word).
    pub fn new(
        statement: impl Into<String>,
        kind: ClaimKind,
        source: ClaimSource,
        verification_level: VerificationLevel,
    ) -> Self {
        Claim {
            id: Uuid::new_v4().to_string(),
            schema_version: 1,
            statement: statement.into(),
            kind,
            status: ClaimStatus::Unverified,
            source,
            evidence_refs: Vec::new(),
            confidence: 0.5,
            verification_level,
            created_at: Utc::now(),
            verified_at: None,
            note: None,
            mission_id: None,
            prev_hash: "0".repeat(64),
            entry_hash: String::new(),
        }
    }

    pub fn with_mission(mut self, mission_id: impl Into<String>) -> Self {
        self.mission_id = Some(mission_id.into());
        self
    }

    /// Confidence is clamped; it never substitutes for evidence.
    pub fn set_confidence(&mut self, confidence: f64) {
        self.confidence = confidence.clamp(0.0, 1.0);
    }

    /// Attach an evidence reference (idempotent).
    pub fn add_evidence(&mut self, kind: EvidenceKind, id: impl Into<String>) {
        let r = EvidenceRef { kind, id: id.into() };
        if !self.evidence_refs.contains(&r) {
            self.evidence_refs.push(r);
        }
    }

    /// Levels only move upward, and never past `R7`.
    pub fn raise_level(&mut self, level: VerificationLevel) {
        if level > self.verification_level {
            self.verification_level = level;
        }
    }

    /// A deterministic component observed the event: requires captured evidence.
    pub fn observe(&mut self) -> Result<()> {
        if self.evidence_refs.is_empty() {
            bail!("observe() refused: no evidence refs — an observation without captured output is an assertion (§3)");
        }
        self.status = ClaimStatus::Observed;
        Ok(())
    }

    /// Mark as a reasoned prediction with no confirming evidence.
    pub fn mark_hypothesis(&mut self) {
        self.status = ClaimStatus::Hypothesis;
    }

    /// Record honest ignorance.
    pub fn mark_unknown(&mut self) {
        self.status = ClaimStatus::Unknown;
    }

    /// Derive the claim from cited premises (parent claims / artifacts).
    pub fn derive(&mut self, note: impl Into<String>) -> Result<()> {
        if self.evidence_refs.is_empty() {
            bail!("derive() refused: a derivation must cite its premises");
        }
        self.note = Some(note.into());
        self.status = ClaimStatus::Derived;
        Ok(())
    }

    /// Evidence conflicts with the statement.
    pub fn contradict(&mut self, note: impl Into<String>) -> Result<()> {
        if self.evidence_refs.is_empty() {
            bail!("contradict() refused: CONTRADICTED requires contradicting evidence (§4)");
        }
        self.note = Some(note.into());
        self.status = ClaimStatus::Contradicted;
        Ok(())
    }

    /// Promote to `Verified` — the fail-closed gate (§3, §4).
    ///
    /// Refuses when evidence is missing or the verification level is below
    /// `R2` (build verified). Success records `verified_at`.
    pub fn promote_verified(&mut self) -> Result<()> {
        if self.evidence_refs.is_empty() {
            bail!("promote_verified() refused: no evidence refs — the claim cannot be represented as PROVEN (§3)");
        }
        if self.verification_level < VerificationLevel::R2BuildVerified {
            bail!(
                "promote_verified() refused: verification level {} is below R2 (build verified)",
                self.verification_level.as_str()
            );
        }
        if self.status == ClaimStatus::Contradicted {
            bail!("promote_verified() refused: claim is contradicted by its own evidence");
        }
        self.status = ClaimStatus::Verified;
        self.verified_at = Some(Utc::now());
        Ok(())
    }

    /// Structural invariant checked before persistence. Refuses anything that
    /// would let a status outrun its evidence.
    pub fn validate(&self) -> Result<()> {
        if !(0.0..=1.0).contains(&self.confidence) {
            bail!("claim {}: confidence out of range", self.id);
        }
        match self.status {
            ClaimStatus::Verified => {
                if self.evidence_refs.is_empty() {
                    bail!("claim {}: VERIFIED without evidence refs", self.id);
                }
                if self.verification_level < VerificationLevel::R2BuildVerified {
                    bail!("claim {}: VERIFIED below level R2", self.id);
                }
                if self.verified_at.is_none() {
                    bail!("claim {}: VERIFIED without verified_at", self.id);
                }
            }
            ClaimStatus::Observed | ClaimStatus::Contradicted => {
                if self.evidence_refs.is_empty() {
                    bail!("claim {}: {:?} without evidence refs", self.id, self.status);
                }
            }
            ClaimStatus::Derived => {
                if self.evidence_refs.is_empty() {
                    bail!("claim {}: derived without cited premises", self.id);
                }
                if self.note.is_none() {
                    bail!("claim {}: derived without stated reasoning", self.id);
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Hash over every field a future reader must be able to trust.
    pub fn outcome_hash(&self) -> String {
        let mut h = Sha256::new();
        h.update(self.id.as_bytes());
        h.update(self.statement.as_bytes());
        h.update(serde_json::to_string(&self.kind).unwrap_or_default());
        h.update(serde_json::to_string(&self.status).unwrap_or_default());
        h.update(serde_json::to_string(&self.source).unwrap_or_default());
        h.update(serde_json::to_string(&self.evidence_refs).unwrap_or_default());
        h.update(self.confidence.to_le_bytes());
        h.update(serde_json::to_string(&self.verification_level).unwrap_or_default());
        h.update(self.created_at.to_rfc3339().as_bytes());
        h.update(self.verified_at.map(|t| t.to_rfc3339()).unwrap_or_default().as_bytes());
        h.update(self.note.clone().unwrap_or_default().as_bytes());
        h.update(self.mission_id.clone().unwrap_or_default().as_bytes());
        h.update(self.prev_hash.as_bytes());
        hex(h.finalize())
    }
}

/// Where claims are persisted (env override → `<root>/.zylcode/claims.json`).
pub fn claims_path(root: &Path) -> PathBuf {
    if let Ok(p) = std::env::var("CLAIMS_PATH") {
        return PathBuf::from(p);
    }
    root.join(".zylcode").join("claims.json")
}

/// File-backed claim store with a hash-chained history (survives restart, §5).
#[derive(Debug)]
pub struct ClaimStore {
    path: PathBuf,
    lock: Mutex<()>,
}

impl ClaimStore {
    /// Open (creating if needed) the claim store for a workspace root.
    pub fn open(root: &Path) -> Result<Self> {
        Ok(Self::with_path(claims_path(root)))
    }

    /// Open an explicit file path (tests, custom embeddings).
    pub fn with_path(path: impl Into<PathBuf>) -> Self {
        ClaimStore {
            path: path.into(),
            lock: Mutex::new(()),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn load(&self) -> Result<Vec<Claim>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let raw = std::fs::read_to_string(&self.path)?;
        if raw.trim().is_empty() {
            return Ok(Vec::new());
        }
        Ok(serde_json::from_str(&raw)?)
    }

    fn persist(&self, claims: &[Claim]) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(claims)?)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    /// Validate, chain, and append a claim. Returns the stored (stamped) claim.
    ///
    /// Refuses invalid claims — persistence is where the fail-closed rules bite
    /// even if a caller bypassed the builder API by constructing fields directly.
    pub fn record(&self, mut claim: Claim) -> Result<Claim> {
        claim.validate()?;
        let _g = self.lock.lock().unwrap();
        let mut claims = self.load()?;
        let prev = claims.last().map(|c| c.entry_hash.clone());
        claim.prev_hash = prev.unwrap_or_else(|| "0".repeat(64));
        claim.entry_hash = claim.outcome_hash();
        claims.push(claim.clone());
        self.persist(&claims)?;
        Ok(claim)
    }

    /// All claims, in insertion order.
    pub fn list(&self) -> Result<Vec<Claim>> {
        let _g = self.lock.lock().unwrap();
        self.load()
    }

    pub fn get(&self, id: &str) -> Result<Option<Claim>> {
        Ok(self.list()?.into_iter().find(|c| c.id == id))
    }

    /// Recompute every entry hash and linkage; `false` means tampering or
    /// corruption was detected.
    pub fn verify_chain(&self) -> Result<bool> {
        let claims = self.list()?;
        let mut expected_prev = "0".repeat(64);
        for c in &claims {
            if c.prev_hash != expected_prev || c.entry_hash != c.outcome_hash() {
                return Ok(false);
            }
            expected_prev = c.entry_hash.clone();
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_store(tag: &str) -> ClaimStore {
        let dir = std::env::temp_dir().join(format!("zylcode-claim-test-{tag}-{}", Uuid::new_v4()));
        ClaimStore::with_path(dir.join("claims.json"))
    }

    fn claim(level: VerificationLevel) -> Claim {
        Claim::new(
            "Rust compilation succeeded",
            ClaimKind::Build,
            ClaimSource::DeterministicTool,
            level,
        )
    }

    #[test]
    fn new_claim_starts_unverified_not_verified() {
        let c = claim(VerificationLevel::R3TestVerified);
        assert_eq!(c.status, ClaimStatus::Unverified);
        assert!(c.verified_at.is_none());
        assert!(c.evidence_refs.is_empty());
    }

    #[test]
    fn promote_refused_without_evidence() {
        let mut c = claim(VerificationLevel::R3TestVerified);
        assert!(c.promote_verified().is_err());
        assert_eq!(c.status, ClaimStatus::Unverified); // fail-closed: unchanged
    }

    #[test]
    fn promote_refused_below_r2_even_with_evidence() {
        let mut c = claim(VerificationLevel::R1StructuralValidity);
        c.add_evidence(EvidenceKind::ProofRecord, "proof-1");
        assert!(c.promote_verified().is_err());
        assert_ne!(c.status, ClaimStatus::Verified);
    }

    #[test]
    fn promote_ok_at_r2_with_evidence_sets_verified_at() {
        let mut c = claim(VerificationLevel::R2BuildVerified);
        c.add_evidence(EvidenceKind::ProofRecord, "proof-1");
        assert!(c.promote_verified().is_ok());
        assert_eq!(c.status, ClaimStatus::Verified);
        assert!(c.verified_at.is_some());
        assert!(c.validate().is_ok());
    }

    #[test]
    fn observe_requires_evidence() {
        let mut c = claim(VerificationLevel::R1StructuralValidity);
        assert!(c.observe().is_err());
        c.add_evidence(EvidenceKind::ToolEvidence, "ev-9");
        assert!(c.observe().is_ok());
        assert_eq!(c.status, ClaimStatus::Observed);
    }

    #[test]
    fn contradiction_requires_evidence() {
        let mut c = claim(VerificationLevel::R2BuildVerified);
        assert!(c.contradict("exit code 101 contradicts the claim").is_err());
        c.add_evidence(EvidenceKind::ProofRecord, "proof-fail");
        assert!(c.contradict("exit code 101 contradicts the claim").is_ok());
        assert_eq!(c.status, ClaimStatus::Contradicted);
        // a contradicted claim can never be promoted
        assert!(c.promote_verified().is_err());
    }

    #[test]
    fn derived_requires_premises_and_note() {
        let mut c = claim(VerificationLevel::R2BuildVerified);
        assert!(c.derive("from claims A and B").is_err());
        c.add_evidence(EvidenceKind::Claim, "claim-a");
        assert!(c.derive("from claims A and B").is_ok());
        assert_eq!(c.status, ClaimStatus::Derived);
        assert!(c.validate().is_ok());
    }

    #[test]
    fn confidence_is_clamped_and_never_evidence() {
        let mut c = claim(VerificationLevel::R0ModelOutput);
        c.set_confidence(1.7);
        assert_eq!(c.confidence, 1.0);
        c.set_confidence(-0.2);
        assert_eq!(c.confidence, 0.0);
        // high confidence does not unlock promotion
        c.set_confidence(1.0);
        assert!(c.promote_verified().is_err());
    }

    #[test]
    fn levels_only_move_up() {
        let mut c = claim(VerificationLevel::R2BuildVerified);
        c.raise_level(VerificationLevel::R1StructuralValidity);
        assert_eq!(c.verification_level, VerificationLevel::R2BuildVerified);
        c.raise_level(VerificationLevel::R3TestVerified);
        assert_eq!(c.verification_level, VerificationLevel::R3TestVerified);
    }

    #[test]
    fn record_reload_roundtrip_and_chain() {
        let store = tmp_store("roundtrip");
        let mut c = claim(VerificationLevel::R3TestVerified);
        c.add_evidence(EvidenceKind::ProofRecord, "proof-1");
        c.set_confidence(0.9);
        let stored = store.record(c).unwrap();
        assert_eq!(stored.prev_hash, "0".repeat(64));
        assert_eq!(stored.entry_hash.len(), 64);

        let mut c2 = claim(VerificationLevel::R2BuildVerified);
        c2.add_evidence(EvidenceKind::LedgerEntry, "led-1");
        let stored2 = store.record(c2).unwrap();
        assert_eq!(stored2.prev_hash, stored.entry_hash);

        let loaded = store.list().unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].statement, "Rust compilation succeeded");
        assert!(store.verify_chain().unwrap());
    }

    #[test]
    fn record_refuses_hand_built_verified_without_evidence() {
        let store = tmp_store("handbuilt");
        let mut c = claim(VerificationLevel::R3TestVerified);
        c.status = ClaimStatus::Verified; // bypass the builder on purpose
        assert!(store.record(c).is_err());
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn tamper_detection() {
        let store = tmp_store("tamper");
        let mut c = claim(VerificationLevel::R2BuildVerified);
        c.add_evidence(EvidenceKind::ProofRecord, "p1");
        c.promote_verified().unwrap();
        store.record(c).unwrap();
        assert!(store.verify_chain().unwrap());

        // tamper: rewrite the statement directly in the file
        let raw = std::fs::read_to_string(store.path()).unwrap();
        let tampered = raw.replace("Rust compilation succeeded", "Everything is fine");
        std::fs::write(store.path(), tampered).unwrap();
        assert!(!store.verify_chain().unwrap(), "tampered chain must not verify");
    }

    #[test]
    fn corrupt_file_is_an_error_not_a_silent_empty_state() {
        let store = tmp_store("corrupt");
        std::fs::create_dir_all(store.path().parent().unwrap()).unwrap();
        std::fs::write(store.path(), "{ not json").unwrap();
        assert!(store.list().is_err());
        assert!(store.verify_chain().is_err());
    }
}
