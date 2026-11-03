//! The tools that change the filesystem: `write_file`, `edit_file`, `delete_path`,
//! and `move_path`.

use std::path::Path;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::{CallKind, CallLabel, Tool, ToolResult, Workspace};

/// Bigger than this and the model should be running a command instead.
const MAX_WRITE_BYTES: usize = 1024 * 1024;
/// Each edit has to be a snippet a human could review.
const MAX_EDITS: usize = 50;

pub struct WriteFile {
    workspace: Workspace,
}

impl WriteFile {
    pub fn new(workspace: Workspace) -> Self {
        Self { workspace }
    }
}

#[derive(Deserialize)]
struct WriteFileInput {
    path: String,
    content: String,
}

impl Tool for WriteFile {
    fn name(&self) -> &str {
        "write_file"
    }

    fn kind(&self) -> CallKind {
        CallKind::Write
    }

    fn label(&self, input: &Value) -> Option<CallLabel> {
        let input: WriteFileInput = serde_json::from_value(input.clone()).ok()?;
        Some(CallLabel::new("Write", input.path))
    }

    fn description(&self) -> &str {
        "Create a file, or replace the whole contents of one. Prefer `edit_file` \
         for changing an existing file: rewriting it hides what changed and \
         throws away anything the model did not know about."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path relative to the workspace root."
                },
                "content": {
                    "type": "string",
                    "description": "The complete new contents of the file."
                }
            },
            "required": ["path", "content"]
        })
    }

    fn needs_approval(&self) -> bool {
        true
    }

    fn run(&self, input: &Value) -> ToolResult {
        let input: WriteFileInput = serde_json::from_value(input.clone())
            .map_err(|error| format!("invalid arguments: {error}"))?;
        if input.content.len() > MAX_WRITE_BYTES {
            return Err(format!(
                "{} is {} bytes; the limit for a write is {MAX_WRITE_BYTES}",
                input.path,
                input.content.len()
            ));
        }

        let path = self.workspace.resolve_for_write(&input.path)?;
        if path.is_dir() {
            return Err(format!("{} is a directory", input.path));
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("could not create {}: {error}", input.path))?;
        }
        write_atomically(&path, &input.content)
            .map_err(|error| format!("could not write {}: {error}", input.path))?;

        Ok(format!(
            "wrote {} bytes to {}",
            input.content.len(),
            input.path
        ))
    }
}

pub struct EditFile {
    workspace: Workspace,
}

impl EditFile {
    pub fn new(workspace: Workspace) -> Self {
        Self { workspace }
    }
}

#[derive(Deserialize)]
struct EditFileInput {
    path: String,
    edits: Vec<Edit>,
}

#[derive(Deserialize)]
struct Edit {
    old_text: String,
    new_text: String,
}

impl Tool for EditFile {
    fn name(&self) -> &str {
        "edit_file"
    }

    fn kind(&self) -> CallKind {
        CallKind::Edit
    }

    fn label(&self, input: &Value) -> Option<CallLabel> {
        let input: EditFileInput = serde_json::from_value(input.clone()).ok()?;
        Some(CallLabel::new("Edit", input.path))
    }

    fn description(&self) -> &str {
        "Replace exact snippets in an existing file. Read the file first: each \
         `old_text` has to appear exactly once, so include enough surrounding \
         lines to be unambiguous, and every edit is applied to the result of \
         the previous one. If any edit does not apply, nothing is written."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path relative to the workspace root."
                },
                "edits": {
                    "type": "array",
                    "description": "Snippet replacements, applied in order.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "old_text": {
                                "type": "string",
                                "description": "Exact text to replace; it must appear exactly once in the file."
                            },
                            "new_text": {
                                "type": "string",
                                "description": "Replacement text. Use an empty string to delete."
                            }
                        },
                        "required": ["old_text", "new_text"]
                    }
                }
            },
            "required": ["path", "edits"]
        })
    }

    fn needs_approval(&self) -> bool {
        true
    }

    fn run(&self, input: &Value) -> ToolResult {
        let input: EditFileInput = serde_json::from_value(input.clone())
            .map_err(|error| format!("invalid arguments: {error}"))?;
        if input.edits.is_empty() {
            return Err("there are no edits to apply".to_owned());
        }
        if input.edits.len() > MAX_EDITS {
            return Err(format!(
                "{} edits is more than the {MAX_EDITS} allowed in one call",
                input.edits.len()
            ));
        }

        let path = self.workspace.resolve(&input.path)?;
        if path.is_dir() {
            return Err(format!("{} is a directory", input.path));
        }
        let contents =
            std::fs::read_to_string(&path).map_err(|error| format!("{}: {error}", input.path))?;

        let mut updated = contents;
        for (index, edit) in input.edits.iter().enumerate() {
            let position = index + 1;
            if edit.old_text.is_empty() {
                return Err(format!("edit {position}: old_text is empty"));
            }

            let matches = updated.matches(&edit.old_text).count();
            match matches {
                1 => {
                    updated = updated.replacen(&edit.old_text, &edit.new_text, 1);
                }
                0 => {
                    return Err(format!(
                        "edit {position}: {:?} was not found in {}",
                        snippet(&edit.old_text),
                        input.path
                    ));
                }
                matches => {
                    return Err(format!(
                        "edit {position}: {:?} appears {matches} times in {}; include more surrounding text",
                        snippet(&edit.old_text),
                        input.path
                    ));
                }
            }
        }

        write_atomically(&path, &updated)
            .map_err(|error| format!("could not write {}: {error}", input.path))?;

        Ok(format!(
            "applied {} edits to {}",
            input.edits.len(),
            input.path
        ))
    }
}

fn snippet(text: &str) -> String {
    let first_line = text.lines().next().unwrap_or_default().trim();
    let shortened: String = first_line.chars().take(60).collect();
    if shortened.len() < first_line.len() {
        format!("{shortened}...")
    } else {
        shortened
    }
}

/// Removes a file, or a directory and everything in it when told to.
pub struct DeletePath {
    workspace: Workspace,
}

impl DeletePath {
    pub fn new(workspace: Workspace) -> Self {
        Self { workspace }
    }
}

#[derive(Deserialize)]
struct DeletePathInput {
    path: String,
    #[serde(default)]
    recursive: Option<bool>,
}

impl Tool for DeletePath {
    fn name(&self) -> &str {
        "delete_path"
    }

    fn kind(&self) -> CallKind {
        CallKind::Delete
    }

    fn label(&self, input: &Value) -> Option<CallLabel> {
        let input: DeletePathInput = serde_json::from_value(input.clone()).ok()?;
        Some(CallLabel::new("Delete", input.path))
    }

    fn description(&self) -> &str {
        "Delete a file, or a directory. A directory that is not empty needs \
         `recursive: true`, which deletes everything inside it. There is no undo."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file or directory, relative to the workspace root."
                },
                "recursive": {
                    "type": "boolean",
                    "description": "Delete a non-empty directory and its contents. Defaults to false."
                }
            },
            "required": ["path"]
        })
    }

    fn needs_approval(&self) -> bool {
        true
    }

    fn run(&self, input: &Value) -> ToolResult {
        let input: DeletePathInput = serde_json::from_value(input.clone())
            .map_err(|error| format!("invalid arguments: {error}"))?;
        let path = self.workspace.resolve(&input.path)?;

        if !path.is_dir() {
            std::fs::remove_file(&path)
                .map_err(|error| format!("could not delete {}: {error}", input.path))?;
            return Ok(format!("deleted {}", input.path));
        }

        // The workspace itself is the session's ground; a tool call cannot remove
        // it out from under everything else.
        if path == self.workspace.root() {
            return Err("the workspace root cannot be deleted".to_owned());
        }

        let entries = std::fs::read_dir(&path)
            .map_err(|error| format!("could not read {}: {error}", input.path))?
            .count();
        if entries > 0 && !input.recursive.unwrap_or(false) {
            return Err(format!(
                "{} is a directory with {entries} entries; pass recursive: true to delete it \
                 and everything inside",
                input.path
            ));
        }

        std::fs::remove_dir_all(&path)
            .map_err(|error| format!("could not delete {}: {error}", input.path))?;
        Ok(format!("deleted {} and its {entries} entries", input.path))
    }
}

/// Moves or renames a file or directory inside the workspace.
pub struct MovePath {
    workspace: Workspace,
}

impl MovePath {
    pub fn new(workspace: Workspace) -> Self {
        Self { workspace }
    }
}

#[derive(Deserialize)]
struct MovePathInput {
    source: String,
    destination: String,
}

impl Tool for MovePath {
    fn name(&self) -> &str {
        "move_path"
    }

    fn kind(&self) -> CallKind {
        CallKind::Move
    }

    fn label(&self, input: &Value) -> Option<CallLabel> {
        let input: MovePathInput = serde_json::from_value(input.clone()).ok()?;
        Some(CallLabel::new(
            "Move",
            format!("{} → {}", input.source, input.destination),
        ))
    }

    fn description(&self) -> &str {
        "Move or rename a file or directory inside the workspace. The destination \
         must not exist yet; its parent directories are created. To replace a file, \
         delete it first."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "source": {
                    "type": "string",
                    "description": "Path to move, relative to the workspace root."
                },
                "destination": {
                    "type": "string",
                    "description": "Where it goes, relative to the workspace root."
                }
            },
            "required": ["source", "destination"]
        })
    }

    fn needs_approval(&self) -> bool {
        true
    }

    fn run(&self, input: &Value) -> ToolResult {
        let input: MovePathInput = serde_json::from_value(input.clone())
            .map_err(|error| format!("invalid arguments: {error}"))?;
        let source = self.workspace.resolve(&input.source)?;
        let destination = self.workspace.resolve_for_write(&input.destination)?;

        if source == destination {
            return Err(format!("{} is already there", input.source));
        }
        if destination.exists() {
            return Err(format!(
                "{} already exists; delete it first if it should be replaced",
                input.destination
            ));
        }
        if source.is_dir() && destination.starts_with(&source) {
            return Err(format!("{} cannot be moved inside itself", input.source));
        }
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("could not create {}: {error}", input.destination))?;
        }

        std::fs::rename(&source, &destination).map_err(|error| {
            format!(
                "could not move {} to {}: {error}",
                input.source, input.destination
            )
        })?;
        Ok(format!("moved {} to {}", input.source, input.destination))
    }
}

/// Writing in place truncates first, so an interrupted write leaves an empty
/// file. Writing beside it and renaming swaps the whole file or nothing.
fn write_atomically(path: &Path, contents: &str) -> std::io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("the path has no parent directory"))?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    let temporary = parent.join(format!(".{name}.humibird.tmp"));

    std::fs::write(&temporary, contents)?;
    std::fs::rename(&temporary, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temporary);
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace_with(files: &[(&str, &str)]) -> (tempfile::TempDir, Workspace) {
        let directory = tempfile::tempdir().expect("temp dir");
        for (path, contents) in files {
            let path = directory.path().join(path);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("create parent");
            }
            std::fs::write(path, contents).expect("write file");
        }
        let workspace = Workspace::new(directory.path()).expect("workspace");
        (directory, workspace)
    }

    fn run(tool: &dyn Tool, input: Value) -> ToolResult {
        tool.run(&input)
    }

    #[test]
    fn write_file_creates_the_file_and_its_directories() {
        let (directory, workspace) = workspace_with(&[]);

        run(
            &WriteFile::new(workspace),
            json!({"path": "src/new.rs", "content": "fn main() {}"}),
        )
        .expect("write");

        let written = std::fs::read_to_string(directory.path().join("src/new.rs")).expect("read");
        assert_eq!(written, "fn main() {}");
    }

    #[test]
    fn write_file_replaces_the_whole_file() {
        let (directory, workspace) = workspace_with(&[("notes.txt", "old contents\n")]);

        run(
            &WriteFile::new(workspace),
            json!({"path": "notes.txt", "content": "new contents\n"}),
        )
        .expect("write");

        assert_eq!(
            std::fs::read_to_string(directory.path().join("notes.txt")).expect("read"),
            "new contents\n"
        );
    }

    #[test]
    fn write_file_refuses_to_escape_the_workspace() {
        let (directory, workspace) = workspace_with(&[]);
        let outside = directory.path().join("..").join("escaped.txt");

        let error = run(
            &WriteFile::new(workspace),
            json!({"path": "../escaped.txt", "content": "no"}),
        )
        .expect_err("escape");

        assert!(error.contains("workspace"), "{error}");
        assert!(!outside.exists(), "nothing may be written outside");
    }

    #[test]
    fn write_file_refuses_a_directory() {
        let (_directory, workspace) = workspace_with(&[("src/main.rs", "")]);

        let error = run(
            &WriteFile::new(workspace),
            json!({"path": "src", "content": "no"}),
        )
        .expect_err("directory");

        assert!(error.contains("directory"), "{error}");
    }

    #[test]
    fn edit_file_replaces_one_snippet() {
        let (directory, workspace) =
            workspace_with(&[("src/main.rs", "fn main() {\n    println!(\"one\");\n}\n")]);

        run(
            &EditFile::new(workspace),
            json!({
                "path": "src/main.rs",
                "edits": [{"old_text": "println!(\"one\");", "new_text": "println!(\"two\");"}]
            }),
        )
        .expect("edit");

        let edited = std::fs::read_to_string(directory.path().join("src/main.rs")).expect("read");
        assert!(edited.contains("println!(\"two\");"), "{edited}");
    }

    #[test]
    fn edit_file_applies_edits_in_order() {
        let (directory, workspace) = workspace_with(&[("a.txt", "alpha\nbeta\n")]);

        run(
            &EditFile::new(workspace),
            json!({
                "path": "a.txt",
                "edits": [
                    {"old_text": "alpha", "new_text": "ALPHA"},
                    {"old_text": "beta", "new_text": "BETA"}
                ]
            }),
        )
        .expect("edit");

        assert_eq!(
            std::fs::read_to_string(directory.path().join("a.txt")).expect("read"),
            "ALPHA\nBETA\n"
        );
    }

    #[test]
    fn edit_file_refuses_an_ambiguous_snippet() {
        let (directory, workspace) = workspace_with(&[("a.txt", "let x = 1;\nlet x = 1;\n")]);

        let error = run(
            &EditFile::new(workspace),
            json!({
                "path": "a.txt",
                "edits": [{"old_text": "let x = 1;", "new_text": "let x = 2;"}]
            }),
        )
        .expect_err("ambiguous");

        assert!(error.contains("appears 2 times"), "{error}");
        assert_eq!(
            std::fs::read_to_string(directory.path().join("a.txt")).expect("read"),
            "let x = 1;\nlet x = 1;\n",
            "nothing is written when an edit fails"
        );
    }

    #[test]
    fn edit_file_refuses_a_missing_snippet() {
        let (_directory, workspace) = workspace_with(&[("a.txt", "hello\n")]);

        let error = run(
            &EditFile::new(workspace),
            json!({
                "path": "a.txt",
                "edits": [{"old_text": "goodbye", "new_text": "hi"}]
            }),
        )
        .expect_err("missing");

        assert!(error.contains("was not found"), "{error}");
    }

    #[test]
    fn write_tools_ask_for_approval() {
        let (_directory, workspace) = workspace_with(&[]);

        for tool in crate::write_tools(workspace.clone()) {
            assert!(tool.needs_approval(), "{}", tool.name());
        }
        for tool in crate::read_only_tools(workspace) {
            assert!(!tool.needs_approval(), "{}", tool.name());
        }
    }

    #[test]
    fn delete_path_removes_a_file() {
        let (directory, workspace) = workspace_with(&[("a.txt", "hi\n"), ("b.txt", "keep\n")]);

        let output = run(&DeletePath::new(workspace), json!({"path": "a.txt"})).expect("delete");

        assert_eq!(output, "deleted a.txt");
        assert!(!directory.path().join("a.txt").exists());
        assert!(
            directory.path().join("b.txt").is_file(),
            "only that file goes"
        );
    }

    #[test]
    fn delete_path_refuses_a_full_directory_until_told_to_recurse() {
        let (directory, workspace) = workspace_with(&[("old/a.txt", "\n"), ("old/b.txt", "\n")]);

        let error = run(&DeletePath::new(workspace), json!({"path": "old"}))
            .expect_err("non-empty directory");

        assert!(error.contains("recursive: true"), "{error}");
        assert!(
            directory.path().join("old/a.txt").is_file(),
            "nothing was deleted"
        );
    }

    #[test]
    fn delete_path_removes_a_directory_when_told_to() {
        let (directory, workspace) =
            workspace_with(&[("old/a.txt", "\n"), ("old/deep/b.txt", "\n")]);

        let output = run(
            &DeletePath::new(workspace),
            json!({"path": "old", "recursive": true}),
        )
        .expect("delete");

        assert!(output.contains("old"), "{output}");
        assert!(!directory.path().join("old").exists());
    }

    #[test]
    fn delete_path_refuses_the_workspace_itself() {
        let (directory, workspace) = workspace_with(&[("a.txt", "hi")]);

        let error = run(
            &DeletePath::new(workspace),
            json!({"path": ".", "recursive": true}),
        )
        .expect_err("the root");

        assert!(error.contains("workspace root"), "{error}");
        assert!(directory.path().join("a.txt").is_file());
    }

    #[test]
    fn move_path_renames_a_file_and_creates_its_directory() {
        let (directory, workspace) = workspace_with(&[("a.txt", "hi\n")]);

        let output = run(
            &MovePath::new(workspace),
            json!({"source": "a.txt", "destination": "src/deep/b.txt"}),
        )
        .expect("move");

        assert_eq!(output, "moved a.txt to src/deep/b.txt");
        assert!(!directory.path().join("a.txt").exists());
        assert_eq!(
            std::fs::read_to_string(directory.path().join("src/deep/b.txt")).expect("read"),
            "hi\n"
        );
    }

    #[test]
    fn move_path_will_not_replace_what_is_already_there() {
        let (directory, workspace) = workspace_with(&[("a.txt", "one\n"), ("b.txt", "two\n")]);

        let error = run(
            &MovePath::new(workspace),
            json!({"source": "a.txt", "destination": "b.txt"}),
        )
        .expect_err("destination exists");

        assert!(error.contains("already exists"), "{error}");
        assert_eq!(
            std::fs::read_to_string(directory.path().join("b.txt")).expect("read"),
            "two\n",
            "the destination is untouched"
        );
    }

    #[test]
    fn move_path_will_not_move_a_directory_inside_itself() {
        let (_directory, workspace) = workspace_with(&[("old/a.txt", "hi\n")]);

        let error = run(
            &MovePath::new(workspace),
            json!({"source": "old", "destination": "old/deep/old"}),
        )
        .expect_err("into itself");

        assert!(error.contains("inside itself"), "{error}");
    }
}
