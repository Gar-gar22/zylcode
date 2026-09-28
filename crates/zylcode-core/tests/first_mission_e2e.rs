//! First Mission end-to-end (Master Transformation Prompt §36): one real
//! specification, real tools, a real recoverable failure, a real repair, and
//! evidence for every mandated step — verified here by re-checking the
//! persisted artifacts, not by trusting the runner's own report.
//!
//! The crash/resume half of §36 needs a real child process (the interruption
//! is `std::process::abort()`), so it lives in
//! `zylcode-cli/tests/first_mission_kill_resume.rs` behind the `mission`
//! subcommand.

use std::path::PathBuf;
use std::process::Command;

use tempfile::TempDir;
use zylcode_core::claim::ClaimStore;
use zylcode_core::evidence_graph::EvidenceGraph;
use zylcode_core::first_mission::{
    run_first_mission, verify_ledger_chain, MissionOutcomeStatus, MissionRunConfig,
};
use zylcode_core::ledger::LedgerEntry;

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

fn cfg(spec: PathBuf, fixture: PathBuf, workdir: &TempDir) -> MissionRunConfig {
    MissionRunConfig {
        spec_path: spec,
        fixture_path: fixture,
        workdir: workdir.path().to_path_buf(),
        fresh: true,
        crash_at: None,
    }
}

/// §36 steps 1–15: the happy path, with every claim re-derived from the
/// files the mission left behind.
#[tokio::test]
async fn first_mission_completes_with_verified_evidence() {
    let tmp = TempDir::new().expect("temp workdir");
    let spec = fixtures().join("first_mission_spec.md");
    let fixture = fixtures().join("first_mission_project");
    let outcome = run_first_mission(cfg(spec, fixture, &tmp))
        .await
        .expect("the mission must run to an outcome");

    // --- §36 step 13: verified vs observed vs derived vs hypothesis vs unknown
    assert_eq!(
        outcome.status,
        MissionOutcomeStatus::Completed,
        "mission must complete: blocked_reason={:?}",
        outcome.blocked_reason
    );
    assert!(
        outcome.verified_claims.len() >= 2,
        "baseline + after_repair must be PROVEN: {:?}",
        outcome.verified_claims
    );
    assert!(
        outcome
            .observed_claims
            .iter()
            .any(|c| c.contains("after_mean_coverage")),
        "the mandated expected-failing run must be OBSERVED: {:?}",
        outcome.observed_claims
    );
    assert!(
        outcome.derived_claims.len() >= 5,
        "one derived claim per specification requirement: {:?}",
        outcome.derived_claims
    );
    assert!(
        outcome
            .hypotheses
            .iter()
            .any(|c| c.contains("Model-driven repair")),
        "model-driven repair must stay a HYPOTHESIS: {:?}",
        outcome.hypotheses
    );
    assert!(
        outcome
            .unknowns
            .iter()
            .any(|c| c.contains("Production readiness")),
        "production readiness must stay UNKNOWN: {:?}",
        outcome.unknowns
    );

    // --- integrity of all three chains (fail-closed gate passed)
    assert!(
        outcome.integrity.ledger_chain && outcome.integrity.graph && outcome.integrity.claims_chain,
        "all chains must verify: {:?}",
        outcome.integrity
    );

    // --- §9: the mandated failure exists, is diagnosed, and was recovered
    assert_eq!(outcome.failure_count, 1, "exactly the mandated failure");
    let failures: Vec<serde_json::Value> = serde_json::from_str(
        &std::fs::read_to_string(outcome.mission_dir.join("failures.json"))
            .expect("failures.json must exist"),
    )
    .expect("failures.json parses");
    assert_eq!(failures[0]["status"], "recovered");
    assert!(
        failures[0]["retry_count"].as_u64().unwrap_or(0) >= 1,
        "recovery must record a real attempt: {:?}",
        failures[0]
    );
    assert!(
        failures[0]["diagnosis"]
            .as_str()
            .unwrap_or_default()
            .contains("AssertionError"),
        "diagnosis must come from captured output: {:?}",
        failures[0]["diagnosis"]
    );
    assert!(
        failures[0]["resolution"].is_string(),
        "a recovered failure carries a resolution"
    );

    // --- §36 step 12: the engineering record
    let record =
        std::fs::read_to_string(&outcome.record_path).expect("engineering record must exist");
    assert!(
        record.contains("**COMPLETED**"),
        "record must state the status"
    );
    assert!(
        !record.contains("NO EVIDENCE"),
        "a completed mission must have evidence for every plan step:\n{record}"
    );
    assert!(
        record.contains("## What this record does NOT establish"),
        "record must state its own gaps (§18)"
    );
    assert!(
        record.contains("## Evidence integrity"),
        "record must report chain integrity"
    );

    // --- §36 step 15: reproducibility package, checksums verified by hand
    let pkg = &outcome.package_path;
    for f in [
        "manifest.json",
        "checksums.sha256",
        "ledger.jsonl",
        "claims.json",
        "evidence_graph.json",
        "failures.json",
        "plan.json",
        "state.json",
        "specification.md",
        "engineering_record.md",
    ] {
        assert!(pkg.join(f).exists(), "package must contain {f}");
    }
    let checksums = std::fs::read_to_string(pkg.join("checksums.sha256")).unwrap();
    assert!(!checksums.trim().is_empty(), "checksums must not be empty");
    for line in checksums.lines() {
        let (sha, name) = line
            .split_once("  ")
            .unwrap_or_else(|| panic!("checksum line format: {line}"));
        let bytes = std::fs::read(pkg.join(name))
            .unwrap_or_else(|e| panic!("package file {name} unreadable: {e}"));
        assert_eq!(
            sha,
            zylcode_core::first_mission::sha256_hex(&bytes),
            "checksum for {name}"
        );
    }

    // --- §5: chains verified independently from the exported evidence
    let ledger_text = std::fs::read_to_string(pkg.join("ledger.jsonl")).unwrap();
    let entries: Vec<LedgerEntry> = ledger_text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("ledger.jsonl line parses"))
        .collect();
    assert!(
        entries.len() >= 8,
        "every step must leave evidence: {}",
        entries.len()
    );
    assert!(
        verify_ledger_chain(&entries),
        "exported ledger chain must verify"
    );
    assert!(
        entries
            .iter()
            .any(|e| e.action_id == "first_mission:finalize"),
        "finalize must be evidenced"
    );

    let graph = EvidenceGraph::open(&outcome.mission_dir).expect("graph opens");
    assert!(
        graph.verify_integrity().expect("graph reads"),
        "evidence graph must verify"
    );
    let claims = ClaimStore::with_path(outcome.mission_dir.join("claims.json"));
    assert!(
        claims.verify_chain().expect("claims read"),
        "claim chain must verify"
    );

    // --- the real project really changed, and *we* re-run the suite (§6:
    //     independent verification, not the runner's word)
    let core = std::fs::read_to_string(
        outcome
            .workdir
            .join("project")
            .join("statslib")
            .join("core.py"),
    )
    .expect("repaired core.py exists");
    assert!(
        core.contains("return sum(values) / len(values)"),
        "repair must land"
    );
    assert!(
        !core.contains("(len(values) - 1)"),
        "defective line must be gone"
    );

    let project = outcome.workdir.join("project");
    let rerun = Command::new("python")
        .args([
            "-m",
            "unittest",
            "discover",
            "-v",
            "-s",
            ".",
            "-p",
            "test_*.py",
        ])
        .current_dir(&project)
        .output()
        .expect("python must run");
    assert!(
        rerun.status.success(),
        "independent re-run of the suite must pass:\n{}\n{}",
        String::from_utf8_lossy(&rerun.stdout),
        String::from_utf8_lossy(&rerun.stderr)
    );

    // The repair commit is in real git history.
    let log = Command::new("git")
        .args(["log", "--format=%s"])
        .current_dir(&project)
        .output()
        .expect("git must run");
    assert!(
        String::from_utf8_lossy(&log.stdout).contains("repair: divide mean() by len(values)"),
        "repair must be committed"
    );

    // --- MissionQueue surface stays truthful (existing product feature)
    let queue_text =
        std::fs::read_to_string(outcome.workdir.join(".zylcode").join("missions.json")).unwrap();
    assert!(
        queue_text.contains("\"done\""),
        "queue must show the mission done:\n{queue_text}"
    );

    // Resume of a completed mission is a no-op that re-reports honestly.
    let again = run_first_mission(MissionRunConfig {
        spec_path: fixtures().join("first_mission_spec.md"),
        fixture_path: fixtures().join("first_mission_project"),
        workdir: tmp.path().to_path_buf(),
        fresh: false,
        crash_at: None,
    })
    .await
    .expect("re-opening a completed mission must work");
    assert_eq!(again.status, MissionOutcomeStatus::Completed);
    assert_eq!(again.resumed_count, 1, "resume must be counted");
}

/// §9/§18: a spec the runner cannot satisfy must BLOCK — visibly, with a
/// structured failure and a record that shows the unevidenced steps.
#[tokio::test]
async fn first_mission_blocks_on_expectation_mismatch() {
    let tmp = TempDir::new().expect("temp workdir");
    let spec_path = tmp.path().join("blocked_spec.md");
    let workdir = tmp.path().join("mission-work");
    std::fs::write(
        &spec_path,
        r#"# Blocked-path mission specification

## Intent
Deliberately expect a failure the passing baseline suite does not produce, so
the runner must block instead of inventing success.

```json
{
  "intent": "Force the mismatch path: expect fail while the suite passes (blocked-path test).",
  "requirements": ["The mission blocks instead of claiming success"],
  "verify_command": ["python", "-m", "unittest", "discover", "-v", "-s", ".", "-p", "test_*.py"],
  "operations": [
    { "op": "run", "id": "impossible_failure", "expect": "fail", "signature": ["THIS_SIGNATURE_NEVER_APPEARS"] }
  ]
}
```
"#,
    )
    .expect("write spec");

    let fixture = fixtures().join("first_mission_project");
    let outcome = run_first_mission(MissionRunConfig {
        spec_path,
        fixture_path: fixture,
        workdir,
        fresh: true,
        crash_at: None,
    })
    .await
    .expect("blocked missions still return an outcome, not an error");

    // The block itself.
    assert_eq!(outcome.status, MissionOutcomeStatus::Blocked);
    let reason = outcome
        .blocked_reason
        .expect("a block must state its reason");
    assert!(
        reason.contains("impossible_failure") && reason.contains("expected Fail"),
        "reason must name the mismatch: {reason}"
    );

    // Structured failure in the §9 taxonomy — blocked, with a resolution.
    let failures: Vec<serde_json::Value> = serde_json::from_str(
        &std::fs::read_to_string(outcome.mission_dir.join("failures.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0]["status"], "blocked");
    assert!(
        failures[0]["resolution"].is_string(),
        "a blocked failure carries why it is blocked: {:?}",
        failures[0]
    );
    assert_eq!(failures[0]["operation"], "step:op:0");

    // The record asserts the block and marks the steps that never ran.
    let record = std::fs::read_to_string(&outcome.record_path).unwrap();
    assert!(
        record.contains("**BLOCKED**"),
        "record must show the block:\n{record}"
    );
    assert!(
        record.contains("NO EVIDENCE"),
        "steps that never ran must read NO EVIDENCE:\n{record}"
    );
    assert!(
        record.contains("Remaining plan steps"),
        "§18 section must call out the gap"
    );

    // Chains stay honest even when blocked.
    assert!(
        outcome.integrity.ledger_chain && outcome.integrity.graph && outcome.integrity.claims_chain
    );
    assert!(
        outcome.package_path.join("manifest.json").exists(),
        "blocked missions still export a package"
    );

    // Queue: blocked, with the reason — never quietly failed or done.
    let queue_text =
        std::fs::read_to_string(outcome.workdir.join(".zylcode").join("missions.json")).unwrap();
    assert!(
        queue_text.contains("\"blocked\""),
        "queue must show blocked:\n{queue_text}"
    );

    // The unexecuted finalize step never grew evidence.
    assert!(
        !outcome
            .verified_claims
            .iter()
            .any(|c| c.contains("after_repair")),
        "no claim may rest on steps that did not run"
    );
}
