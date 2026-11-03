//! The `terminal` tool: one shell command, with a timeout.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::{CallKind, CallLabel, Tool, ToolResult, Workspace};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);
const MIN_TIMEOUT_MS: u64 = 100;
const MAX_TIMEOUT_MS: u64 = 600_000;
const MAX_OUTPUT_CHARS: usize = 20_000;
const POLL_INTERVAL: Duration = Duration::from_millis(20);
/// After the shell exits, how long to wait for a pipe to drain before giving up
/// on it. A background child holding the pipe open must not hang the turn.
const DRAIN_GRACE: Duration = Duration::from_secs(1);

pub struct Terminal {
    workspace: Workspace,
}

impl Terminal {
    pub fn new(workspace: Workspace) -> Self {
        Self { workspace }
    }
}

#[derive(Deserialize)]
struct TerminalInput {
    command: String,
    #[serde(default)]
    cd: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

impl Tool for Terminal {
    fn name(&self) -> &str {
        "terminal"
    }

    fn kind(&self) -> CallKind {
        CallKind::Execute
    }

    fn label(&self, input: &Value) -> Option<CallLabel> {
        let input: TerminalInput = serde_json::from_value(input.clone()).ok()?;
        Some(CallLabel::new("Run", input.command))
    }

    fn description(&self) -> &str {
        "Run a shell one-liner in the workspace and return its output. Use the \
         `cd` argument for the working directory instead of a leading `cd` inside \
         the command. Standard output and standard error are returned separately; \
         a non-zero exit code is reported with the output, so read it before \
         assuming the command failed."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The command line to run with the user's shell."
                },
                "cd": {
                    "type": "string",
                    "description": "Directory to run in, relative to the workspace root. Defaults to the root."
                },
                "timeout_ms": {
                    "type": "integer",
                    "description": "How long to wait before stopping the command. Defaults to 120000."
                }
            },
            "required": ["command"]
        })
    }

    fn needs_approval(&self) -> bool {
        true
    }

    fn run(&self, input: &Value) -> ToolResult {
        let input: TerminalInput = serde_json::from_value(input.clone())
            .map_err(|error| format!("invalid arguments: {error}"))?;
        if input.command.trim().is_empty() {
            return Err("the command is empty".to_owned());
        }

        let directory = match &input.cd {
            Some(path) => {
                let directory = self.workspace.resolve(path)?;
                if !directory.is_dir() {
                    return Err(format!("{path} is not a directory"));
                }
                directory
            }
            None => self.workspace.root().to_path_buf(),
        };

        let timeout = Duration::from_millis(
            input
                .timeout_ms
                .unwrap_or(DEFAULT_TIMEOUT.as_millis() as u64)
                .clamp(MIN_TIMEOUT_MS, MAX_TIMEOUT_MS),
        );

        run(&input.command, &directory, timeout)
    }
}

fn run(command: &str, directory: &Path, timeout: Duration) -> ToolResult {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(directory)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("could not start a shell: {error}"))?;

    let stdout = child.stdout.take().map(drain);
    let stderr = child.stderr.take().map(drain);

    let started = Instant::now();
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() >= timeout => {
                timed_out = true;
                child.kill().ok();
                break child
                    .wait()
                    .map_err(|error| format!("could not stop the command: {error}"))?;
            }
            Ok(None) => std::thread::sleep(POLL_INTERVAL),
            Err(error) => return Err(format!("could not wait for the command: {error}")),
        }
    };

    let stdout = stdout.and_then(drain_output).unwrap_or_default();
    let stderr = stderr.and_then(drain_output).unwrap_or_default();

    let mut output = String::new();
    output.push_str(&stdout);
    if !stderr.is_empty() {
        if !output.is_empty() && !output.ends_with('\n') {
            output.push('\n');
        }
        if !stdout.is_empty() {
            output.push_str("--- stderr ---\n");
        }
        output.push_str(&stderr);
    }
    let output = truncate(output);

    if timed_out {
        return Err(format!(
            "the command was stopped after {} ms\n{output}",
            timeout.as_millis()
        ));
    }

    match status.code() {
        Some(0) if output.trim().is_empty() => Ok("(no output)".to_owned()),
        Some(0) => Ok(output),
        Some(code) => Ok(format!("{output}exit code {code}\n")),
        None => Ok(format!("{output}the command was stopped by a signal\n")),
    }
}

fn drain(pipe: impl Read + Send + 'static) -> mpsc::Receiver<String> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut pipe = pipe;
        pipe.read_to_end(&mut bytes).ok();
        let _ = sender.send(String::from_utf8_lossy(&bytes).into_owned());
    });
    receiver
}

fn drain_output(receiver: mpsc::Receiver<String>) -> Option<String> {
    receiver.recv_timeout(DRAIN_GRACE).ok()
}

/// Keeps the start and the end of a long output: the model usually needs the
/// command line that failed and the summary at the bottom.
fn truncate(output: String) -> String {
    if output.chars().count() <= MAX_OUTPUT_CHARS {
        return output;
    }

    let head: String = output.chars().take(MAX_OUTPUT_CHARS * 3 / 4).collect();
    let tail: String = output
        .chars()
        .rev()
        .take(MAX_OUTPUT_CHARS / 4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{head}\n... (output truncated) ...\n{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace() -> (tempfile::TempDir, Workspace) {
        let directory = tempfile::tempdir().expect("temp dir");
        std::fs::create_dir(directory.path().join("sub")).expect("create sub");
        let workspace = Workspace::new(directory.path()).expect("workspace");
        (directory, workspace)
    }

    fn run_tool(workspace: Workspace, input: Value) -> ToolResult {
        Terminal::new(workspace).run(&input)
    }

    #[test]
    fn runs_a_command_and_returns_its_output() {
        let (_directory, workspace) = workspace();

        let output = run_tool(workspace, json!({"command": "echo hello"})).expect("run");

        assert_eq!(output, "hello\n");
    }

    #[test]
    fn runs_in_a_subdirectory() {
        let (directory, workspace) = workspace();

        let output = run_tool(workspace, json!({"command": "pwd", "cd": "sub"})).expect("run");

        let expected = std::fs::canonicalize(directory.path().join("sub")).expect("canonical");
        assert_eq!(output.trim(), expected.display().to_string());
    }

    #[test]
    fn a_failing_command_reports_its_exit_code() {
        let (_directory, workspace) = workspace();

        let error_only = run_tool(
            workspace.clone(),
            json!({"command": "echo oops >&2; exit 3"}),
        )
        .expect("run");
        assert!(error_only.contains("oops"), "{error_only}");
        assert!(error_only.contains("exit code 3"), "{error_only}");

        let both = run_tool(
            workspace,
            json!({"command": "echo out; echo err >&2; exit 1"}),
        )
        .expect("run");
        assert!(both.contains("out"), "{both}");
        assert!(both.contains("--- stderr ---"), "{both}");
        assert!(both.contains("err"), "{both}");
        assert!(both.contains("exit code 1"), "{both}");
    }

    #[test]
    fn refuses_a_working_directory_outside_the_workspace() {
        let (_directory, workspace) = workspace();

        let error =
            run_tool(workspace, json!({"command": "pwd", "cd": "../"})).expect_err("escape");

        assert!(error.contains("workspace"), "{error}");
    }

    #[test]
    fn stops_a_command_that_runs_too_long() {
        let (_directory, workspace) = workspace();

        let error = run_tool(workspace, json!({"command": "sleep 5", "timeout_ms": 200}))
            .expect_err("timeout");

        assert!(error.contains("stopped after"), "{error}");
    }

    #[test]
    fn truncates_a_huge_output() {
        let (_directory, workspace) = workspace();

        let output =
            run_tool(workspace, json!({"command": "yes abcdefgh | head -n 5000"})).expect("run");

        assert!(output.contains("(output truncated)"), "{}", &output[..80]);
        assert!(output.chars().count() < MAX_OUTPUT_CHARS + 100);
    }
}
