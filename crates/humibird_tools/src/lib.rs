//! The tools the model can call.
//!
//! Tools are described by a JSON Schema and return either text for the model or
//! a message explaining why the call failed; a failure is a tool result, not a
//! crashed turn. Every path is resolved inside one workspace, so a tool cannot
//! be talked into reading `/etc/passwd`.

mod ask_user;
mod delegate;
mod fetch;
mod mcp;
mod memory;
mod read_only;
mod search;
mod session_search;
mod skill;
mod terminal;
mod write;

pub use ask_user::{AskUser, DECLINED_TEXT, Question, answer_text};
pub use delegate::{CANCELLED_TEXT as DELEGATION_CANCELLED, Delegate, Delegation};
pub use fetch::Fetch;
pub use humibird_memory::MemoryPaths;
pub use humibird_skills::SkillIndex;
pub use mcp::{McpTool, approval_key, model_name};
pub use memory::MemoryTool;
pub use read_only::{FindPath, Grep, ListDirectory, ReadFile, workspace_files};
pub use search::Search;
pub use session_search::SessionSearch;
pub use skill::ReadSkill;
pub use terminal::Terminal;
pub use write::{DeletePath, EditFile, MovePath, WriteFile};

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use serde_json::Value;

/// What a tool reports when the call cannot be completed. The text goes back to
/// the model verbatim, so it has to explain what to do instead.
pub type ToolError = String;
pub type ToolResult = Result<String, ToolError>;

/// What a call is doing, so the window can pick an icon and decide whether the
/// call deserves a card. The tools crate stays free of UI types.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallKind {
    Read,
    List,
    Search,
    Write,
    Edit,
    Delete,
    Move,
    Execute,
    Fetch,
    Remember,
    Skill,
    Ask,
    Delegate,
    /// A tool a context server offers (MCP). It is not one of ours and it can
    /// reach anything the server can, so it reads as a change and asks first.
    Mcp,
}

/// How a call reads in the window: a verb and the one thing it acted on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallLabel {
    pub verb: &'static str,
    pub target: Option<String>,
}

impl CallLabel {
    /// The target is collapsed to a single line: a label is one line of the
    /// timeline, and commands and questions arrive with newlines in them.
    pub fn new(verb: &'static str, target: impl AsRef<str>) -> Self {
        let target = target
            .as_ref()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        Self {
            verb,
            target: (!target.is_empty()).then_some(target),
        }
    }
}

pub trait Tool: Send + Sync {
    /// The name the model calls. Built-in tools answer with a literal; a tool that
    /// comes from a context server answers with the name that server gave it.
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    /// JSON Schema for the arguments.
    fn parameters(&self) -> Value;
    /// What the call is doing. The window uses it for the icon and the card.
    fn kind(&self) -> CallKind {
        CallKind::Read
    }
    /// How the call reads — a verb and what it acted on. `None` (or arguments
    /// that do not parse) falls back to the name and the argument summary.
    fn label(&self, _input: &Value) -> Option<CallLabel> {
        None
    }
    /// The key an "always allow" is remembered under. `None` remembers the tool
    /// name, which is right for a tool whose permission does not depend on its
    /// arguments; `fetch` answers one key per host. The turn asks for the key on
    /// the thread that draws the window, so computing it must not block.
    fn approval_key(&self, _input: &Value) -> Option<String> {
        None
    }
    /// Whether the user has to agree before the call runs. Anything that
    /// changes the filesystem says yes.
    fn needs_approval(&self) -> bool {
        false
    }
    /// Whether the user has to answer the call. The loop collects the answer and
    /// never calls `run`.
    fn asks_user(&self) -> bool {
        false
    }
    /// Whether the call runs a subagent. The loop drives the second conversation
    /// and never calls `run`.
    fn delegates(&self) -> bool {
        false
    }
    fn run(&self, input: &Value) -> ToolResult;
}

/// The directory a conversation is allowed to touch.
#[derive(Clone, Debug)]
pub struct Workspace {
    root: PathBuf,
}

impl Workspace {
    /// Canonicalizes the root once, so later checks compare like with like.
    pub fn new(root: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let root = std::fs::canonicalize(root.into())?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolves a model-provided path. Absolute paths, `..`, and symlinks that
    /// lead outside the workspace are refused rather than clamped, because a
    /// silently rewritten path is how an agent edits the wrong file.
    pub fn resolve(&self, path: &str) -> Result<PathBuf, ToolError> {
        let relative = Path::new(path);
        if relative.is_absolute() {
            return Err(format!(
                "{path} is absolute; paths are relative to the workspace root"
            ));
        }
        if relative
            .components()
            .any(|component| matches!(component, Component::ParentDir))
        {
            return Err(format!("{path} leaves the workspace; `..` is not allowed"));
        }

        let candidate = self.root.join(relative);
        let resolved = std::fs::canonicalize(&candidate).map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => format!("{path} does not exist"),
            _ => format!("{path} cannot be read: {error}"),
        })?;

        if !resolved.starts_with(&self.root) {
            return Err(format!("{path} resolves outside the workspace"));
        }
        Ok(resolved)
    }

    /// The path as the model should see it: relative to the root.
    pub fn display(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .map(|relative| {
                if relative.as_os_str().is_empty() {
                    ".".to_owned()
                } else {
                    relative.display().to_string()
                }
            })
            .unwrap_or_else(|_| path.display().to_string())
    }

    /// Resolves a path that may not exist yet, for tools that create files.
    /// The longest existing prefix is canonicalized and checked, so a symlinked
    /// directory cannot be used to write outside the workspace.
    pub fn resolve_for_write(&self, path: &str) -> Result<PathBuf, ToolError> {
        let relative = Path::new(path);
        if relative.is_absolute() {
            return Err(format!(
                "{path} is absolute; paths are relative to the workspace root"
            ));
        }

        let mut existing = self.root.clone();
        let mut missing = Vec::new();
        for component in relative.components() {
            let Component::Normal(name) = component else {
                return Err(format!("{path} is not a plain path inside the workspace"));
            };
            if !missing.is_empty() {
                missing.push(name.to_owned());
                continue;
            }
            let candidate = existing.join(name);
            if candidate.exists() {
                existing = candidate;
            } else {
                missing.push(name.to_owned());
            }
        }

        if missing.is_empty() && relative.as_os_str().is_empty() {
            return Err(format!("{path} is not a file path"));
        }

        let resolved = std::fs::canonicalize(&existing)
            .map_err(|error| format!("{path} cannot be written: {error}"))?;
        if !resolved.starts_with(&self.root) {
            return Err(format!("{path} resolves outside the workspace"));
        }
        Ok(missing
            .into_iter()
            .fold(resolved, |path, name| path.join(name)))
    }
}

/// The read-only tools offered to every conversation.
pub fn read_only_tools(workspace: Workspace) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(ReadFile::new(workspace.clone())),
        Arc::new(ListDirectory::new(workspace.clone())),
        Arc::new(FindPath::new(workspace.clone())),
        Arc::new(Grep::new(workspace)),
    ]
}

/// The tools that change things or run code. They ask for approval first.
pub fn write_tools(workspace: Workspace) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(WriteFile::new(workspace.clone())),
        Arc::new(EditFile::new(workspace.clone())),
        Arc::new(DeletePath::new(workspace.clone())),
        Arc::new(MovePath::new(workspace.clone())),
        Arc::new(Terminal::new(workspace)),
    ]
}

/// The tool that reads a URL. It reaches the network rather than the workspace,
/// so a profile that refuses a workspace still gets it; it asks once per host,
/// because the host is what the person is agreeing to.
pub fn fetch_tools() -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(Fetch::new(fetch::client()))]
}

/// DeepSeek's Search API, as one tool. Only a conversation with a DeepSeek key is
/// offered it: the key is the account the searches are billed to.
pub fn search_tools(api_key: impl Into<String>) -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(Search::new(api_key))]
}

/// The tools that edit the profile's memory. They are not workspace-scoped:
/// the files belong to the profile, not to the directory being worked in.
pub fn memory_tools(memories: MemoryPaths) -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(MemoryTool::new(memories))]
}

/// The tool that reads a skill's procedure. Like memory, skills live in the
/// profile, so the workspace tools cannot reach them. A profile with
/// no skills is offered nothing: every tool definition is sent on every
/// request, so a tool that can only fail is not worth its tokens.
pub fn skill_tools(index: std::sync::Arc<SkillIndex>) -> Vec<Arc<dyn Tool>> {
    if index.is_empty() {
        return Vec::new();
    }
    vec![Arc::new(ReadSkill::new(index))]
}

/// The tool that asks the user a question. It belongs to no workspace and no
/// profile: the answer comes from whoever is looking at the window.
pub fn ask_tools() -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(AskUser)]
}

/// The tool that hands one task to a subagent. Only the person's own conversation
/// is offered it: a subagent cannot delegate again.
pub fn delegate_tools() -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(Delegate)]
}

/// Everything a conversation is offered.
pub fn tools(workspace: Workspace) -> Vec<Arc<dyn Tool>> {
    let mut tools = read_only_tools(workspace.clone());
    tools.extend(write_tools(workspace));
    tools
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_refuses_paths_that_leave_the_workspace() {
        let directory = tempfile::tempdir().expect("temp dir");
        std::fs::write(directory.path().join("file.txt"), "hi").expect("write file");
        let workspace = Workspace::new(directory.path()).expect("workspace");

        assert!(workspace.resolve("file.txt").is_ok());
        assert!(workspace.resolve("/etc/passwd").is_err());
        assert!(workspace.resolve("../file.txt").is_err());
        assert!(workspace.resolve("nested/../../file.txt").is_err());
    }

    #[test]
    fn resolve_reports_a_missing_file_plainly() {
        let directory = tempfile::tempdir().expect("temp dir");
        let workspace = Workspace::new(directory.path()).expect("workspace");

        let error = workspace.resolve("nope.txt").expect_err("missing");

        assert!(error.contains("does not exist"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn resolve_refuses_a_symlink_that_escapes() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().expect("temp dir");
        let outside = tempfile::tempdir().expect("outside dir");
        std::fs::write(outside.path().join("secret.txt"), "secret").expect("write secret");
        symlink(
            outside.path().join("secret.txt"),
            directory.path().join("link.txt"),
        )
        .expect("symlink");
        let workspace = Workspace::new(directory.path()).expect("workspace");

        let error = workspace.resolve("link.txt").expect_err("escape");

        assert!(error.contains("outside the workspace"), "{error}");
    }

    #[test]
    fn the_workspace_tools_do_not_include_memory() {
        let directory = tempfile::tempdir().expect("temp dir");
        let workspace = Workspace::new(directory.path()).expect("workspace");

        let installed = tools(workspace);
        let names: Vec<_> = installed.iter().map(|tool| tool.name()).collect();

        assert_eq!(names.len(), 9, "{names:?}");
        assert!(!names.contains(&"memory"), "{names:?}");
    }

    #[test]
    fn the_memory_tool_is_offered_alone_and_needs_no_approval() {
        let directory = tempfile::tempdir().expect("temp dir");
        let memories = MemoryPaths::in_profile(directory.path());

        let tools = memory_tools(memories.clone());

        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name(), "memory");
        assert!(!tools[0].needs_approval());
        assert_eq!(
            tools[0].parameters()["properties"]["operation"]["enum"],
            serde_json::json!(["add", "patch", "remove"])
        );
    }

    #[test]
    fn every_tool_says_what_it_is_doing() {
        let directory = tempfile::tempdir().expect("temp dir");
        std::fs::write(directory.path().join("a.txt"), "hi\n").expect("write a file");
        let workspace = Workspace::new(directory.path()).expect("workspace");
        let mut tools = tools(workspace);
        tools.extend(fetch_tools());
        tools.extend(memory_tools(MemoryPaths::in_profile(directory.path())));
        tools.push(Arc::new(AskUser));
        tools.push(Arc::new(ReadSkill::new(std::sync::Arc::new(
            SkillIndex::default(),
        ))));

        let cases = [
            (
                "read_file",
                serde_json::json!({"path": "a.txt"}),
                CallKind::Read,
                "Read",
                "a.txt",
            ),
            (
                "list_directory",
                serde_json::json!({}),
                CallKind::List,
                "List",
                ".",
            ),
            (
                "find_path",
                serde_json::json!({"glob": "**/*.rs"}),
                CallKind::Search,
                "Find",
                "**/*.rs",
            ),
            (
                "grep",
                serde_json::json!({"regex": "hi"}),
                CallKind::Search,
                "Search",
                "hi",
            ),
            (
                "write_file",
                serde_json::json!({"path": "b.txt", "content": "x"}),
                CallKind::Write,
                "Write",
                "b.txt",
            ),
            (
                "edit_file",
                serde_json::json!({"path": "a.txt", "edits": []}),
                CallKind::Edit,
                "Edit",
                "a.txt",
            ),
            (
                "delete_path",
                serde_json::json!({"path": "a.txt"}),
                CallKind::Delete,
                "Delete",
                "a.txt",
            ),
            (
                "move_path",
                serde_json::json!({"source": "a.txt", "destination": "b.txt"}),
                CallKind::Move,
                "Move",
                "a.txt → b.txt",
            ),
            (
                "terminal",
                serde_json::json!({"command": "ls\n-la"}),
                CallKind::Execute,
                "Run",
                "ls -la",
            ),
            (
                "fetch",
                serde_json::json!({"url": "https://example.com/docs"}),
                CallKind::Fetch,
                "Fetch",
                "https://example.com/docs",
            ),
            (
                "memory",
                serde_json::json!({"operation": "add", "target": "user", "text": "likes tables"}),
                CallKind::Remember,
                "Remember",
                "likes tables",
            ),
            (
                "ask_user",
                serde_json::json!({"question": "which?", "allow_free_text": true}),
                CallKind::Ask,
                "Ask",
                "which?",
            ),
            (
                "read_skill",
                serde_json::json!({"name": "card"}),
                CallKind::Skill,
                "Read skill",
                "card",
            ),
        ];

        for (name, arguments, kind, verb, target) in cases {
            let tool = tools.iter().find(|tool| tool.name() == name).expect(name);
            assert_eq!(tool.kind(), kind, "{name}");
            let label = tool
                .label(&arguments)
                .unwrap_or_else(|| panic!("{name} has no label"));
            assert_eq!(label.verb, verb, "{name}");
            assert_eq!(label.target.as_deref(), Some(target), "{name}");
        }
    }

    #[test]
    fn a_label_is_one_line_and_unparseable_arguments_have_none() {
        let directory = tempfile::tempdir().expect("temp dir");
        let workspace = Workspace::new(directory.path()).expect("workspace");
        let terminal = Terminal::new(workspace);

        let label = terminal
            .label(&serde_json::json!({"command": "  python3 -V\n  && ls   -la "}))
            .expect("a label");
        assert_eq!(label.target.as_deref(), Some("python3 -V && ls -la"));
        assert!(terminal.label(&serde_json::json!({"cd": "sub"})).is_none());
    }

    #[test]
    fn the_question_tool_is_offered_alone_and_needs_no_approval() {
        let tools = ask_tools();

        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name(), "ask_user");
        assert!(tools[0].asks_user());
        assert!(!tools[0].needs_approval());
    }

    #[test]
    fn the_skill_tool_is_offered_only_when_a_skill_is_installed() {
        let directory = tempfile::tempdir().expect("temp dir");
        let skills = directory.path().join("skills");

        let empty = std::sync::Arc::new(SkillIndex::load(&skills));
        assert!(skill_tools(empty).is_empty());

        let skill = skills.join("card");
        std::fs::create_dir_all(&skill).expect("create the skill");
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: card\ndescription: makes a card\n---\n",
        )
        .expect("write SKILL.md");

        let installed = std::sync::Arc::new(SkillIndex::load(&skills));
        let tools = skill_tools(installed);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name(), "read_skill");
        assert!(!tools[0].needs_approval());
    }
}
