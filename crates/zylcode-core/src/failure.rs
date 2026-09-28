//! First-class Failure object — Master Transformation Prompt §9.
//!
//! Every failed operation produces structured information instead of an opaque
//! error string, and the taxonomy distinguishes outcomes that generic
//! "success/failure" collapses:
//!
//! ```text
//! FAILED · BLOCKED · TIMED_OUT · CANCELLED · UNKNOWN · PARTIALLY_COMPLETED · RECOVERED
//! ```
//!
//! Rule (§9): a failure is never turned into a success merely because the
//! model believes it can fix it. `Recovered` is only reachable through a
//! recorded diagnosis, a recorded recovery attempt, cited evidence, and an
//! explicit resolution — the same fail-closed posture as `claim::Verified`.

use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::claim::EvidenceRef;

/// Lifecycle taxonomy of a failure (§9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureStatus {
    /// The operation ran and failed.
    Failed,
    /// The operation could not run (permissions, missing dependency, refusal).
    Blocked,
    /// The operation exceeded its time budget.
    TimedOut,
    /// The operation was cancelled before completing.
    Cancelled,
    /// Cause not established — recorded as unknown, never as success.
    Unknown,
    /// Some effects landed, some did not; requires reconciliation.
    PartiallyCompleted,
    /// A diagnosed failure was repaired and re-verification passed (evidence-backed).
    Recovered,
}

/// Structured record of one failed operation (§9 fields).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Failure {
    pub id: String,
    /// Owning mission, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mission_id: Option<String>,
    /// The operation that failed (command id, step id, tool name).
    pub operation: String,
    pub status: FailureStatus,
    /// Observed cause (parsed from real output, or stated refusal reason).
    pub cause: String,
    /// Evidence refs backing this record (ledger entries, outputs, artifacts).
    #[serde(default)]
    pub evidence_refs: Vec<EvidenceRef>,
    /// Artifacts affected by the failure.
    #[serde(default)]
    pub affected_artifacts: Vec<String>,
    /// Stated strategy for recovery (empty until a recovery is attempted).
    #[serde(default)]
    pub recovery_strategy: String,
    /// How many recovery attempts have been made.
    pub retry_count: u32,
    /// Parsed diagnosis (root cause), recorded before recovery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnosis: Option<String>,
    /// Final resolution narrative; required for `recovered` / `blocked`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<String>,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_at: Option<DateTime<Utc>>,
}

impl Failure {
    /// Record a fresh failure. Initial status must be one of the non-terminal
    /// states; terminal states are only reachable through explicit transitions.
    pub fn new(
        operation: impl Into<String>,
        cause: impl Into<String>,
        status: FailureStatus,
    ) -> Result<Self> {
        if matches!(status, FailureStatus::Recovered) {
            bail!("a failure cannot be created as `recovered` — recovery must be earned (§9)");
        }
        let operation = operation.into();
        if operation.trim().is_empty() {
            bail!("failure: operation id must not be empty");
        }
        Ok(Failure {
            id: Uuid::new_v4().to_string(),
            mission_id: None,
            operation,
            status,
            cause: cause.into(),
            evidence_refs: Vec::new(),
            affected_artifacts: Vec::new(),
            recovery_strategy: String::new(),
            retry_count: 0,
            diagnosis: None,
            resolution: None,
            created_at: Utc::now(),
            resolved_at: None,
        })
    }

    pub fn with_mission(mut self, mission_id: impl Into<String>) -> Self {
        self.mission_id = Some(mission_id.into());
        self
    }

    pub fn add_evidence(&mut self, kind: crate::claim::EvidenceKind, id: impl Into<String>) {
        let r = EvidenceRef {
            kind,
            id: id.into(),
        };
        if !self.evidence_refs.contains(&r) {
            self.evidence_refs.push(r);
        }
    }

    pub fn affect_artifact(&mut self, path: impl Into<String>) {
        let p = path.into();
        if !self.affected_artifacts.contains(&p) {
            self.affected_artifacts.push(p);
        }
    }

    /// Record the parsed diagnosis. Required before recovery can succeed.
    pub fn diagnose(&mut self, diagnosis: impl Into<String>) {
        self.diagnosis = Some(diagnosis.into());
    }

    /// Record a recovery attempt; increments the retry counter and stores the
    /// strategy. Does NOT change status — only evidence-backed re-verification
    /// may do that (via `resolve_recovered`).
    pub fn attempt_recovery(&mut self, strategy: impl Into<String>) {
        self.recovery_strategy = strategy.into();
        self.retry_count += 1;
    }

    /// Terminal success transition. Fail-closed: requires a diagnosis, at
    /// least one recovery attempt, cited evidence, and a resolution.
    pub fn resolve_recovered(&mut self, resolution: impl Into<String>) -> Result<()> {
        if self.diagnosis.is_none() {
            bail!("resolve_recovered refused: no diagnosis recorded");
        }
        if self.retry_count == 0 {
            bail!("resolve_recovered refused: no recovery attempt recorded");
        }
        if self.evidence_refs.is_empty() {
            bail!("resolve_recovered refused: no evidence refs — a repair is not a recovery until it is evidenced (§9)");
        }
        self.resolution = Some(resolution.into());
        self.status = FailureStatus::Recovered;
        self.resolved_at = Some(Utc::now());
        Ok(())
    }

    /// Terminal blocked transition: the failure stands and the reason is on
    /// record. Used when re-verification does NOT pass.
    pub fn resolve_blocked(&mut self, resolution: impl Into<String>) -> Result<()> {
        if matches!(self.status, FailureStatus::Recovered) {
            bail!("resolve_blocked refused: failure already recovered");
        }
        self.resolution = Some(resolution.into());
        self.status = FailureStatus::Blocked;
        self.resolved_at = Some(Utc::now());
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        if self.operation.trim().is_empty() {
            bail!("failure {}: empty operation", self.id);
        }
        match self.status {
            FailureStatus::Recovered => {
                if self.resolution.is_none() || self.diagnosis.is_none() || self.retry_count == 0 {
                    bail!(
                        "failure {}: recovered without diagnosis/attempt/resolution",
                        self.id
                    );
                }
                if self.evidence_refs.is_empty() {
                    bail!("failure {}: recovered without evidence", self.id);
                }
            }
            FailureStatus::Blocked => {
                if self.resolution.is_none() {
                    bail!("failure {}: blocked without resolution", self.id);
                }
            }
            _ => {
                if self.resolved_at.is_some() {
                    bail!(
                        "failure {}: non-terminal status carries resolved_at",
                        self.id
                    );
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claim::EvidenceKind;

    #[test]
    fn cannot_create_as_recovered() {
        assert!(Failure::new("run_suite", "assertion", FailureStatus::Recovered).is_err());
    }

    #[test]
    fn failure_lifecycle_to_recovered() {
        let mut f = Failure::new(
            "run_suite",
            "AssertionError: 3.0 != 2.0",
            FailureStatus::Failed,
        )
        .unwrap();
        f.affect_artifact("statslib/core.py");
        // recovery refused before diagnosis
        assert!(f.resolve_recovered("fixed").is_err());
        f.diagnose("off-by-one divisor: len(values) - 1");
        // recovery refused before an attempt
        assert!(f.resolve_recovered("fixed").is_err());
        f.attempt_recovery("replace divisor with len(values)");
        // recovery refused without evidence
        assert!(f.resolve_recovered("fixed").is_err());
        f.add_evidence(EvidenceKind::LedgerEntry, "led-1");
        f.add_evidence(EvidenceKind::LedgerEntry, "led-2");
        assert!(f
            .resolve_recovered("repair applied; suite green (led-2)")
            .is_ok());
        assert_eq!(f.status, FailureStatus::Recovered);
        assert!(f.resolved_at.is_some());
        assert_eq!(f.retry_count, 1);
        assert!(f.validate().is_ok());
    }

    #[test]
    fn blocked_requires_resolution_and_cannot_flip_to_recovered_later() {
        let mut f =
            Failure::new("run_suite", "signature not found", FailureStatus::Failed).unwrap();
        assert!(f.resolve_blocked("no matching failure signature").is_ok());
        assert_eq!(f.status, FailureStatus::Blocked);
        assert!(f.resolve_recovered("late fix").is_err());
        assert!(f.validate().is_ok());
    }

    #[test]
    fn taxonomy_roundtrip_serde() {
        for s in [
            FailureStatus::Failed,
            FailureStatus::Blocked,
            FailureStatus::TimedOut,
            FailureStatus::Cancelled,
            FailureStatus::Unknown,
            FailureStatus::PartiallyCompleted,
            FailureStatus::Recovered,
        ] {
            let json = serde_json::to_string(&s).unwrap();
            let back: FailureStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(back, s);
        }
        assert_eq!(
            serde_json::to_string(&FailureStatus::TimedOut).unwrap(),
            "\"timed_out\""
        );
    }

    #[test]
    fn terminal_guard_on_nonterminal_validate() {
        let mut f = Failure::new("op", "cause", FailureStatus::Failed).unwrap();
        f.resolved_at = Some(Utc::now());
        assert!(f.validate().is_err());
    }
}
