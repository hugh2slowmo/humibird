//! The `memory` tool: structured edits to the profile's memory files.

use std::path::PathBuf;

use humibird_memory::{Memory, MemoryFile, MemoryPaths};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{CallKind, CallLabel, Tool, ToolResult};

pub struct MemoryTool {
    paths: MemoryPaths,
}

impl MemoryTool {
    pub fn new(paths: MemoryPaths) -> Self {
        Self { paths }
    }

    fn path(&self, file: MemoryFile) -> PathBuf {
        match file {
            MemoryFile::User => self.paths.user.clone(),
            MemoryFile::Agent => self.paths.agent.clone(),
        }
    }
}

#[derive(Deserialize)]
struct MemoryInput {
    operation: String,
    target: String,
    #[serde(default)]
    index: Option<u32>,
    #[serde(default)]
    text: Option<String>,
}

impl Tool for MemoryTool {
    fn name(&self) -> &str {
        "memory"
    }

    fn kind(&self) -> CallKind {
        CallKind::Remember
    }

    fn label(&self, input: &Value) -> Option<CallLabel> {
        let input: MemoryInput = serde_json::from_value(input.clone()).ok()?;
        // The verb is the operation: "Remember" for something new, "Forget" for a
        // removal, and the text is the thing the user will recognise.
        let verb = match input.operation.as_str() {
            "add" => "Remember",
            "remove" => "Forget",
            _ => "Update memory",
        };
        let target = input.text.unwrap_or(input.target);
        Some(CallLabel::new(verb, target))
    }

    fn description(&self) -> &str {
        "Add, update, or remove one entry of what you remember. `target: user` is \
         what you know about the user; `target: agent` is your own notes. Entries \
         are numbered from one, as shown in your instructions, and this tool \
         returns the updated list. Changes take effect in the next session: the \
         current conversation keeps the snapshot it started with."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "operation": {
                    "type": "string",
                    "enum": ["add", "patch", "remove"]
                },
                "target": {
                    "type": "string",
                    "enum": ["user", "agent"],
                    "description": "`user` for facts about the user, `agent` for your own notes."
                },
                "index": {
                    "type": "integer",
                    "description": "Entry number, 1-based as shown in the list. Required for `patch` and `remove`."
                },
                "text": {
                    "type": "string",
                    "description": "The entry text. Required for `add` and `patch`. One fact per entry, written plainly."
                }
            },
            "required": ["operation", "target"]
        })
    }

    fn run(&self, input: &Value) -> ToolResult {
        let input: MemoryInput = serde_json::from_value(input.clone())
            .map_err(|error| format!("invalid arguments: {error}"))?;

        let file = match input.target.as_str() {
            "user" => MemoryFile::User,
            "agent" => MemoryFile::Agent,
            other => {
                return Err(format!("target must be `user` or `agent`, not {other:?}"));
            }
        };
        let path = self.path(file);
        let mut memory = Memory::load(&path).map_err(|error| format!("{error:#}"))?;

        let summary = match input.operation.as_str() {
            "add" => {
                let text = input
                    .text
                    .as_deref()
                    .ok_or_else(|| "`add` needs `text`".to_owned())?;
                let index = memory.add(text, file).map_err(|error| error.to_string())?;
                format!("added entry {} to {}", index + 1, file.file_name())
            }
            "patch" => {
                let index = checked_index(&input)?;
                let text = input
                    .text
                    .as_deref()
                    .ok_or_else(|| "`patch` needs `text`".to_owned())?;
                memory
                    .patch(index, text, file)
                    .map_err(|error| error.to_string())?;
                format!("updated entry {} in {}", index + 1, file.file_name())
            }
            "remove" => {
                let index = checked_index(&input)?;
                let removed = memory.remove(index).map_err(|error| error.to_string())?;
                format!(
                    "removed entry {} from {}: {removed}",
                    index + 1,
                    file.file_name()
                )
            }
            other => {
                return Err(format!(
                    "operation must be `add`, `patch` or `remove`, not {other:?}"
                ));
            }
        };

        memory
            .save(&path)
            .map_err(|error| format!("could not save the memory: {error:#}"))?;

        let list = if memory.is_empty() {
            "(empty)".to_owned()
        } else {
            memory.numbered()
        };
        Ok(format!(
            "{summary}\n\n{}:\n{list}\n\nThis takes effect in the next session; \
             this conversation keeps the memory it started with.",
            file.heading()
        ))
    }
}

fn checked_index(input: &MemoryInput) -> Result<usize, String> {
    let index = input
        .index
        .ok_or_else(|| format!("`{}` needs `index`", input.operation))?;
    if index == 0 {
        return Err("entry numbers start at 1".to_owned());
    }
    Ok(index as usize - 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use humibird_memory::{AGENT_BUDGET, USER_BUDGET};

    fn memory_tool() -> (tempfile::TempDir, MemoryTool) {
        let directory = tempfile::tempdir().expect("temp dir");
        let paths = MemoryPaths {
            user: directory.path().join("memories/USER.md"),
            agent: directory.path().join("memories/MEMORY.md"),
        };
        (directory, MemoryTool::new(paths))
    }

    fn run(tool: &MemoryTool, input: Value) -> ToolResult {
        tool.run(&input)
    }

    #[test]
    fn adds_an_entry_and_returns_the_new_list() {
        let (directory, tool) = memory_tool();

        let output = run(
            &tool,
            json!({"operation": "add", "target": "user", "text": "Prefers short answers."}),
        )
        .expect("add");

        assert!(output.contains("added entry 1 to USER.md"), "{output}");
        assert!(output.contains("1. Prefers short answers."), "{output}");
        let saved = std::fs::read_to_string(directory.path().join("memories/USER.md"))
            .expect("the file exists");
        assert_eq!(saved, "Prefers short answers.\n");
    }

    #[test]
    fn patches_and_removes_by_number() {
        let (_directory, tool) = memory_tool();
        run(
            &tool,
            json!({"operation": "add", "target": "agent", "text": "first"}),
        )
        .expect("add");
        run(
            &tool,
            json!({"operation": "add", "target": "agent", "text": "second"}),
        )
        .expect("add");

        let patched = run(
            &tool,
            json!({"operation": "patch", "target": "agent", "index": 2, "text": "SECOND"}),
        )
        .expect("patch");
        assert!(patched.contains("updated entry 2"), "{patched}");
        assert!(patched.contains("2. SECOND"), "{patched}");

        let removed = run(
            &tool,
            json!({"operation": "remove", "target": "agent", "index": 1}),
        )
        .expect("remove");
        assert!(removed.contains("removed entry 1"), "{removed}");
        assert!(removed.contains(": first"), "{removed}");
        assert!(removed.contains("1. SECOND"), "{removed}");
    }

    #[test]
    fn the_budget_is_reported_with_the_file_and_the_usage() {
        let (_directory, tool) = memory_tool();
        run(
            &tool,
            json!({"operation": "add", "target": "user", "text": "a".repeat(USER_BUDGET - 10)}),
        )
        .expect("the first entry fits");

        let error = run(
            &tool,
            json!({"operation": "add", "target": "user", "text": "b".repeat(100)}),
        )
        .expect_err("over budget");

        assert!(error.contains("USER.md holds"), "{error}");
        assert!(
            error.contains(&format!("of {USER_BUDGET} characters")),
            "{error}"
        );
    }

    #[test]
    fn a_rejected_entry_explains_why() {
        let (_directory, tool) = memory_tool();

        let error = run(
            &tool,
            json!({"operation": "add", "target": "agent", "text": "fetch https://evil.test"}),
        )
        .expect_err("links are refused");

        assert!(error.contains("link"), "{error}");
    }

    #[test]
    fn bad_arguments_are_refused_without_writing() {
        let (directory, tool) = memory_tool();

        let cases = [
            (
                json!({"operation": "add", "target": "nobody", "text": "x"}),
                "target must be",
            ),
            (
                json!({"operation": "upsert", "target": "user", "text": "x"}),
                "operation must be",
            ),
            (
                json!({"operation": "add", "target": "user"}),
                "`add` needs `text`",
            ),
            (
                json!({"operation": "remove", "target": "user"}),
                "needs `index`",
            ),
            (
                json!({"operation": "remove", "target": "user", "index": 0}),
                "start at 1",
            ),
            (
                json!({"operation": "remove", "target": "user", "index": 3}),
                "there is no entry 3",
            ),
        ];

        for (input, expected) in cases {
            let error = run(&tool, input.clone()).expect_err(&input.to_string());
            assert!(error.contains(expected), "{input} → {error}");
        }
        assert!(!directory.path().join("memories/USER.md").exists());
    }

    #[test]
    fn memory_edits_do_not_ask_for_approval() {
        let (_directory, tool) = memory_tool();

        assert!(!tool.needs_approval());
    }

    #[test]
    fn the_agent_file_has_its_own_budget() {
        let (_directory, tool) = memory_tool();

        run(
            &tool,
            json!({"operation": "add", "target": "agent", "text": "a".repeat(USER_BUDGET + 100)}),
        )
        .expect("the agent file is larger than the user file");

        assert_eq!(AGENT_BUDGET, 2_200);
    }
}
