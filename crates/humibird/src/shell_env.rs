//! The environment a double-clicked app runs in.
//!
//! macOS gives a bundle started from Finder the launchd environment: a `PATH` of
//! four system directories and no toolchain. The `terminal` tool then cannot find
//! `cargo` or `node` even though the person's own terminal can, and the model
//! reports a machine as missing tools it has.
//!
//! So the login shell is asked once, before the window opens, the way Zed does it
//! (`crates/util/src/shell_env.rs`): the shell prints its environment as JSON and
//! the app adopts it. A launch from a terminal is left alone — the environment it
//! was handed is already the right one.

use std::collections::HashMap;
use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::Deserialize as _;

/// A shell that never finishes must not hold the window back forever.
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(5);

/// How often the child is checked for having exited.
const POLL: Duration = Duration::from_millis(10);

/// Prints this process's environment as JSON. The login shell runs the binary
/// with this flag, so the environment comes back as JSON rather than as `env`
/// text, whose values may hold newlines and quotes.
pub const PRINT_ENV: &str = "--print-env";

/// Replaces this process's environment with the login shell's, unless the app was
/// started from a terminal.
pub fn adopt_login_shell() {
    if std::io::stdout().is_terminal() {
        // Started from a terminal: that environment is already the right one, and
        // overwriting it would take away variables the caller meant to set.
        return;
    }

    let Some(captured) = capture() else {
        return;
    };
    let path = captured.get("PATH").cloned().unwrap_or_default();
    let count = captured.len();
    // Called before the app (and its threads) exist, so no other thread can be
    // reading the environment while it changes.
    for (name, value) in adopted(captured) {
        unsafe { std::env::set_var(name, value) };
    }
    log::info!("adopted {count} variables from the login shell; PATH={path}");
}

/// Writes this process's environment as JSON on stdout. Values that are not valid
/// Unicode are left out rather than aborting: one odd variable should not cost the
/// whole capture.
pub fn print_environment() {
    let environment: HashMap<String, String> = std::env::vars_os()
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
        .collect();
    match serde_json::to_string(&environment) {
        Ok(json) => println!("{json}"),
        Err(error) => {
            eprintln!("could not write the environment as JSON: {error}");
            std::process::exit(1);
        }
    }
}

/// Everything the shell reported except `SHLVL` and anything that looks like a
/// credential. `SHLVL` was incremented by the capture shell, and a login shell
/// that exports `DEEPSEEK_API_KEY` for some other tool has no business handing it
/// to an app with its own `.env` and its own precedence rules.
fn adopted(captured: HashMap<String, String>) -> Vec<(String, String)> {
    captured
        .into_iter()
        .filter(|(name, _)| name != "SHLVL" && !is_credential(name))
        .collect()
}

fn is_credential(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    ["KEY", "TOKEN", "SECRET", "PASSWORD", "CREDENTIAL"]
        .iter()
        .any(|marker| upper.contains(marker))
}

fn capture() -> Option<HashMap<String, String>> {
    let executable = std::env::current_exe().ok()?;
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let shell = login_shell();

    let mut command = capture_command(&shell, &executable, &home);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            log::warn!(
                "could not ask {} for the login environment: {error}; keeping the one the app was started with",
                shell.display()
            );
            return None;
        }
    };

    let stdout = child.stdout.take().map(drain);
    let stderr = child.stderr.take().map(drain);

    let started = Instant::now();
    let exited = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if started.elapsed() >= CAPTURE_TIMEOUT => {
                log::warn!(
                    "the login shell did not answer within {CAPTURE_TIMEOUT:?}; keeping the environment the app was started with"
                );
                if let Err(error) = child.kill() {
                    log::debug!("could not stop the login shell: {error}");
                }
                if let Err(error) = child.wait() {
                    log::debug!("could not reap the login shell: {error}");
                }
                break None;
            }
            Ok(None) => std::thread::sleep(POLL),
            Err(error) => {
                log::warn!("could not wait for the login shell: {error}");
                break None;
            }
        }
    };

    let output = stdout
        .and_then(join)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned());
    let noise = stderr
        .and_then(join)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned());
    let status = exited?;
    let output = output?;

    // Startup files that print or fail are common; only the JSON matters, so a
    // non-zero exit is a note rather than a failure.
    match parse(&output) {
        Some(environment) => {
            if !status.success() {
                log::debug!(
                    "the login shell exited with {status} but its environment was readable: {noise:?}"
                );
            }
            Some(environment)
        }
        None => {
            log::warn!(
                "could not read an environment out of the login shell ({status}); output: {output:?}, stderr: {noise:?}"
            );
            None
        }
    }
}

/// The environment object in whatever the shell printed. Startup files may print
/// banners before it and after it, so every `{` is tried as a start.
fn parse(output: &str) -> Option<HashMap<String, String>> {
    for (position, _) in output.match_indices('{') {
        let mut deserializer = serde_json::Deserializer::from_str(&output[position..]);
        if let Ok(environment) = HashMap::<String, String>::deserialize(&mut deserializer) {
            return Some(environment);
        }
    }
    None
}

fn login_shell() -> PathBuf {
    std::env::var_os("SHELL")
        .map(PathBuf::from)
        .filter(|shell| shell.is_file())
        .unwrap_or_else(|| PathBuf::from("/bin/zsh"))
}

/// The command that makes `shell` print its environment: a login, interactive
/// shell, so the files that set up nvm, asdf, direnv and friends have run.
fn capture_command(shell: &Path, executable: &Path, home: &Path) -> Command {
    let name = shell
        .file_name()
        .map(|name| name.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();

    let mut script = String::new();
    // asdf, direnv and mise hook the prompt event in fish, and `cd` hooks, so the
    // prompt is emitted and the shell is started in the home directory.
    if name == "fish" {
        script.push_str("emit fish_prompt;");
    }
    script.push_str(&format!(
        "cd {}; {} {PRINT_ENV}",
        quote(home),
        quote(executable)
    ));

    let mut command = Command::new(shell);
    if name == "csh" || name == "tcsh" {
        // csh and tcsh become login shells through argv[0], not `-l`.
        use std::os::unix::process::CommandExt as _;
        command.arg0("-");
        command.args(["-c", &script]);
    } else {
        command.args(["-l", "-i", "-c", &script]);
    }
    command
}

/// Single-quoted for the shell, with the quote itself escaped the POSIX way.
fn quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

/// Reads a pipe to its end on its own thread, so a chatty shell cannot fill the
/// pipe and block on a write while this thread waits for it to exit.
fn drain(mut pipe: impl std::io::Read + Send + 'static) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Err(error) = pipe.read_to_end(&mut bytes) {
            log::debug!("could not read the login shell's output: {error}");
        }
        bytes
    })
}

fn join(handle: std::thread::JoinHandle<Vec<u8>>) -> Option<Vec<u8>> {
    match handle.join() {
        Ok(bytes) => Some(bytes),
        Err(_) => {
            log::warn!("the thread reading the login shell's output panicked");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_credential_in_the_login_shell_is_not_adopted() {
        let mut captured = HashMap::new();
        captured.insert("PATH".to_owned(), "/opt/homebrew/bin".to_owned());
        captured.insert("DEEPSEEK_API_KEY".to_owned(), "sk-shell".to_owned());
        captured.insert("GITHUB_TOKEN".to_owned(), "ghp".to_owned());
        captured.insert("SSH_AUTH_SOCK".to_owned(), "/tmp/sock".to_owned());
        captured.insert("SHLVL".to_owned(), "2".to_owned());

        let names: Vec<String> = adopted(captured)
            .into_iter()
            .map(|(name, _)| name)
            .collect();

        assert!(names.iter().any(|name| name == "PATH"), "{names:?}");
        assert!(
            names.iter().any(|name| name == "SSH_AUTH_SOCK"),
            "{names:?}"
        );
        assert!(
            !names.iter().any(|name| name == "DEEPSEEK_API_KEY"),
            "{names:?}"
        );
        assert!(
            !names.iter().any(|name| name == "GITHUB_TOKEN"),
            "{names:?}"
        );
        assert!(!names.iter().any(|name| name == "SHLVL"), "{names:?}");
    }

    #[test]
    fn reads_the_environment_out_of_a_noisy_shell() {
        let output = "Last login: yesterday\nsome banner\n\
                      {\"PATH\":\"/opt/homebrew/bin:/usr/bin\",\"SHELL\":\"/bin/zsh\"}\n\
                      prompt noise: {not json}\n";

        let environment = parse(output).expect("the object");

        assert_eq!(
            environment.get("PATH").map(String::as_str),
            Some("/opt/homebrew/bin:/usr/bin")
        );
        assert_eq!(
            environment.get("SHELL").map(String::as_str),
            Some("/bin/zsh")
        );
    }

    #[test]
    fn a_value_with_newlines_and_quotes_survives() {
        // The reason this goes through JSON instead of `env` text.
        let output = r#"{"PROMPT":"one\ntwo \"quoted\""}"#;

        assert_eq!(
            parse(output).and_then(|environment| environment.get("PROMPT").cloned()),
            Some("one\ntwo \"quoted\"".to_owned())
        );
    }

    #[test]
    fn a_capture_without_an_object_is_rejected() {
        for output in [
            "",
            "zsh: command not found: humibird\n",
            "{ broken",
            "{\"PATH\": 3}",
        ] {
            assert_eq!(parse(output), None, "{output:?}");
        }
    }

    #[test]
    fn the_shell_is_asked_as_a_login_interactive_shell() {
        let command = capture_command(
            Path::new("/bin/zsh"),
            Path::new("/Applications/Humibird.app/Contents/MacOS/humibird"),
            Path::new("/Users/me"),
        );
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();

        assert_eq!(args.first().map(String::as_str), Some("-l"));
        assert_eq!(args.get(1).map(String::as_str), Some("-i"));
        assert_eq!(args.get(2).map(String::as_str), Some("-c"));
        assert_eq!(
            args.get(3).map(String::as_str),
            Some(
                "cd '/Users/me'; '/Applications/Humibird.app/Contents/MacOS/humibird' --print-env"
            )
        );
    }

    #[test]
    fn fish_is_told_to_run_its_prompt_hooks() {
        let command = capture_command(
            Path::new("/opt/homebrew/bin/fish"),
            Path::new("/tmp/humibird"),
            Path::new("/Users/me"),
        );
        let script = command
            .get_args()
            .nth(3)
            .map(|arg| arg.to_string_lossy().into_owned())
            .unwrap_or_default();

        assert!(script.starts_with("emit fish_prompt;"), "{script}");
    }

    #[test]
    fn a_path_with_quotes_is_quoted_for_the_shell() {
        let command = capture_command(
            Path::new("/bin/zsh"),
            Path::new("/tmp/it's here/humibird"),
            Path::new("/Users/me"),
        );
        let script = command
            .get_args()
            .nth(3)
            .map(|arg| arg.to_string_lossy().into_owned())
            .unwrap_or_default();

        assert!(script.contains(r"'/tmp/it'\''s here/humibird'"), "{script}");
    }

    #[test]
    fn the_capture_shell_does_not_raise_shlvl() {
        let mut captured = HashMap::new();
        captured.insert("SHLVL".to_owned(), "2".to_owned());
        captured.insert("PATH".to_owned(), "/opt/homebrew/bin:/usr/bin".to_owned());

        assert_eq!(
            adopted(captured),
            vec![("PATH".to_owned(), "/opt/homebrew/bin:/usr/bin".to_owned())],
            "the shell's own SHLVL is not the app's"
        );
    }
}
