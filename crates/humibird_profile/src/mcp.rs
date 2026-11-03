//! The profile's `mcp.json`: the context servers the app may start.
//!
//! A separate file rather than a table in `config.toml` because server snippets
//! are copied out of MCP documentation, which is JSON. Missing keys fall back to
//! their defaults and unknown keys are ignored, so a file written for a newer
//! version still loads; an invalid value is an error, because a typo in a command
//! should be visible rather than silently disabled.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context as _, Result};
use serde::Deserialize;

use crate::Profile;

/// The context servers a profile names, by the name the file gives them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct McpConfig {
    pub servers: BTreeMap<String, McpServer>,
}

/// One context server: how to reach it, and whether to.
#[derive(Clone, PartialEq, Eq)]
pub struct McpServer {
    pub transport: McpTransport,
    pub enabled: bool,
    /// Seconds one call may take. `None` leaves it to the client's default.
    pub timeout: Option<u64>,
}

/// Where the server is. A command is run and spoken to over its standard input and
/// output; a URL is spoken to with streamable HTTP.
#[derive(Clone, PartialEq, Eq)]
pub enum McpTransport {
    Stdio {
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
    },
    Http {
        url: String,
        headers: BTreeMap<String, String>,
    },
}

impl std::fmt::Debug for McpServer {
    /// Environment values and request headers are where tokens end up, so the ones
    /// that look like secrets are not printed. The names are, because knowing which
    /// variable a server was given is what makes a failure diagnosable.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut debug = f.debug_struct("McpServer");
        match &self.transport {
            McpTransport::Stdio { command, args, env } => {
                debug
                    .field("command", command)
                    .field("args", args)
                    .field("env", &redacted(env));
            }
            McpTransport::Http { url, headers } => {
                debug.field("url", url).field("headers", &redacted(headers));
            }
        }
        debug
            .field("enabled", &self.enabled)
            .field("timeout", &self.timeout)
            .finish()
    }
}

fn redacted(values: &BTreeMap<String, String>) -> BTreeMap<&str, &str> {
    values
        .iter()
        .map(|(name, value)| {
            let shown = if looks_secret(name) {
                "***"
            } else {
                value.as_str()
            };
            (name.as_str(), shown)
        })
        .collect()
}

fn looks_secret(name: &str) -> bool {
    let name = name.to_ascii_uppercase();
    ["TOKEN", "KEY", "SECRET", "PASS", "CREDENTIAL"]
        .iter()
        .any(|hint| name.contains(hint))
}

/// What the file looks like. Kept apart from [`McpServer`] so the shape on disk
/// can default and validate differently from the shape the app uses.
#[derive(Deserialize)]
struct File {
    #[serde(default)]
    servers: BTreeMap<String, RawServer>,
}

#[derive(Deserialize)]
struct RawServer {
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    url: Option<String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default = "enabled_by_default")]
    enabled: bool,
    timeout: Option<u64>,
}

fn enabled_by_default() -> bool {
    true
}

impl McpConfig {
    /// Reads the servers out of the file's text. Every error names the server it
    /// is about, so a file with two servers does not leave the reader guessing.
    pub fn parse(contents: &str) -> Result<Self> {
        let file: File = serde_json::from_str(contents).context("failed to parse mcp.json")?;
        let mut servers = BTreeMap::new();
        for (name, raw) in file.servers {
            anyhow::ensure!(!name.trim().is_empty(), "a server has no name");
            anyhow::ensure!(
                raw.timeout != Some(0),
                "server `{name}` asks for a zero-second timeout"
            );
            let transport = match (raw.command.as_deref(), raw.url.as_deref()) {
                (Some(command), None) => {
                    anyhow::ensure!(!command.trim().is_empty(), "server `{name}` has no command");
                    McpTransport::Stdio {
                        command: command.to_owned(),
                        args: raw.args,
                        env: raw.env,
                    }
                }
                (None, Some(url)) => {
                    anyhow::ensure!(!url.trim().is_empty(), "server `{name}` has no URL");
                    McpTransport::Http {
                        url: url.to_owned(),
                        headers: raw.headers,
                    }
                }
                (Some(_), Some(_)) => anyhow::bail!(
                    "server `{name}` names both a command and a URL; it is one or the other"
                ),
                (None, None) => {
                    anyhow::bail!("server `{name}` has neither a command to run nor a URL to reach")
                }
            };
            servers.insert(
                name,
                McpServer {
                    transport,
                    enabled: raw.enabled,
                    timeout: raw.timeout,
                },
            );
        }
        Ok(Self { servers })
    }

    /// The servers that are switched on, ready to start.
    pub fn enabled(&self) -> impl Iterator<Item = (&str, &McpServer)> {
        self.servers
            .iter()
            .filter(|(_, server)| server.enabled)
            .map(|(name, server)| (name.as_str(), server))
    }
}

impl Profile {
    pub fn mcp_path(&self) -> PathBuf {
        self.root().join("mcp.json")
    }

    /// Reads `mcp.json`. A missing file is a profile with no context servers.
    pub fn mcp_config(&self) -> Result<McpConfig> {
        let path = self.mcp_path();
        match std::fs::read_to_string(&path) {
            Ok(contents) => McpConfig::parse(&contents)
                .with_context(|| format!("failed to read {}", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(McpConfig::default()),
            Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_profile_without_the_file_has_no_servers() {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        let config = profile
            .mcp_config()
            .expect("a missing file is not an error");
        assert!(config.servers.is_empty());
        assert_eq!(config.enabled().count(), 0);
    }

    #[test]
    fn servers_are_read_with_their_arguments_and_environment() {
        let config = McpConfig::parse(
            r#"{
                "servers": {
                    "github": {
                        "command": "npx",
                        "args": ["-y", "@modelcontextprotocol/server-github"],
                        "env": {"GITHUB_TOKEN": "abc"}
                    },
                    "notes": {"command": "/opt/notes", "enabled": false, "timeout": 5}
                }
            }"#,
        )
        .expect("the file is valid");

        let github = config.servers.get("github").expect("github");
        match &github.transport {
            McpTransport::Stdio { command, args, env } => {
                assert_eq!(command, "npx");
                assert_eq!(
                    args.as_slice(),
                    [
                        "-y".to_owned(),
                        "@modelcontextprotocol/server-github".to_owned()
                    ]
                );
                assert_eq!(env.get("GITHUB_TOKEN").map(String::as_str), Some("abc"));
            }
            _ => panic!("github should be a stdio server"),
        }
        assert!(github.enabled, "a server without `enabled` is on");
        assert_eq!(github.timeout, None);

        let notes = config.servers.get("notes").expect("notes");
        assert!(!notes.enabled);
        assert_eq!(notes.timeout, Some(5));
        assert_eq!(config.enabled().count(), 1, "only the switched-on server");
        assert_eq!(
            config.enabled().next().map(|(name, _)| name),
            Some("github")
        );
    }

    #[test]
    fn a_url_and_its_headers_are_read() {
        let config = McpConfig::parse(
            r#"{"servers": {"remote": {"url": "https://example.com/mcp", "headers": {"Authorization": "Bearer abc"}}}}"#,
        )
        .expect("the file is valid");

        match &config.servers.get("remote").expect("remote").transport {
            McpTransport::Http { url, headers } => {
                assert_eq!(url, "https://example.com/mcp");
                assert_eq!(
                    headers.get("Authorization").map(String::as_str),
                    Some("Bearer abc")
                );
            }
            _ => panic!("remote should be an http server"),
        }
    }

    #[test]
    fn a_server_names_a_command_or_a_url_but_not_both() {
        let both = McpConfig::parse(
            r#"{"servers": {"x": {"command": "npx", "url": "https://example.com/mcp"}}}"#,
        )
        .expect_err("one or the other");
        assert!(format!("{both:#}").contains("both"), "{both:#}");

        let neither = McpConfig::parse(r#"{"servers": {"x": {"enabled": true}}}"#)
            .expect_err("something has to say where the server is");
        assert!(format!("{neither:#}").contains("neither"), "{neither:#}");
    }

    #[test]
    fn a_file_for_a_newer_version_still_loads() {
        let config = McpConfig::parse(
            r#"{"servers": {"x": {"command": "x", "transport": "stdio"}}, "version": 2}"#,
        )
        .expect("unknown keys are ignored");
        assert!(config.servers.contains_key("x"));
    }

    #[test]
    fn a_broken_file_is_an_error_that_names_the_path() {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        std::fs::create_dir_all(profile.root()).expect("profile directory");
        std::fs::write(profile.mcp_path(), "{ not json").expect("write mcp.json");

        let error = profile.mcp_config().expect_err("a typo is visible");
        let message = format!("{error:#}");
        assert!(message.contains("mcp.json"), "{message}");
    }

    #[test]
    fn a_server_without_a_command_names_itself() {
        let error = McpConfig::parse(r#"{"servers": {"broken": {"command": "  "}}}"#)
            .expect_err("a command is required");
        assert!(format!("{error:#}").contains("broken"), "{error:#}");
    }

    #[test]
    fn a_zero_second_timeout_is_refused() {
        let error = McpConfig::parse(r#"{"servers": {"x": {"command": "x", "timeout": 0}}}"#)
            .expect_err("a call needs time to run");
        assert!(format!("{error:#}").contains("zero-second"), "{error:#}");
    }

    #[test]
    fn debug_does_not_print_a_token() {
        let config = McpConfig::parse(
            r#"{"servers": {"github": {"command": "npx", "env": {"GITHUB_TOKEN": "abc", "MODE": "read"}}}}"#,
        )
        .expect("valid");
        let printed = format!("{:?}", config.servers.get("github").expect("github"));
        assert!(!printed.contains("abc"), "{printed}");
        assert!(printed.contains("GITHUB_TOKEN"), "{printed}");
        assert!(printed.contains("read"), "{printed}");
    }
}
