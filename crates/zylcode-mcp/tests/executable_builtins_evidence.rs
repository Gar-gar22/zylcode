//! Durable evidence for a `fs.read` that runs through the product's
//! registration **and** dispatch path.
//!
//! `TOOL-01` is only half closed by "the read works". The other half is that
//! what happened — including what was refused — reaches the audit log. A
//! result that never lands in the sink is indistinguishable from a result that
//! never happened.
//!
//! This lives in its own integration-test binary because the evidence
//! destination is captured when a tool's runtime is built, i.e. at
//! registration time, so `$ZYLCODE_EVIDENCE_LOG` has to be set in a process
//! where nothing else is competing to redirect it. There is deliberately a
//! single test in this file.

use zylcode_mcp::{
    register_executable_builtins, with_workspace_root, JsonlEvidenceSink, ToolRegistry,
};

#[tokio::test]
async fn the_registered_read_and_its_refusal_are_both_persisted_as_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("evidence.jsonl");
    std::fs::write(dir.path().join("probe.txt"), "evidence-bearing read\n").unwrap();

    // Captured by `JsonlEvidenceSink::from_env()` when the runtime is built,
    // so this has to happen before registration.
    std::env::set_var("ZYLCODE_EVIDENCE_LOG", &log);

    let registry = ToolRegistry::new();
    register_executable_builtins(&registry).await;
    let tool = registry
        .get("fs.read")
        .await
        .expect("`fs.read` must be reachable");

    // 1. A read inside the workspace: executed, allowed, recorded.
    let ok = with_workspace_root(
        dir.path(),
        tool.call(serde_json::json!({ "path": "probe.txt" })),
    )
    .await
    .expect("an in-workspace read must succeed");
    assert_eq!(ok["success"].as_bool(), Some(true));

    // 2. The same tool, same gate, an escape attempt: refused, and the refusal
    //    recorded too. The gate still allowed this call — `fs.read` is
    //    read-class — so the record must distinguish "the gate permitted it and
    //    the executor refused it" from "the gate denied it".
    let refused = with_workspace_root(
        dir.path(),
        tool.call(serde_json::json!({ "path": "../probe.txt" })),
    )
    .await
    .expect_err("the escape must be refused");
    assert!(
        refused.to_string().contains("`..`"),
        "the caller must be told why: {refused}"
    );

    std::env::remove_var("ZYLCODE_EVIDENCE_LOG");

    let records = JsonlEvidenceSink::new(&log)
        .read_all()
        .expect("the evidence log must be readable");
    assert_eq!(
        records.len(),
        2,
        "both the executed read and the refusal must be persisted"
    );

    let allowed = &records[0];
    assert_eq!(allowed.tool_id, "fs.read");
    assert!(
        allowed
            .approval_decision
            .as_deref()
            .unwrap_or_default()
            .starts_with("allow: "),
        "the gate decision must travel with the record: {allowed:?}"
    );
    assert!(
        allowed
            .stdout
            .as_deref()
            .unwrap_or_default()
            .contains("evidence-bearing read"),
        "the content that was actually read must be in the record"
    );
    assert!(
        allowed.end_time.is_some(),
        "a finished record has an end time"
    );

    let denied = &records[1];
    assert_eq!(denied.tool_id, "fs.read");
    assert!(
        denied
            .approval_decision
            .as_deref()
            .unwrap_or_default()
            .starts_with("allow: "),
        "fs.read is read-class, so the gate allowed it: {denied:?}"
    );
    assert!(
        denied
            .stderr
            .as_deref()
            .unwrap_or_default()
            .contains("`..`"),
        "the executor's refusal must be what was persisted: {denied:?}"
    );
    assert!(
        denied.stdout.is_none(),
        "nothing was read, so nothing may be recorded as read"
    );
}
