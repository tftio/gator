//! Harness adapter interface for LLM coding agents.
//!
//! This module defines the [`Harness`] trait that all agent adapters
//! implement, plus the supporting types ([`AgentHandle`], [`AgentEvent`],
//! [`MaterializedTask`]) and the [`HarnessRegistry`] for runtime lookup.
//!
//! # Architecture
//!
//! ```text
//! Orchestrator
//!     |
//!     v
//! HarnessRegistry --get("claude-code")--> &dyn Harness
//!     |                                        |
//!     |   spawn(task) -------------------------+
//!     |        |
//!     |        v
//!     |   AgentHandle { pid, stdin, task_id, ... }
//!     |        |
//!     |   events(handle) --> Stream<AgentEvent>
//!     |   send(handle, msg)
//!     |   kill(handle)
//!     |   is_running(handle)
//! ```

pub mod claude_code;
pub mod registry;
pub mod trait_def;
pub mod types;

// Re-export the primary public API at the module level.
pub use claude_code::ClaudeCodeAdapter;
pub use registry::HarnessRegistry;
pub use trait_def::Harness;
pub use types::{AgentEvent, AgentHandle, MaterializedTask};
