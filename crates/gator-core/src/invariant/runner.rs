use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use gator_db::models::Invariant;

/// The result of executing a single invariant check.
#[derive(Debug, Clone)]
pub struct InvariantResult {
    /// Whether the invariant passed (exit code matched expected).
    pub passed: bool,
    /// The actual exit code returned by the process, or `None` if the
    /// process was terminated by a signal.
    pub exit_code: Option<i32>,
    /// Captured standard output.
    pub stdout: String,
    /// Captured standard error.
    pub stderr: String,
    /// Wall-clock duration in milliseconds.
    pub duration_ms: i64,
}

/// Run an invariant's command in the given working directory and return
/// the result.
///
/// The command is spawned as a child process with `stdout` and `stderr`
/// captured.  The exit code is compared against
/// [`Invariant::expected_exit_code`] to determine pass/fail.
pub async fn run_invariant(invariant: &Invariant, working_dir: &Path) -> Result<InvariantResult> {
    let start = Instant::now();
    let timeout = Duration::from_secs(invariant.timeout_secs.max(1) as u64);

    let mut child = Command::new(&invariant.command)
        .args(&invariant.args)
        .current_dir(working_dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| {
            format!(
                "failed to execute invariant {:?} (command: {} {})",
                invariant.name,
                invariant.command,
                invariant.args.join(" "),
            )
        })?;

    // Take stdout/stderr handles so we can read them concurrently with
    // waiting for the process. This avoids deadlocks if the child fills the
    // pipe buffer.
    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();

    let read_stdout = async {
        let mut buf = Vec::new();
        if let Some(ref mut pipe) = stdout_pipe {
            pipe.read_to_end(&mut buf).await.ok();
        }
        String::from_utf8_lossy(&buf).into_owned()
    };

    let read_stderr = async {
        let mut buf = Vec::new();
        if let Some(ref mut pipe) = stderr_pipe {
            pipe.read_to_end(&mut buf).await.ok();
        }
        String::from_utf8_lossy(&buf).into_owned()
    };

    // Wait for exit + read output concurrently, with a timeout.
    match tokio::time::timeout(timeout, async {
        let (wait_result, stdout, stderr) = tokio::join!(child.wait(), read_stdout, read_stderr);
        (wait_result, stdout, stderr)
    })
    .await
    {
        Ok((Ok(status), stdout, stderr)) => {
            let duration_ms = i64::try_from(start.elapsed().as_millis()).unwrap_or(i64::MAX);
            let exit_code = status.code();
            let passed = exit_code == Some(invariant.expected_exit_code);

            Ok(InvariantResult {
                passed,
                exit_code,
                stdout,
                stderr,
                duration_ms,
            })
        }
        Ok((Err(e), _, _)) => Err(e).with_context(|| {
            format!(
                "failed to wait on invariant {:?} (command: {} {})",
                invariant.name,
                invariant.command,
                invariant.args.join(" "),
            )
        }),
        Err(_) => {
            // Timeout: kill the child process.
            let _ = child.kill().await;
            let duration_ms = i64::try_from(start.elapsed().as_millis()).unwrap_or(i64::MAX);

            Ok(InvariantResult {
                passed: false,
                exit_code: None,
                stdout: String::new(),
                stderr: format!(
                    "invariant {:?} timed out after {}s",
                    invariant.name, invariant.timeout_secs
                ),
                duration_ms,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use gator_db::models::{InvariantKind, InvariantScope};
    use uuid::Uuid;

    /// Helper to build a minimal [`Invariant`] for testing.
    fn test_invariant(command: &str, args: &[&str], expected_exit_code: i32) -> Invariant {
        Invariant {
            id: Uuid::new_v4(),
            name: "test_invariant".to_owned(),
            description: None,
            kind: InvariantKind::Custom,
            command: command.to_owned(),
            args: args.iter().map(|s| (*s).to_owned()).collect(),
            expected_exit_code,
            threshold: None,
            scope: InvariantScope::Project,
            timeout_secs: 300,
            created_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn run_true_command_passes() {
        let inv = test_invariant("true", &[], 0);
        let result = run_invariant(&inv, Path::new("/tmp"))
            .await
            .expect("should succeed");

        assert!(result.passed, "true should pass with exit code 0");
        assert_eq!(result.exit_code, Some(0));
        assert!(result.duration_ms >= 0);
    }

    #[tokio::test]
    async fn run_false_command_fails() {
        let inv = test_invariant("false", &[], 0);
        let result = run_invariant(&inv, Path::new("/tmp"))
            .await
            .expect("should succeed (process ran, just returned non-zero)");

        assert!(!result.passed, "false should fail with exit code 1");
        assert_eq!(result.exit_code, Some(1));
    }

    #[tokio::test]
    async fn run_false_with_expected_1_passes() {
        let inv = test_invariant("false", &[], 1);
        let result = run_invariant(&inv, Path::new("/tmp"))
            .await
            .expect("should succeed");

        assert!(result.passed, "false with expected_exit_code=1 should pass");
        assert_eq!(result.exit_code, Some(1));
    }

    #[tokio::test]
    async fn captures_stdout() {
        let inv = test_invariant("echo", &["hello world"], 0);
        let result = run_invariant(&inv, Path::new("/tmp"))
            .await
            .expect("should succeed");

        assert!(result.passed);
        assert!(
            result.stdout.contains("hello world"),
            "stdout should contain the echoed text, got: {:?}",
            result.stdout
        );
    }

    #[tokio::test]
    async fn captures_stderr() {
        // Use sh -c to write to stderr.
        let inv = test_invariant("sh", &["-c", "echo error_msg >&2"], 0);
        let result = run_invariant(&inv, Path::new("/tmp"))
            .await
            .expect("should succeed");

        assert!(result.passed);
        assert!(
            result.stderr.contains("error_msg"),
            "stderr should contain the error text, got: {:?}",
            result.stderr
        );
    }

    #[tokio::test]
    async fn nonexistent_command_returns_error() {
        let inv = test_invariant("this_command_does_not_exist_gator_test", &[], 0);
        let result = run_invariant(&inv, Path::new("/tmp")).await;

        assert!(
            result.is_err(),
            "running a nonexistent command should return an error"
        );
    }

    #[tokio::test]
    async fn timeout_kills_slow_invariant() {
        let mut inv = test_invariant("sleep", &["60"], 0);
        inv.timeout_secs = 1;
        let result = run_invariant(&inv, Path::new("/tmp"))
            .await
            .expect("should succeed even on timeout");

        assert!(!result.passed, "timed-out invariant should fail");
        assert!(
            result.exit_code.is_none(),
            "killed process has no exit code"
        );
        assert!(
            result.stderr.contains("timed out"),
            "stderr should mention timeout, got: {:?}",
            result.stderr
        );
    }

    #[tokio::test]
    async fn duration_is_positive() {
        // Use a command that takes a tiny but measurable amount of time.
        let inv = test_invariant("true", &[], 0);
        let result = run_invariant(&inv, Path::new("/tmp"))
            .await
            .expect("should succeed");

        assert!(result.duration_ms >= 0, "duration should be non-negative");
    }
}
