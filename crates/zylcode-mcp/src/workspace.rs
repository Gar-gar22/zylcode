//! Which workspace a tool call belongs to.
//!
//! # Why this exists
//!
//! `fs.*` has to resolve and contain paths against *the* workspace, but the
//! `Tool` trait's `call(&self, params)` carries no context, so
//! [`crate::tool::DynamicTool`] was falling back to `std::env::current_dir()`.
//! That is only correct when the process happens to have been started in the
//! workspace: `zylcode --workspace D:\other build` run from anywhere else would
//! contain `fs.read` against the caller's shell directory instead of against
//! `--workspace`, and a containment root that is not the workspace is not
//! containment.
//!
//! Widening the trait would touch every implementor, so the workspace is
//! ambient for the duration of a unit of work — exactly the instrument
//! [`crate::actor`] already uses for "who is acting", and for the same reason.
//!
//! # Usage
//!
//! ```ignore
//! let result = zylcode_mcp::workspace::with_workspace_root(root, async {
//!     tool.call(params).await
//! }).await;
//! ```
//!
//! Nested scopes are allowed and the innermost wins. An unbound task yields
//! `None`, and the caller falls back to the process working directory.

use std::path::PathBuf;

tokio::task_local! {
    static WORKSPACE_ROOT: PathBuf;
}

/// Run `future` with `root` bound as the current workspace root.
///
/// The binding is task-local, so it does not leak to sibling tasks spawned
/// elsewhere. A task spawned *inside* the scope does **not** inherit it — tokio
/// task-local values do not propagate across `tokio::spawn`. The caller must
/// re-bind inside the spawned closure if it wants containment to follow.
pub async fn with_workspace_root<F, T>(root: impl Into<PathBuf>, future: F) -> T
where
    F: std::future::Future<Output = T>,
{
    WORKSPACE_ROOT.scope(root.into(), future).await
}

/// The workspace root bound to the current task, if any.
pub fn current_workspace_root() -> Option<PathBuf> {
    WORKSPACE_ROOT.try_with(|r| r.clone()).ok()
}

/// The directory a tool must contain itself within: the bound workspace root,
/// else the process working directory, else `.`.
///
/// This is the single place the fallback is decided, so a caller cannot pick a
/// different root by accident.
pub fn tool_working_directory() -> PathBuf {
    current_workspace_root()
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    #[tokio::test]
    async fn unbound_tasks_have_no_workspace_root() {
        assert_eq!(current_workspace_root(), None);
    }

    #[tokio::test]
    async fn the_root_is_visible_inside_the_scope() {
        let seen = with_workspace_root("/srv/repo", async { current_workspace_root() }).await;
        assert_eq!(seen.as_deref(), Some(Path::new("/srv/repo")));
    }

    #[tokio::test]
    async fn the_root_does_not_leak_past_the_scope() {
        with_workspace_root("/srv/repo", async {}).await;
        assert_eq!(
            current_workspace_root(),
            None,
            "the binding must not outlive the scope"
        );
    }

    #[tokio::test]
    async fn the_innermost_scope_wins() {
        let seen = with_workspace_root("/srv/outer", async {
            with_workspace_root("/srv/inner", async { current_workspace_root() }).await
        })
        .await;
        assert_eq!(seen.as_deref(), Some(Path::new("/srv/inner")));
    }

    #[tokio::test]
    async fn sibling_tasks_are_unaffected() {
        let outer = with_workspace_root("/srv/repo", async { current_workspace_root() });
        let inner = async { current_workspace_root() };
        let (a, b) = tokio::join!(outer, inner);
        assert_eq!(a.as_deref(), Some(Path::new("/srv/repo")));
        assert_eq!(b, None, "a sibling future must not see the binding");
    }

    /// The bound root must win over the process working directory, because
    /// that substitution is the entire point of the module.
    #[tokio::test]
    async fn the_bound_root_is_preferred_over_the_cwd() {
        let cwd = std::env::current_dir().unwrap();
        let bound = with_workspace_root("/srv/repo", async { tool_working_directory() }).await;
        assert_eq!(bound, PathBuf::from("/srv/repo"));
        assert_ne!(
            bound, cwd,
            "if these coincide the test proves nothing; run it with a distinct CWD"
        );
    }
}
