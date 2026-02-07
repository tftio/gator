//! Claude Code harness adapter.
//!
//! Spawns `claude -p --output-format stream-json` as a subprocess and
//! parses its JSONL output into [`AgentEvent`] variants.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use futures::Stream;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStdout, Command};
use tokio::sync::Mutex;
use tracing::{debug, warn};

use super::trait_def::Harness;
use super::types::{AgentEvent, AgentHandle, MaterializedTask};

/// Internal state kept per spawned process.
struct ProcessState {
    /// The child process handle (for kill / is_running).
    child: Child,
    /// Stdout reader; `Option` so it can be `.take()`-ed once for streaming.
    stdout: Option<ChildStdout>,
}

/// Harness adapter for [Claude Code](https://docs.anthropic.com/en/docs/claude-code).
///
/// Launches `claude -p --output-format stream-json` and streams events
/// by parsing each JSONL line emitted on stdout.
#[derive(Clone)]
pub struct ClaudeCodeAdapter {
    /// Path to the `claude` binary. Defaults to `"claude"` (found via `$PATH`).
    claude_binary_path: String,
    /// Per-process bookkeeping, keyed by OS pid.
    processes: Arc<Mutex<HashMap<u32, ProcessState>>>,
}

impl std::fmt::Debug for ClaudeCodeAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClaudeCodeAdapter")
            .field("claude_binary_path", &self.claude_binary_path)
            .finish()
    }
}

impl ClaudeCodeAdapter {
    /// Create a new adapter that will look for `claude` on `$PATH`.
    pub fn new() -> Self {
        Self {
            claude_binary_path: "claude".to_string(),
            processes: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Create a new adapter with a custom binary path.
    ///
    /// Useful for testing or when `claude` is installed in a non-standard
    /// location.
    pub fn with_binary(path: impl Into<String>) -> Self {
        Self {
            claude_binary_path: path.into(),
            processes: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl Default for ClaudeCodeAdapter {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// JSONL parsing helpers
// ---------------------------------------------------------------------------

/// Parse a single JSONL line from Claude Code's stream-json output into
/// zero or more `AgentEvent` values.
///
/// Returns `Ok(events)` on success or `Err` if the line is not valid JSON.
/// Callers should treat `Err` as a warning and continue reading.
fn parse_stream_json_line(line: &str) -> Result<Vec<AgentEvent>> {
    let v: serde_json::Value =
        serde_json::from_str(line).context("malformed JSON in stream output")?;

    let mut events = Vec::new();

    // Determine the event type from the top-level "type" field.
    let event_type = v.get("type").and_then(|t| t.as_str()).unwrap_or("");

    match event_type {
        // ----------------------------------------------------------------
        // assistant -- contains a message with content blocks and usage
        // ----------------------------------------------------------------
        "assistant" => {
            if let Some(message) = v.get("message") {
                // Extract text content from content blocks.
                if let Some(content_arr) = message.get("content").and_then(|c| c.as_array()) {
                    for block in content_arr {
                        let block_type = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
                        match block_type {
                            "text" => {
                                if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                                    events.push(AgentEvent::Message {
                                        role: "assistant".to_string(),
                                        content: text.to_string(),
                                    });
                                }
                            }
                            "tool_use" => {
                                let tool_name = block
                                    .get("name")
                                    .and_then(|n| n.as_str())
                                    .unwrap_or("unknown")
                                    .to_string();
                                let input = block
                                    .get("input")
                                    .cloned()
                                    .unwrap_or(serde_json::Value::Null);
                                events.push(AgentEvent::ToolCall {
                                    tool: tool_name,
                                    input,
                                });
                            }
                            _ => {
                                // Unknown content block type; skip.
                            }
                        }
                    }
                }

                // Extract token usage if present.
                if let Some(usage) = message.get("usage") {
                    let input_tokens =
                        usage.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                    let output_tokens = usage
                        .get("output_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    if input_tokens > 0 || output_tokens > 0 {
                        events.push(AgentEvent::TokenUsage {
                            input_tokens,
                            output_tokens,
                        });
                    }
                }
            }
        }

        // ----------------------------------------------------------------
        // tool_use -- agent invoked a tool (sometimes top-level)
        // ----------------------------------------------------------------
        "tool_use" => {
            let tool_name = v
                .get("name")
                .or_else(|| v.get("tool"))
                .and_then(|n| n.as_str())
                .unwrap_or("unknown")
                .to_string();
            let input = v
                .get("input")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            events.push(AgentEvent::ToolCall {
                tool: tool_name,
                input,
            });
        }

        // ----------------------------------------------------------------
        // tool_result -- a tool returned a value
        // ----------------------------------------------------------------
        "tool_result" => {
            let tool_name = v
                .get("name")
                .or_else(|| v.get("tool"))
                .and_then(|n| n.as_str())
                .unwrap_or("unknown")
                .to_string();
            let output = v
                .get("output")
                .or_else(|| v.get("content"))
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            events.push(AgentEvent::ToolResult {
                tool: tool_name,
                output,
            });
        }

        // ----------------------------------------------------------------
        // result -- final result message from Claude Code
        // ----------------------------------------------------------------
        "result" => {
            // The result type often contains a final text and usage info.
            if let Some(result_text) = v.get("result").and_then(|r| r.as_str()) {
                events.push(AgentEvent::Message {
                    role: "assistant".to_string(),
                    content: result_text.to_string(),
                });
            }
            // Also check for usage at top level of result.
            if let Some(usage) = v.get("usage") {
                let input_tokens =
                    usage.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                let output_tokens = usage
                    .get("output_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                if input_tokens > 0 || output_tokens > 0 {
                    events.push(AgentEvent::TokenUsage {
                        input_tokens,
                        output_tokens,
                    });
                }
            }
        }

        // ----------------------------------------------------------------
        // error -- an error from the agent
        // ----------------------------------------------------------------
        "error" => {
            let message = v
                .get("error")
                .and_then(|e| e.get("message").and_then(|m| m.as_str()))
                .or_else(|| v.get("message").and_then(|m| m.as_str()))
                .unwrap_or("unknown error")
                .to_string();
            events.push(AgentEvent::Error { message });
        }

        // ----------------------------------------------------------------
        // Unrecognised -- ignore but log
        // ----------------------------------------------------------------
        other => {
            debug!(event_type = other, "ignoring unrecognised stream-json event type");
        }
    }

    Ok(events)
}

// ---------------------------------------------------------------------------
// Harness trait implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl Harness for ClaudeCodeAdapter {
    fn name(&self) -> &str {
        "claude-code"
    }

    async fn spawn(&self, task: &MaterializedTask) -> Result<AgentHandle> {
        // Build the system prompt / task instructions that will be appended.
        let system_instructions = format!(
            "You are working on task: {name}\n\n{description}\n\n\
             Available invariant commands:\n{invariants}\n\n\
             When you are done, run: gator done",
            name = task.name,
            description = task.description,
            invariants = task
                .invariant_commands
                .iter()
                .map(|c| format!("  - {c}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );

        let mut cmd = Command::new(&self.claude_binary_path);

        cmd.arg("-p")
            .arg("--output-format")
            .arg("stream-json")
            .arg("--verbose")
            .arg("--allowedTools")
            .arg("Bash,Read,Edit,Write,Glob,Grep")
            .arg("--append-system-prompt")
            .arg(&system_instructions);

        // Working directory.
        cmd.current_dir(&task.working_dir);

        // Environment variables (merge, don't replace the entire env).
        for (key, value) in &task.env_vars {
            cmd.env(key, value);
        }

        // We need stdin, stdout, and stderr pipes.
        cmd.stdin(std::process::Stdio::piped());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::null());

        let mut child = cmd.spawn().with_context(|| {
            format!(
                "failed to spawn claude binary at '{}' -- is it installed and on PATH?",
                self.claude_binary_path
            )
        })?;

        let pid = child.id().context("child process has no pid")?;

        // Take the piped stdin so we can give it to AgentHandle.
        let stdin = child.stdin.take();
        // Take stdout so we can store it for events().
        let stdout = child.stdout.take();

        // Store the child + stdout for later use by events(), kill(),
        // is_running().
        {
            let mut processes = self.processes.lock().await;
            processes.insert(
                pid,
                ProcessState {
                    child,
                    stdout,
                },
            );
        }

        Ok(AgentHandle {
            pid,
            stdin,
            task_id: task.task_id,
            attempt: 0,
            harness_name: self.name().to_string(),
        })
    }

    fn events(&self, handle: &AgentHandle) -> Pin<Box<dyn Stream<Item = AgentEvent> + Send>> {
        let pid = handle.pid;
        let processes = Arc::clone(&self.processes);

        // We build an async_stream that:
        // 1. Takes stdout from the process state.
        // 2. Reads it line by line.
        // 3. Parses each line and yields AgentEvent values.
        // 4. On EOF or error, yields Completed.
        let stream = async_stream::stream! {
            // Take stdout out of the process state.
            let stdout = {
                let mut procs = processes.lock().await;
                procs.get_mut(&pid).and_then(|state| state.stdout.take())
            };

            let Some(stdout) = stdout else {
                warn!(pid, "no stdout available for pid -- events already consumed or process missing");
                yield AgentEvent::Error {
                    message: "stdout not available (already consumed or process not found)".to_string(),
                };
                yield AgentEvent::Completed;
                return;
            };

            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();

            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        match parse_stream_json_line(trimmed) {
                            Ok(events) => {
                                for event in events {
                                    yield event;
                                }
                            }
                            Err(e) => {
                                warn!(line = trimmed, error = %e, "skipping malformed JSONL line");
                            }
                        }
                    }
                    Ok(None) => {
                        // EOF -- process closed stdout.
                        break;
                    }
                    Err(e) => {
                        warn!(error = %e, "error reading agent stdout");
                        yield AgentEvent::Error {
                            message: format!("stdout read error: {e}"),
                        };
                        break;
                    }
                }
            }

            yield AgentEvent::Completed;
        };

        Box::pin(stream)
    }

    async fn send(&self, handle: &AgentHandle, message: &str) -> Result<()> {
        // AgentHandle.stdin is Option<ChildStdin>, but we only have &handle,
        // not &mut handle. For real usage the caller must hold &mut handle
        // and take() the stdin. However, the trait signature uses &AgentHandle.
        //
        // We fall back to a stored reference, but ideally the caller should
        // have taken stdin from the handle and written directly.
        //
        // For now: bail with a descriptive error since we cannot mutate the
        // handle through a shared reference. A production implementation
        // would use interior mutability on the handle or store stdin in the
        // adapter's process map. Let's store a channel instead -- but to keep
        // it simple, we note that `send` is primarily for --resume flows
        // and stdin is typically written before events() consumes stdout.
        //
        // Practical approach: we don't have the stdin here, but we can still
        // write to /proc/<pid>/fd/0 on Linux. That won't work portably.
        // Instead, let's just log a warning for now. This will be refined
        // when conversation continuation is implemented in T013.
        let _ = handle;
        let _ = message;
        bail!(
            "send() is not yet supported for ClaudeCodeAdapter -- \
             conversation continuation via --resume will be added in T013"
        )
    }

    async fn kill(&self, handle: &AgentHandle) -> Result<()> {
        let pid = handle.pid;

        let mut processes = self.processes.lock().await;

        if let Some(state) = processes.get_mut(&pid) {
            // First attempt: SIGTERM via Child::kill() which on Unix sends
            // SIGKILL. We want SIGTERM first, so use nix/libc if available.
            // Since we target macOS/Linux, use libc::kill directly.
            //
            // Send SIGTERM.
            #[cfg(unix)]
            {
                // SAFETY: pid is a valid u32 from a child we spawned.
                let ret = unsafe { libc::kill(pid as i32, libc::SIGTERM) };
                if ret != 0 {
                    warn!(pid, "SIGTERM failed, proceeding to SIGKILL");
                }
            }

            // Wait briefly for graceful shutdown.
            let exited = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                state.child.wait(),
            )
            .await;

            match exited {
                Ok(Ok(_status)) => {
                    debug!(pid, "process exited after SIGTERM");
                }
                _ => {
                    // Still running or error waiting -- force kill.
                    debug!(pid, "process did not exit after SIGTERM, sending SIGKILL");
                    let _ = state.child.kill().await;
                }
            }

            // Remove from our process map.
            processes.remove(&pid);
        } else {
            debug!(pid, "kill called but process not in map (already exited?)");
        }

        Ok(())
    }

    async fn is_running(&self, handle: &AgentHandle) -> bool {
        let pid = handle.pid;
        let mut processes = self.processes.lock().await;

        if let Some(state) = processes.get_mut(&pid) {
            // try_wait returns Ok(Some(status)) if exited, Ok(None) if still
            // running, Err on failure.
            match state.child.try_wait() {
                Ok(Some(_status)) => {
                    // Exited -- clean up.
                    processes.remove(&pid);
                    false
                }
                Ok(None) => true,
                Err(e) => {
                    warn!(pid, error = %e, "error checking process status");
                    false
                }
            }
        } else {
            false
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use uuid::Uuid;

    /// Helper: create a `MaterializedTask` suitable for testing.
    fn test_task(working_dir: &std::path::Path) -> MaterializedTask {
        MaterializedTask {
            task_id: Uuid::new_v4(),
            name: "test-task".to_string(),
            description: "A test task for unit testing.".to_string(),
            invariant_commands: vec!["echo ok".to_string()],
            working_dir: working_dir.to_path_buf(),
            env_vars: HashMap::from([
                ("GATOR_AGENT_TOKEN".to_string(), "gator_at_test_0_abc".to_string()),
            ]),
        }
    }

    // -- JSONL parsing tests -----------------------------------------------

    #[test]
    fn parse_assistant_message_with_text() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Hello, world!"}],"usage":{"input_tokens":10,"output_tokens":5}}}"#;
        let events = parse_stream_json_line(line).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0],
            AgentEvent::Message {
                role: "assistant".to_string(),
                content: "Hello, world!".to_string(),
            }
        );
        assert_eq!(
            events[1],
            AgentEvent::TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
            }
        );
    }

    #[test]
    fn parse_assistant_message_with_tool_use_block() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"ls -la"}}]}}"#;
        let events = parse_stream_json_line(line).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0],
            AgentEvent::ToolCall {
                tool: "Bash".to_string(),
                input: serde_json::json!({"command": "ls -la"}),
            }
        );
    }

    #[test]
    fn parse_top_level_tool_use() {
        let line = r#"{"type":"tool_use","name":"Read","input":{"path":"/tmp/file.rs"}}"#;
        let events = parse_stream_json_line(line).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0],
            AgentEvent::ToolCall {
                tool: "Read".to_string(),
                input: serde_json::json!({"path": "/tmp/file.rs"}),
            }
        );
    }

    #[test]
    fn parse_tool_result() {
        let line = r#"{"type":"tool_result","name":"Bash","output":"file.rs\nlib.rs\n"}"#;
        let events = parse_stream_json_line(line).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0],
            AgentEvent::ToolResult {
                tool: "Bash".to_string(),
                output: serde_json::json!("file.rs\nlib.rs\n"),
            }
        );
    }

    #[test]
    fn parse_result_type() {
        let line = r#"{"type":"result","result":"Task completed successfully.","usage":{"input_tokens":500,"output_tokens":200}}"#;
        let events = parse_stream_json_line(line).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0],
            AgentEvent::Message {
                role: "assistant".to_string(),
                content: "Task completed successfully.".to_string(),
            }
        );
        assert_eq!(
            events[1],
            AgentEvent::TokenUsage {
                input_tokens: 500,
                output_tokens: 200,
            }
        );
    }

    #[test]
    fn parse_error_type() {
        let line = r#"{"type":"error","error":{"message":"rate limit exceeded"}}"#;
        let events = parse_stream_json_line(line).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0],
            AgentEvent::Error {
                message: "rate limit exceeded".to_string(),
            }
        );
    }

    #[test]
    fn parse_error_type_flat() {
        let line = r#"{"type":"error","message":"something broke"}"#;
        let events = parse_stream_json_line(line).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0],
            AgentEvent::Error {
                message: "something broke".to_string(),
            }
        );
    }

    #[test]
    fn parse_unknown_type_returns_empty() {
        let line = r#"{"type":"system","data":"warmup"}"#;
        let events = parse_stream_json_line(line).unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn parse_malformed_json_returns_error() {
        let line = "this is not json";
        assert!(parse_stream_json_line(line).is_err());
    }

    #[test]
    fn parse_empty_content_array() {
        let line = r#"{"type":"assistant","message":{"content":[]}}"#;
        let events = parse_stream_json_line(line).unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn parse_no_usage_in_assistant() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"}]}}"#;
        let events = parse_stream_json_line(line).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0],
            AgentEvent::Message {
                role: "assistant".to_string(),
                content: "hi".to_string(),
            }
        );
    }

    // -- Integration tests with real subprocesses --------------------------

    #[tokio::test]
    async fn spawn_echo_subprocess_and_stream_events() {
        // Use a shell script that emits JSONL to simulate Claude Code output.
        // We create a ClaudeCodeAdapter pointing at a custom "binary" that
        // is actually a shell command.
        let tmp = tempfile::tempdir().unwrap();
        let script_path = tmp.path().join("fake_claude.sh");
        std::fs::write(
            &script_path,
            "#!/bin/sh\n\
             echo '{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"Hello from fake claude\"}],\"usage\":{\"input_tokens\":42,\"output_tokens\":7}}}'\n\
             echo '{\"type\":\"tool_use\",\"name\":\"Bash\",\"input\":{\"command\":\"ls\"}}'\n\
             echo '{\"type\":\"tool_result\",\"name\":\"Bash\",\"output\":\"file.txt\"}'\n\
             echo '{\"type\":\"result\",\"result\":\"Done.\",\"usage\":{\"input_tokens\":100,\"output_tokens\":50}}'\n",
        )
        .unwrap();

        // Make executable.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }

        let adapter = ClaudeCodeAdapter::with_binary(script_path.to_str().unwrap());
        let task = test_task(tmp.path());

        let handle = adapter.spawn(&task).await.unwrap();
        assert!(handle.pid > 0);
        assert_eq!(handle.harness_name, "claude-code");
        assert_eq!(handle.task_id, task.task_id);

        let events: Vec<AgentEvent> = adapter.events(&handle).collect().await;

        // We expect:
        // 1. Message "Hello from fake claude"
        // 2. TokenUsage 42/7
        // 3. ToolCall Bash
        // 4. ToolResult Bash
        // 5. Message "Done."
        // 6. TokenUsage 100/50
        // 7. Completed
        assert!(events.len() >= 5, "expected at least 5 events, got {}", events.len());

        // Check first event is the assistant message.
        assert_eq!(
            events[0],
            AgentEvent::Message {
                role: "assistant".to_string(),
                content: "Hello from fake claude".to_string(),
            }
        );

        // Last event should always be Completed.
        assert_eq!(events.last().unwrap(), &AgentEvent::Completed);

        // Check a ToolCall is present.
        assert!(
            events.iter().any(|e| matches!(e, AgentEvent::ToolCall { tool, .. } if tool == "Bash")),
            "expected a ToolCall for Bash"
        );

        // Check a ToolResult is present.
        assert!(
            events.iter().any(|e| matches!(e, AgentEvent::ToolResult { tool, .. } if tool == "Bash")),
            "expected a ToolResult for Bash"
        );
    }

    #[tokio::test]
    async fn spawn_handles_malformed_lines_gracefully() {
        let tmp = tempfile::tempdir().unwrap();
        let script_path = tmp.path().join("bad_claude.sh");
        std::fs::write(
            &script_path,
            "#!/bin/sh\n\
             echo 'this is not json'\n\
             echo ''\n\
             echo '{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"valid line\"}]}}'\n\
             echo 'another bad line {{{{'\n",
        )
        .unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }

        let adapter = ClaudeCodeAdapter::with_binary(script_path.to_str().unwrap());
        let task = test_task(tmp.path());

        let handle = adapter.spawn(&task).await.unwrap();
        let events: Vec<AgentEvent> = adapter.events(&handle).collect().await;

        // Should get the valid message + Completed, malformed lines skipped.
        assert!(
            events.iter().any(|e| matches!(e, AgentEvent::Message { content, .. } if content == "valid line")),
            "expected the valid message event"
        );
        assert_eq!(events.last().unwrap(), &AgentEvent::Completed);
    }

    #[tokio::test]
    async fn spawn_binary_not_found_returns_error() {
        let adapter = ClaudeCodeAdapter::with_binary("/nonexistent/path/to/claude");
        let task = test_task(std::path::Path::new("/tmp"));

        let result = adapter.spawn(&task).await;
        assert!(result.is_err());
        let err_msg = format!("{:#}", result.unwrap_err());
        assert!(
            err_msg.contains("failed to spawn claude binary"),
            "error message should mention binary spawn failure, got: {err_msg}"
        );
    }

    #[tokio::test]
    async fn kill_terminates_subprocess() {
        let tmp = tempfile::tempdir().unwrap();
        // A script that sleeps forever (until killed).
        let script_path = tmp.path().join("sleepy_claude.sh");
        std::fs::write(
            &script_path,
            "#!/bin/sh\nsleep 3600\n",
        )
        .unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }

        let adapter = ClaudeCodeAdapter::with_binary(script_path.to_str().unwrap());
        let task = test_task(tmp.path());

        let handle = adapter.spawn(&task).await.unwrap();
        assert!(adapter.is_running(&handle).await);

        adapter.kill(&handle).await.unwrap();

        // After kill, process should no longer be running.
        assert!(!adapter.is_running(&handle).await);
    }

    #[tokio::test]
    async fn is_running_returns_false_after_exit() {
        let tmp = tempfile::tempdir().unwrap();
        let script_path = tmp.path().join("quick_claude.sh");
        std::fs::write(
            &script_path,
            "#!/bin/sh\necho done\n",
        )
        .unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }

        let adapter = ClaudeCodeAdapter::with_binary(script_path.to_str().unwrap());
        let task = test_task(tmp.path());

        let handle = adapter.spawn(&task).await.unwrap();

        // Poll until the process exits (with a timeout to avoid hanging).
        for _ in 0..20 {
            if !adapter.is_running(&handle).await {
                return; // success
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        panic!("process did not exit within 2 seconds");
    }

    #[tokio::test]
    async fn events_called_twice_yields_error_then_completed() {
        let tmp = tempfile::tempdir().unwrap();
        let script_path = tmp.path().join("once_claude.sh");
        std::fs::write(
            &script_path,
            "#!/bin/sh\necho '{\"type\":\"result\",\"result\":\"ok\"}'\n",
        )
        .unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }

        let adapter = ClaudeCodeAdapter::with_binary(script_path.to_str().unwrap());
        let task = test_task(tmp.path());

        let handle = adapter.spawn(&task).await.unwrap();

        // First call should work fine.
        let events1: Vec<AgentEvent> = adapter.events(&handle).collect().await;
        assert!(events1.iter().any(|e| matches!(e, AgentEvent::Completed)));

        // Second call should get an error + completed (stdout already consumed).
        let events2: Vec<AgentEvent> = adapter.events(&handle).collect().await;
        assert!(events2.iter().any(|e| matches!(e, AgentEvent::Error { .. })));
        assert_eq!(events2.last().unwrap(), &AgentEvent::Completed);
    }

    #[test]
    fn adapter_name_is_claude_code() {
        let adapter = ClaudeCodeAdapter::new();
        assert_eq!(adapter.name(), "claude-code");
    }

    #[test]
    fn adapter_debug_does_not_panic() {
        let adapter = ClaudeCodeAdapter::new();
        let debug_str = format!("{adapter:?}");
        assert!(debug_str.contains("ClaudeCodeAdapter"));
        assert!(debug_str.contains("claude"));
    }

    #[test]
    fn adapter_default_binary_path() {
        let adapter = ClaudeCodeAdapter::new();
        assert_eq!(adapter.claude_binary_path, "claude");
    }

    #[test]
    fn adapter_custom_binary_path() {
        let adapter = ClaudeCodeAdapter::with_binary("/usr/local/bin/claude");
        assert_eq!(adapter.claude_binary_path, "/usr/local/bin/claude");
    }

    #[test]
    fn adapter_implements_default() {
        let adapter = ClaudeCodeAdapter::default();
        assert_eq!(adapter.name(), "claude-code");
    }

    #[tokio::test]
    async fn adapter_can_register_in_harness_registry() {
        let mut registry = super::super::HarnessRegistry::new();
        let adapter = ClaudeCodeAdapter::new();
        registry.register(adapter);
        assert!(registry.get("claude-code").is_some());
        assert_eq!(registry.get("claude-code").unwrap().name(), "claude-code");
    }

    #[tokio::test]
    async fn spawn_sets_working_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let script_path = tmp.path().join("pwd_claude.sh");
        // Script that prints its working directory as a JSONL message.
        std::fs::write(
            &script_path,
            "#!/bin/sh\nCWD=$(pwd)\necho \"{\\\"type\\\":\\\"result\\\",\\\"result\\\":\\\"$CWD\\\"}\"\n",
        )
        .unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }

        // Create a sub-directory to use as working dir.
        let work_dir = tmp.path().join("workdir");
        std::fs::create_dir(&work_dir).unwrap();

        let adapter = ClaudeCodeAdapter::with_binary(script_path.to_str().unwrap());
        let mut task = test_task(&work_dir);
        task.working_dir = work_dir.clone();

        let handle = adapter.spawn(&task).await.unwrap();
        let events: Vec<AgentEvent> = adapter.events(&handle).collect().await;

        // Find the message event that contains the working directory.
        let has_workdir_event = events.iter().any(|e| {
            if let AgentEvent::Message { content, .. } = e {
                // Resolve symlinks for macOS /private/var vs /var.
                let canonical_work = work_dir.canonicalize().unwrap_or(work_dir.clone());
                let canonical_content = PathBuf::from(content)
                    .canonicalize()
                    .unwrap_or(PathBuf::from(content));
                canonical_content == canonical_work
            } else {
                false
            }
        });
        assert!(has_workdir_event, "expected working directory in events output, events: {events:?}");
    }

    #[tokio::test]
    async fn spawn_injects_env_vars() {
        let tmp = tempfile::tempdir().unwrap();
        let script_path = tmp.path().join("env_claude.sh");
        // Script that outputs GATOR_AGENT_TOKEN as a JSONL message.
        std::fs::write(
            &script_path,
            "#!/bin/sh\necho \"{\\\"type\\\":\\\"result\\\",\\\"result\\\":\\\"$GATOR_AGENT_TOKEN\\\"}\"\n",
        )
        .unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }

        let adapter = ClaudeCodeAdapter::with_binary(script_path.to_str().unwrap());
        let task = test_task(tmp.path());

        let handle = adapter.spawn(&task).await.unwrap();
        let events: Vec<AgentEvent> = adapter.events(&handle).collect().await;

        let has_token_event = events.iter().any(|e| {
            if let AgentEvent::Message { content, .. } = e {
                content == "gator_at_test_0_abc"
            } else {
                false
            }
        });
        assert!(has_token_event, "expected GATOR_AGENT_TOKEN in output, events: {events:?}");
    }

    #[tokio::test]
    async fn process_exit_emits_completed() {
        let tmp = tempfile::tempdir().unwrap();
        let script_path = tmp.path().join("exit_claude.sh");
        // Script that exits with code 1 without any output.
        std::fs::write(
            &script_path,
            "#!/bin/sh\nexit 1\n",
        )
        .unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }

        let adapter = ClaudeCodeAdapter::with_binary(script_path.to_str().unwrap());
        let task = test_task(tmp.path());

        let handle = adapter.spawn(&task).await.unwrap();
        let events: Vec<AgentEvent> = adapter.events(&handle).collect().await;

        // Should still get Completed even with non-zero exit.
        assert_eq!(events.last().unwrap(), &AgentEvent::Completed);
    }
}
