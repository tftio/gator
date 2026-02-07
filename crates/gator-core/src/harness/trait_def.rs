//! The `Harness` trait -- the adapter interface for agent runtimes.
//!
//! Each concrete harness (Claude Code, Codex CLI, etc.) implements this
//! trait. The trait is intentionally object-safe so it can be stored as
//! `Box<dyn Harness>` in the [`super::HarnessRegistry`].

use std::pin::Pin;

use anyhow::Result;
use async_trait::async_trait;
use futures::Stream;

use super::types::{AgentEvent, AgentHandle, MaterializedTask};

/// Adapter interface for spawning and managing LLM coding agents.
///
/// Implementors wrap a specific agent CLI (e.g. `claude`, `codex`) and
/// translate its I/O into the common [`AgentEvent`] stream.
///
/// # Object Safety
///
/// This trait is object-safe: every method either returns a concrete type
/// or a boxed trait object. This means you can store `Box<dyn Harness>`
/// in collections such as [`super::HarnessRegistry`].
#[async_trait]
pub trait Harness: Send + Sync {
    /// Human-readable name for this harness (e.g. "claude-code").
    fn name(&self) -> &str;

    /// Spawn an agent process for the given task.
    ///
    /// The harness should:
    /// 1. Build the subprocess command with appropriate flags.
    /// 2. Set `task.working_dir` as the current directory.
    /// 3. Inject `task.env_vars` into the process environment.
    /// 4. Return an [`AgentHandle`] with the process ID and stdin.
    async fn spawn(&self, task: &MaterializedTask) -> Result<AgentHandle>;

    /// Return a stream of events from a running agent.
    ///
    /// The stream should yield events until the agent exits, at which
    /// point it should emit [`AgentEvent::Completed`] and terminate.
    fn events(&self, handle: &AgentHandle) -> Pin<Box<dyn Stream<Item = AgentEvent> + Send>>;

    /// Send a text message to the agent's stdin.
    ///
    /// Used for conversation continuation (e.g. via `--resume`).
    async fn send(&self, handle: &AgentHandle, message: &str) -> Result<()>;

    /// Terminate the agent process.
    ///
    /// Implementations should send SIGTERM first, wait briefly, then
    /// SIGKILL if the process has not exited.
    async fn kill(&self, handle: &AgentHandle) -> Result<()>;

    /// Check whether the agent process is still alive.
    async fn is_running(&self, handle: &AgentHandle) -> bool;
}

// Compile-time assertion: Harness must be object-safe.
// If this line compiles, the trait can be used as `dyn Harness`.
const _: () = {
    fn _assert_object_safe(_: &dyn Harness) {}
};

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    /// A trivial harness that does nothing, used only to prove the trait
    /// can be implemented and used as `dyn Harness`.
    struct NoopHarness;

    #[async_trait]
    impl Harness for NoopHarness {
        fn name(&self) -> &str {
            "noop"
        }

        async fn spawn(&self, _task: &MaterializedTask) -> Result<AgentHandle> {
            Ok(AgentHandle {
                pid: 0,
                stdin: None,
                task_id: Uuid::nil(),
                attempt: 0,
                harness_name: "noop".to_string(),
            })
        }

        fn events(&self, _handle: &AgentHandle) -> Pin<Box<dyn Stream<Item = AgentEvent> + Send>> {
            Box::pin(futures::stream::empty())
        }

        async fn send(&self, _handle: &AgentHandle, _message: &str) -> Result<()> {
            Ok(())
        }

        async fn kill(&self, _handle: &AgentHandle) -> Result<()> {
            Ok(())
        }

        async fn is_running(&self, _handle: &AgentHandle) -> bool {
            false
        }
    }

    #[test]
    fn harness_is_object_safe() {
        // If this compiles, the trait is object-safe.
        let harness: Box<dyn Harness> = Box::new(NoopHarness);
        assert_eq!(harness.name(), "noop");
    }

    #[tokio::test]
    async fn noop_harness_spawn_and_query() {
        let harness: Box<dyn Harness> = Box::new(NoopHarness);

        let task = MaterializedTask {
            task_id: Uuid::new_v4(),
            name: "test".to_string(),
            description: "A test task.".to_string(),
            invariant_commands: vec![],
            working_dir: std::path::PathBuf::from("/tmp"),
            env_vars: std::collections::HashMap::new(),
        };

        let handle = harness.spawn(&task).await.unwrap();
        assert_eq!(handle.pid, 0);
        assert_eq!(handle.harness_name, "noop");
        assert!(!harness.is_running(&handle).await);

        // send and kill should succeed without error
        harness.send(&handle, "hello").await.unwrap();
        harness.kill(&handle).await.unwrap();
    }

    #[tokio::test]
    async fn noop_harness_events_stream_is_empty() {
        use futures::StreamExt;

        let harness = NoopHarness;
        let handle = AgentHandle {
            pid: 0,
            stdin: None,
            task_id: Uuid::nil(),
            attempt: 0,
            harness_name: "noop".to_string(),
        };

        let events: Vec<AgentEvent> = harness.events(&handle).collect().await;
        assert!(events.is_empty());
    }
}
