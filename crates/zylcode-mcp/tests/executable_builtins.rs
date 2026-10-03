//! **TOOL-01** — `docs/governance/REPOSITORY_INTELLIGENCE_WAVE_2026-09-28.md` §17.1.
//!
//! `ToolRegistry` starts empty and is the only table the agent loop consults,
//! so a loop built the way `ZylCodeEngine` builds one failed at its first tool
//! step with `Tool not found: fs.read` even though `get_real_tool("fs.read")`
//! had a real executor all along.
//!
//! These tests cover the two halves of the fix, through the public API and
//! against real files — never a mock:
//!
//! 1. registration fills an empty registry with exactly the ids that have
//!    executors, and only those;
//! 2. a registered `fs.read` executes through `DynamicTool::call` →
//!    `real_tools::dispatch` → `PermissionGate` → `FileSystemTool`, reading a
//!    real file inside a bound workspace, and refusing one outside it.

use std::sync::Arc;
use zylcode_mcp::{
    get_real_tool, register_executable_builtins, with_workspace_root, Catalogue, DynamicTool,
    McpToolConfig, McpTransport, Tool, ToolRegistry,
};

/// Every catalogue-executable id must be registered, and every one of them
/// must have a real executor — a definition-only id would be registered as a
/// tool the loop then cannot run.
#[tokio::test]
async fn registration_covers_every_catalogue_executable_id() {
    let registry = ToolRegistry::new();
    assert!(
        registry.is_empty().await,
        "the registry must start empty, or this test proves nothing"
    );

    let added = register_executable_builtins(&registry).await;
    let expected: Vec<String> = Catalogue::canonical()
        .executable_ids()
        .into_iter()
        .map(str::to_string)
        .collect();

    assert_eq!(
        added, expected,
        "every executable id must be added, in order"
    );
    for id in &expected {
        assert!(registry.get(id).await.is_some(), "{id} was not registered");
        assert!(
            get_real_tool(id).is_some(),
            "{id} has no executor — a definition-only id must never be registered"
        );
    }
}

/// Registration is fill-in only: a tool already configured from
/// `mcp.tools.yaml` keeps its descriptor, and a second call adds nothing.
#[tokio::test]
async fn registration_fills_in_only_missing_ids_and_is_idempotent() {
    let registry = ToolRegistry::new();
    let configured = Arc::new(DynamicTool::new(McpToolConfig {
        id: "fs.read".to_string(),
        command: "configured-read".to_string(),
        transport: McpTransport::Stdio,
        env: Default::default(),
        enabled: true,
        description: Some("from mcp.tools.yaml".to_string()),
    })) as Arc<dyn Tool>;
    registry.register(configured).await;

    let added = register_executable_builtins(&registry).await;
    assert!(
        !added.contains(&"fs.read".to_string()),
        "an id the caller already configured must be left alone"
    );
    assert_eq!(
        added.len(),
        Catalogue::canonical().executable_ids().len() - 1,
        "every other executable id must still be filled in"
    );

    let descriptor = registry.get("fs.read").await.unwrap().descriptor();
    assert_eq!(descriptor.command, "configured-read");
    assert_eq!(
        descriptor.description.as_deref(),
        Some("from mcp.tools.yaml")
    );

    let again = register_executable_builtins(&registry).await;
    assert!(
        again.is_empty(),
        "a second pass must add nothing: {again:?}"
    );
}

/// The production dispatch path, end to end: the registry lookup that used to
/// return `None`, then `DynamicTool::call` → `dispatch` → the restrictive gate
/// → `FileSystemTool`, reading a real file from a workspace that is **not** the
/// process working directory.
#[tokio::test]
async fn a_registered_fs_read_executes_through_the_product_dispatch_path() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("probe.txt"), "bounded real read\n").unwrap();

    let registry = ToolRegistry::new();
    let added = register_executable_builtins(&registry).await;
    assert!(added.contains(&"fs.read".to_string()));

    let tool = registry
        .get("fs.read")
        .await
        .expect("the agent loop's lookup must now resolve `fs.read`");

    let out = with_workspace_root(
        dir.path(),
        tool.call(serde_json::json!({ "path": "probe.txt" })),
    )
    .await
    .expect("a read inside the workspace must succeed");

    assert_eq!(out["executed"].as_bool(), Some(true));
    assert_eq!(out["success"].as_bool(), Some(true));
    assert_eq!(
        out["output"]["content"].as_str(),
        Some("bounded real read\n")
    );
    assert_eq!(out["bound_operation"].as_str(), Some("fs.read"));
    assert!(
        out["approval_decision"]
            .as_str()
            .unwrap_or_default()
            .starts_with("allow: "),
        "a read-class tool must be allowed by the restrictive gate: {out}"
    );
}

/// The same registered tool, same dispatch path, must refuse a path outside the
/// bound workspace rather than read it.
#[tokio::test]
async fn a_registered_fs_read_refuses_a_path_outside_the_workspace() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("inside.txt"), "inside\n").unwrap();

    let registry = ToolRegistry::new();
    register_executable_builtins(&registry).await;
    let tool = registry.get("fs.read").await.unwrap();

    let err = with_workspace_root(
        dir.path(),
        tool.call(serde_json::json!({ "path": "../inside.txt" })),
    )
    .await
    .expect_err("a parent hop out of the workspace must be refused");

    assert!(err.to_string().contains("`..`"), "{err}");
}
