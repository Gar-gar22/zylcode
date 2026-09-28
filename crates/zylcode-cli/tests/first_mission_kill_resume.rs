//! Crash/resume half of the First Mission (Master Transformation Prompt §36,
//! step 14): a real child process is interrupted with `std::process::abort()`
//! after a step's side effects landed but before its evidence was written,
//! and a second invocation must reconcile the filesystem against expected
//! effects, reconstruct the lost evidence, and finish the mission honestly.

use std::path::PathBuf;
use std::process::Command;

use uuid::Uuid;
use zylcode_core::ledger::LedgerStore;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_zylcode")
}

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("zylcode-core")
        .join("tests")
        .join("fixtures")
}

fn mission_args(
    spec: &std::path::Path,
    fixture: &std::path::Path,
    workdir: &std::path::Path,
) -> Vec<String> {
    vec![
        "mission".into(),
        "--spec".into(),
        spec.display().to_string(),
        "--fixture".into(),
        fixture.display().to_string(),
        "--workdir".into(),
        workdir.display().to_string(),
    ]
}

fn read_json(path: &std::path::Path) -> serde_json::Value {
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("reading {} must succeed: {e}", path.display()));
    serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("parsing {} must succeed: {e}", path.display()))
}

/// One kill at `op:2:pre-evidence`, one honest resume (§36 steps 6, 7, 9, 10,
/// 11, 14, 15 in a single scenario).
#[tokio::test]
async fn killed_first_mission_reconciles_and_completes_on_resume() {
    let tmp = tempfile::tempdir().expect("temp workdir");
    let workdir = tmp.path().join("work");
    let spec = fixtures().join("first_mission_spec.md");
    let fixture = fixtures().join("first_mission_project");
    let mission_dir = workdir.join(".zylcode").join("first_mission");

    // --- 1. Kill: the interruption lands after the repair is written to disk
    //        but before its evidence entry exists (state checkpoint still at 2).
    let out = Command::new(bin())
        .args(mission_args(&spec, &fixture, &workdir))
        .arg("--crash-at")
        .arg("op:2:pre-evidence")
        .output()
        .expect("first invocation must spawn");
    assert!(
        !out.status.success(),
        "the interrupted run must not report success (status: {:?})",
        out.status
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("harness interruption injected"),
        "the process must die at the injected interruption point:\n{stderr}"
    );

    // --- 2. Durable aftermath: side effects present, evidence absent.
    let core = std::fs::read_to_string(workdir.join("project").join("statslib").join("core.py"))
        .expect("the fixture copy must exist");
    assert!(
        core.contains("return sum(values) / len(values)"),
        "the repair must have landed before the kill"
    );
    let state = read_json(&mission_dir.join("state.json"));
    assert_eq!(
        state["next_step"], 4,
        "the checkpoint must still point at the replace step (plan index 4: \
         prepare=0, baseline=1, op:0=2, op:1=3, op:2=4): {state}"
    );
    assert_eq!(state["status"], "running");
    assert!(
        state["reconciled_steps"]
            .as_array()
            .map(|a| a.is_empty())
            .unwrap_or(true),
        "nothing has been reconciled yet: {state}"
    );

    // --- 3. Resume (no crash point): reconcile must reconstruct the lost
    //        evidence from the durable artifact and finish the mission.
    let out2 = Command::new(bin())
        .args(mission_args(&spec, &fixture, &workdir))
        .env_remove("ZYLCODE_MISSION_CRASH_AT")
        .output()
        .expect("second invocation must spawn");
    let stderr2 = String::from_utf8_lossy(&out2.stderr);
    assert!(
        out2.status.success(),
        "the resumed run must complete:\n--- stderr ---\n{stderr2}"
    );
    let stdout = String::from_utf8_lossy(&out2.stdout);
    assert!(
        stdout.contains("status: COMPLETED"),
        "stdout must state completion:\n{stdout}"
    );
    assert!(
        stdout.contains("integrity: ledger=true graph=true claims=true"),
        "all chains must verify after resume:\n{stdout}"
    );
    assert!(
        stdout.contains("[proven]"),
        "verified claims must be reported:\n{stdout}"
    );

    // --- 4. Checkpoint truth: counted resume + reconstruction on record.
    let state2 = read_json(&mission_dir.join("state.json"));
    assert!(
        state2["resumed_count"].as_u64().unwrap_or(0) >= 1,
        "the interruption must be counted: {state2}"
    );
    assert_eq!(state2["status"], "completed");
    let reconciled: Vec<String> = state2["reconciled_steps"]
        .as_array()
        .expect("reconciled_steps is an array")
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();
    assert!(
        reconciled.contains(&"op:2".to_string()),
        "the killed step must be reconciled, not blindly repeated: {reconciled:?}"
    );

    // The op:2 evidence was reconstructed from the artifact (§10), not
    // re-executed: its payload carries the `reconciled` marker.
    let state = read_json(&mission_dir.join("state.json"));
    let session = Uuid::parse_str(state["session_id"].as_str().expect("session_id")).expect("uuid");
    let ledger = zylcode_core::sqlite_ledger::SqliteLedgerStore::new(
        mission_dir
            .join("ledger.sqlite")
            .to_str()
            .expect("utf-8 path"),
    )
    .expect("ledger opens");
    let entries = ledger.get_entries(session).await.expect("entries read");
    let op2 = entries
        .iter()
        .find(|e| e.action_id == "first_mission:op:2")
        .expect("op:2 must end up evidenced");
    assert_eq!(
        op2.payload.as_ref().and_then(|p| p.get("reconciled")),
        Some(&serde_json::Value::Bool(true)),
        "op:2 evidence must be the reconstructed one: {:?}",
        op2.payload
    );
    assert!(
        op2.payload
            .as_ref()
            .and_then(|p| p.get("after_sha256"))
            .is_some(),
        "reconstructed evidence must cite the artifact hash"
    );

    // --- 5. Failure recovered with evidence (§9), after a real attempt.
    let failures: Vec<serde_json::Value> =
        serde_json::from_str(&std::fs::read_to_string(mission_dir.join("failures.json")).unwrap())
            .expect("failures.json parses");
    assert_eq!(
        failures.len(),
        1,
        "exactly the mandated failure: {failures:?}"
    );
    assert_eq!(failures[0]["status"], "recovered");
    assert!(
        failures[0]["retry_count"].as_u64().unwrap_or(0) >= 1,
        "recovery needs a recorded attempt: {:?}",
        failures[0]
    );
    assert!(
        failures[0]["resolution"].is_string(),
        "recovered carries a resolution"
    );

    // --- 6. Record + package exist, with no unevidenced plan steps.
    let record =
        std::fs::read_to_string(mission_dir.join("engineering_record.md")).expect("record exists");
    assert!(
        record.contains("**COMPLETED**"),
        "record must state completion:\n{record}"
    );
    assert!(
        !record.contains("NO EVIDENCE"),
        "every plan step must carry evidence after resume:\n{record}"
    );
    let pkg = mission_dir.join("repro");
    for f in [
        "manifest.json",
        "checksums.sha256",
        "ledger.jsonl",
        "claims.json",
    ] {
        assert!(pkg.join(f).exists(), "package must contain {f}");
    }

    // Queue surface: done, not failed, not blocked.
    let queue = std::fs::read_to_string(workdir.join(".zylcode").join("missions.json"))
        .expect("queue file");
    assert!(queue.contains("\"done\""), "queue must show done:\n{queue}");

    // --- 7. A third invocation on a completed mission is a stable no-op.
    let out3 = Command::new(bin())
        .args(mission_args(&spec, &fixture, &workdir))
        .env_remove("ZYLCODE_MISSION_CRASH_AT")
        .output()
        .expect("third invocation must spawn");
    assert!(
        out3.status.success(),
        "re-reporting a completed mission must succeed"
    );
    assert!(
        String::from_utf8_lossy(&out3.stdout).contains("status: COMPLETED"),
        "third run must still report COMPLETED"
    );
    let state3 = read_json(&mission_dir.join("state.json"));
    assert!(
        state3["resumed_count"].as_u64().unwrap_or(0) >= 2,
        "every reopen is counted: {state3}"
    );
}
