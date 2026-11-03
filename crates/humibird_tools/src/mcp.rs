//! Tools a context server offers (MCP), as the rest of the app sees them.

use std::sync::{Arc, Mutex};

use humibird_mcp::{RemoteTool, Server};
use serde_json::Value;

use crate::{CallKind, CallLabel, Tool, ToolResult};

/// The model-facing name of a context server's tool.
///
/// A provider accepts only `[a-zA-Z0-9_-]` in a function name and not more than
/// 64 characters, while an MCP tool may be called anything. The server's name is
/// part of it so two servers cannot collide, and everything a provider would
/// refuse becomes `_`. Truncation happens at the end, where the tool name is the
/// part worth losing last.
pub fn model_name(server: &str, tool: &str) -> String {
    let mut name = String::with_capacity(server.len() + tool.len() + 5);
    name.push_str("mcp__");
    name.push_str(&sanitize(server));
    name.push_str("__");
    name.push_str(&sanitize(tool));
    name.truncate(64);
    name
}

fn sanitize(part: &str) -> String {
    part.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' || character == '-' {
                character
            } else {
                '_'
            }
        })
        .collect()
}

/// The key an "allowed for this session" is remembered under. It is not the name
/// the model sees: the server and the tool stay readable in the approval card.
pub fn approval_key(server: &str, tool: &str) -> String {
    format!("mcp:{server}:{tool}")
}

/// One tool of one running server.
pub struct McpTool {
    server: Arc<Mutex<Server>>,
    server_name: String,
    name: String,
    tool: RemoteTool,
}

impl McpTool {
    pub fn new(server: Arc<Mutex<Server>>, server_name: &str, tool: RemoteTool) -> Self {
        let name = model_name(server_name, &tool.name);
        McpTool {
            server,
            server_name: server_name.to_owned(),
            name,
            tool,
        }
    }

    /// The name the server gave the tool, which is what a call must send.
    pub fn remote_name(&self) -> &str {
        &self.tool.name
    }
}

impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        if self.tool.description.is_empty() {
            // A server that describes nothing still needs a line the model can
            // read; saying nothing makes the tool look broken.
            "A tool from a context server."
        } else {
            &self.tool.description
        }
    }

    fn parameters(&self) -> Value {
        self.tool.input_schema.clone()
    }

    fn kind(&self) -> CallKind {
        CallKind::Mcp
    }

    fn label(&self, _input: &Value) -> Option<CallLabel> {
        Some(CallLabel::new(
            "MCP",
            format!("{} · {}", self.server_name, self.tool.name),
        ))
    }

    /// A context server's tool can do whatever its server can, so every call asks
    /// first. The key is the tool, not the server: approving `search` must not
    /// approve `delete`.
    fn needs_approval(&self) -> bool {
        true
    }

    fn approval_key(&self, _input: &Value) -> Option<String> {
        Some(approval_key(&self.server_name, &self.tool.name))
    }

    fn run(&self, input: &Value) -> ToolResult {
        let mut server = self
            .server
            .lock()
            .map_err(|_| "the context server connection is unusable".to_owned())?;
        server
            .call(&self.tool.name, input)
            // The chain keeps the "mcp server: reason" form the client builds.
            .map_err(|error| format!("{error:#}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use humibird_mcp::Options;
    use std::time::Duration;

    #[test]
    fn a_name_is_what_a_provider_accepts() {
        assert_eq!(
            model_name("github", "create_issue"),
            "mcp__github__create_issue"
        );
        assert_eq!(
            model_name("my-server", "list.files"),
            "mcp__my-server__list_files"
        );
        assert_eq!(
            model_name("github.com", "issues/search"),
            "mcp__github_com__issues_search"
        );
    }

    #[test]
    fn a_long_name_is_cut_to_what_a_provider_takes() {
        let name = model_name("server", &"x".repeat(100));
        assert_eq!(name.len(), 64);
        assert!(name.starts_with("mcp__server__"));
    }

    #[test]
    fn the_approval_key_keeps_the_server_and_the_tool_readable() {
        assert_eq!(
            approval_key("github", "create_issue"),
            "mcp:github:create_issue"
        );
    }

    #[test]
    fn a_call_reaches_the_server() {
        let script = r#"
            while IFS= read -r line; do
                case "$line" in
                    *'"method":"initialize"'*)
                        printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{}}}'
                        ;;
                    *'"method":"tools/call"'*)
                        printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"note one"}]}}'
                        ;;
                esac
            done
        "#;
        let options = Options::stdio("sh")
            .args(vec!["-c".to_owned(), script.to_owned()])
            .timeout(Duration::from_secs(5));
        let server = Server::connect("notes", &options).expect("the server starts");
        let tool = McpTool::new(
            Arc::new(Mutex::new(server)),
            "notes",
            RemoteTool {
                name: "read".to_owned(),
                description: "Read a note".to_owned(),
                input_schema: serde_json::json!({"type": "object"}),
            },
        );

        assert_eq!(tool.name(), "mcp__notes__read");
        assert!(tool.needs_approval());
        assert_eq!(
            tool.run(&serde_json::json!({"path": "notes.md"}))
                .expect("the call"),
            "note one"
        );
    }
}
