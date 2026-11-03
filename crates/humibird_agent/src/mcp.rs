//! The context servers (MCP) a session runs.
//!
//! Everything here blocks — starting a server means spawning a process and asking
//! it for its tools — so it belongs on a background thread, not in a window. The
//! result is a plain snapshot: the tools it offers, and the servers that did not
//! come up.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use humibird_mcp::{Options, RemoteTool, Server};
use humibird_profile::{McpConfig, McpTransport};
use humibird_tools::{McpTool, Tool};

/// What a session's context servers offer, and what went wrong for the ones that
/// could not start. Cheap to share: the tools keep their connections alive.
pub struct McpServers {
    /// What each server that came up offered, for the window to report.
    connected: Vec<(String, usize)>,
    tools: Vec<Arc<McpTool>>,
    failures: Vec<String>,
}

impl McpServers {
    /// Starts every server the profile switched on and asks each for its tools.
    /// A server that fails is a line in `failures`, never a reason to give up on
    /// the others.
    pub fn start(config: &McpConfig) -> Arc<Self> {
        let mut connected = Vec::new();
        let mut tools = Vec::new();
        let mut failures = Vec::new();

        for (name, server) in config.enabled() {
            let options = match &server.transport {
                McpTransport::Stdio { command, args, env } => {
                    Options::stdio(command.clone()).args(args.clone()).env(
                        env.iter()
                            .map(|(key, value)| (key.clone(), value.clone()))
                            .collect(),
                    )
                }
                McpTransport::Http { url, headers } => Options::http(url.clone()).headers(
                    headers
                        .iter()
                        .map(|(key, value)| (key.clone(), value.clone()))
                        .collect(),
                ),
            };
            let options = match server.timeout {
                Some(seconds) => options.timeout(Duration::from_secs(seconds)),
                None => options,
            };
            let connection = match Server::connect(name, &options) {
                Ok(server) => Arc::new(Mutex::new(server)),
                Err(error) => {
                    log::warn!("the context server {name} did not start: {error:#}");
                    failures.push(format!("{name}: {error:#}"));
                    continue;
                }
            };
            match listed(&connection) {
                Ok(listed) => {
                    log::info!("the context server {name} offers {} tools", listed.len());
                    connected.push((name.to_owned(), listed.len()));
                    for tool in listed {
                        tools.push(Arc::new(McpTool::new(connection.clone(), name, tool)));
                    }
                }
                Err(error) => {
                    log::warn!("the context server {name} offered no tools: {error:#}");
                    failures.push(format!("{name}: {error:#}"));
                }
            }
        }

        Arc::new(McpServers {
            connected,
            tools,
            failures,
        })
    }

    /// The tools in the shape the conversation's list takes.
    pub fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.tools
            .iter()
            .map(|tool| tool.clone() as Arc<dyn Tool>)
            .collect()
    }

    /// One line per server that could not be used, for the window to show.
    pub fn failures(&self) -> &[String] {
        &self.failures
    }

    /// The servers that came up, with how many tools each offered.
    pub fn connected(&self) -> &[(String, usize)] {
        &self.connected
    }
}

fn listed(server: &Arc<Mutex<Server>>) -> anyhow::Result<Vec<RemoteTool>> {
    let mut server = server
        .lock()
        .map_err(|_| anyhow::anyhow!("the connection is unusable"))?;
    server.tools()
}
