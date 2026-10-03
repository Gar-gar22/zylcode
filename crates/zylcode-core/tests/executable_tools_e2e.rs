//! **TOOL-01** at the agent-loop boundary —
//! `docs/governance/REPOSITORY_INTELLIGENCE_WAVE_2026-09-28.md` §17.1.
//!
//! The defect was a name/dispatch mismatch: `ToolRegistry` starts empty, the
//! loop resolves tools *only* through it, and `ZylCodeEngine` never filled it
//! for the CLI. The first tool step of a real turn therefore ended in
//! `Tool not found: fs.read`.
//!
//! Two tests, and the second is what keeps the first honest:
//!
//! * an unfilled registry must still fail loudly at the first tool step — if
//!   this ever stops failing, the positive test below could be passing for the
//!   wrong reason;
//! * a registry filled by `register_executable_builtins` (what
//!   `ZylCodeEngine::ensure_executable_tools` runs) must let `fs.read` execute
//!   for real, with no `Tool not found` anywhere in the session.

use anyhow::Result;
use std::path::PathBuf;
use std::sync::Arc;
use zylcode_core::agent::{AgentLoop, AgentState, TestModelClient};
use zylcode_core::memory_ledger::MemoryLedgerStore;
use zylcode_mcp::ToolRegistry;

const PLAN: &str = r#"{"action": "Plan", "payload": {"steps": [{"id": "1", "description": "Read Cargo.toml", "expected_files": ["Cargo.toml"], "expected_tools": ["fs.read"], "risk": "Read", "verification": "File read successfully"}]}}"#;
const READ: &str = r#"{"action": "ToolCall", "payload": {"tool_id": "fs.read", "arguments": {"action": "read", "path": "Cargo.toml"}, "reason": "Need Cargo.toml", "expected_result": "File content"}}"#;
const DONE: &str = r#"{"action": "Complete", "payload": {"summary": "read the manifest", "evidence": ["read Cargo.toml"], "remaining_limitations": []}}"#;

fn scripted_responses() -> Vec<String> {
    vec![PLAN.to_string(), READ.to_string(), DONE.to_string()]
}

fn loop_over(registry: Arc<ToolRegistry>) -> AgentLoop {
    AgentLoop::new(
        "Read Cargo.toml",
        PathBuf::from("."),
        None,
        registry,
        Arc::new(TestModelClient::new(scripted_responses())),
        Arc::new(MemoryLedgerStore::new()),
    )
}

/// The precondition of TOOL-01, and the negative control for the test below:
/// an unfilled registry stops the run at its first tool step, loudly.
#[tokio::test]
async fn an_unfilled_registry_stops_the_first_tool_step() -> Result<()> {
    let registry = Arc::new(ToolRegistry::new());
    assert!(
        registry.is_empty().await,
        "this test is the TOOL-01 precondition: the engine's registry starts empty"
    );

    let mut agent = loop_over(registry);
    let err = agent
        .run()
        .await
        .expect_err("an empty registry must not let a tool step succeed");

    assert!(
        err.to_string().contains("Tool not found: fs.read"),
        "expected the TOOL-01 failure, got: {err}"
    );
    Ok(())
}

/// The fix: the same loop, same script, over a registry filled from the
/// catalogue, must read a real file and record it as read.
#[tokio::test]
async fn filling_the_registry_makes_fs_read_reachable() -> Result<()> {
    let registry = Arc::new(ToolRegistry::new());
    let added = zylcode_mcp::register_executable_builtins(&registry).await;
    assert!(
        added.contains(&"fs.read".to_string()),
        "`fs.read` must be among the ids that get filled in"
    );

    let mut agent = loop_over(Arc::clone(&registry));
    let final_state = agent.run().await?;

    assert_eq!(
        final_state,
        AgentState::Completed,
        "the run must reach completion rather than fail at the tool step"
    );

    assert!(
        agent
            .session()
            .context
            .files_read
            .iter()
            .any(|p| p == std::path::Path::new("Cargo.toml")),
        "the file that was actually read must be recorded: {:?}",
        agent.session().context.files_read
    );

    // The loop's own "not found" message is emitted as the whole content of a
    // system message. Matching on the prefix rather than the substring keeps a
    // file the context builder happened to read from tripping the assertion —
    // this test's own doc comment quotes the phrase.
    for message in &agent.session().messages {
        assert!(
            !message.content.starts_with("Tool not found"),
            "no tool may be unresolved once the registry is filled: {}",
            message.content
        );
    }

    assert!(
        !agent.session().context.files_read.is_empty(),
        "the observation must survive into the session"
    );
    Ok(())
}
