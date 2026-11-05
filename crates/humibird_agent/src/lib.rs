//! The conversation runtime.
//!
//! This crate owns a conversation — the provider, the tools, the transcript, the
//! frozen prompt — and drives the turn loop. It does not depend on GPUI and takes
//! no `Window`: the window renders a projection of what happens here and sends
//! intents back.
//!
//! The session owns its *inputs* here; the turn loop itself is in [`turn`].

use std::path::Path;
use std::sync::Arc;

use humibird_memory::{Memory, MemoryPaths};
use humibird_profile::Profile;
use humibird_prompt::PromptInputs;
use humibird_providers::{Provider, Thinking, ToolDefinition};
use humibird_skills::SkillIndex;
use humibird_store::{Compaction, Store};
use humibird_tools::Tool;

mod history;
mod mcp;
mod turn;

pub use history::{AUTO_COMPACT_AT, COMPACTION_PROMPT, CompactionRun, THREAD_TITLE_PROMPT};
pub use mcp::McpServers;
pub use turn::{
    AgentEvent, Background, MAX_TOOL_ROUNDS, PROVIDER_IDLE_TIMEOUT, RecordedCall, TurnHost,
    TurnOutcome, TurnRun, summarize_arguments,
};

/// One tool call the model made during a turn, as it appears to the window:
/// what was asked, and what came back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolStep {
    pub name: String,
    pub summary: String,
    /// The raw arguments, so the window can show them on demand.
    pub arguments: String,
    /// What the call returned, or the error text. Empty while it runs.
    pub result: String,
    pub state: StepState,
}

/// Where a call is in its life. `Awaiting` is the only state that needs a person:
/// an approval, or an answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StepState {
    Awaiting,
    Running,
    Done { failed: bool },
}

/// What the user did with an approval prompt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Once,
    Always,
    Deny,
}

/// What the user did with a question: picked or typed an answer, or skipped it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answer {
    Selected(String),
    Declined,
}

/// One conversation and everything it was handed when it started.
///
/// The fields are the inputs the model sees, not the window's: the skill index,
/// the memory paths, and the prompt assembled from them are read once and kept,
/// because changing any of them mid-session would change the cached prefix.
/// The tool list is the exception: the prompt does not name
/// tools, so a context server that connects mid-session can add its tools
/// (`use_mcp_servers`) without touching the prompt.
///
/// It also holds the transcript, the summary that stands in for its older turns,
/// and the saved row both are written to, so a request is assembled here and not
/// in the window.
pub struct Conversation {
    profile: Option<Profile>,
    workspace: String,
    model: String,
    /// What the window asked about thinking. `None` leaves the model's default
    /// alone; changing it touches no part of the prompt, so it never costs a
    /// cached prefix.
    thinking: Option<Thinking>,
    memories: Option<MemoryPaths>,
    skills: Arc<SkillIndex>,
    /// The tools this session was built with, kept so the list can be rebuilt
    /// when context servers add theirs.
    builtin: Arc<Vec<Arc<dyn Tool>>>,
    /// DeepSeek's search tool, when the profile has a key for it. Outside `builtin`
    /// because it appears and disappears with the key.
    search: Option<Arc<dyn Tool>>,
    /// Every tool the request carries: the built-in ones, plus the search tool when
    /// it applies, plus any a context server offered.
    tools: Arc<Vec<Arc<dyn Tool>>>,
    definitions: Vec<ToolDefinition>,
    /// The context servers this session connected to, kept alive for as long as
    /// the conversation is.
    mcp: Option<Arc<McpServers>>,
    system_prompt: Option<String>,
    /// The provider the window resolved. `None` when the profile has none, which
    /// the window reports before a turn starts.
    provider: Option<Arc<dyn Provider>>,
    store: Option<Store>,
    /// The saved row this conversation writes to. The window creates it on the
    /// first send, so an empty thread never appears in the sidebar.
    session: Option<String>,
    /// What the model sees, in order, with the turn each message belongs to: a
    /// summary stands in for whole turns, and a compaction records its boundary as
    /// the `seq` of the turn's first message.
    transcript: Vec<Entry>,
    compaction: Option<Compaction>,
    /// What the provider last reported as the size of the conversation, in tokens.
    /// Auto-compaction measures itself against this.
    context_tokens: u32,
    /// A window supplied from outside (the provider's listing, or the profile),
    /// which wins over what the adapter catalogues: the listing can know a model
    /// the preset does not, and the profile can know better than both.
    explicit_window: Option<u32>,
    /// Whether the last turn was refused for being too long. A reported size is
    /// the cheap signal, but it only exists for models the catalogue describes
    /// and only after a reply; a refusal is what a thread on any other model has.
    too_long: bool,
    /// Whether this conversation is a thread a person reads. A subagent's session is
    /// written the same way but stays unnamed: its parent owns it.
    names_thread: bool,
}

/// One message the model can see, and where it sits in the conversation.
struct Entry {
    message: humibird_providers::Message,
    /// Which turn this message belongs to, counted from zero.
    turn: usize,
    /// The `seq` of the turn's user message, when it reached the store.
    turn_seq: Option<i64>,
}

/// Which tools a conversation is offered. The person's own conversation gets
/// everything; a subagent gets the workspace and the skills, but nothing that
/// reaches the person (a question, another delegation) and nothing that writes
/// memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scope {
    Root,
    Subagent,
}

impl Conversation {
    pub fn new(
        profile: Option<Profile>,
        workspace: impl Into<String>,
        model: impl Into<String>,
        provider: Option<Arc<dyn Provider>>,
        store: Option<Store>,
    ) -> Self {
        Self::with(Scope::Root, profile, workspace, model, provider, store)
    }

    /// A conversation for a subagent: the same inputs and a narrower toolset. Its
    /// transcript is written to a session of its own, owned by the session that
    /// delegated it, and it never names that session: a subagent is not a thread.
    pub fn subagent(
        profile: Option<Profile>,
        workspace: impl Into<String>,
        model: impl Into<String>,
        provider: Option<Arc<dyn Provider>>,
        store: Option<Store>,
    ) -> Self {
        Self::with(Scope::Subagent, profile, workspace, model, provider, store)
    }

    fn with(
        scope: Scope,
        profile: Option<Profile>,
        workspace: impl Into<String>,
        model: impl Into<String>,
        provider: Option<Arc<dyn Provider>>,
        store: Option<Store>,
    ) -> Self {
        let workspace = workspace.into();
        let model = model.into();
        let memories = profile
            .as_ref()
            .map(|profile| MemoryPaths::in_profile(profile.root()));
        let skills = Arc::new(
            profile
                .as_ref()
                .map(|profile| SkillIndex::load(&profile.skills_dir()))
                .unwrap_or_default(),
        );
        let (tools, definitions) = install_tools(
            scope,
            &workspace,
            memories.clone(),
            profile.as_ref(),
            Some(skills.clone()),
        );

        let mut conversation = Self {
            profile,
            workspace,
            model,
            thinking: None,
            memories,
            skills,
            builtin: tools.clone(),
            search: None,
            tools,
            definitions,
            mcp: None,
            system_prompt: None,
            provider,
            store,
            session: None,
            transcript: Vec::new(),
            compaction: None,
            context_tokens: 0,
            explicit_window: None,
            too_long: false,
            names_thread: scope == Scope::Root,
        };
        conversation.rebuild_tools();
        conversation
    }

    /// The directory the model works in. Frozen: a workspace change is a new
    /// conversation.
    pub fn workspace(&self) -> &str {
        &self.workspace
    }

    /// The model this conversation talks to. Frozen into the prompt, so a switch
    /// rebuilds it.
    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn tools(&self) -> &Arc<Vec<Arc<dyn Tool>>> {
        &self.tools
    }

    /// The tool list as the request carries it, in the same order as `tools`.
    pub fn definitions(&self) -> &[ToolDefinition] {
        &self.definitions
    }

    /// Adds the tools of the context servers that connected while this session was
    /// open. Only the request changes: the prompt is left exactly as it was built,
    /// which is why a server can arrive mid-session at all.
    ///
    /// Called again when servers reconnect; it replaces the previous set. A tool
    /// whose name is already taken is dropped rather than sent twice, because a
    /// provider rejects a duplicate function name outright.
    pub fn use_mcp_servers(&mut self, servers: Arc<McpServers>) {
        self.mcp = Some(servers);
        self.rebuild_tools();
    }

    /// Offers DeepSeek's search tool, or withdraws it when the key goes away. The
    /// prompt names no tools, so the list can change without a new cached prefix.
    pub fn set_search_key(&mut self, key: Option<String>) {
        self.search = key.map(|key| Arc::new(humibird_tools::Search::new(key)) as Arc<dyn Tool>);
        self.rebuild_tools();
    }

    /// The request's tool list: built-ins, history recall excluding the active
    /// session, web search when configured, and context server tools. A name is
    /// offered once — a provider rejects a duplicate function name outright.
    fn rebuild_tools(&mut self) {
        let mut tools: Vec<Arc<dyn Tool>> = self.builtin.iter().cloned().collect();
        if self.names_thread
            && let Some(store) = &self.store
        {
            tools.push(Arc::new(humibird_tools::SessionSearch::new(
                store.clone(),
                self.workspace.clone(),
                self.session.clone(),
            )));
        }
        if let Some(search) = self.search.clone() {
            tools.push(search);
        }
        if let Some(servers) = &self.mcp {
            let mut names: std::collections::HashSet<String> =
                tools.iter().map(|tool| tool.name().to_owned()).collect();
            for tool in servers.tools() {
                if names.insert(tool.name().to_owned()) {
                    tools.push(tool);
                } else {
                    log::warn!(
                        "the tool {} collides with one already offered; leaving it out",
                        tool.name()
                    );
                }
            }
        }

        self.definitions = tools
            .iter()
            .map(|tool| {
                ToolDefinition::function(tool.name(), tool.description(), tool.parameters())
            })
            .collect();
        self.tools = Arc::new(tools);
    }

    /// Switches the model from the next turn on. The prompt names the model, so it
    /// is rebuilt: a switch is a new cached prefix either way.
    pub fn set_model(&mut self, model: impl Into<String>) {
        self.model = model.into();
        self.system_prompt = None;
    }

    /// Takes the provider the window resolved. A saved or reset key rebuilds it,
    /// and the next turn has to use the new one: a conversation that keeps the
    /// old instance keeps the old key.
    pub fn set_provider(&mut self, provider: Option<Arc<dyn Provider>>) {
        self.provider = provider;
    }

    /// Whether a provider is in hand, for the window to check after a rebuild.
    pub fn has_provider(&self) -> bool {
        self.provider.is_some()
    }

    /// Sets the thinking switch for the next turn. Nothing in the prompt names
    /// it, so the cached prefix is untouched; unlike a model switch this costs
    /// nothing to change mid-thread.
    pub fn set_thinking(&mut self, thinking: Option<Thinking>) {
        self.thinking = thinking;
    }

    /// The prompt for this session, assembled on first use and kept
    /// byte-identical afterwards. Memory is read here, which is why a session that
    /// never sends reads nothing.
    pub fn system_prompt(&mut self) -> String {
        if let Some(prompt) = &self.system_prompt {
            return prompt.clone();
        }

        let soul = self.profile.as_ref().and_then(|profile| {
            std::fs::read_to_string(profile.soul_path())
                .inspect_err(|error| log::debug!("no SOUL.md, continuing without one: {error}"))
                .ok()
        });
        let user_memory = self
            .memories
            .as_ref()
            .and_then(|memories| load_memory(&memories.user));
        let agent_memory = self
            .memories
            .as_ref()
            .and_then(|memories| load_memory(&memories.agent));
        // Only the names: the prompt decides whether there are tools at all and
        // whether `memory` and `read_skill` are among them. The schemas travel in
        // the request, so the prompt does not change when the list does.
        let tools = self
            .tools
            .iter()
            .map(|tool| tool.name())
            .collect::<Vec<_>>();
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "sh".to_owned());
        let date = chrono::Local::now().format("%Y-%m-%d").to_string();

        let prompt = humibird_prompt::build(&PromptInputs {
            model: &self.model,
            os: std::env::consts::OS,
            shell: &shell,
            date: &date,
            workspace: &self.workspace,
            tools: &tools,
            soul: soul.as_deref(),
            skills: self.skills.skills(),
            user_memory: user_memory.as_ref(),
            agent_memory: agent_memory.as_ref(),
        });
        log::info!(
            "assembled the system prompt for this session ({} bytes)",
            prompt.len()
        );
        self.system_prompt = Some(prompt.clone());
        prompt
    }
}

/// Everything the model can call, in the order the window shows it. A workspace
/// the profile refuses gets no file tools at all; skills that cannot be
/// read are warned about and skipped.
fn install_tools(
    scope: Scope,
    workspace: &str,
    memories: Option<MemoryPaths>,
    profile: Option<&Profile>,
    skills: Option<Arc<SkillIndex>>,
) -> (Arc<Vec<Arc<dyn Tool>>>, Vec<ToolDefinition>) {
    let mut tools: Vec<Arc<dyn Tool>> = Vec::new();

    let accepted = profile
        .map(|profile| profile.accept_workspace(Path::new(workspace)))
        .unwrap_or(Ok(()));
    match accepted {
        Ok(()) => match humibird_tools::Workspace::new(workspace) {
            Ok(root) => tools.extend(humibird_tools::tools(root)),
            Err(error) => log::warn!("the file tools are disabled: {error:#}"),
        },
        Err(error) => log::warn!("the file tools are disabled: {error:#}"),
    }

    if let Some(skills) = skills {
        tools.extend(humibird_tools::skill_tools(skills));
    }
    // Fetching is not a file operation, so it is offered even when the profile
    // accepts no workspace, and a subagent researches with it too.
    tools.extend(humibird_tools::fetch_tools());
    match scope {
        Scope::Root => {
            if let Some(memories) = memories {
                tools.extend(humibird_tools::memory_tools(memories));
            }
            tools.extend(humibird_tools::ask_tools());
            tools.extend(humibird_tools::delegate_tools());
        }
        // A subagent may not ask the person, delegate its own work, or write to the
        // profile's memory: its job is one report back to the parent.
        Scope::Subagent => {}
    }

    let definitions = tools
        .iter()
        .map(|tool| ToolDefinition::function(tool.name(), tool.description(), tool.parameters()))
        .collect();
    (Arc::new(tools), definitions)
}

/// Reads one memory file. A missing or unreadable file is simply no memory: the
/// session still runs, it just knows less.
fn load_memory(path: &Path) -> Option<Memory> {
    Memory::load(path)
        .inspect_err(|error| log::warn!("continuing without {}: {error:#}", path.display()))
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(profile: &Profile, name: &str, description: &str) {
        let directory = profile.skills_dir().join(name);
        std::fs::create_dir_all(&directory).expect("create the skill");
        std::fs::write(
            directory.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\n\n# Card\n"),
        )
        .expect("write SKILL.md");
    }

    /// A workspace next to the profile, so the profile accepts it.
    fn conversation(directory: &Path, profile: Option<Profile>) -> Conversation {
        let workspace = directory.join("project");
        std::fs::create_dir_all(&workspace).expect("create the workspace");
        Conversation::new(
            profile,
            workspace.to_string_lossy(),
            "deepseek-flash",
            None,
            None,
        )
    }

    #[test]
    fn history_tool_tracks_the_active_session_without_changing_the_prompt() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(&directory.path().join("history.db")).unwrap();
        let old = store.create_session("/workspace", None, None).unwrap();
        let current = store.create_session("/workspace", None, None).unwrap();
        for id in [&old.id, &current.id] {
            store
                .append_message(id, humibird_store::Role::User, "remember SQLite")
                .unwrap();
        }
        let mut conversation =
            Conversation::new(None, "/workspace", "test", None, Some(store.clone()));
        let prompt = conversation.system_prompt();
        let definitions = conversation.definitions().to_vec();
        let search = |conversation: &Conversation| {
            let tool = conversation
                .tools()
                .iter()
                .find(|tool| tool.name() == "session_search")
                .unwrap();
            serde_json::from_str::<serde_json::Value>(
                &tool
                    .run(&serde_json::json!({"operation": "search", "query": "SQLite"}))
                    .unwrap(),
            )
            .unwrap()
        };
        assert_eq!(
            search(&conversation)["matches"].as_array().unwrap().len(),
            2
        );
        conversation.attach_session(current.id.clone());
        assert_eq!(search(&conversation)["matches"][0]["session_id"], old.id);
        conversation.open(&old);
        assert_eq!(
            search(&conversation)["matches"][0]["session_id"],
            current.id
        );
        assert_eq!(conversation.system_prompt(), prompt);
        assert_eq!(
            serde_json::to_value(conversation.definitions()).unwrap(),
            serde_json::to_value(definitions).unwrap()
        );
        assert_eq!(
            conversation
                .tools()
                .iter()
                .filter(|tool| tool.name() == "session_search")
                .count(),
            1
        );
        let child = Conversation::subagent(None, "/workspace", "test", None, Some(store));
        assert!(
            !child
                .tools()
                .iter()
                .any(|tool| tool.name() == "session_search")
        );
        let no_store = Conversation::new(None, "/workspace", "test", None, None);
        assert!(
            !no_store
                .tools()
                .iter()
                .any(|tool| tool.name() == "session_search")
        );
    }

    #[test]
    fn the_search_tool_is_offered_only_while_the_deepseek_key_is() {
        let directory = tempfile::tempdir().expect("temp dir");
        let mut conversation = conversation(directory.path(), None);
        let names = |conversation: &Conversation| {
            conversation
                .tools()
                .iter()
                .map(|tool| tool.name().to_owned())
                .collect::<Vec<_>>()
        };

        assert!(
            !names(&conversation).contains(&"web_search".to_owned()),
            "a conversation with no key is not offered DeepSeek's search"
        );

        conversation.set_search_key(Some("sk-test".to_owned()));
        let with_search = names(&conversation);
        assert!(
            with_search.contains(&"web_search".to_owned()),
            "{with_search:?}"
        );
        assert_eq!(
            conversation.definitions().len(),
            with_search.len(),
            "the tool travels with a definition like every other"
        );

        conversation.set_search_key(None);
        assert!(!names(&conversation).contains(&"web_search".to_owned()));
    }

    #[test]
    fn the_prompt_names_the_skills_but_not_the_tools() {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        write_skill(&profile, "card", "makes a card");
        let mut conversation = conversation(directory.path(), Some(profile));

        let prompt = conversation.system_prompt();

        for expected in ["You are humibird", "## Skills", "- `card`: makes a card"] {
            assert!(
                prompt.contains(expected),
                "missing {expected:?} in:\n{prompt}"
            );
        }
        for absent in ["`read_file`", "`ask_user`", "Available:"] {
            assert!(
                !prompt.contains(absent),
                "the tool schemas travel in the request; {absent:?} in:\n{prompt}"
            );
        }
    }

    #[test]
    fn a_profile_without_skills_gets_no_section_and_no_skill_tool() {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        let mut conversation = conversation(directory.path(), Some(profile));

        let prompt = conversation.system_prompt();

        assert!(!prompt.contains("## Skills"), "{prompt}");
        let names: Vec<_> = conversation
            .tools()
            .iter()
            .map(|tool| tool.name())
            .collect();
        assert!(!names.contains(&"read_skill"), "{names:?}");
        assert!(names.contains(&"ask_user"), "{names:?}");
        assert_eq!(
            conversation.definitions().len(),
            names.len(),
            "every tool is offered with a definition"
        );
    }

    #[test]
    fn a_subagent_gets_no_way_to_ask_delegate_or_write_memory() {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        let root = conversation(directory.path(), Some(profile.clone()));
        // The same workspace the profile accepts for the root conversation.
        let subagent = Conversation::subagent(
            Some(profile),
            directory.path().join("project").to_string_lossy(),
            "deepseek-flash",
            None,
            None,
        );

        let root_names: Vec<_> = root.tools().iter().map(|tool| tool.name()).collect();
        let subagent_names: Vec<_> = subagent.tools().iter().map(|tool| tool.name()).collect();
        assert!(root_names.contains(&"delegate"), "{root_names:?}");
        assert!(root_names.contains(&"ask_user"), "{root_names:?}");
        assert!(root_names.contains(&"memory"), "{root_names:?}");
        for absent in ["delegate", "ask_user", "memory"] {
            assert!(
                !subagent_names.contains(&absent),
                "{absent} in {subagent_names:?}"
            );
        }
        assert!(subagent_names.contains(&"read_file"), "{subagent_names:?}");
        assert_eq!(
            subagent.definitions().len(),
            subagent_names.len(),
            "every tool is offered with a definition"
        );
    }

    /// A context server that speaks just enough protocol: it answers the handshake
    /// and offers one tool, `ping`.
    fn server_script() -> String {
        r#"
            while IFS= read -r line; do
                case "$line" in
                    *'"method":"initialize"'*)
                        printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{}}}'
                        ;;
                    *'"method":"tools/list"'*)
                        printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"ping","description":"Ping the server","inputSchema":{"type":"object","properties":{}}}]}}'
                        ;;
                esac
            done
        "#
        .to_owned()
    }

    fn config_with(servers: &str) -> humibird_profile::McpConfig {
        humibird_profile::McpConfig::parse(servers).expect("a valid mcp.json")
    }

    #[test]
    fn a_context_server_adds_its_tools_to_the_request_but_not_the_prompt() {
        let directory = tempfile::tempdir().expect("temp dir");
        let mut conversation = conversation(directory.path(), None);
        let prompt = conversation.system_prompt();
        let before = conversation.definitions().len();

        let config = config_with(&format!(
            r#"{{"servers": {{"fake": {{"command": "sh", "args": ["-c", {}]}}}}}}"#,
            serde_json::to_string(&server_script()).expect("json string")
        ));
        let servers = McpServers::start(&config);
        assert!(servers.failures().is_empty(), "{:?}", servers.failures());
        conversation.use_mcp_servers(servers);

        let names: Vec<_> = conversation
            .tools()
            .iter()
            .map(|tool| tool.name().to_owned())
            .collect();
        assert!(names.contains(&"mcp__fake__ping".to_owned()), "{names:?}");
        assert_eq!(conversation.definitions().len(), before + 1);
        assert_eq!(
            conversation.system_prompt(),
            prompt,
            "the prompt does not name tools, so a server arriving leaves it alone"
        );
    }

    #[test]
    fn a_url_server_is_reached_with_http_and_a_refusal_is_a_line() {
        // Nothing listens on port 1: the point is that a `url` takes the HTTP path
        // at all, and that its failure is reported like any other.
        let config = config_with(r#"{"servers": {"remote": {"url": "http://127.0.0.1:1/mcp"}}}"#);
        let servers = McpServers::start(&config);

        assert!(servers.tools().is_empty());
        assert_eq!(servers.failures().len(), 1, "{:?}", servers.failures());
        assert!(
            servers.failures()[0].contains("remote"),
            "{:?}",
            servers.failures()
        );
    }

    #[test]
    fn a_server_that_does_not_start_is_a_line_not_a_broken_session() {
        let directory = tempfile::tempdir().expect("temp dir");
        let mut conversation = conversation(directory.path(), None);
        let before = conversation.definitions().len();

        let config =
            config_with(r#"{"servers": {"gone": {"command": "/nonexistent/mcp-server"}}}"#);
        let servers = McpServers::start(&config);
        assert!(servers.tools().is_empty());
        assert_eq!(servers.failures().len(), 1);
        assert!(
            servers.failures()[0].contains("gone"),
            "{:?}",
            servers.failures()
        );

        conversation.use_mcp_servers(servers);
        assert_eq!(
            conversation.definitions().len(),
            before,
            "a server that never came up adds nothing"
        );
    }

    #[test]
    fn a_workspace_the_profile_refuses_loses_the_file_tools() {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        let conversation = Conversation::new(
            Some(profile.clone()),
            profile.root().to_string_lossy(),
            "deepseek-flash",
            None,
            None,
        );

        let names: Vec<_> = conversation
            .tools()
            .iter()
            .map(|tool| tool.name())
            .collect();

        assert!(!names.contains(&"read_file"), "{names:?}");
        assert!(names.contains(&"ask_user"), "{names:?}");
    }

    #[test]
    fn the_prompt_is_frozen_until_the_model_changes() {
        let directory = tempfile::tempdir().expect("temp dir");
        let mut conversation = conversation(directory.path(), None);

        let first = conversation.system_prompt();
        assert_eq!(first, conversation.system_prompt(), "assembled once");
        assert!(first.contains("- Model: deepseek-flash"), "{first}");

        conversation.set_model("deepseek-reasoner");
        let second = conversation.system_prompt();

        assert!(second.contains("- Model: deepseek-reasoner"), "{second}");
        assert_eq!(second, conversation.system_prompt());
    }

    #[test]
    fn memory_is_read_once_with_the_prompt() {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        let memories = MemoryPaths::in_profile(profile.root());
        std::fs::create_dir_all(memories.user.parent().expect("memories dir"))
            .expect("create the memories directory");
        std::fs::write(&memories.user, "Prefers short answers.\n").expect("write USER.md");
        let mut conversation = conversation(directory.path(), Some(profile));

        let first = conversation.system_prompt();
        assert!(
            first.contains("1. Prefers short answers."),
            "the snapshot is in the prompt"
        );

        std::fs::write(&memories.user, "Prefers long answers.\n").expect("rewrite USER.md");
        assert_eq!(
            first,
            conversation.system_prompt(),
            "frozen for the session"
        );
    }
}
