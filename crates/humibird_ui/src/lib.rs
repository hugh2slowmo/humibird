use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use base64::Engine as _;
use futures::channel::oneshot;
use futures::future::{FutureExt as _, LocalBoxFuture};
use gpui::{
    App, AsyncApp, ClickEvent, Context, Entity, FocusHandle, Focusable, FontWeight, IntoElement,
    KeyBinding, Menu, MenuItem, MouseButton, MouseDownEvent, ParentElement, Render, ScrollHandle,
    Styled, Subscription, WeakEntity, Window, actions, div, point, prelude::*, px, rgb,
};
use humibird_agent::{
    AgentEvent, Answer, Background, CompactionRun, Conversation, Decision, McpServers, StepState,
    ToolStep, TurnHost, TurnOutcome, summarize_arguments,
};
use humibird_profile::{Config, Effort as ProfileEffort, Profile, Theme};
use humibird_providers::{
    Anthropic, Attachment, DiscoveredModel, Effort, OpenAiCompatible, Provider,
    Thinking as ThinkingRequest,
};
use humibird_store::{Compaction, Role, Session, Store, ToolCallStatus};
use humibird_tools::{DELEGATION_CANCELLED, Delegation, Question};
use language::LanguageRegistry;
use markdown::Markdown;
use settings_window::SettingsWindow;
use ui::{Color, Icon, IconName, IconSize};
use util::ResultExt as _;

pub(crate) use palette::{Appearance, Palette};
pub use text_input::{TextInput, TextInputEvent};
use widgets::theme_toggle;

actions!(
    humibird,
    [
        OpenSettings,
        NewThread,
        CloseSettings,
        /// Quits the application.
        Quit,
        /// Hides the application.
        Hide,
        /// Hides every other application.
        HideOthers,
        /// Shows every hidden application.
        ShowAll,
        /// Minimizes the window.
        Minimize,
        /// Zooms the window.
        Zoom,
        /// Toggles full screen.
        ToggleFullScreen,
    ]
);

/// How the process environment is read. Injected rather than called directly so
/// tests can control whether an exported key is visible.
pub(crate) type EnvLookup = Rc<dyn Fn(&str) -> Option<String>>;

/// Builds a provider only to ask it which models it serves, for the picker. The
/// window's own provider carries the profile's overrides for the one in use; this
/// builds the others from their presets.
pub(crate) type ListingProviderFactory =
    Rc<dyn Fn(&'static humibird_providers::Preset) -> Result<Arc<dyn Provider>, String>>;

pub use assets::Assets;

mod assets;
mod composer;
mod markdown_style;
mod palette;
mod settings_window;
mod sidebar;
mod syntax;
mod text_input;
mod timeline;
mod widgets;
mod zed_theme;

/// Initializes the Zed pieces the markdown renderer depends on: an (empty)
/// settings store and the default theme. Must run before any view is created.
pub fn init(cx: &mut App) {
    settings::init(cx);
    theme_settings::init(::theme::LoadThemes::JustBase, cx);
    zed_theme::apply(
        cx,
        Appearance::Light,
        Palette::for_appearance(Appearance::Light),
    );
    text_input::bind_keys(cx);
    bind_keys(cx);
    cx.set_menus(app_menus());
}

/// The menu bar. macOS gives the first menu the application's name and puts the
/// standard items there; the rest are the two menus a window needs — editing and
/// the window itself. Every item dispatches an action the app already handles, so
/// a menu click and its shortcut are the same code path.
fn app_menus() -> [Menu; 3] {
    [
        Menu::new("humibird").items([
            MenuItem::action("Settings…", OpenSettings),
            MenuItem::separator(),
            #[cfg(target_os = "macos")]
            MenuItem::os_submenu("Services", gpui::SystemMenuType::Services),
            #[cfg(target_os = "macos")]
            MenuItem::separator(),
            MenuItem::action("Hide humibird", Hide),
            MenuItem::action("Hide Others", HideOthers),
            MenuItem::action("Show All", ShowAll),
            MenuItem::separator(),
            MenuItem::action("Quit humibird", Quit),
        ]),
        Menu::new("Edit").items([
            MenuItem::action("Cut", text_input::Cut),
            MenuItem::action("Copy", text_input::Copy),
            MenuItem::action("Paste", text_input::Paste),
            MenuItem::action("Select All", text_input::SelectAll),
        ]),
        Menu::new("Window").items([
            MenuItem::action("Minimize", Minimize),
            MenuItem::action("Zoom", Zoom),
            MenuItem::separator(),
            MenuItem::action("Enter Full Screen", ToggleFullScreen),
        ]),
    ]
}

fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("cmd-,", OpenSettings, None),
        KeyBinding::new("cmd-n", NewThread, None),
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("cmd-m", Minimize, None),
        KeyBinding::new("ctrl-cmd-f", ToggleFullScreen, None),
        KeyBinding::new("escape", CloseSettings, Some("SettingsWindow")),
        // The text-editing actions are bound where they act, under the
        // `TextInput` context. A menu item shows the shortcut its action is bound
        // to, and the lookup is made without that context, so the same actions
        // are also bound here: the menu reads the way the keyboard works, and the
        // focused field still gets the more specific binding.
        KeyBinding::new("cmd-x", text_input::Cut, None),
        KeyBinding::new("cmd-c", text_input::Copy, None),
        KeyBinding::new("cmd-v", text_input::Paste, None),
        KeyBinding::new("cmd-a", text_input::SelectAll, None),
    ]);

    cx.on_action(|_: &Quit, cx: &mut App| cx.quit());
    cx.on_action(|_: &Hide, cx: &mut App| cx.hide());
    cx.on_action(|_: &HideOthers, cx: &mut App| cx.hide_other_apps());
    cx.on_action(|_: &ShowAll, cx: &mut App| cx.unhide_other_apps());
    cx.on_action(|_: &Minimize, cx: &mut App| {
        with_active_window(cx, |_, window, _| window.minimize_window());
    });
    cx.on_action(|_: &Zoom, cx: &mut App| {
        with_active_window(cx, |_, window, _| window.zoom_window());
    });
    cx.on_action(|_: &ToggleFullScreen, cx: &mut App| {
        with_active_window(cx, |_, window, _| window.toggle_fullscreen());
    });

    // Element-level `on_action` only fires for actions dispatched into a focused
    // subtree, so these are app-level handlers: the shortcuts work even when the
    // window has no focus (a fresh window, or a click on empty space). Updating
    // the window has to be deferred: the listener runs while the dispatching
    // window is already being updated.
    on_action::<OpenSettings>(cx, |root, _window, cx| root.open_settings(cx));
    on_action::<NewThread>(cx, |root, window, cx| root.new_thread(window, cx));
    on_action::<CloseSettings>(cx, |root, window, cx| root.close_model_menu(window, cx));

    // Escape belongs to whichever window is focused, and the settings window is
    // not a `HumibirdRoot`.
    cx.on_action(|_: &CloseSettings, cx: &mut App| {
        cx.defer(|cx| {
            let windows = settings_windows(cx);
            for window in windows {
                window
                    .update(cx, |_, window, _| window.remove_window())
                    .log_err();
            }
        });
    });
}

fn settings_windows(cx: &App) -> Vec<gpui::WindowHandle<SettingsWindow>> {
    cx.windows()
        .into_iter()
        .filter_map(|window| window.downcast::<SettingsWindow>())
        .collect()
}

fn on_action<A: gpui::Action>(
    cx: &mut App,
    update: fn(&mut HumibirdRoot, &mut Window, &mut Context<HumibirdRoot>),
) {
    cx.on_action(move |_: &A, cx: &mut App| {
        cx.defer(move |cx| update_roots(cx, update));
    });
}

/// Runs a command against the window the menu was used in. The menu bar is there
/// even when no window has focus, so no window is not an error; and the update is
/// deferred because an action can arrive while that window is already borrowed by
/// the key event that dispatched it.
fn with_active_window(cx: &mut App, update: fn(gpui::AnyView, &mut Window, &mut App)) {
    let Some(window) = cx.active_window() else {
        return;
    };
    cx.defer(move |cx| {
        window.update(cx, update).log_err();
    });
}

fn update_roots(
    cx: &mut App,
    update: fn(&mut HumibirdRoot, &mut Window, &mut Context<HumibirdRoot>),
) {
    let windows = cx
        .windows()
        .into_iter()
        .filter_map(|window| window.downcast::<HumibirdRoot>())
        .collect::<Vec<_>>();
    for window in windows {
        window.update(cx, update).log_err();
    }
}

/// Whether a scroll view is showing its end. `offset` is negative when scrolled
/// down and `max_offset` is positive, so at the end they cancel out. A wheel can
/// push the offset past the end until the next layout clamps it, which still
/// counts as being at the end.
fn at_end(scroll: &ScrollHandle) -> bool {
    scroll.offset().y + scroll.max_offset().y <= px(8.)
}

/// One exchange in the conversation. The assistant side streams into its own
/// markdown entity.
pub(crate) struct Turn {
    pub(crate) user: String,
    /// What came with the message: the model sees it, and the bubble names it so
    /// the person can see what was sent.
    pub(crate) attachments: Vec<Attachment>,
    pub(crate) assistant: Entity<Markdown>,
    pub(crate) assistant_text: String,
    /// What the model thought, in the stretches it thought it: the model thinks,
    /// acts, then thinks again, and each stretch belongs where it happened rather
    /// than under the whole turn.
    pub(crate) thinking: Vec<Thinking>,
    pub(crate) state: TurnState,
    pub(crate) tool_steps: Vec<ToolStep>,
    /// Which steps the user opened to see their arguments and result.
    pub(crate) expanded_steps: HashSet<usize>,
    /// The `seq` of this turn's user message in the transcript. Compaction uses it
    /// to say which turns a summary stands in for.
    pub(crate) seq: Option<i64>,
    /// The answer is rendered by its own entity, so its markdown can settle after
    /// the root has rendered and grow the timeline with nobody asking to follow.
    _answer_watch: Subscription,
}

/// One stretch of thinking. `after_steps` is how many calls had run when it
/// started, which is what puts it back in order without a separate entry list.
pub(crate) struct Thinking {
    pub(crate) after_steps: usize,
    pub(crate) text: String,
    /// True until the model acts or answers, which is what ends a stretch.
    pub(crate) streaming: bool,
    /// The user's own choice: auto-collapse never overrides it.
    pub(crate) touched: bool,
    pub(crate) expanded: bool,
    /// The block's own scroll position, so a streaming preview can pin itself to
    /// its newest line without moving the conversation.
    pub(crate) scroll: ScrollHandle,
}

impl Turn {
    /// Appends a reasoning delta, opening a stretch when the last one ended.
    fn think(&mut self, delta: &str) {
        if !self.thinking.last().is_some_and(|chunk| chunk.streaming) {
            self.thinking.push(Thinking {
                after_steps: self.tool_steps.len(),
                text: String::new(),
                streaming: true,
                touched: false,
                // Open while it streams; ending the stretch folds it away.
                expanded: true,
                scroll: ScrollHandle::new(),
            });
        }
        if let Some(chunk) = self.thinking.last_mut() {
            chunk.text.push_str(delta);
        }
    }

    /// Ends the open stretch: the model is acting or answering now. A stretch the
    /// user opened stays open.
    fn stop_thinking(&mut self) {
        if let Some(chunk) = self.thinking.last_mut()
            && chunk.streaming
        {
            chunk.streaming = false;
            if !chunk.touched {
                chunk.expanded = false;
            }
        }
    }

    fn toggle_thinking(&mut self, index: usize) {
        if let Some(chunk) = self.thinking.get_mut(index) {
            chunk.expanded = !chunk.expanded;
            chunk.touched = true;
        }
    }
}

/// One delegated task, as the window shows it: its own report, its own steps, and the
/// future that runs it. It belongs to a step of the turn that asked for it, so the
/// card can be drawn in place.
pub(crate) struct Subagent {
    /// The parent turn and the step that delegated the task.
    pub(crate) turn: usize,
    pub(crate) step: usize,
    /// What the subagent has said and done so far: the same projection a turn uses,
    /// rendered as a report instead of a conversation.
    pub(crate) work: Turn,
    /// Dropping it stops the subagent.
    pub(crate) task: Option<gpui::Task<()>>,
}

/// A call that is waiting for the user to agree to it. The controls live on the
/// step itself, the way Zed puts them on the tool call card.
pub(crate) struct PendingApproval {
    pub(crate) name: String,
    /// What an "always" would remember: the tool name, or one host of `fetch`.
    pub(crate) key: String,
    pub(crate) summary: String,
    respond: Option<oneshot::Sender<Decision>>,
}

/// A question the model asked that is waiting for the person. The card
/// sits under the step it belongs to, where the approval controls also live.
pub(crate) struct PendingQuestion {
    pub(crate) question: String,
    pub(crate) options: Vec<String>,
    pub(crate) allow_free_text: bool,
    /// The root's answer field, shared so typing and Enter are handled once.
    pub(crate) input: Entity<TextInput>,
    respond: Option<oneshot::Sender<Answer>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TurnState {
    Streaming,
    Done,
    /// The user stopped the turn; what streamed is kept, the rest is not.
    Stopped,
    Failed(String),
}

/// A message the user wrote while a turn was running. `steer` asks for the turn
/// to end at its next boundary instead of running to completion, because the
/// transcript is append-only: this is the next turn, not an edit of this one.
pub(crate) struct QueuedMessage {
    pub(crate) text: String,
    /// What was attached to it; the front message's send keeps them.
    pub(crate) attachments: Vec<Attachment>,
    pub(crate) steer: bool,
}

/// The window's root view: sidebar, thread header, timeline, composer.
pub struct HumibirdRoot {
    appearance: Appearance,
    config: Config,
    composer_input: Entity<TextInput>,
    profile: Option<Profile>,
    env: EnvLookup,
    store: Option<Store>,
    workspace: String,
    /// The model this thread talks to. A switch sets it for the thread, the
    /// session row, and the profile's default for the next one.
    model: String,
    model_menu_open: bool,
    /// The effort menu is showing. Only one composer menu is open at a time.
    thinking_menu_open: bool,
    /// Which row of the open picker the keyboard is on.
    model_menu_highlight: usize,
    model_menu_focus: FocusHandle,
    /// Why the last attempt to change workspace failed, shown until the next one.
    workspace_notice: Option<String>,
    /// The turn that is running, if any. Dropping the task cancels it, which is
    /// what the stop button does.
    turn: Option<gpui::Task<()>>,
    /// The tasks the turns delegated, with what each subagent has done. Dropping
    /// their tasks is what makes a stop cancel the whole subtree.
    subagents: Vec<Subagent>,
    /// The compaction this window is showing. A recorded summary is the only way
    /// the request gets shorter, and a turn makes one for itself when
    /// the context is close to the limit; there is no button for it.
    compaction: Option<Compaction>,
    /// Messages written while the turn was busy, oldest first.
    queued: Vec<QueuedMessage>,
    /// What is attached to the message being written, in the order it was added.
    attachments: Vec<Attachment>,
    /// Why the last attachment was refused, shown above the composer until the
    /// next attempt.
    attachment_notice: Option<String>,
    /// The `@` query the composer currently ends with, the files it matches, and
    /// the query Escape closed (so it stays closed until the person types).
    file_query: Option<String>,
    file_visible: Vec<String>,
    /// Every file under the workspace, read the first time a `@` menu opens and
    /// dropped when it closes, so a file created meanwhile shows up next time.
    file_choices: Option<Vec<String>>,
    dismissed_file_query: Option<String>,
    /// Whether the pointer is over the context meter.
    context_meter_hovered: bool,
    /// The context servers (MCP) this window connected to, once they are up. Shared
    /// with every conversation the window opens after that.
    mcp: Option<Arc<McpServers>>,
    /// A context server that could not be used, shown in the composer until it is
    /// dismissed. Not fatal: the thread runs with the tools it has.
    mcp_notice: Option<String>,
    /// The conversation the model sees: its tools, its skill index, its memory
    /// snapshot, and its frozen prompt. Rebuilt when a session starts.
    conversation: Conversation,
    /// Tools the user allowed for the rest of this session.
    approved_tools: HashSet<String>,
    /// Calls waiting for consent, oldest first. A turn and the subagents it
    /// started run at the same time, so more than one call can be waiting; a
    /// single slot would drop the one that arrived first, which the turn reads as
    /// a denial.
    pending_approvals: VecDeque<PendingApproval>,
    pending_question: Option<PendingQuestion>,
    /// The field a free-text answer is typed into. One per window, cleared
    /// between questions.
    answer_input: Entity<TextInput>,
    /// Whether new output should bring the timeline with it. The reader turns it
    /// off by scrolling away from the end and back on by returning.
    pinned: bool,
    /// The thread whose delete control was pressed once. The row asks again,
    /// because deleting a thread has no undo.
    pending_delete: Option<String>,
    /// The row the pointer is over. It is the only row whose delete control is
    /// on screen, and a hidden control is not under the pointer, so this is also
    /// what keeps the control from being clickable blind.
    hovered_thread: Option<String>,
    sessions: Vec<Session>,
    session_search: Entity<TextInput>,
    session: Option<Session>,
    turns: Vec<Turn>,
    /// Models each provider named itself, keyed by provider id, on top of its preset
    /// catalogue. Every provider this profile can reach is asked when the picker
    /// opens, so a provider whose list only it knows is usable without switching to
    /// it first. The window the listing carries is what lets a thread on such a
    /// model compact before it fails.
    discovered: BTreeMap<String, Vec<DiscoveredModel>>,
    /// The providers whose request is still in the air.
    asking: BTreeSet<String>,
    /// The key each finished listing was made with. A key saved or reset in Settings
    /// changes it, which is what makes the picker ask that provider again.
    asked_with: BTreeMap<String, Option<String>>,
    /// Builds a provider for a preset other than the one in use, only to ask it for
    /// its models. Tests replace it so no request leaves the process.
    listing_provider: ListingProviderFactory,
    /// The window this view draws in. The composer's Enter is handled by the input,
    /// whose listener is not given a window, and the fix for a blocked send is the
    /// model list — which needs one.
    window: Option<gpui::AnyWindowHandle>,
    provider: Result<Arc<dyn Provider>, String>,
    languages: Arc<LanguageRegistry>,
    scroll_handle: ScrollHandle,
    _composer_subscription: Subscription,
    _answer_subscription: Subscription,
    _session_search_subscription: Subscription,
}

impl HumibirdRoot {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let profile = Profile::default_location();
        let config = load_config(&profile);
        let env = process_env();
        let provider = default_provider(&profile, &config, &env);
        Self::build(cx, profile, provider, env, config, None)
    }

    pub fn new_with(
        cx: &mut Context<Self>,
        profile: Option<Profile>,
        provider: Result<Arc<dyn Provider>, String>,
    ) -> Self {
        Self::new_with_env(cx, profile, provider, process_env())
    }

    fn new_with_env(
        cx: &mut Context<Self>,
        profile: Option<Profile>,
        provider: Result<Arc<dyn Provider>, String>,
        env: EnvLookup,
    ) -> Self {
        let config = load_config(&profile);
        Self::build(cx, profile, provider, env, config, None)
    }

    /// Tests pin the workspace so tool calls cannot touch the repository.
    #[cfg(test)]
    fn new_with_workspace(
        cx: &mut Context<Self>,
        profile: Option<Profile>,
        provider: Result<Arc<dyn Provider>, String>,
        env: EnvLookup,
        workspace: String,
    ) -> Self {
        let config = load_config(&profile);
        Self::build(cx, profile, provider, env, config, Some(workspace))
    }

    /// Tests hand out no provider for a preset other than the one in use, so no
    /// request leaves the process.
    #[cfg(test)]
    fn without_listing_providers(mut self) -> Self {
        self.listing_provider = Rc::new(|_| Err("tests do not ask another provider".to_owned()));
        self
    }

    fn build(
        cx: &mut Context<Self>,
        profile: Option<Profile>,
        provider: Result<Arc<dyn Provider>, String>,
        env: EnvLookup,
        config: Config,
        workspace: Option<String>,
    ) -> Self {
        let appearance = Appearance::from(config.theme);
        // The markdown renderer reads Zed's theme, which has to match the
        // palette before the first frame.
        zed_theme::apply(cx, appearance, Palette::for_appearance(appearance));

        let languages = syntax::language_registry(cx);
        let listing_provider = listing_provider_factory(profile.clone(), env.clone());
        let search_key = deepseek_key(&profile, &config, &env);
        let composer_input = cx.new(|cx| TextInput::new("Message humibird", cx));
        let _composer_subscription = cx.subscribe(
            &composer_input,
            |this, input, event: &TextInputEvent, cx| match event {
                TextInputEvent::Submit => this.submit_draft(cx),
                TextInputEvent::Changed => {
                    let text = input.read(cx).value().to_owned();
                    this.update_file_menu(&text, cx);
                }
                TextInputEvent::PastedImage(image) => this.attach_image(image, cx),
            },
        );
        let answer_input = cx.new(|cx| TextInput::new("Type an answer", cx));
        let session_search = cx.new(|cx| TextInput::new("Search threads…", cx));
        let _session_search_subscription =
            cx.subscribe(&session_search, |this, _, event: &TextInputEvent, cx| {
                if matches!(event, TextInputEvent::Changed) {
                    this.hovered_thread = None;
                    this.pending_delete = None;
                    cx.notify();
                }
            });
        let _answer_subscription =
            cx.subscribe(&answer_input, |this, _input, event: &TextInputEvent, cx| {
                if matches!(event, TextInputEvent::Submit) {
                    this.submit_answer(cx);
                }
            });

        let store = profile.as_ref().and_then(|profile| {
            Store::open(&profile.database_path())
                .inspect_err(|error| log::warn!("session storage is disabled: {error:#}"))
                .ok()
        });
        let sessions = store
            .as_ref()
            .and_then(|store| {
                store
                    .sessions()
                    .inspect_err(|error| log::warn!("could not list sessions: {error:#}"))
                    .ok()
            })
            .unwrap_or_default();
        let session = sessions.first().cloned();
        let (turns, subagents) = match (&store, &session) {
            (Some(store), Some(session)) => {
                let (turns, delegated) = restore_turns(cx, store, &session.id, &languages);
                let subagents = delegated
                    .iter()
                    .filter_map(|record| {
                        restore_subagent(cx, store, &session.id, record, &languages)
                    })
                    .collect();
                (turns, subagents)
            }
            _ => (Vec::new(), Vec::new()),
        };
        let workspace = session
            .as_ref()
            .map(|session| session.workspace.clone())
            .or(workspace)
            .or_else(|| {
                profile.as_ref().and_then(|profile| {
                    profile
                        .workspace_dir()
                        .inspect_err(|error| log::warn!("no default workspace: {error:#}"))
                        .ok()
                        .map(|path| path.to_string_lossy().into_owned())
                })
            })
            .unwrap_or_default();
        // A workspace is only usable if the profile accepts it: a stored session
        // may point at `/` or at the profile itself.
        let workspace_notice = profile.as_ref().and_then(|profile| {
            profile
                .accept_workspace(Path::new(&workspace))
                .err()
                .map(|error| error.to_string())
        });
        // A restored thread keeps the model it was using — unless the profile's
        // provider has moved since, in which case it follows the profile and says so;
        // a new thread starts from the profile's default.
        let (model, provider_notice) = thread_model_and_notice(session.as_ref(), &config);
        let workspace_notice = workspace_notice.or(provider_notice);
        let mut conversation = Conversation::new(
            profile.clone(),
            workspace.clone(),
            model.clone(),
            provider.as_ref().ok().cloned(),
            store.clone(),
        );
        conversation.set_thinking(Some(thinking_request(&config)));
        conversation.set_search_key(search_key);
        let discovered = BTreeMap::new();
        conversation.set_context_window(resolve_context_window(&config, &discovered, &model));
        if let Some(session) = &session {
            // The row says what the thread is talking to now, so the next open does
            // not have to work it out again.
            if let Some(store) = &store {
                record_thread_context(store, session, &config.provider.id, &model);
            }
            conversation.open(session);
        }
        let compaction = conversation.compaction().cloned();

        let mut root = Self {
            appearance,
            config,
            composer_input,
            profile,
            env,
            store,
            workspace,
            model,
            model_menu_open: false,
            thinking_menu_open: false,
            model_menu_highlight: 0,
            model_menu_focus: cx.focus_handle(),
            workspace_notice,
            turn: None,
            subagents,
            compaction,
            queued: Vec::new(),
            attachments: Vec::new(),
            attachment_notice: None,
            file_query: None,
            file_visible: Vec::new(),
            file_choices: None,
            dismissed_file_query: None,
            context_meter_hovered: false,
            mcp: None,
            mcp_notice: None,
            conversation,
            approved_tools: HashSet::new(),
            pending_approvals: VecDeque::new(),
            pending_question: None,
            answer_input,
            pinned: true,
            pending_delete: None,
            hovered_thread: None,
            sessions,
            session_search,
            session,
            turns,
            discovered,
            asking: BTreeSet::new(),
            asked_with: BTreeMap::new(),
            listing_provider,
            window: None,
            provider,
            languages,
            scroll_handle: ScrollHandle::new(),
            _composer_subscription,
            _answer_subscription,
            _session_search_subscription,
        };
        root.start_context_servers(cx);
        root
    }

    pub(crate) fn new_thread(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.cancel_turn();
        self.compaction = None;
        self.session = None;
        self.pending_delete = None;
        // A new thread is a new conversation: the skill index, the tools built from
        // it, and the memory snapshot are read again here and nowhere else
        // mid-session.
        self.conversation = self.fresh_conversation();
        // The queue belongs to the thread that was open, and so do the cards of the
        // tasks it delegated and the files that had not been sent yet.
        self.queued.clear();
        self.attachments.clear();
        self.attachment_notice = None;
        self.turns.clear();
        self.subagents.clear();
        self.approved_tools.clear();
        self.pending_approvals.clear();
        self.pending_question = None;
        self.composer_input
            .update(cx, |input, cx| input.clear(window, cx));
        self.scroll_handle.set_offset(point(px(0.), px(0.)));
        let focus = self.composer_input.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
        cx.notify();
    }

    /// A conversation for the session about to start, with the inputs as they are
    /// on disk now. This is the only place they are re-read.
    fn fresh_conversation(&self) -> Conversation {
        let mut conversation = Conversation::new(
            self.profile.clone(),
            self.workspace.clone(),
            self.model.clone(),
            self.provider.as_ref().ok().cloned(),
            self.store.clone(),
        );
        conversation.set_thinking(Some(thinking_request(&self.config)));
        conversation.set_search_key(self.search_key());
        conversation.set_context_window(resolve_context_window(
            &self.config,
            &self.discovered,
            &self.model,
        ));
        // Servers that are already up join every conversation the window opens;
        // the prompt is unaffected either way.
        if let Some(servers) = &self.mcp {
            conversation.use_mcp_servers(servers.clone());
        }
        conversation
    }

    /// Connects the context servers the profile names, off the UI thread, and hands
    /// their tools to the conversation that is open when they are ready.
    ///
    /// Every server is a child process and a handshake, so this cannot run where the
    /// window is built. A server that fails is a line in the composer, not a failed
    /// window: the thread runs with the tools it has.
    fn start_context_servers(&mut self, cx: &mut Context<Self>) {
        let Some(profile) = self.profile.clone() else {
            return;
        };
        let config = match profile.mcp_config() {
            Ok(config) => config,
            Err(error) => {
                // A broken mcp.json is worth saying out loud: nothing will work
                // until it is fixed, and silence would look like "no servers".
                self.mcp_notice = Some(format!("{error:#}"));
                return;
            }
        };
        if config.enabled().next().is_none() {
            return;
        }

        cx.spawn(async move |this, cx| {
            let servers = cx
                .background_spawn(async move { McpServers::start(&config) })
                .await;
            this.update(cx, |root, cx| {
                if let Some(failure) = servers.failures().first() {
                    root.mcp_notice = Some(format!("Context server {failure}"));
                }
                root.mcp = Some(servers.clone());
                root.conversation.use_mcp_servers(servers);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Switches to a stored session and rebuilds its turns from the transcript.
    pub(crate) fn open_session(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self
            .sessions
            .iter()
            .find(|session| session.id == id)
            .cloned()
        else {
            log::warn!("ignoring a click on unknown session {id}");
            return;
        };

        let (turns, subagents) = match &self.store {
            Some(store) => {
                let languages = self.languages.clone();
                let (turns, delegated) = restore_turns(cx, store, &session.id, &languages);
                let subagents = delegated
                    .iter()
                    .filter_map(|record| {
                        restore_subagent(cx, store, &session.id, record, &languages)
                    })
                    .collect();
                (turns, subagents)
            }
            None => (Vec::new(), Vec::new()),
        };
        self.turns = turns;
        self.subagents = subagents;
        self.workspace = session.workspace.clone();
        self.workspace_notice = None;

        // A model id only means something at the provider it came from, so a thread
        // written under another provider cannot carry its model over: it follows the
        // profile and says so, instead of sending an id the new provider may not know.
        let (model, provider_notice) = thread_model_and_notice(Some(&session), &self.config);
        self.model = model;
        if provider_notice.is_some() {
            self.workspace_notice = provider_notice;
        }
        if let Some(store) = &self.store {
            record_thread_context(store, &session, &self.config.provider.id, &self.model);
        }
        self.cancel_turn();
        self.queued.clear();
        self.attachments.clear();
        self.attachment_notice = None;
        // The conversation rebuilds what the model sees: the transcript, the summary
        // that stands in for its older turns, and the size last reported.
        self.conversation = self.fresh_conversation();
        self.conversation.open(&session);
        // The rendering copy of the summary: the divider between the turns it
        // stands in for and the live ones reads it.
        self.compaction = self.conversation.compaction().cloned();
        self.approved_tools.clear();
        self.pending_approvals.clear();
        self.pending_question = None;
        self.pending_delete = None;
        self.session = Some(session);
        self.pinned = true;
        self.scroll_handle.scroll_to_bottom();
        let focus = self.composer_input.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
        cx.notify();
    }

    /// The first press on a row's delete control: the row asks for confirmation,
    /// and pressing it again deletes. Pressing it once more is a cancel, so a
    /// second press never deletes by surprise.
    pub(crate) fn arm_delete_thread(&mut self, id: &str, cx: &mut Context<Self>) {
        self.pending_delete = (self.pending_delete.as_deref() != Some(id)).then(|| id.to_owned());
        cx.notify();
    }

    /// Which row the pointer is over, so that row can show its delete control.
    ///
    /// An exit only clears the row it belongs to: when the pointer moves from one
    /// row to the next, both rows report in the same dispatch, and the one being
    /// left must not take the state from the one being entered.
    pub(crate) fn set_hovered_thread(&mut self, id: &str, hovered: bool, cx: &mut Context<Self>) {
        let next = if hovered {
            Some(id.to_owned())
        } else if self.hovered_thread.as_deref() == Some(id) {
            None
        } else {
            self.hovered_thread.clone()
        };
        if self.hovered_thread != next {
            self.hovered_thread = next;
            cx.notify();
        }
    }

    /// Removes a thread and everything the store keeps for it. The window never
    /// writes the store for a turn, but deleting is a thing the person asked for,
    /// so it is the window's write (see the store's `delete_thread`).
    ///
    /// If the thread was the open one the window starts a new, empty thread: the
    /// store is the source of truth, so the timeline cannot keep showing turns
    /// whose transcript is gone.
    pub(crate) fn confirm_delete_thread(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pending_delete = None;
        let was_open = self
            .session
            .as_ref()
            .is_some_and(|session| session.id == id);
        // Cancel before the row goes: the task is dropped on the foreground thread,
        // so it cannot come back to write a reply into a thread that is no longer
        // there.
        if was_open {
            self.cancel_turn();
        }

        let Some(store) = &self.store else {
            return;
        };
        if let Err(error) = store.delete_thread(id) {
            log::warn!("could not delete the thread {id}: {error:#}");
            cx.notify();
            return;
        }

        self.refresh_sessions(cx);
        if was_open {
            self.new_thread(window, cx);
        }
        cx.notify();
    }

    /// Creates the session for this conversation on first use, so an empty
    /// thread never shows up in the sidebar.
    fn ensure_session(&mut self, cx: &mut Context<Self>) -> Option<String> {
        if let Some(session) = &self.session {
            return Some(session.id.clone());
        }

        let model = self.model();
        let store = self.store.as_ref()?;
        match store.create_session(
            &self.workspace,
            Some(&self.config.provider.id),
            Some(&model),
        ) {
            Ok(session) => {
                self.session = Some(session.clone());
                self.refresh_sessions(cx);
                Some(session.id)
            }
            Err(error) => {
                log::warn!("could not start a session: {error:#}");
                None
            }
        }
    }

    fn refresh_sessions(&mut self, cx: &mut Context<Self>) {
        let Some(store) = &self.store else {
            return;
        };
        match store.sessions() {
            Ok(sessions) => {
                self.sessions = sessions;
                if let Some(current) = &self.session {
                    self.session = self
                        .sessions
                        .iter()
                        .find(|session| session.id == current.id)
                        .cloned();
                }
                cx.notify();
            }
            Err(error) => log::warn!("could not list sessions: {error:#}"),
        }
    }

    pub(crate) fn open_settings(&mut self, cx: &mut Context<Self>) {
        let root = cx.weak_entity();
        SettingsWindow::open(
            cx,
            self.profile.clone(),
            root,
            self.appearance,
            self.env.clone(),
            self.mcp.clone(),
        );
    }

    /// Takes up what the settings window wrote: the provider, and the model the
    /// profile now names. The copy of the configuration in memory is stale — the
    /// settings window writes `config.toml` directly — so it is read again.
    pub(crate) fn refresh_provider(&mut self, cx: &mut Context<Self>) {
        let previous = self.config.clone();
        if let Some(profile) = &self.profile {
            match profile.config() {
                Ok(config) => self.config = config,
                Err(error) => log::warn!("keeping the configuration in memory: {error:#}"),
            }
        }
        self.provider = default_provider(&self.profile, &self.config, &self.env);
        // The conversation holds the provider it was built with. A key saved or
        // reset produces a new one, and a thread that kept the old instance would
        // keep sending the old key.
        self.conversation
            .set_provider(self.provider.as_ref().ok().cloned());
        // The search tool is DeepSeek's, so it comes and goes with DeepSeek's key.
        self.conversation
            .set_search_key(deepseek_key(&self.profile, &self.config, &self.env));
        // A record that changed is a provider that has to be asked afresh; a switch
        // to another provider is not that, because each keeps its own record and its
        // own list. The key a listing was made with is what the picker checks.
        let mut reconfigured: Vec<String> = self
            .config
            .providers
            .iter()
            .filter(|(id, overrides)| previous.providers.get(*id) != Some(*overrides))
            .map(|(id, _)| id.clone())
            .collect();
        reconfigured.extend(
            previous
                .providers
                .keys()
                .filter(|id| !self.config.providers.contains_key(*id))
                .cloned(),
        );
        if previous.provider.id == self.config.provider.id
            && (previous.provider.base_url != self.config.provider.base_url
                || previous.provider.key_env != self.config.provider.key_env)
        {
            reconfigured.push(self.config.provider.id.clone());
        }
        for id in reconfigured {
            self.discovered.remove(&id);
            self.asked_with.remove(&id);
        }

        // A provider switch, or a model the profile now names, decides this thread's
        // model until the composer chip says otherwise. Saving a key changes neither,
        // so a reopened thread keeps the model it was using.
        let provider_changed = self.config.provider.id != previous.provider.id;
        let model_changed = self.config.provider.model != previous.provider.model;
        if provider_changed || model_changed {
            // The thread row is what a restart reads the thread back from, so it says
            // what the thread is actually talking to.
            if provider_changed
                && let (Some(session), Some(store)) = (&self.session, &self.store)
                && let Err(error) =
                    store.set_session_provider(&session.id, &self.config.provider.id)
            {
                log::warn!("could not record the provider of the thread: {error:#}");
            }
            if let Some(model) = configured_model(&self.config)
                && model != self.model
            {
                log::info!("following the profile to {model}");
                self.model = model.clone();
                self.conversation.set_model(model.clone());
                if let (Some(session), Some(store)) = (&self.session, &self.store)
                    && let Err(error) = store.set_session_model(&session.id, &model)
                {
                    log::warn!("could not record the model of the thread: {error:#}");
                }
            }
        }
        // The window may have changed on its own (the settings form saved one),
        // so this is applied even when the provider and model did not.
        self.apply_context_window(cx);
        cx.notify();
    }

    pub(crate) fn submit_draft(&mut self, cx: &mut Context<Self>) {
        // Enter belongs to the `@` menu while it is offering something: picking a
        // file is what the person is aiming at, not sending half a sentence.
        if let Some(path) = self.offered_file() {
            self.accept_file(path, cx);
            return;
        }
        let text = self.composer_input.read(cx).value().trim().to_owned();
        if text.is_empty() && self.attachments.is_empty() {
            return;
        }
        // Nothing can answer, so there is no turn to start: the draft stays, and the
        // list that can fix it opens. A turn that fails before it is sent looks like
        // the app tried a provider, which is exactly what it must not do.
        if self.provider.is_err() {
            self.open_model_menu_deferred(cx);
            return;
        }
        let attachments = std::mem::take(&mut self.attachments);
        self.attachment_notice = None;
        self.composer_input
            .update(cx, |input, cx| input.set_value("", cx));
        // A message sent while the turn is busy is kept and becomes the next turn;
        // dropping it on the floor was the old behaviour and it looked like a hang.
        if !self.pending_approvals.is_empty() || self.turn_running() {
            log::info!("queueing the message: the turn is still running");
            self.queued.push(QueuedMessage {
                text,
                attachments,
                steer: false,
            });
            cx.notify();
            return;
        }
        self.send(text, attachments, cx);
    }

    pub(crate) fn toggle_queued_steer(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(message) = self.queued.get_mut(index) {
            message.steer = !message.steer;
            cx.notify();
        }
    }

    pub(crate) fn remove_queued(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.queued.len() {
            self.queued.remove(index);
            cx.notify();
        }
    }

    /// Whether the turn should end at its next message boundary: only the front
    /// queued message can ask for that. Messages behind it wait their turn.
    pub(crate) fn front_wants_steer(&self) -> bool {
        self.queued.first().is_some_and(|message| message.steer)
    }

    /// Sends the front queued message as the next turn. The caller has decided the
    /// previous turn is over; the send is deferred so it does not replace the task
    /// that is calling this.
    fn send_queued_next(&mut self, cx: &mut Context<Self>) {
        if !self.pending_approvals.is_empty() {
            return;
        }
        if self.queued.is_empty() {
            return;
        }
        let next = self.queued.remove(0);
        log::info!("sending the queued message");
        let this = cx.entity();
        cx.defer(move |cx| {
            this.update(cx, |root, cx| root.send(next.text, next.attachments, cx));
        });
        cx.notify();
    }

    fn send(&mut self, text: String, attachments: Vec<Attachment>, cx: &mut Context<Self>) {
        log::info!("sending {}", text.chars().take(60).collect::<String>());
        let languages = self.languages.clone();
        let assistant = cx.new(|cx| Markdown::new_deferred("", Some(languages), cx));
        let answer_watch = Self::watch_answer(&assistant, cx);

        // Without a provider the turn never starts; the failure is shown where the
        // answer would have been.
        if let Err(error) = &self.provider {
            self.turns.push(Turn {
                user: text,
                attachments,
                assistant,
                assistant_text: String::new(),
                thinking: Vec::new(),
                state: TurnState::Failed(error.clone()),
                tool_steps: Vec::new(),
                expanded_steps: HashSet::new(),
                seq: None,
                _answer_watch: answer_watch,
            });
            cx.notify();
            return;
        }

        let turn_index = self.turns.len();
        self.turns.push(Turn {
            user: text.clone(),
            attachments: attachments.clone(),
            assistant: assistant.clone(),
            assistant_text: String::new(),
            thinking: Vec::new(),
            state: TurnState::Streaming,
            tool_steps: Vec::new(),
            expanded_steps: HashSet::new(),
            seq: None,
            _answer_watch: answer_watch,
        });
        self.pinned = true;
        self.scroll_handle.scroll_to_bottom();
        cx.notify();

        // The runtime writes into the session the window just opened, so the sidebar
        // row and the transcript agree on where this turn goes.
        if let Some(session) = self.ensure_session(cx) {
            self.conversation.attach_session(session);
        }
        // The message reaches the store before anything can fail, and before the
        // turn is built: a summary that goes first must still count this turn among
        // the recent ones.
        self.conversation.submit(&text, &attachments);
        if let Some(turn) = self.turns.get_mut(turn_index) {
            // The summary divider needs to know which turn this is in the store.
            turn.seq = self.conversation.last_turn_seq();
        }
        // A turn that would not fit is summarized before it is sent: the way Zed
        // compacts on the way into a request, rather than mid-answer.
        let plan = self.auto_compaction_plan();

        let task = cx.spawn(async move |this, cx| {
            if let Some(run) = plan {
                log::info!("compacting before this turn");
                let compaction = run.run().await;
                this.update(cx, |root, cx| {
                    if let Some(compaction) = compaction {
                        root.conversation.set_compaction(Some(compaction.clone()));
                        // The rendering copy: the divider reads it, and it never
                        // decides what the model sees.
                        root.compaction = Some(compaction);
                    }
                    cx.notify();
                })
                .ok();
            }

            // `send` already failed the turn when there is no provider, so this only
            // comes back empty if the window went away.
            let prepared = this.update(cx, |root, _| {
                root.conversation.start_turn(root.approved_tools.clone())
            });
            let Ok(Some(run)) = prepared else {
                return;
            };

            let background = OnGpui {
                executor: cx.background_executor().clone(),
            };
            let mut host = TurnSink {
                this: this.clone(),
                sink: Sink::Turn(turn_index),
                cx,
            };
            let outcome = run.run(&mut host, &background).await;
            // The sink held the context for as long as the turn ran.
            let cx = host.cx;

            this.update(cx, |root, cx| {
                root.conversation.finish_turn(&outcome);
                root.settle_turn(turn_index, &outcome, cx);
                root.refresh_sessions(cx);
            })
            .ok();
        });
        self.turn = Some(task);
    }

    /// Drops the running turn and everything it started: a subagent runs on the
    /// turn's behalf, so it stops with it. What a subagent already did
    /// stays on screen; only the work stops.
    fn cancel_turn(&mut self) {
        self.turn = None;
        for subagent in &mut self.subagents {
            subagent.task = None;
            if matches!(subagent.work.state, TurnState::Streaming) {
                subagent.work.state = TurnState::Stopped;
                subagent.work.stop_thinking();
            }
        }
    }

    /// Whether a turn is still running. A finished task stays in the field until
    /// the next turn replaces it, so the button follows readiness, not presence.
    pub(crate) fn turn_running(&self) -> bool {
        self.turn.as_ref().is_some_and(|task| !task.is_ready())
    }

    /// Stops the running turn. The reply is not appended to the transcript — only
    /// complete replies are — and any call that was mid-flight is
    /// marked as not completed.
    pub(crate) fn stop_turn(&mut self, cx: &mut Context<Self>) {
        if !self.turn_running() {
            return;
        }
        log::info!("stopping the running turn");
        self.cancel_turn();
        self.pending_approvals.clear();
        self.pending_question = None;
        if let Some(turn) = self.turns.last_mut()
            && matches!(turn.state, TurnState::Streaming)
        {
            turn.state = TurnState::Stopped;
            turn.stop_thinking();
            for step in &mut turn.tool_steps {
                if matches!(step.state, StepState::Awaiting | StepState::Running) {
                    step.state = StepState::Done { failed: true };
                }
            }
        }
        cx.notify();
        self.send_queued_next(cx);
    }

    /// Asks the platform for a directory and, if the profile accepts it, starts a
    /// new thread there. Changing the workspace never re-roots an open thread.
    pub(crate) fn choose_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose a workspace".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            this.update_in(cx, |this, window, cx| {
                this.switch_workspace(path, window, cx)
            })
            .ok();
        })
        .detach();
    }

    pub(crate) fn switch_workspace(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let accepted = match &self.profile {
            Some(profile) => profile.accept_workspace(&path),
            None => Ok(()),
        };
        if let Err(error) = accepted {
            log::warn!("refusing the workspace: {error:#}");
            self.workspace_notice = Some(format!("{error:#}"));
            cx.notify();
            return;
        }

        let workspace = path.to_string_lossy().into_owned();
        if workspace == self.workspace {
            self.workspace_notice = None;
            cx.notify();
            return;
        }

        log::info!("switching to the workspace {workspace}");
        self.workspace_notice = None;
        self.workspace = workspace;
        self.new_thread(window, cx);
        cx.notify();
    }

    /// Asks the platform for files and attaches what comes back. The picker is
    /// the platform's, so a directory, a package or an alias behaves the way it
    /// does in every other app.
    pub(crate) fn choose_attachments(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Attach files".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            this.update_in(cx, |this, _, cx| {
                for path in paths {
                    this.attach(&path, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Reads one file and puts it on the message being written. A refusal is a
    /// sentence above the composer, never a silently dropped attachment.
    pub(crate) fn attach(&mut self, path: &Path, cx: &mut Context<Self>) {
        if self.attachments.len() >= MAX_ATTACHMENTS {
            self.attachment_notice = Some(format!(
                "A message can carry at most {MAX_ATTACHMENTS} attachments."
            ));
            cx.notify();
            return;
        }
        match load_attachment(&self.workspace, path) {
            Ok(attachment) => {
                log::info!("attaching {}", attachment.name());
                self.attachments.push(attachment);
                self.attachment_notice = None;
            }
            Err(reason) => {
                log::warn!("not attaching {}: {reason}", path.display());
                self.attachment_notice = Some(reason);
            }
        }
        cx.notify();
    }

    /// Takes an image pasted from the clipboard: the platform hands the bytes,
    /// so the composer holds it the same way it holds a picked file.
    pub(crate) fn attach_image(&mut self, image: &gpui::Image, cx: &mut Context<Self>) {
        if self.attachments.len() >= MAX_ATTACHMENTS {
            self.attachment_notice = Some(format!(
                "A message can carry at most {MAX_ATTACHMENTS} attachments."
            ));
            cx.notify();
            return;
        }
        if image.bytes.len() as u64 > MAX_IMAGE_BYTES {
            self.attachment_notice = Some(format!(
                "The pasted image is larger than {} MiB.",
                MAX_IMAGE_BYTES / (1024 * 1024)
            ));
            cx.notify();
            return;
        }
        let name = format!(
            "pasted-{}.{}",
            self.attachments.len() + 1,
            image.format.extension()
        );
        log::info!("attaching {name}");
        self.attachments.push(Attachment::Image {
            name,
            media_type: image.format.mime_type().to_owned(),
            data: base64::engine::general_purpose::STANDARD.encode(&image.bytes),
        });
        self.attachment_notice = None;
        cx.notify();
    }

    pub(crate) fn remove_attachment(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.attachments.len() {
            self.attachments.remove(index);
            self.attachment_notice = None;
            cx.notify();
        }
    }

    /// Keeps the `@` menu in step with the text: it opens on a trailing token
    /// starting with `@`, filters as the token grows, and closes when the token
    /// goes away, a file is picked, or Escape dismissed it.
    fn update_file_menu(&mut self, text: &str, cx: &mut Context<Self>) {
        let Some(query) = trailing_file_query(text).map(str::to_owned) else {
            self.close_file_menu(cx);
            self.dismissed_file_query = None;
            return;
        };
        if self.dismissed_file_query.as_deref() == Some(query.as_str()) {
            self.file_query = None;
            self.file_visible.clear();
            cx.notify();
            return;
        }
        if self.file_query.as_deref() == Some(query.as_str()) {
            return;
        }

        let workspace = self.workspace.clone();
        let choices = self.file_choices.get_or_insert_with(|| {
            humibird_tools::workspace_files(Path::new(&workspace), FILE_LIST_LIMIT)
        });
        let needle = query.to_lowercase();
        self.file_visible = choices
            .iter()
            .filter(|path| path.to_lowercase().contains(&needle))
            .take(FILE_MENU_ROWS)
            .cloned()
            .collect();
        self.file_query = Some(query);
        cx.notify();
    }

    fn close_file_menu(&mut self, cx: &mut Context<Self>) {
        if self.file_query.take().is_some() || !self.file_visible.is_empty() {
            self.file_visible.clear();
            self.file_choices = None;
            cx.notify();
        }
    }

    /// Takes one file from the menu: the `@` token becomes its path, and the file
    /// is attached so the model has the content now instead of a tool call later.
    pub(crate) fn accept_file(&mut self, path: String, cx: &mut Context<Self>) {
        let text = self.composer_input.read(cx).value().to_owned();
        let start = trailing_token_start(&text);
        let next = format!("{}{} ", &text[..start], path);
        self.composer_input
            .update(cx, |input, cx| input.set_value(next, cx));
        let full = Path::new(&self.workspace).join(&path);
        self.attach(&full, cx);
        self.close_file_menu(cx);
        self.dismissed_file_query = None;
        cx.notify();
    }

    pub(crate) fn dismiss_file_menu(&mut self, cx: &mut Context<Self>) {
        if self.file_query.is_some() {
            self.dismissed_file_query = self.file_query.clone();
            self.close_file_menu(cx);
        }
    }

    /// Whether the `@` menu is open, which is what the composer and its Escape
    /// handler read.
    pub(crate) fn file_menu_open(&self) -> bool {
        self.file_query.is_some()
    }

    /// Whether the `@` menu has anything to offer. Only then does Enter pick a
    /// file instead of sending the message.
    fn offered_file(&self) -> Option<String> {
        self.file_query
            .as_ref()
            .and_then(|_| self.file_visible.first().cloned())
    }

    fn answer_approval(&mut self, decision: Decision, cx: &mut Context<Self>) {
        let Some(mut pending) = self.pending_approvals.pop_front() else {
            return;
        };
        // "Always" is this session's allow-list: the turn is told by the decision,
        // and every turn after it starts from this set.
        if decision == Decision::Always {
            self.approved_tools.insert(pending.key.clone());
        }
        if let Some(respond) = pending.respond.take() {
            // A closed channel means the turn is already gone; nothing to tell.
            let _ = respond.send(decision);
        }
        cx.notify();
    }

    /// Puts the question on its step and starts waiting. The field is emptied and
    /// focused first, so an answer meant for the previous question is never sent.
    fn install_question(
        &mut self,
        question: Question,
        respond: oneshot::Sender<Answer>,
        cx: &mut Context<Self>,
    ) {
        self.answer_input
            .update(cx, |input, cx| input.set_value("", cx));
        let focus = question
            .allow_free_text
            .then(|| self.answer_input.read(cx).focus_handle(cx));

        self.pending_question = Some(PendingQuestion {
            question: question.question,
            options: question.options,
            allow_free_text: question.allow_free_text,
            input: self.answer_input.clone(),
            respond: Some(respond),
        });
        self.follow();
        cx.notify();

        if let Some(focus) = focus {
            self.focus_in_chat_window(focus, cx);
        }
    }

    /// Answers the pending question, if there is one. A closed channel means the
    /// turn is already gone, the same as for approval.
    fn answer_question(&mut self, answer: Answer, cx: &mut Context<Self>) {
        let Some(mut pending) = self.pending_question.take() else {
            return;
        };
        if let Some(respond) = pending.respond.take() {
            // A closed channel means the turn is already gone; nothing to tell.
            let _ = respond.send(answer);
        }
        self.answer_input
            .update(cx, |input, cx| input.set_value("", cx));
        self.focus_composer(cx);
        cx.notify();
    }

    /// The answer typed into the field. Blank input is not an answer: the card
    /// stays until the user types something or skips.
    fn submit_answer(&mut self, cx: &mut Context<Self>) {
        let text = self.answer_input.read(cx).value().trim().to_owned();
        if text.is_empty() {
            return;
        }
        self.answer_question(Answer::Selected(text), cx);
    }

    /// Focuses a field in the chat window. The window comes from the window list
    /// rather than from the platform's active window: that is `None` in tests and
    /// can be the settings window in the app. Deferred, so focus does not move
    /// during the click that caused it.
    fn focus_in_chat_window(&self, focus: FocusHandle, cx: &mut Context<Self>) {
        cx.defer(move |cx| {
            for window in cx.windows() {
                let Some(window) = window.downcast::<Self>() else {
                    continue;
                };
                window
                    .update(cx, |_, window, cx| window.focus(&focus, cx))
                    .log_err();
                return;
            }
        });
    }

    /// Hands the keyboard back to the composer after an answer.
    fn focus_composer(&self, cx: &mut Context<Self>) {
        let focus = self.composer_input.read(cx).focus_handle(cx);
        self.focus_in_chat_window(focus, cx);
    }

    /// Follows new output, unless the user has scrolled back to read something:
    /// a streaming answer or a stretch of thinking must not pull the conversation
    /// out from under them.
    fn follow(&self) {
        if self.pinned {
            self.scroll_handle.scroll_to_bottom();
        }
    }

    /// A scroll happened. Reading the new position is deferred because the scroll
    /// machinery still holds it while the event is dispatched.
    fn scrolled(&mut self, cx: &mut Context<Self>) {
        let this = cx.entity().downgrade();
        cx.defer(move |cx| {
            this.update(cx, |root, cx| {
                root.pinned = at_end(&root.scroll_handle);
                cx.notify();
            })
            .ok();
        });
    }

    /// Keeps following an answer that is still being laid out. The answer renders
    /// itself, so its markdown settles after the root has already rendered: the
    /// last paragraph would land below the fold with nobody left to follow it.
    fn watch_answer(assistant: &Entity<Markdown>, cx: &mut Context<Self>) -> Subscription {
        cx.observe(assistant, |root, _, _| root.follow())
    }

    /// Applies one thing the conversation did. The window is a projection: it
    /// holds what to draw, never what the model sees.
    fn apply(&mut self, sink: Sink, event: AgentEvent, cx: &mut Context<Self>) {
        match event {
            AgentEvent::Text(delta) => {
                if let Some(work) = self.work_mut(sink) {
                    // The answer starts where the thinking stopped.
                    work.stop_thinking();
                    work.assistant_text.push_str(&delta);
                    let assistant = work.assistant.clone();
                    assistant.update(cx, |markdown, cx| markdown.append(&delta, cx));
                }
            }
            AgentEvent::Thinking(delta) => {
                // Only the person's own conversation shows what the model thought: a
                // subagent's reasoning in a card would be noise around its report.
                if let Sink::Turn(turn) = sink
                    && let Some(current) = self.turns.get_mut(turn)
                {
                    current.think(&delta);
                }
            }
            AgentEvent::Step { index, step } => {
                if let Some(work) = self.work_mut(sink) {
                    // The model is acting, so the stretch of thinking that led here
                    // is over.
                    work.stop_thinking();
                    debug_assert_eq!(index, work.tool_steps.len(), "steps arrive in order");
                    work.tool_steps.push(step);
                }
            }
            AgentEvent::StepState { index, state } => {
                if let Some(step) = self
                    .work_mut(sink)
                    .and_then(|work| work.tool_steps.get_mut(index))
                {
                    step.state = state;
                }
            }
            AgentEvent::StepDone {
                index,
                failed,
                result,
            } => {
                if let Some(step) = self
                    .work_mut(sink)
                    .and_then(|work| work.tool_steps.get_mut(index))
                {
                    step.state = StepState::Done { failed };
                    step.result = result;
                }
            }
            AgentEvent::Approval {
                name,
                key,
                summary,
                respond,
                ..
            } => {
                self.pending_approvals.push_back(PendingApproval {
                    name,
                    key,
                    summary,
                    respond: Some(respond),
                });
            }
            AgentEvent::Question {
                question, respond, ..
            } => {
                self.install_question(question, respond, cx);
            }
            AgentEvent::Delegate {
                call,
                delegation,
                respond,
                ..
            } => match sink {
                // Depth is one: a subagent is not offered this tool, so this is a
                // broken definition rather than a task to run.
                Sink::Delegation { .. } => cancel_delegation(respond, "a subagent cannot delegate"),
                Sink::Turn(turn) => {
                    let step = self
                        .work_mut(sink)
                        .map_or(0, |work| work.tool_steps.len().saturating_sub(1));
                    self.delegate(turn, step, &call, delegation, respond, cx);
                }
            },
        }
        self.follow();
        cx.notify();
    }

    /// The projection these events belong to: the person's turn, or the card of a
    /// task it delegated.
    fn work_mut(&mut self, sink: Sink) -> Option<&mut Turn> {
        match sink {
            Sink::Turn(turn) => self.turns.get_mut(turn),
            Sink::Delegation { turn, step } => self
                .subagents
                .iter_mut()
                .find(|subagent| subagent.turn == turn && subagent.step == step)
                .map(|subagent| &mut subagent.work),
        }
    }

    /// Stops one delegated task. The parent learns about it as a cancelled call, so
    /// the turn carries on instead of waiting for a report that will never come.
    pub(crate) fn stop_subagent(&mut self, turn: usize, step: usize, cx: &mut Context<Self>) {
        if let Some(subagent) = self
            .subagents
            .iter_mut()
            .find(|subagent| subagent.turn == turn && subagent.step == step)
        {
            log::info!("stopping the subagent for step {step}");
            subagent.task = None;
            subagent.work.state = TurnState::Stopped;
            subagent.work.stop_thinking();
            cx.notify();
        }
    }

    /// Runs a delegated task: a second conversation over the same profile and
    /// workspace, with its own context, whose report is the parent's tool result.
    /// Its approvals are the window's, because the parent is blocked and
    /// there is nobody else to ask; its own work is shown in the step that delegated
    /// it.
    fn delegate(
        &mut self,
        turn: usize,
        step: usize,
        call: &str,
        delegation: Delegation,
        respond: oneshot::Sender<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(provider) = self.provider.as_ref().ok().cloned() else {
            cancel_delegation(respond, "there is no model provider");
            return;
        };
        let mut conversation = Conversation::subagent(
            self.profile.clone(),
            self.workspace.clone(),
            self.model.clone(),
            Some(provider),
            self.store.clone(),
        );
        conversation.set_search_key(self.search_key());
        // The subagent writes a session of its own, owned by this thread and by the
        // call that delegated it, so what it did outlives the window.
        if let (Some(store), Some(parent)) = (&self.store, &self.session)
            && let Err(error) = store
                .create_subagent_session(
                    &parent.id,
                    call,
                    &self.workspace,
                    Some(&self.config.provider.id),
                    Some(&self.model),
                )
                .map(|session| conversation.attach_session(session.id))
        {
            log::warn!("could not record the subagent's session: {error:#}");
        }
        conversation.submit(&delegation.prompt, &[]);
        let Some(run) = conversation.start_turn(self.approved_tools.clone()) else {
            cancel_delegation(respond, "the subagent could not start");
            return;
        };
        log::info!("delegating step {step}: {}", delegation.description);

        let languages = self.languages.clone();
        let report = cx.new(|cx| Markdown::new_deferred("", Some(languages), cx));
        let watch = Self::watch_answer(&report, cx);
        let work = Turn {
            user: delegation.prompt.clone(),
            attachments: Vec::new(),
            assistant: report,
            assistant_text: String::new(),
            thinking: Vec::new(),
            state: TurnState::Streaming,
            tool_steps: Vec::new(),
            expanded_steps: HashSet::new(),
            seq: None,
            _answer_watch: watch,
        };

        let background = OnGpui {
            executor: cx.background_executor().clone(),
        };
        let task = cx.spawn(async move |this, cx| {
            let mut sink = TurnSink {
                this: this.clone(),
                sink: Sink::Delegation { turn, step },
                cx,
            };
            let outcome = run.run(&mut sink, &background).await;
            let cx = sink.cx;
            // The subagent's own record: its report and the calls behind it, written
            // to the session it was given.
            conversation.finish_turn(&outcome);

            let text = match &outcome {
                TurnOutcome::Done { reply, .. } => reply.clone(),
                TurnOutcome::Failed { message, .. } => format!("error: {message}"),
            };
            this.update(cx, |root, cx| {
                if let Some(subagent) = root
                    .subagents
                    .iter_mut()
                    .find(|subagent| subagent.turn == turn && subagent.step == step)
                {
                    subagent.work.state = match &outcome {
                        TurnOutcome::Done { .. } => TurnState::Done,
                        TurnOutcome::Failed { message, .. } => TurnState::Failed(message.clone()),
                    };
                    subagent.work.stop_thinking();
                }
                cx.notify();
            })
            .ok();
            if respond.send(text).is_err() {
                log::debug!("the parent turn ended before the subagent answered");
            }
        });

        self.subagents.push(Subagent {
            turn,
            step,
            work,
            task: Some(task),
        });
        cx.notify();
    }

    /// Applies what a turn ended with, and lets the queue move on.
    fn settle_turn(&mut self, turn: usize, outcome: &TurnOutcome, cx: &mut Context<Self>) {
        if let Some(current) = self.turns.get_mut(turn) {
            match outcome {
                TurnOutcome::Done { .. } => {
                    current.state = TurnState::Done;
                    // The answer is what the user came for; the thinking folds away
                    // and stays one click from view.
                    current.stop_thinking();
                }
                TurnOutcome::Failed { message, .. } => {
                    current.state = TurnState::Failed(message.clone())
                }
            }
        }
        self.follow();
        self.send_queued_next(cx);
        cx.notify();
    }

    /// Shows or hides one stretch of thinking. View state only: the reasoning is
    /// never sent back and never written to the transcript.
    pub(crate) fn toggle_thinking(&mut self, turn: usize, chunk: usize, cx: &mut Context<Self>) {
        if let Some(turn) = self.turns.get_mut(turn) {
            turn.toggle_thinking(chunk);
            cx.notify();
        }
    }

    /// The model this thread talks to.
    pub(crate) fn model(&self) -> String {
        self.model.clone()
    }

    fn search_key(&self) -> Option<String> {
        deepseek_key(&self.profile, &self.config, &self.env)
    }

    /// Switches the thread to a model of another provider, or to another model of
    /// the one it is on. The provider being left keeps its endpoint and window; the
    /// new one starts from its own record.
    pub(crate) fn set_model(
        &mut self,
        provider: String,
        model: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.model_menu_open = false;
        self.focus_composer_if_the_menu_had_it(window, cx);
        // The picker always names a model; a call without one has nothing to do.
        if model.is_empty() {
            cx.notify();
            return;
        }
        let switching = provider != self.config.provider.id;
        if !switching && self.model == model {
            cx.notify();
            return;
        }
        log::info!("switching this thread to {provider}/{model}");
        if switching {
            self.config.keep_overrides_with_their_provider();
            self.config.provider.id = provider;
            self.config.provider.model = None;
            if let (Some(session), Some(store)) = (&self.session, &self.store)
                && let Err(error) =
                    store.set_session_provider(&session.id, &self.config.provider.id)
            {
                log::warn!("could not record the provider of the thread: {error:#}");
            }
        }
        self.model = model.clone();
        self.conversation.set_model(model.clone());
        // The next thread and the next launch start here too.
        self.config.provider.model = Some(model.clone());
        if let Some(profile) = &self.profile
            && let Err(error) = profile.save_config(&self.config)
        {
            log::warn!("could not save the model: {error:#}");
        }
        if switching {
            self.refresh_provider(cx);
        } else {
            self.apply_context_window(cx);
        }
        if let Some(session) = &self.session
            && let Some(store) = &self.store
            && let Err(error) = store.set_session_model(&session.id, &model)
        {
            log::warn!("could not record the model of the thread: {error:#}");
        }
        cx.notify();
    }

    /// Whether the picker may offer a provider, the way Zed's picker means it: the
    /// provider in use resolved, a key resolves for another one, or — for a provider
    /// that wants no key — it named a model of its own or answered with a list.
    fn provider_ready(&self, preset: &humibird_providers::Preset) -> bool {
        if preset.id == self.config.provider.id && self.provider.is_ok() {
            return true;
        }
        let overrides = self.config.overrides(preset.id);
        if preset.key_env.is_some() || overrides.key_env.is_some() {
            return self.provider_key(preset).is_some();
        }
        overrides.model.is_some()
            || self
                .discovered
                .get(preset.id)
                .is_some_and(|models| !models.is_empty())
    }

    /// The picker's rows, grouped by every provider that can answer.
    fn model_groups(&self) -> Vec<ModelGroup> {
        let named: BTreeMap<&str, &str> = self
            .config
            .providers
            .iter()
            .filter_map(|(id, overrides)| {
                overrides.model.as_deref().map(|model| (id.as_str(), model))
            })
            .collect();
        model_groups(
            &self.model,
            &self.config.provider.id,
            &self.discovered,
            &named,
            |preset| self.provider_ready(preset),
        )
    }

    /// Opens the model list from a place that has no window: Enter is handled by the
    /// composer input's listener, which is not given one.
    fn open_model_menu_deferred(&mut self, cx: &mut Context<Self>) {
        if self.model_menu_open {
            return;
        }
        let Some(window) = self.window else {
            return;
        };
        let root = cx.weak_entity();
        cx.defer(move |cx| {
            let opened = window.update(cx, |_, window, cx| {
                root.update(cx, |root, cx| root.toggle_model_menu(window, cx))
            });
            if let Err(error) = opened.and_then(|inner| inner) {
                log::warn!("could not open the model list: {error:#}");
            }
        });
    }

    pub(crate) fn toggle_thinking_mode(&mut self, cx: &mut Context<Self>) {
        self.config.thinking.enabled = !self.config.thinking.enabled;
        if !self.config.thinking.enabled {
            self.thinking_menu_open = false;
        }
        self.apply_thinking(cx);
    }

    pub(crate) fn set_thinking_effort(&mut self, effort: ProfileEffort, cx: &mut Context<Self>) {
        self.thinking_menu_open = false;
        if self.config.thinking.effort == effort {
            cx.notify();
            return;
        }
        self.config.thinking.effort = effort;
        self.apply_thinking(cx);
    }

    pub(crate) fn toggle_thinking_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.thinking_menu_open = !self.thinking_menu_open;
        if self.thinking_menu_open {
            self.close_model_menu(window, cx);
        }
        cx.notify();
    }

    pub(crate) fn is_thinking_menu_open(&self) -> bool {
        self.thinking_menu_open
    }

    pub(crate) fn close_thinking_menu(&mut self, cx: &mut Context<Self>) {
        if self.thinking_menu_open {
            self.thinking_menu_open = false;
            cx.notify();
        }
    }

    /// Hands the switch to the next turn and keeps it in `config.toml`. The
    /// prompt does not name it, so a change costs no cached prefix.
    fn apply_thinking(&mut self, cx: &mut Context<Self>) {
        log::info!(
            "thinking {} at {:?}",
            self.config.thinking.enabled,
            self.config.thinking.effort
        );
        self.conversation
            .set_thinking(Some(thinking_request(&self.config)));
        if let Some(profile) = &self.profile
            && let Err(error) = profile.save_config(&self.config)
        {
            log::warn!("could not save the thinking setting: {error:#}");
        }
        cx.notify();
    }

    /// Keeps the conversation measuring against the best window known right now:
    /// the listing can arrive after the conversation was built, and a model switch
    /// changes which entry applies.
    fn apply_context_window(&mut self, cx: &mut Context<Self>) {
        let window = resolve_context_window(&self.config, &self.discovered, &self.model);
        self.conversation.set_context_window(window);
        cx.notify();
    }

    /// The key a provider's listing would be made with under the configuration as
    /// it stands.
    fn provider_key(&self, preset: &humibird_providers::Preset) -> Option<String> {
        let overrides = self.config.overrides(preset.id);
        let variable = overrides.key_env.as_deref().or(preset.key_env);
        let profile = self.profile.as_ref()?;
        variable.and_then(|variable| profile.secret_with(variable, |name| (self.env)(name)))
    }

    /// Asks every provider this profile can reach which models it serves. A
    /// provider is asked once per key: a key saved or reset in Settings changes it,
    /// and the next open asks again. A failed request is a log line and a retry on
    /// the next open, because nothing was learned.
    fn ensure_models(&mut self, cx: &mut Context<Self>) {
        for preset in humibird_providers::PRESETS {
            if self.asking.contains(preset.id) {
                continue;
            }
            let in_use = preset.id == self.config.provider.id;
            let overrides = self.config.overrides(preset.id);
            let key = self.provider_key(preset);
            let needs_key = preset.key_env.is_some() || overrides.key_env.is_some();
            if needs_key && key.is_none() && !(in_use && self.provider.is_ok()) {
                // No key resolves and there is nothing built to ask, so anything a
                // key once told us is not an answer for the profile as it stands.
                self.discovered.remove(preset.id);
                self.asked_with.remove(preset.id);
                continue;
            }
            // A provider that wants no key is asked only once the profile names where
            // it is. Probing every local port on every open is noise, and a failed
            // probe is not a model list.
            if !needs_key && !in_use && overrides.base_url.is_none() {
                continue;
            }
            if self.asked_with.get(preset.id) == Some(&key) {
                continue;
            }

            // The provider in use carries the profile's endpoint and model
            // overrides; the others are built from their presets only to list. A
            // provider that cannot be built for a turn still has a list to give —
            // no key yet, or no model chosen — so the picker can offer one.
            let provider = if in_use {
                self.provider.as_ref().ok().cloned()
            } else {
                None
            }
            .or_else(|| (self.listing_provider)(preset).ok());
            let Some(models) = provider.and_then(|provider| provider.list_models()) else {
                continue;
            };

            self.asking.insert(preset.id.to_owned());
            let asked_for = preset.id.to_owned();
            cx.spawn(async move |this, cx| {
                let result = models.await;
                this.update(cx, |root, cx| {
                    root.asking.remove(&asked_for);
                    match result {
                        Ok(models) => {
                            log::info!("{} named {} models", asked_for, models.len());
                            root.discovered.insert(asked_for.clone(), models);
                            root.asked_with.insert(asked_for.clone(), key);
                            // The listing may be the first word on this model's window.
                            if asked_for == root.config.provider.id {
                                root.apply_context_window(cx);
                            }
                        }
                        Err(error) => {
                            log::warn!("could not list {asked_for}'s models: {error:#}");
                            root.discovered.remove(&asked_for);
                            root.asked_with.remove(&asked_for);
                        }
                    }
                    cx.notify();
                })
                .ok();
            })
            .detach();
        }
    }

    pub(crate) fn toggle_model_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.model_menu_open {
            self.close_model_menu(window, cx);
            return;
        }
        self.thinking_menu_open = false;
        self.ensure_models(cx);
        let groups = self.model_groups();
        self.model_menu_highlight = model_rows(&groups)
            .iter()
            .position(|(provider, model)| {
                *provider == self.config.provider.id && *model == self.model
            })
            .unwrap_or(0);
        self.model_menu_open = true;
        // The list takes focus so the arrow keys and Enter reach it instead of
        // moving the caret in the draft.
        window.focus(&self.model_menu_focus, cx);
        cx.notify();
    }

    pub(crate) fn close_model_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.model_menu_open {
            return;
        }
        self.model_menu_open = false;
        self.focus_composer_if_the_menu_had_it(window, cx);
        cx.notify();
    }

    fn focus_composer_if_the_menu_had_it(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.model_menu_focus.is_focused(window) {
            let focus = self.composer_input.read(cx).focus_handle(cx);
            window.focus(&focus, cx);
        }
    }

    pub(crate) fn move_model_highlight(&mut self, delta: isize, cx: &mut Context<Self>) {
        let groups = self.model_groups();
        let count = model_rows(&groups).len();
        if count == 0 {
            return;
        }
        let current = self.model_menu_highlight.min(count - 1) as isize;
        self.model_menu_highlight = (current + delta).rem_euclid(count as isize) as usize;
        cx.notify();
    }

    pub(crate) fn accept_model_highlight(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let groups = self.model_groups();
        match model_rows(&groups)
            .into_iter()
            .nth(self.model_menu_highlight)
        {
            Some((provider, model)) => self.set_model(provider.to_owned(), model, window, cx),
            None => self.close_model_menu(window, cx),
        }
    }

    /// A compaction the next turn should make before it is sent. The runtime
    /// decides from the size the provider reported; the window only drives the
    /// request it hands back. There is no way to ask for one by hand: a summary is
    /// worth its extra request only when the thread is about to run out of room.
    fn auto_compaction_plan(&mut self) -> Option<CompactionRun> {
        if !self.conversation.compaction_due() {
            return None;
        }
        self.conversation.compaction_run()
    }

    /// Whether a turn has been replaced in the request by the summary.
    pub(crate) fn is_covered(&self, turn: &Turn) -> bool {
        self.compaction
            .as_ref()
            .is_some_and(|compaction| turn.seq.is_some_and(|seq| seq <= compaction.upto_seq))
    }

    /// Shows or hides what a call was given and what it returned. View state only.
    pub(crate) fn toggle_step(&mut self, turn: usize, step: usize, cx: &mut Context<Self>) {
        if let Some(turn) = self.turns.get_mut(turn) {
            if !turn.expanded_steps.remove(&step) {
                turn.expanded_steps.insert(step);
            }
            cx.notify();
        }
    }

    fn toggle_appearance(&mut self, cx: &mut Context<Self>) {
        self.appearance = self.appearance.toggled();
        self.config.theme = Theme::from(self.appearance);
        if let Some(profile) = &self.profile
            && let Err(error) = profile.save_config(&self.config)
        {
            log::warn!("could not save the theme: {error:#}");
        }

        let appearance = self.appearance;
        for window in settings_windows(cx) {
            window
                .update(cx, |settings, _, cx| {
                    settings.set_appearance(appearance, cx)
                })
                .log_err();
        }
    }

    fn header(&self, cx: &mut Context<Self>, theme: Palette) -> impl IntoElement {
        let title = self
            .session
            .as_ref()
            .and_then(|session| session.title.clone())
            .unwrap_or_else(|| "New thread".to_owned());
        let workspace = display_workspace(&self.workspace);
        let workspace_dir = workspace_name(&self.workspace);

        div()
            .id("thread-header")
            .debug_selector(|| "thread-header".into())
            .flex_none()
            .flex()
            .items_center()
            .justify_between()
            .h(px(54.))
            .px(px(18.))
            .border_b_1()
            .border_color(rgb(theme.border))
            .child(
                div()
                    .id("thread-header-title")
                    .debug_selector(|| "thread-header-title".into())
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(px(13.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(title),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .text_size(px(10.5))
                            .text_color(rgb(theme.text_muted))
                            .child(
                                div()
                                    .id("header-workspace")
                                    .debug_selector(move || {
                                        format!("header-workspace-{workspace_dir}")
                                    })
                                    .min_w_0()
                                    .flex()
                                    .items_center()
                                    .gap(px(4.))
                                    .px(px(5.))
                                    .py(px(1.))
                                    .rounded_sm()
                                    .cursor_pointer()
                                    .hover(move |this| {
                                        this.bg(rgb(theme.hover_bg)).text_color(rgb(theme.text))
                                    })
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.choose_workspace(window, cx)
                                    }))
                                    .child("Workspace")
                                    .child("·")
                                    .child(div().min_w_0().truncate().child(workspace))
                                    .child(
                                        Icon::new(IconName::ChevronDown)
                                            .size(IconSize::Indicator)
                                            .color(Color::Muted),
                                    ),
                            ),
                    ),
            )
            .child(
                div()
                    .id("thread-header-actions")
                    .debug_selector(|| "thread-header-actions".into())
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(theme_toggle(
                        self.appearance,
                        theme,
                        cx.listener(|this, _: &ClickEvent, _: &mut Window, cx| {
                            this.toggle_appearance(cx);
                            zed_theme::apply(
                                cx,
                                this.appearance,
                                Palette::for_appearance(this.appearance),
                            );
                            cx.notify();
                        }),
                    )),
            )
    }
}

impl Render for HumibirdRoot {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.window = Some(window.window_handle());
        let theme = Palette::for_appearance(self.appearance);
        let composer_input = self.composer_input.clone();
        let model = self.model();
        let provider = self.config.provider.id.clone();
        let groups = self.model_groups();
        let summarized = self
            .turns
            .iter()
            .filter(|turn| self.is_covered(turn))
            .count();

        let prompt = timeline::prompt(
            theme,
            self.pending_approvals.front(),
            self.pending_question.as_ref(),
            cx,
        );

        let main = div()
            .id("main")
            .debug_selector(|| "main".into())
            .flex()
            .flex_col()
            .flex_1()
            .h_full()
            .min_w_0()
            .child(self.header(cx, theme))
            .child(timeline::render(
                theme,
                &self.turns,
                timeline::Notice {
                    summarized,
                    without_provider: self.provider.as_ref().err().map(String::as_str),
                },
                &self.scroll_handle,
                self.conversation.tools(),
                &self.subagents,
                cx,
            ))
            .children(prompt.map(|prompt| {
                div().flex_none().px(px(20.)).pb(px(2.)).child(
                    div()
                        .w_full()
                        .max_w(px(720.))
                        .min_w_0()
                        .mx_auto()
                        .child(prompt),
                )
            }))
            .child(composer::render(
                theme,
                composer_input,
                composer::ComposerState {
                    model: &model,
                    provider_ready: self.provider.is_ok(),
                    provider_id: &provider,
                    groups: &groups,
                    // Shown from the first frame: a thread with no reply yet is at
                    // 0%, which is true, and hiding it made the control look absent.
                    context: self.conversation.context_window().map(|limit| {
                        composer::ContextUsage {
                            used: self.conversation.context_tokens(),
                            limit,
                        }
                    }),
                    context_hovered: self.context_meter_hovered,
                    thinking: composer::ThinkingChoice {
                        enabled: self.config.thinking.enabled,
                        effort: self.config.thinking.effort,
                        menu_open: self.thinking_menu_open,
                        available: thinking_available(&self.config),
                    },
                    menu: self.model_menu_open.then_some(composer::ModelMenu {
                        focus: &self.model_menu_focus,
                        highlight: self.model_menu_highlight,
                    }),
                    attachments: &self.attachments,
                    files: self.file_menu_open().then_some(composer::FileMenu {
                        choices: &self.file_visible,
                    }),
                    notice: self
                        .attachment_notice
                        .as_deref()
                        .or(self.mcp_notice.as_deref())
                        .or(self.workspace_notice.as_deref()),
                    running: self.turn_running(),
                    queued: &self.queued,
                },
                cx,
            ));

        div()
            .id("root")
            .debug_selector(|| "root".into())
            .flex()
            .size_full()
            .bg(rgb(theme.window_bg))
            .text_color(rgb(theme.text))
            .text_size(px(13.))
            // A click anywhere else dismisses the picker; the chip and the list
            // stop the press so their own handlers see it.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, window, cx| {
                    this.close_model_menu(window, cx);
                    this.close_thinking_menu(cx);
                    if this.pending_delete.take().is_some() {
                        cx.notify();
                    }
                }),
            )
            .child(sidebar::render(
                &self.sessions,
                self.session_search.clone(),
                sidebar::Rows {
                    active: self.session.as_ref().map(|session| session.id.as_str()),
                    confirming: self.pending_delete.as_deref(),
                    hovered: self.hovered_thread.as_deref(),
                },
                &self.workspace,
                theme,
                cx,
            ))
            .child(main)
    }
}

/// A step that delegated a task, so its card can be rebuilt from the subagent's own
/// session after a restart.
struct DelegatedStep {
    turn: usize,
    step: usize,
    call: String,
}

fn restore_turns(
    cx: &mut Context<HumibirdRoot>,
    store: &Store,
    session_id: &str,
    languages: &Arc<LanguageRegistry>,
) -> (Vec<Turn>, Vec<DelegatedStep>) {
    let messages = match store.messages(session_id) {
        Ok(messages) => messages,
        Err(error) => {
            log::warn!("could not read the transcript of {session_id}: {error:#}");
            return (Vec::new(), Vec::new());
        }
    };
    let calls = match store.tool_calls(session_id) {
        Ok(calls) => calls,
        Err(error) => {
            log::warn!("could not read the tool calls of {session_id}: {error:#}");
            Vec::new()
        }
    };

    let mut turns: Vec<Turn> = Vec::new();
    let mut delegated: Vec<DelegatedStep> = Vec::new();
    for message in messages {
        match message.role {
            Role::User => {
                let assistant =
                    cx.new(|cx| Markdown::new_deferred("", Some(languages.clone()), cx));
                let answer_watch = HumibirdRoot::watch_answer(&assistant, cx);
                turns.push(Turn {
                    user: message.content,
                    attachments: Attachment::list_from_json(&message.attachments),
                    assistant,
                    assistant_text: String::new(),
                    thinking: Vec::new(),
                    state: TurnState::Done,
                    tool_steps: Vec::new(),
                    expanded_steps: HashSet::new(),
                    seq: Some(message.seq),
                    _answer_watch: answer_watch,
                })
            }
            Role::Assistant => {
                let turn_index = turns.len().saturating_sub(1);
                match turns.last_mut() {
                    Some(turn) => {
                        turn.assistant
                            .update(cx, |markdown, cx| markdown.append(&message.content, cx));
                        turn.assistant_text = message.content;
                        for call in calls.iter().filter(|call| call.message_id == message.id) {
                            if call.name == "delegate" {
                                delegated.push(DelegatedStep {
                                    turn: turn_index,
                                    step: turn.tool_steps.len(),
                                    call: call.id.clone(),
                                });
                            }
                            turn.tool_steps.push(ToolStep {
                                name: call.name.clone(),
                                summary: summarize_arguments(&call.arguments),
                                arguments: call.arguments.clone(),
                                result: call.result.clone(),
                                state: StepState::Done {
                                    failed: call.status == ToolCallStatus::Failed,
                                },
                            });
                        }
                    }
                    None => log::warn!("ignoring message {}: it answers no turn", message.id),
                }
            }
            Role::Tool => {}
        }
    }
    (turns, delegated)
}

/// The card of a task a restored turn delegated: what it was asked, the calls it
/// made, and what it reported, all read from the subagent's own session.
fn restore_subagent(
    cx: &mut Context<HumibirdRoot>,
    store: &Store,
    parent: &str,
    record: &DelegatedStep,
    languages: &Arc<LanguageRegistry>,
) -> Option<Subagent> {
    let session = store
        .subagent_sessions(parent)
        .inspect_err(|error| log::warn!("could not read the subagents of {parent}: {error:#}"))
        .ok()?
        .into_iter()
        .find(|session| session.parent_tool_call.as_deref() == Some(record.call.as_str()))?;
    let messages = store
        .messages(&session.id)
        .inspect_err(|error| log::warn!("could not read the subagent's transcript: {error:#}"))
        .ok()?;
    let calls = store
        .tool_calls(&session.id)
        .inspect_err(|error| log::warn!("could not read the subagent's calls: {error:#}"))
        .unwrap_or_default();

    let prompt = messages
        .iter()
        .find(|message| message.role == Role::User)
        .map(|message| message.content.clone())
        .unwrap_or_default();
    let report = messages
        .iter()
        .rev()
        .find(|message| message.role == Role::Assistant)
        .map(|message| message.content.clone());

    let assistant = cx.new(|cx| Markdown::new_deferred("", Some(languages.clone()), cx));
    if let Some(report) = &report {
        assistant.update(cx, |markdown, cx| markdown.append(report, cx));
    }
    let watch = HumibirdRoot::watch_answer(&assistant, cx);
    let work = Turn {
        user: prompt,
        attachments: Vec::new(),
        assistant,
        assistant_text: report.clone().unwrap_or_default(),
        thinking: Vec::new(),
        // A subagent that never answered was cancelled or failed before its report.
        state: if report.is_some() {
            TurnState::Done
        } else {
            TurnState::Stopped
        },
        tool_steps: calls
            .into_iter()
            .map(|call| ToolStep {
                name: call.name.clone(),
                summary: summarize_arguments(&call.arguments),
                arguments: call.arguments.clone(),
                result: call.result.clone(),
                state: StepState::Done {
                    failed: call.status == ToolCallStatus::Failed,
                },
            })
            .collect(),
        expanded_steps: HashSet::new(),
        seq: None,
        _answer_watch: watch,
    };

    Some(Subagent {
        turn: record.turn,
        step: record.step,
        work,
        task: None,
    })
}

/// One provider's models in the picker. A provider with nothing to offer is not
/// listed at all, the way Zed's picker leaves out a provider it cannot reach.
pub(crate) struct ModelGroup {
    pub(crate) provider: &'static humibird_providers::Preset,
    pub(crate) models: Vec<(String, String)>,
}

/// The rows of the model picker, grouped by provider: every provider that can
/// answer, with the models the app catalogues for it, the current model when the
/// provider in use was configured by hand, a model the provider's record names, and
/// whatever the provider itself listed.
///
/// `ready` is what decides whether a provider is offered at all: a key that does
/// not resolve, or a local server that never answered, leaves it out, so the list
/// says what can actually be used.
pub(crate) fn model_groups(
    current: &str,
    provider: &str,
    discovered: &BTreeMap<String, Vec<DiscoveredModel>>,
    named: &BTreeMap<&str, &str>,
    ready: impl Fn(&humibird_providers::Preset) -> bool,
) -> Vec<ModelGroup> {
    let mut groups: Vec<ModelGroup> = Vec::new();
    for preset in humibird_providers::PRESETS {
        if !ready(preset) {
            continue;
        }
        let mut models: Vec<(String, String)> = Vec::new();
        let add = |id: &str, label: &str, models: &mut Vec<(String, String)>| {
            if !models.iter().any(|(seen, _)| seen == id) {
                models.push((id.to_owned(), label.to_owned()));
            }
        };
        if preset.id == provider && !current.is_empty() {
            add(current, &display_model(current), &mut models);
        }
        if let Some(model) = named.get(preset.id) {
            add(model, &display_model(model), &mut models);
        }
        for info in preset.models {
            add(info.id, info.display_name, &mut models);
        }
        if let Some(fetched) = discovered.get(preset.id) {
            for model in fetched {
                let label = model.display_name.as_deref().unwrap_or(&model.id);
                add(&model.id, label, &mut models);
            }
        }
        if models.is_empty() {
            continue;
        }
        groups.push(ModelGroup {
            provider: preset,
            models,
        });
    }
    groups
}

/// Every pickable row, in menu order, as `(provider id, model id)`.
pub(crate) fn model_rows(groups: &[ModelGroup]) -> Vec<(&'static str, String)> {
    groups
        .iter()
        .flat_map(|group| {
            group
                .models
                .iter()
                .map(move |(id, _)| (group.provider.id, id.clone()))
        })
        .collect()
}

/// The name the picker shows for a model id: the catalogue's label, or the id
/// itself when it was configured by hand.
pub(crate) fn display_model(model: &str) -> String {
    humibird_providers::model_info(model)
        .map(|info| info.display_name.to_owned())
        .unwrap_or_else(|| model.to_owned())
}

fn workspace_name(workspace: &str) -> String {
    std::path::Path::new(workspace)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| display_workspace(workspace))
}

/// Shortens the home directory to `~`, the way a shell prompt does.
fn display_workspace(workspace: &str) -> String {
    let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) else {
        return workspace.to_owned();
    };

    match std::path::Path::new(workspace).strip_prefix(&home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".to_owned(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => workspace.to_owned(),
    }
}

fn load_config(profile: &Option<Profile>) -> Config {
    profile
        .as_ref()
        .and_then(|profile| {
            profile
                .config()
                .inspect_err(|error| log::warn!("using the default configuration: {error:#}"))
                .ok()
        })
        .unwrap_or_default()
}

fn process_env() -> EnvLookup {
    Rc::new(|name: &str| std::env::var(name).ok())
}

impl From<Theme> for Appearance {
    fn from(theme: Theme) -> Self {
        match theme {
            Theme::Light => Self::Light,
            Theme::Dark => Self::Dark,
        }
    }
}

impl From<Appearance> for Theme {
    fn from(appearance: Appearance) -> Self {
        match appearance {
            Appearance::Light => Self::Light,
            Appearance::Dark => Self::Dark,
        }
    }
}

/// Tells the parent's loop that a delegated task never started. A closed channel
/// means the parent is already gone, which is worth no more than a debug line.
fn cancel_delegation(respond: oneshot::Sender<String>, why: &str) {
    log::warn!("not delegating: {why}");
    if respond.send(DELEGATION_CANCELLED.to_owned()).is_err() {
        log::debug!("the parent turn ended before the delegation was refused");
    }
}

/// Where the events of a running conversation land: the person's own turn, or the
/// card of a task it delegated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sink {
    Turn(usize),
    Delegation { turn: usize, step: usize },
}

/// The window's end of a running turn: it applies what happens and answers the
/// one question only the window can answer.
struct TurnSink<'a> {
    this: WeakEntity<HumibirdRoot>,
    sink: Sink,
    cx: &'a mut AsyncApp,
}

impl TurnHost for TurnSink<'_> {
    fn event(&mut self, event: AgentEvent) {
        let sink = self.sink;
        self.this
            .update(self.cx, |root, cx| root.apply(sink, event, cx))
            .ok();
    }

    fn steer_requested(&mut self) -> bool {
        self.this
            .update(self.cx, |root, _| root.front_wants_steer())
            .unwrap_or(false)
    }
}

/// The turn's off-thread and clock seam, on GPUI's executors. Neither needs
/// `Send`: the turn runs on the foreground thread and only hands the blocking
/// part to a background executor.
struct OnGpui {
    executor: gpui::BackgroundExecutor,
}

impl Background for OnGpui {
    fn run<R: Send + 'static, F: FnOnce() -> R + Send + 'static>(
        &self,
        work: F,
    ) -> LocalBoxFuture<'static, R> {
        self.executor.spawn(async move { work() }).boxed_local()
    }

    fn sleep(&self, duration: std::time::Duration) -> LocalBoxFuture<'static, ()> {
        self.executor.timer(duration).boxed_local()
    }
}

/// The name to show for a provider id, known to this build or not: an old thread
/// may name a provider that has since left the list.
fn provider_name(id: &str) -> &str {
    humibird_providers::preset(id)
        .map(|preset| preset.display_name)
        .unwrap_or(id)
}

/// The model a stored thread continues on, and the notice to show when the
/// profile's provider has moved since the thread was written. A model id only
/// means something at the provider it came from, so a thread from another provider
/// cannot carry its model over: it follows the profile, and the person is told why.
fn thread_model_and_notice(session: Option<&Session>, config: &Config) -> (String, Option<String>) {
    let current = config.provider.id.as_str();
    let configured = configured_model(config);

    if let Some(session) = session {
        let from_another_provider = session
            .provider
            .as_deref()
            .is_some_and(|recorded| recorded != current);
        if from_another_provider {
            let was = session.provider.as_deref().unwrap_or_default();
            // The model only means something at the provider it came from, so the
            // thread takes this profile's own: its default when the provider names
            // one, and none until one is picked when it does not.
            let model = configured.unwrap_or_default();
            let notice = format!(
                "This thread was on {}; this profile uses {} now, so it continues there.",
                provider_name(was),
                provider_name(current)
            );
            return (model, Some(notice));
        }
        if let Some(model) = &session.model {
            return (model.clone(), None);
        }
    }

    (configured.unwrap_or_default(), None)
}

/// Keeps the thread row saying what the thread is talking to: a row written before
/// the column existed gets a provider, and a thread that had to move gets the new
/// provider and model.
fn record_thread_context(store: &Store, session: &Session, provider: &str, model: &str) {
    if session.provider.as_deref() != Some(provider)
        && let Err(error) = store.set_session_provider(&session.id, provider)
    {
        log::warn!("could not record the provider of the thread: {error:#}");
    }
    if session.model.as_deref() != Some(model)
        && let Err(error) = store.set_session_model(&session.id, model)
    {
        log::warn!("could not record the model of the thread: {error:#}");
    }
}

/// The model the profile's provider settings name: its own choice, or the
/// provider's default. `None` when the provider names no models of its own and
/// the profile has not filled one in.
/// The profile's thinking choice, in the provider's terms.
fn thinking_request(config: &Config) -> ThinkingRequest {
    ThinkingRequest {
        enabled: config.thinking.enabled,
        effort: match config.thinking.effort {
            ProfileEffort::Low => Effort::Low,
            ProfileEffort::Medium => Effort::Medium,
            ProfileEffort::High => Effort::High,
        },
    }
}

/// Whether this profile's provider has a thinking switch on its wire at all.
fn thinking_available(config: &Config) -> bool {
    humibird_providers::preset(&config.provider.id)
        .is_some_and(|preset| preset.reasoning != humibird_providers::Reasoning::None)
}

/// How many files one message may carry: enough for a few references, small
/// enough that the request still reads as a message.
const MAX_ATTACHMENTS: usize = 4;
/// A text file larger than this crowds out the conversation it was attached to
/// inform; the person can point the agent at it instead.
const MAX_TEXT_BYTES: u64 = 256 * 1024;
/// Images are sent whole, so this is what the app will hold and the wire will
/// accept.
const MAX_IMAGE_BYTES: u64 = 4 * 1024 * 1024;

/// How many files the `@` menu offers at once, and how many are read from the
/// workspace to start with. A monorepo gets a prefix, not a stall.
const FILE_MENU_ROWS: usize = 6;
const FILE_LIST_LIMIT: usize = 2_000;

/// Where the last whitespace-delimited token of the text starts.
fn trailing_token_start(text: &str) -> usize {
    text.rfind(char::is_whitespace).map_or(0, |index| index + 1)
}

/// The query of an `@` mention at the end of the text: the last token starts
/// with `@`. An `@` inside a word is an email address, not a mention.
fn trailing_file_query(text: &str) -> Option<&str> {
    text[trailing_token_start(text)..].strip_prefix('@')
}

/// What the model and the composer call an attached file: its path inside the
/// workspace when it is there, so the agent can reach the same file with its
/// tools, and the path it was picked at otherwise.
fn attachment_name(workspace: &str, path: &Path) -> String {
    let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let root = std::fs::canonicalize(workspace).unwrap_or_else(|_| PathBuf::from(workspace));
    resolved
        .strip_prefix(&root)
        .unwrap_or(&resolved)
        .to_string_lossy()
        .into_owned()
}

fn image_media_type(path: &Path) -> Option<&'static str> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match extension.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        _ => return None,
    })
}

/// Reads one file into an attachment. Images are taken whole and base64-encoded,
/// because that is what every wire wants; anything else is read as text, and a
/// binary file that is not an image is refused with a sentence rather than
/// silently mangled.
fn load_attachment(workspace: &str, path: &Path) -> Result<Attachment, String> {
    let metadata =
        std::fs::metadata(path).map_err(|error| format!("{}: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!("{} is not a file.", path.display()));
    }
    let name = attachment_name(workspace, path);

    if let Some(media_type) = image_media_type(path) {
        if metadata.len() > MAX_IMAGE_BYTES {
            return Err(format!(
                "{name} is larger than {} MiB.",
                MAX_IMAGE_BYTES / (1024 * 1024)
            ));
        }
        let bytes =
            std::fs::read(path).map_err(|error| format!("could not read {name}: {error}"))?;
        return Ok(Attachment::Image {
            name,
            media_type: media_type.to_owned(),
            data: base64::engine::general_purpose::STANDARD.encode(bytes),
        });
    }

    if metadata.len() > MAX_TEXT_BYTES {
        return Err(format!(
            "{name} is larger than {} KiB.",
            MAX_TEXT_BYTES / 1024
        ));
    }
    let text = std::fs::read_to_string(path).map_err(|_| {
        format!("{name} is not text; images are the only binary files this can attach.")
    })?;
    Ok(Attachment::Text { name, text })
}

/// The window to measure `model` against: what the provider reported when it
/// listed its models first, then the preset's catalogue. A provider's listing can
/// name a model the catalogue does not, and it is sometimes the only source that
/// says how large that model's window is.
fn resolve_context_window(
    config: &Config,
    discovered: &BTreeMap<String, Vec<DiscoveredModel>>,
    model: &str,
) -> Option<u32> {
    // The profile's word is the last resort and the first authority: it is the only
    // way to describe a model nobody's listing knows, and it can correct a listing
    // that guesses.
    config
        .overrides(&config.provider.id)
        .context_window
        .or_else(|| {
            let from_listing = discovered
                .get(&config.provider.id)
                .and_then(|models| models.iter().find(|candidate| candidate.id == model))
                .and_then(|candidate| candidate.context_tokens);

            from_listing.or_else(|| {
                humibird_providers::preset(&config.provider.id)
                    .and_then(|preset| preset.model(model))
                    .map(|info| info.context_tokens)
            })
        })
}

/// The key DeepSeek's Search API would be billed to, when this profile has one. The
/// tool is DeepSeek's own service, so it is offered only to a profile that could
/// call it.
fn deepseek_key(profile: &Option<Profile>, config: &Config, env: &EnvLookup) -> Option<String> {
    let preset = humibird_providers::preset("deepseek")?;
    let overrides = config.overrides(preset.id);
    let variable = overrides.key_env.as_deref().or(preset.key_env)?;
    profile.as_ref()?.secret_with(variable, |name| env(name))
}

fn configured_model(config: &Config) -> Option<String> {
    let overrides = config.overrides(&config.provider.id);
    config
        .provider
        .model
        .clone()
        .or(overrides.model)
        .or_else(|| {
            humibird_providers::preset(&config.provider.id)
                .and_then(|preset| preset.default_model())
                .map(str::to_owned)
        })
}

fn default_provider(
    profile: &Option<Profile>,
    config: &Config,
    env: &EnvLookup,
) -> Result<Arc<dyn Provider>, String> {
    let profile = profile
        .as_ref()
        .ok_or_else(|| "Could not find a home directory for the profile.".to_owned())?;
    let provider = &config.provider;
    let preset = humibird_providers::preset(&provider.id).ok_or_else(|| {
        format!(
            "Unknown provider \"{}\". Choose one of: {}.",
            provider.id,
            humibird_providers::PRESETS
                .iter()
                .map(|preset| preset.id)
                .collect::<Vec<_>>()
                .join(", ")
        )
    })?;
    // Overrides belong to the provider, so a custom endpoint keeps them when the
    // profile talks to something else.
    let overrides = config.overrides(&provider.id);

    // A preset that names a key variable needs it; `key_env` lets a profile point
    // an endpoint without one of its own at a variable it defines.
    let key_variable = overrides.key_env.as_deref().or(preset.key_env);
    let api_key = match key_variable {
        Some(variable) => Some(profile.secret_with(variable, |name| env(name)).ok_or_else(
            || {
                format!(
                    "No {} API key yet: set {variable} in the profile or add one in Settings.",
                    preset.display_name
                )
            },
        )?),
        None => None,
    };

    let base_url = overrides
        .base_url
        .clone()
        .unwrap_or_else(|| preset.base_url.to_owned());
    if base_url.trim().is_empty() {
        return Err(format!(
            "{} has no endpoint yet: set `provider.base_url` in config.toml.",
            preset.display_name
        ));
    }

    let model = provider
        .model
        .clone()
        .or_else(|| overrides.model.clone())
        .or_else(|| preset.default_model().map(str::to_owned))
        .ok_or_else(|| {
            format!(
                "No {} model is chosen yet: set `provider.model` in config.toml.",
                preset.display_name
            )
        })?;

    let client = reqwest_client::ReqwestClient::user_agent("humibird/0.1")
        .map_err(|error| format!("Failed to create the HTTP client: {error}"))?;

    match preset.wire {
        humibird_providers::Wire::OpenAiCompatible => {
            let provider = OpenAiCompatible::new(preset, Arc::new(client), api_key)
                .with_base_url(base_url)
                .with_model(model);
            Ok(Arc::new(provider))
        }
        humibird_providers::Wire::Anthropic => {
            let Some(api_key) = api_key else {
                return Err(format!(
                    "{} needs an API key: set one for the profile.",
                    preset.display_name
                ));
            };
            let provider = Anthropic::new(preset, Arc::new(client), api_key)
                .with_base_url(base_url)
                .with_model(model);
            Ok(Arc::new(provider))
        }
    }
}

/// The way the window builds a provider it only wants a model list from. It reads
/// the profile and the environment when called, so a key saved later is seen
/// without rebuilding the window.
fn listing_provider_factory(profile: Option<Profile>, env: EnvLookup) -> ListingProviderFactory {
    Rc::new(move |preset| {
        let profile = profile
            .as_ref()
            .ok_or_else(|| "Could not find a home directory for the profile.".to_owned())?;
        // Read at call time: a provider is configured in Settings, and its record is
        // what says where it is and which key it wants.
        let config = match profile.config() {
            Ok(config) => config,
            Err(error) => {
                log::warn!("could not read the configuration for a model list: {error:#}");
                Config::default()
            }
        };
        provider_for_listing(profile, &config, preset, &env)
    })
}

/// Builds a provider only to ask it which models it serves. Unlike
/// [`default_provider`] this needs no model: the picker lists providers the profile
/// is not on, and nothing is being sent to them.
fn provider_for_listing(
    profile: &Profile,
    config: &Config,
    preset: &'static humibird_providers::Preset,
    env: &EnvLookup,
) -> Result<Arc<dyn Provider>, String> {
    let overrides = config.overrides(preset.id);
    let api_key = match overrides.key_env.as_deref().or(preset.key_env) {
        Some(variable) => Some(
            profile
                .secret_with(variable, |name| env(name))
                .ok_or_else(|| format!("No {} API key yet.", preset.display_name))?,
        ),
        None => None,
    };
    let base_url = overrides
        .base_url
        .clone()
        .unwrap_or_else(|| preset.base_url.to_owned());
    if base_url.trim().is_empty() {
        return Err(format!("{} has no endpoint yet.", preset.display_name));
    }

    let client = reqwest_client::ReqwestClient::user_agent("humibird/0.1")
        .map_err(|error| format!("Failed to create the HTTP client: {error}"))?;
    match preset.wire {
        humibird_providers::Wire::OpenAiCompatible => Ok(Arc::new(
            OpenAiCompatible::new(preset, Arc::new(client), api_key).with_base_url(base_url),
        )),
        humibird_providers::Wire::Anthropic => {
            let Some(api_key) = api_key else {
                return Err(format!("{} needs an API key.", preset.display_name));
            };
            Ok(Arc::new(
                Anthropic::new(preset, Arc::new(client), api_key).with_base_url(base_url),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt as _;
    use gpui::{
        Bounds, Focusable, Modifiers, ScrollDelta, ScrollEvent, TestAppContext, VisualTestContext,
        WindowBounds, WindowHandle, WindowOptions, point, px, size,
    };
    use humibird_agent::{COMPACTION_PROMPT, THREAD_TITLE_PROMPT};
    use humibird_memory::MemoryPaths;
    use humibird_profile::{ProviderConfig, ProviderOverride};
    use humibird_providers::{
        ContextExceeded, DEFAULT_MODEL, EventStream, Message, Request, StopReason, StreamEvent,
        ToolCall,
    };
    use theme::ActiveTheme as _;

    fn open_window(cx: &mut TestAppContext) -> (WindowHandle<HumibirdRoot>, VisualTestContext) {
        open_window_with(cx, None, Err("not configured".to_owned()))
    }

    fn open_window_with(
        cx: &mut TestAppContext,
        profile: Option<Profile>,
        provider: Result<Arc<dyn Provider>, String>,
    ) -> (WindowHandle<HumibirdRoot>, VisualTestContext) {
        open_window_with_env(cx, profile, provider, no_process_env())
    }

    fn open_window_with_env(
        cx: &mut TestAppContext,
        profile: Option<Profile>,
        provider: Result<Arc<dyn Provider>, String>,
        env: EnvLookup,
    ) -> (WindowHandle<HumibirdRoot>, VisualTestContext) {
        cx.update(init);

        let window = cx.update(move |cx| {
            let bounds = Bounds::centered(None, size(px(1280.), px(820.)), cx);
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                move |_, cx| {
                    cx.new(move |cx| {
                        HumibirdRoot::new_with_env(cx, profile, provider, env)
                            .without_listing_providers()
                    })
                },
            )
            .expect("window opens")
        });

        let visual = VisualTestContext::from_window(window.into(), cx);
        (window, visual)
    }

    /// The tests never inherit the shell's key, so each state of the settings
    /// window is reachable.
    fn no_process_env() -> EnvLookup {
        Rc::new(|_: &str| None)
    }

    fn settings_window(
        cx: &mut TestAppContext,
    ) -> Option<(WindowHandle<SettingsWindow>, VisualTestContext)> {
        let handle = cx
            .windows()
            .into_iter()
            .find_map(|window| window.downcast::<SettingsWindow>())?;
        let visual = VisualTestContext::from_window(handle.into(), cx);
        Some((handle, visual))
    }

    /// Opens settings from the chat window with the shortcut and returns a
    /// context for the window that it created.
    fn open_settings(cx: &mut VisualTestContext) -> VisualTestContext {
        cx.simulate_keystrokes("cmd-,");
        cx.update(|_, _| {});
        let (_, mut settings) = settings_window(cx).expect("the settings window opens");
        settings.update(|_, _| {});
        settings
    }

    /// `debug_bounds` only takes `&'static str`, and session ids are generated.
    fn selector(name: String) -> &'static str {
        Box::leak(name.into_boxed_str())
    }

    fn send(cx: &mut VisualTestContext, root: &Entity<HumibirdRoot>, text: &str) {
        let input = root.read_with(&**cx, |root, _| root.composer_input.clone());
        input.update_in(cx, |input, window, cx| {
            window.focus(&input.focus_handle(cx), cx);
        });
        cx.simulate_input(text);
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
    }

    /// Puts the pointer on a thread row, which is what shows its delete control.
    fn hover_row(cx: &mut VisualTestContext, session_id: &str) {
        let row = cx
            .debug_bounds(selector(format!("thread-{session_id}")))
            .expect("the row is laid out");
        cx.simulate_mouse_move(row.center(), None, Modifiers::default());
        cx.update(|_, _| {});
    }

    /// The labels of a menu's items, so a test reads like the menu does.
    fn menu_labels(items: &[MenuItem]) -> Vec<&str> {
        items
            .iter()
            .filter_map(|item| match item {
                MenuItem::Action { name, .. } => Some(name.as_ref()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn the_menu_bar_is_the_app_the_edit_menu_and_the_window() {
        let menus = app_menus();

        let names: Vec<_> = menus.iter().map(|menu| menu.name.as_ref()).collect();
        assert_eq!(
            names,
            ["humibird", "Edit", "Window"],
            "macOS makes the first menu the application menu"
        );

        let app = menu_labels(&menus[0].items);
        for expected in ["Settings…", "Hide humibird", "Quit humibird"] {
            assert!(app.contains(&expected), "{expected} is missing: {app:?}");
        }
        assert_eq!(
            menu_labels(&menus[1].items),
            ["Cut", "Copy", "Paste", "Select All"]
        );
        assert_eq!(
            menu_labels(&menus[2].items),
            ["Minimize", "Zoom", "Enter Full Screen"]
        );
    }

    #[gpui::test]
    fn a_menu_shortcut_is_bound_with_nothing_focused(cx: &mut TestAppContext) {
        cx.update(init);

        // A menu item shows the shortcut its action is bound to, and the menu bar
        // looks that binding up with no view focused. A binding that only existed
        // under `TextInput` would work from the keyboard and still leave the item
        // bare, which is the half-done menu this test is here to prevent.
        for (keystroke, action) in [
            ("cmd-q", "humibird::Quit"),
            ("cmd-,", "humibird::OpenSettings"),
            ("cmd-x", "text_input::Cut"),
            ("cmd-c", "text_input::Copy"),
            ("cmd-v", "text_input::Paste"),
            ("cmd-a", "text_input::SelectAll"),
            ("cmd-m", "humibird::Minimize"),
        ] {
            let bindings = cx.update(|cx| {
                cx.all_bindings_for_input(
                    &[gpui::Keystroke::parse(keystroke).expect("a keystroke")],
                )
            });

            assert!(
                bindings
                    .iter()
                    .any(|binding| binding.action().name() == action),
                "{keystroke} has to reach {action} with nothing focused, it reaches {:?}",
                bindings
                    .iter()
                    .map(|binding| binding.action().name())
                    .collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn a_text_attachment_is_named_by_its_path_inside_the_workspace() {
        let directory = tempfile::tempdir().expect("temp dir");
        let workspace = directory.path();
        std::fs::create_dir_all(workspace.join("src")).expect("src");
        let path = workspace.join("src/main.rs");
        std::fs::write(&path, "fn main() {}").expect("write");

        let attachment =
            load_attachment(&workspace.to_string_lossy(), &path).expect("a readable file");

        assert_eq!(
            attachment,
            Attachment::Text {
                name: "src/main.rs".to_owned(),
                text: "fn main() {}".to_owned(),
            },
            "the model gets a path its own tools can use"
        );
    }

    #[test]
    fn an_image_is_read_whole_and_base64_encoded() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("shot.png");
        std::fs::write(&path, [1u8, 2, 3]).expect("write");

        let attachment =
            load_attachment(&directory.path().to_string_lossy(), &path).expect("an image");

        assert_eq!(
            attachment,
            Attachment::Image {
                name: "shot.png".to_owned(),
                media_type: "image/png".to_owned(),
                data: "AQID".to_owned(),
            }
        );
    }

    #[test]
    fn a_text_file_too_large_to_inform_the_request_is_refused() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("huge.log");
        std::fs::write(&path, vec![b'a'; MAX_TEXT_BYTES as usize + 1]).expect("write");

        let refused = load_attachment(&directory.path().to_string_lossy(), &path)
            .expect_err("it would crowd out the conversation");

        assert!(refused.contains("huge.log"), "{refused}");
        assert!(refused.contains("larger"), "{refused}");
    }

    #[gpui::test]
    fn a_file_is_attached_sent_and_named_on_the_turn(cx: &mut TestAppContext) {
        let (directory, workspace) = temp_workspace();
        std::fs::write(workspace.join("notes.txt"), "the build is flaky").expect("write");
        let profile = Profile::at(directory.path().join("profile"));
        let (provider, requests) = scripted(vec![vec![text("ok"), finished()]], Vec::new());
        let (window, mut cx) =
            open_window_in_workspace(cx, Some(profile), Ok(provider), &workspace);
        let root = window.root(&mut cx).expect("root view");

        let attach = cx.debug_bounds("attach").expect("the attach button");
        cx.simulate_click(attach.center(), Modifiers::default());
        assert!(cx.did_prompt_for_paths(), "the platform is asked for files");
        let chosen = workspace.join("notes.txt");
        cx.simulate_path_prompt_response(move |_| Some(vec![chosen.clone()]));
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("attachment-0").is_some(),
            "it waits in the composer"
        );
        assert_eq!(root.read_with(&cx, |root, _| root.attachments.len()), 1);

        send(&mut cx, &root, "what is wrong?");
        cx.run_until_parked();

        assert_eq!(
            root.read_with(&cx, |root, _| root.turns[0].attachments.clone()),
            vec![Attachment::Text {
                name: "notes.txt".to_owned(),
                text: "the build is flaky".to_owned(),
            }],
            "the turn keeps what was sent with it"
        );
        assert!(
            cx.debug_bounds("turn-attachments").is_some(),
            "and the bubble names it"
        );

        let sent = requests
            .lock()
            .expect("requests")
            .iter()
            .find(|request| {
                request
                    .messages
                    .iter()
                    .any(|message| message.content == "what is wrong?")
            })
            .cloned()
            .expect("the turn's request");
        assert_eq!(
            sent.messages
                .iter()
                .find(|message| message.content == "what is wrong?")
                .map(|message| message.attachments.len()),
            Some(1),
            "the model is sent the file, not just its name"
        );
    }

    #[test]
    fn a_mention_is_the_last_token_and_not_an_email_address() {
        assert_eq!(trailing_file_query("look at @src/ma"), Some("src/ma"));
        assert_eq!(trailing_file_query("@"), Some(""));
        assert_eq!(
            trailing_file_query("mail a@b.com"),
            None,
            "an @ inside a word is not a mention"
        );
        assert_eq!(trailing_file_query("no mention"), None);
    }

    #[gpui::test]
    fn typing_a_mention_offers_workspace_files_and_enter_attaches_one(cx: &mut TestAppContext) {
        let (directory, workspace) = temp_workspace();
        std::fs::write(workspace.join("notes.txt"), "the build is flaky").expect("write");
        let profile = Profile::at(directory.path().join("profile"));
        let (provider, _requests) = scripted(vec![vec![text("ok"), finished()]], Vec::new());
        let (window, mut cx) =
            open_window_in_workspace(cx, Some(profile), Ok(provider), &workspace);
        let root = window.root(&mut cx).expect("root view");
        let input = root.read_with(&cx, |root, _| root.composer_input.clone());
        input.update_in(&mut cx, |input, window, cx| {
            window.focus(&input.focus_handle(cx), cx);
        });

        cx.simulate_input("check @not");
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("file-menu").is_some(),
            "the menu follows the mention"
        );
        assert!(cx.debug_bounds("file-option-0").is_some());

        cx.simulate_keystrokes("enter");
        cx.run_until_parked();

        assert_eq!(
            input.read_with(&cx, |input, _| input.value().to_owned()),
            "check notes.txt ",
            "the mention becomes the path"
        );
        assert_eq!(
            root.read_with(&cx, |root, _| root.attachments.clone()),
            vec![Attachment::Text {
                name: "notes.txt".to_owned(),
                text: "the build is flaky".to_owned(),
            }],
            "and the file comes with the message"
        );
        assert!(
            cx.debug_bounds("file-menu").is_none(),
            "picking a file closes the menu"
        );
    }

    #[gpui::test]
    fn escape_closes_the_mention_menu_until_the_query_changes(cx: &mut TestAppContext) {
        let (directory, workspace) = temp_workspace();
        std::fs::write(workspace.join("notes.txt"), "x").expect("write");
        let profile = Profile::at(directory.path().join("profile"));
        let (provider, _requests) = scripted(Vec::new(), Vec::new());
        let (window, mut cx) =
            open_window_in_workspace(cx, Some(profile), Ok(provider), &workspace);
        let root = window.root(&mut cx).expect("root view");
        let input = root.read_with(&cx, |root, _| root.composer_input.clone());
        input.update_in(&mut cx, |input, window, cx| {
            window.focus(&input.focus_handle(cx), cx);
        });

        cx.simulate_input("@not");
        cx.run_until_parked();
        assert!(cx.debug_bounds("file-menu").is_some());

        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("file-menu").is_none(),
            "Escape puts the menu away"
        );

        cx.simulate_input("e");
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("file-menu").is_some(),
            "typing again asks the new question"
        );
    }

    #[gpui::test]
    fn pasting_an_image_attaches_it(cx: &mut TestAppContext) {
        let (window, mut cx) = open_window(cx);
        let root = window.root(&mut cx).expect("root view");
        let input = root.read_with(&cx, |root, _| root.composer_input.clone());
        input.update_in(&mut cx, |input, window, cx| {
            window.focus(&input.focus_handle(cx), cx);
        });

        let image = gpui::Image::from_bytes(gpui::ImageFormat::Png, vec![1, 2, 3]);
        cx.update(|_, cx| cx.write_to_clipboard(gpui::ClipboardItem::new_image(&image)));
        cx.simulate_keystrokes("cmd-v");
        cx.run_until_parked();

        assert_eq!(
            root.read_with(&cx, |root, _| root.attachments.clone()),
            vec![Attachment::Image {
                name: "pasted-1.png".to_owned(),
                media_type: "image/png".to_owned(),
                data: "AQID".to_owned(),
            }],
            "a pasted screenshot is part of the message, not text that happened to paste"
        );
    }

    #[test]
    fn the_context_meter_reads_the_reported_usage_as_a_clamped_share() {
        assert_eq!(composer::context_percent(500_000, 1_000_000), 50);
        assert_eq!(composer::context_percent(760_000, 1_000_000), 76);
        assert_eq!(
            composer::context_percent(2_000_000, 1_000_000),
            100,
            "a provider that reports more than its own window fills the bar"
        );
        assert_eq!(composer::context_fraction(1, 0), 0.0);
    }

    #[test]
    fn token_counts_stay_short_enough_for_the_composer() {
        assert_eq!(composer::tokens_label(950), "950");
        assert_eq!(composer::tokens_label(1_500), "1.5k");
        assert_eq!(composer::tokens_label(12_300), "12k");
        assert_eq!(composer::tokens_label(120_000), "120k");
        assert_eq!(composer::tokens_label(1_000_000), "1M");
        assert_eq!(composer::tokens_label(1_500_000), "1.5M");
    }

    #[gpui::test]
    fn the_composer_shows_how_full_the_model_window_is(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        let reply = vec![
            StreamEvent::Text("sure".to_owned()),
            StreamEvent::Usage(humibird_providers::Usage {
                prompt_tokens: 500_000,
                completion_tokens: 10,
                total_tokens: 500_010,
                cache_hit_tokens: 0,
            }),
            finished(),
        ];
        let (provider, _requests) = scripted(vec![reply], Vec::new());
        let (window, mut cx) = open_window_with(cx, Some(profile), Ok(provider));
        let root = window.root(&mut cx).expect("root view");

        assert!(
            cx.debug_bounds("context-meter").is_some(),
            "the meter is there from the first frame, at 0%, because the window is known"
        );

        send(&mut cx, &root, "hello");
        cx.run_until_parked();

        let meter = cx
            .debug_bounds("context-meter")
            .expect("and it still reads what the provider reported");
        assert!(
            cx.debug_bounds("context-tooltip").is_none(),
            "the numbers stay out of the composer until they are asked for"
        );

        cx.simulate_mouse_move(meter.center(), None, Modifiers::default());
        cx.update(|_, _| {});
        let tooltip = cx
            .debug_bounds("context-tooltip")
            .expect("pointing at the ring explains it");
        assert!(
            (tooltip.right() - meter.right()).abs() < px(1.),
            "the explanation hangs from the ring: {tooltip:?} / {meter:?}"
        );
        assert!(
            tooltip.bottom() <= meter.top(),
            "and above it: {tooltip:?} / {meter:?}"
        );

        cx.simulate_mouse_move(point(px(5.), px(5.)), None, Modifiers::default());
        cx.update(|_, _| {});
        assert!(
            cx.debug_bounds("context-tooltip").is_none(),
            "and it goes away with the pointer"
        );
    }

    /// A context server that answers the handshake and offers one tool.
    fn context_server_script() -> String {
        r#"
            while IFS= read -r line; do
                case "$line" in
                    *'"method":"initialize"'*)
                        printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{}}}'
                        ;;
                    *'"method":"tools/list"'*)
                        printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"ping","description":"Ping","inputSchema":{"type":"object","properties":{}}}]}}'
                        ;;
                esac
            done
        "#
        .to_owned()
    }

    fn write_mcp(profile: &Profile, servers: &str) {
        std::fs::create_dir_all(profile.root()).expect("profile directory");
        std::fs::write(profile.mcp_path(), servers).expect("write mcp.json");
    }

    #[gpui::test]
    fn a_profile_with_a_context_server_gets_its_tools(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        write_mcp(
            &profile,
            &format!(
                r#"{{"servers": {{"fake": {{"command": "sh", "args": ["-c", {}]}}}}}}"#,
                serde_json::to_string(&context_server_script()).expect("json string")
            ),
        );
        let (provider, _requests) = scripted(Vec::new(), Vec::new());
        let (window, mut cx) = open_window_with(cx, Some(profile), Ok(provider));
        let root = window.root(&mut cx).expect("root view");
        // The servers start on the background executor, so the window is up first.
        cx.run_until_parked();

        let names: Vec<String> = root.read_with(&cx, |root, _| {
            root.conversation
                .tools()
                .iter()
                .map(|tool| tool.name().to_owned())
                .collect()
        });
        assert!(
            names.iter().any(|name| name == "mcp__fake__ping"),
            "the server's tool is in the request: {names:?}"
        );
    }

    #[gpui::test]
    fn a_broken_mcp_file_is_reported_in_the_composer(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        write_mcp(&profile, "{ not json");
        let (provider, _requests) = scripted(Vec::new(), Vec::new());
        let (window, mut cx) = open_window_with(cx, Some(profile), Ok(provider));
        let root = window.root(&mut cx).expect("root view");
        cx.run_until_parked();

        let notice = root
            .read_with(&cx, |root, _| root.mcp_notice.clone())
            .unwrap_or_default();
        assert!(notice.contains("mcp.json"), "{notice}");
    }

    #[gpui::test]
    fn root_fills_the_window(cx: &mut TestAppContext) {
        let (_, mut cx) = open_window(cx);
        let bounds = cx.debug_bounds("root").expect("root element is laid out");

        assert!(bounds.size.width > px(0.));
        assert!(bounds.size.height > px(0.));
    }

    #[gpui::test]
    fn sidebar_keeps_its_width_and_the_timeline_gets_the_rest(cx: &mut TestAppContext) {
        let (_, mut cx) = open_window(cx);

        let sidebar = cx.debug_bounds("sidebar").expect("sidebar laid out");
        let main = cx.debug_bounds("main").expect("main laid out");
        let timeline = cx.debug_bounds("timeline").expect("timeline laid out");
        let composer = cx.debug_bounds("composer").expect("composer laid out");

        assert_eq!(sidebar.size.width, px(264.));
        assert!(main.size.width > sidebar.size.width);
        assert!(timeline.size.height > px(0.));
        assert!(composer.size.height > px(0.));
        assert_eq!(
            cx.debug_bounds("thread-header")
                .map(|bounds| bounds.size.height),
            Some(px(54.))
        );
    }

    #[gpui::test]
    fn a_long_workspace_name_does_not_push_the_theme_toggle_out_of_the_header(
        cx: &mut TestAppContext,
    ) {
        let directory = tempfile::tempdir().expect("temp dir");
        // Each component stays under the 255-byte limit on macOS/APFS; three of
        // them together are far wider than the header.
        let component = "a-workspace-name-long-enough-to-fill-the-header".repeat(3);
        let workspace = directory
            .path()
            .join(&component)
            .join(format!("{component}-nested"));
        std::fs::create_dir_all(&workspace).expect("create workspace");

        let (_, mut cx) =
            open_window_in_workspace(cx, None, Err("not configured".to_owned()), &workspace);

        let header = cx.debug_bounds("thread-header").expect("header laid out");
        let title = cx
            .debug_bounds("thread-header-title")
            .expect("title column laid out");
        let actions = cx
            .debug_bounds("thread-header-actions")
            .expect("header actions laid out");

        assert!(
            actions.right() <= header.right(),
            "the toggle stays inside the header: {:?} vs {:?}",
            actions,
            header
        );
        assert!(
            title.right() <= actions.left(),
            "the title gives way to the toggle: {:?} vs {:?}",
            title,
            actions
        );
    }

    #[gpui::test]
    fn the_sidebar_brand_is_the_app_mark(cx: &mut TestAppContext) {
        let (_, mut cx) = open_window(cx);

        let mark = cx.debug_bounds("brand-mark").expect("brand mark laid out");
        let sidebar = cx.debug_bounds("sidebar").expect("sidebar laid out");
        assert_eq!(mark.size, size(px(18.), px(18.)));
        assert!(sidebar.contains(&mark.origin));
        assert!(mark.left() >= sidebar.left());
        assert!(mark.top() >= sidebar.top());
    }

    #[gpui::test]
    fn the_sidebar_offers_a_new_thread_and_says_when_there_are_none(cx: &mut TestAppContext) {
        let (_, mut cx) = open_window(cx);

        assert!(cx.debug_bounds("new-thread").is_some());
        assert!(cx.debug_bounds("threads-empty").is_some());
    }

    #[gpui::test]
    fn sending_a_message_starts_a_session_and_lists_it(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        let provider = Arc::new(FakeProvider {
            requests: Arc::new(std::sync::Mutex::new(Vec::new())),
            events: vec![
                StreamEvent::Text("sure".to_owned()),
                StreamEvent::Finished {
                    reason: StopReason::Stop,
                },
            ],
            title: None,
        });
        let (window, mut cx) = open_window_with(cx, Some(profile), Ok(provider));
        let root = window.root(&mut cx).expect("root view");

        send(&mut cx, &root, "hello");
        cx.update(|_, _| {});

        let sessions = root.read_with(&cx, |root, _| root.sessions.clone());
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].model.as_deref(), Some(DEFAULT_MODEL));
        assert!(
            cx.debug_bounds(selector(format!("thread-{}", sessions[0].id)))
                .is_some(),
            "the sidebar should list the new session"
        );
        assert!(cx.debug_bounds("threads-empty").is_none());
    }

    /// A provider that answers `/models`, the way a real endpoint does.
    fn discovered(id: &str) -> DiscoveredModel {
        DiscoveredModel {
            id: id.to_owned(),
            display_name: None,
            context_tokens: None,
        }
    }

    struct ListingProvider {
        asks: Option<Arc<std::sync::atomic::AtomicUsize>>,
    }

    impl ListingProvider {
        fn new() -> Self {
            Self { asks: None }
        }

        fn counting(asks: Arc<std::sync::atomic::AtomicUsize>) -> Self {
            Self { asks: Some(asks) }
        }
    }

    impl Provider for ListingProvider {
        fn request(&self, messages: Vec<Message>) -> Request {
            Request::new("deepseek-flash", messages)
        }

        fn stream(&self, _: Request) -> EventStream {
            futures::stream::empty().boxed()
        }

        fn list_models(
            &self,
        ) -> Option<futures::future::BoxFuture<'static, anyhow::Result<Vec<DiscoveredModel>>>>
        {
            if let Some(asks) = &self.asks {
                asks.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            Some(Box::pin(async {
                Ok(vec![
                    discovered("provider-model-one"),
                    discovered("provider-model-two"),
                ])
            }))
        }
    }

    #[test]
    fn the_picker_merges_the_catalogue_and_the_providers_own_list() {
        let discovered = BTreeMap::from([(
            "deepseek".to_owned(),
            vec![discovered("zeta"), discovered(DEFAULT_MODEL)],
        )]);

        let groups = model_groups("zeta", "deepseek", &discovered, &BTreeMap::new(), |_| true);
        let choices = &groups
            .iter()
            .find(|group| group.provider.id == "deepseek")
            .expect("the provider in use")
            .models;

        assert_eq!(
            choices.iter().filter(|(id, _)| id == "zeta").count(),
            1,
            "a fetched model is listed once: {choices:?}"
        );
        assert_eq!(
            choices.iter().filter(|(id, _)| id == DEFAULT_MODEL).count(),
            1,
            "a model both the catalogue and the provider name is listed once"
        );
        assert!(
            choices
                .iter()
                .any(|(id, label)| id == DEFAULT_MODEL && label == "DeepSeek V4.1 Flash"),
            "the catalogue keeps its label: {choices:?}"
        );

        let typed = model_groups(
            "typed-by-hand",
            "deepseek",
            &BTreeMap::new(),
            &BTreeMap::new(),
            |_| true,
        );
        let typed = &typed
            .iter()
            .find(|group| group.provider.id == "deepseek")
            .expect("the provider in use")
            .models;
        assert_eq!(typed[0].0, "typed-by-hand");
    }

    #[test]
    fn a_provider_with_no_catalogue_offers_the_model_in_use() {
        let groups = model_groups(
            "claude-sonnet-4-5",
            "anthropic",
            &BTreeMap::new(),
            &BTreeMap::new(),
            |_| true,
        );
        let choices = &groups
            .iter()
            .find(|group| group.provider.id == "anthropic")
            .expect("the provider in use")
            .models;

        assert_eq!(
            choices
                .iter()
                .map(|(id, _)| id.as_str())
                .collect::<Vec<_>>(),
            ["claude-sonnet-4-5"],
            "Anthropic catalogues nothing here, so only the chosen model is offered"
        );
    }

    #[test]
    fn every_reachable_provider_offers_the_models_it_names() {
        let discovered =
            BTreeMap::from([("openrouter".to_owned(), vec![discovered("vendor/model")])]);
        let groups = model_groups(
            DEFAULT_MODEL,
            "deepseek",
            &discovered,
            &BTreeMap::new(),
            |_| true,
        );
        let openrouter = &groups
            .iter()
            .find(|group| group.provider.id == "openrouter")
            .expect("a provider that is not in use")
            .models;

        assert_eq!(
            openrouter
                .iter()
                .map(|(id, _)| id.as_str())
                .collect::<Vec<_>>(),
            ["vendor/model"],
            "a provider's own list is offered without switching to it first"
        );
    }

    #[gpui::test]
    fn an_unconfigured_local_provider_is_not_asked(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        let (window, mut cx) =
            open_window_with(cx, Some(profile), Ok(Arc::new(ListingProvider::new())));
        let root = window.root(&mut cx).expect("root view");
        let asked = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        cx.update(|_, cx| {
            root.update(cx, |root, _| {
                let asked = asked.clone();
                root.listing_provider = Rc::new(move |preset| {
                    asked.lock().expect("asked").push(preset.id.to_owned());
                    Err("not in this test".to_owned())
                });
            });
        });

        let chip = cx
            .debug_bounds(selector(format!("composer-model-{DEFAULT_MODEL}")))
            .expect("the model chip");
        cx.simulate_click(chip.center(), Modifiers::default());
        cx.run_until_parked();

        let asked = asked.lock().expect("asked").clone();
        assert!(
            !asked.contains(&"ollama".to_owned()),
            "the profile never named an Ollama address, so nothing should probe one: {asked:?}"
        );
        assert!(
            !asked.contains(&"custom".to_owned()),
            "the same goes for a custom endpoint: {asked:?}"
        );
    }

    #[gpui::test]
    fn naming_a_local_providers_address_is_what_makes_it_asked(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        profile
            .save_config(&Config {
                providers: BTreeMap::from([(
                    "ollama".to_owned(),
                    ProviderOverride {
                        base_url: Some("http://localhost:11434/v1".to_owned()),
                        ..ProviderOverride::default()
                    },
                )]),
                ..Config::default()
            })
            .expect("write config");
        let (window, mut cx) =
            open_window_with(cx, Some(profile), Ok(Arc::new(ListingProvider::new())));
        let root = window.root(&mut cx).expect("root view");
        let asked = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        cx.update(|_, cx| {
            root.update(cx, |root, _| {
                let asked = asked.clone();
                root.listing_provider = Rc::new(move |preset| {
                    asked.lock().expect("asked").push(preset.id.to_owned());
                    Err("not in this test".to_owned())
                });
            });
        });

        let chip = cx
            .debug_bounds(selector(format!("composer-model-{DEFAULT_MODEL}")))
            .expect("the model chip");
        cx.simulate_click(chip.center(), Modifiers::default());
        cx.run_until_parked();

        assert!(
            asked.lock().expect("asked").contains(&"ollama".to_owned()),
            "the named address is the one the picker asks"
        );
    }

    #[gpui::test]
    fn the_picker_lists_the_models_the_provider_names(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        let (window, mut cx) =
            open_window_with(cx, Some(profile), Ok(Arc::new(ListingProvider::new())));
        let root = window.root(&mut cx).expect("root view");

        let chip = cx
            .debug_bounds(selector(format!("composer-model-{DEFAULT_MODEL}")))
            .expect("the model chip");
        cx.simulate_click(chip.center(), Modifiers::default());
        cx.run_until_parked();
        cx.update(|_, _| {});

        assert_eq!(
            root.read_with(&cx, |root, _| root
                .discovered
                .get("deepseek")
                .expect("the provider listed a model")
                .iter()
                .map(|model| model.id.clone())
                .collect::<Vec<_>>()),
            ["provider-model-one", "provider-model-two"]
        );
        assert!(
            cx.debug_bounds("model-option-deepseek/provider-model-one")
                .is_some(),
            "the picker shows what the provider said it serves"
        );
        assert!(
            cx.debug_bounds(selector(format!("model-option-deepseek/{DEFAULT_MODEL}")))
                .is_some(),
            "and the catalogue is still there"
        );
    }

    #[test]
    fn a_window_from_the_listing_beats_the_catalogue_and_a_stale_listing_is_ignored() {
        let mut config = Config::default();
        config.provider.id = "openrouter".to_owned();
        config.provider.model = Some("vendor/model".to_owned());
        let listing = BTreeMap::from([(
            "openrouter".to_owned(),
            vec![DiscoveredModel {
                id: "vendor/model".to_owned(),
                display_name: None,
                context_tokens: Some(123_000),
            }],
        )]);

        assert_eq!(
            resolve_context_window(&config, &listing, "vendor/model"),
            Some(123_000),
            "the listing is the only word on this model's window"
        );
        let other_provider = BTreeMap::from([(
            "openai".to_owned(),
            vec![DiscoveredModel {
                id: "vendor/model".to_owned(),
                display_name: None,
                context_tokens: Some(123_000),
            }],
        )]);
        assert_eq!(
            resolve_context_window(&config, &other_provider, "vendor/model"),
            None,
            "another provider's listing is not this provider's answer"
        );

        config.provider.context_window = Some(50_000);
        assert_eq!(
            resolve_context_window(&config, &listing, "vendor/model"),
            Some(50_000),
            "the profile can correct a listing, or describe a model it does not cover"
        );
        config.provider.context_window = None;

        let mut deepseek = Config::default();
        deepseek.provider.id = "deepseek".to_owned();
        assert_eq!(
            resolve_context_window(&deepseek, &BTreeMap::new(), "deepseek-flash"),
            Some(1_000_000),
            "without a listing the catalogue still answers"
        );
    }

    /// A provider that knows nothing about its model's window but names it in a
    /// listing, the way OpenRouter does.
    struct WindowListingProvider;

    impl Provider for WindowListingProvider {
        fn request(&self, messages: Vec<Message>) -> Request {
            Request::new("vendor/model", messages)
        }

        fn stream(&self, _: Request) -> EventStream {
            futures::stream::iter(vec![
                Ok(StreamEvent::Text("ok".to_owned())),
                Ok(StreamEvent::Usage(humibird_providers::Usage {
                    prompt_tokens: 50_000,
                    completion_tokens: 10,
                    total_tokens: 50_010,
                    cache_hit_tokens: 0,
                })),
                Ok(finished()),
            ])
            .boxed()
        }

        fn list_models(
            &self,
        ) -> Option<futures::future::BoxFuture<'static, anyhow::Result<Vec<DiscoveredModel>>>>
        {
            Some(Box::pin(async {
                Ok(vec![DiscoveredModel {
                    id: "vendor/model".to_owned(),
                    display_name: Some("Vendor Model".to_owned()),
                    context_tokens: Some(100_000),
                }])
            }))
        }
    }

    #[gpui::test]
    fn a_window_from_the_providers_listing_reaches_the_meter(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        profile
            .save_config(&Config {
                provider: humibird_profile::ProviderConfig {
                    id: "openrouter".to_owned(),
                    model: Some("vendor/model".to_owned()),
                    ..Default::default()
                },
                ..Config::default()
            })
            .expect("write config");
        let (window, mut cx) =
            open_window_with(cx, Some(profile), Ok(Arc::new(WindowListingProvider)));
        let root = window.root(&mut cx).expect("root view");

        assert_eq!(
            root.read_with(&cx, |root, _| root.conversation.context_window()),
            None,
            "nobody has said how large this model's window is"
        );

        let chip = cx
            .debug_bounds(selector("composer-model-vendor/model".to_owned()))
            .expect("the model chip");
        cx.simulate_click(chip.center(), Modifiers::default());
        cx.run_until_parked();
        cx.update(|_, _| {});

        assert_eq!(
            root.read_with(&cx, |root, _| root.conversation.context_window()),
            Some(100_000),
            "the listing is the only word on this model's window"
        );

        // Close the picker, then send: the reply reports half the listed window.
        cx.simulate_click(chip.center(), Modifiers::default());
        send(&mut cx, &root, "hello");
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("context-meter").is_some(),
            "the meter can now draw a percentage"
        );
    }

    /// A provider whose model list fails once, then works, the way a flaky
    /// connection does.
    struct FlakyListingProvider {
        attempts: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl Provider for FlakyListingProvider {
        fn request(&self, messages: Vec<Message>) -> Request {
            Request::new("deepseek-flash", messages)
        }

        fn stream(&self, _: Request) -> EventStream {
            futures::stream::empty().boxed()
        }

        fn list_models(
            &self,
        ) -> Option<futures::future::BoxFuture<'static, anyhow::Result<Vec<DiscoveredModel>>>>
        {
            let attempt = self
                .attempts
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Some(Box::pin(async move {
                if attempt == 0 {
                    Err(anyhow::anyhow!("the connection dropped"))
                } else {
                    Ok(vec![discovered("recovered-model")])
                }
            }))
        }
    }

    #[gpui::test]
    fn a_model_list_that_failed_is_asked_for_again(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (window, mut cx) = open_window_with(
            cx,
            Some(profile),
            Ok(Arc::new(FlakyListingProvider {
                attempts: attempts.clone(),
            })),
        );
        let root = window.root(&mut cx).expect("root view");

        let chip = cx
            .debug_bounds(selector(format!("composer-model-{DEFAULT_MODEL}")))
            .expect("the model chip");
        cx.simulate_click(chip.center(), Modifiers::default());
        cx.run_until_parked();
        cx.update(|_, _| {});
        assert!(
            root.read_with(&cx, |root, _| root.discovered.is_empty()),
            "the first attempt failed, so there is nothing extra to list"
        );

        // Opening it again is not a second wasted request: the failure was not an
        // answer, so the picker asks once more. The first click closes the list it
        // opened, the second opens it again.
        cx.simulate_click(chip.center(), Modifiers::default());
        cx.simulate_click(chip.center(), Modifiers::default());
        cx.run_until_parked();
        cx.update(|_, _| {});

        assert_eq!(
            root.read_with(&cx, |root, _| root
                .discovered
                .get("deepseek")
                .expect("the retry was answered")
                .iter()
                .map(|model| model.id.clone())
                .collect::<Vec<_>>()),
            ["recovered-model"]
        );
        assert_eq!(attempts.load(std::sync::atomic::Ordering::Relaxed), 2);
    }

    #[gpui::test]
    fn the_picker_asks_the_provider_once(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        let asks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (window, mut cx) = open_window_with(
            cx,
            Some(profile),
            Ok(Arc::new(ListingProvider::counting(asks.clone()))),
        );
        let _root = window.root(&mut cx).expect("root view");

        let chip = cx
            .debug_bounds(selector(format!("composer-model-{DEFAULT_MODEL}")))
            .expect("the model chip");
        cx.simulate_click(chip.center(), Modifiers::default());
        cx.run_until_parked();
        cx.update(|_, _| {});
        cx.simulate_click(chip.center(), Modifiers::default());
        cx.run_until_parked();
        cx.update(|_, _| {});
        cx.simulate_click(chip.center(), Modifiers::default());
        cx.run_until_parked();

        assert_eq!(
            asks.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "opening the picker twice more must not ask the provider twice"
        );
    }

    #[gpui::test]
    fn the_model_menu_floats_above_the_chip(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        let fallback = vec![StreamEvent::Text("ok".to_owned()), finished()];
        let (provider, _requests) = scripted(Vec::new(), fallback);
        let (window, mut cx) = open_window_with(cx, Some(profile), Ok(provider));
        let _root = window.root(&mut cx).expect("root view");

        let chip = cx
            .debug_bounds(selector(format!("composer-model-{DEFAULT_MODEL}")))
            .expect("the model chip");
        let closed_card = cx.debug_bounds("composer-card").expect("the composer");
        cx.simulate_click(chip.center(), Modifiers::default());

        let card = cx.debug_bounds("composer-card").expect("the composer");
        let menu = cx.debug_bounds("model-menu").expect("the list");

        assert_eq!(
            card.size, closed_card.size,
            "the picker floats over the composer instead of growing it"
        );
        assert!(
            menu.origin.x >= card.origin.x
                && menu.origin.x + menu.size.width <= card.origin.x + card.size.width,
            "the list stays inside the composer card horizontally: menu={menu:?} card={card:?}"
        );
        assert!(
            menu.origin.y + menu.size.height <= chip.origin.y,
            "the list sits above the chip: menu={menu:?} chip={chip:?}"
        );
        assert!(
            menu.origin.x + menu.size.width <= chip.origin.x + chip.size.width,
            "and is aligned with the chip's right edge"
        );

        for id in [DEFAULT_MODEL, "deepseek-v4-pro"] {
            let row = cx
                .debug_bounds(selector(format!("model-option-deepseek/{id}")))
                .expect("a model row");
            assert!(
                row.size.width + px(8.) <= menu.size.width
                    && row.size.width + px(16.) >= menu.size.width,
                "{id} spans the list instead of overflowing it: {row:?} in {menu:?}"
            );
            assert!(
                row.origin.x >= menu.origin.x
                    && row.origin.x + row.size.width <= menu.origin.x + menu.size.width,
                "{id} stays inside the list"
            );
            assert!(
                row.origin.y >= menu.origin.y
                    && row.origin.y + row.size.height <= menu.origin.y + menu.size.height,
                "{id} stays inside the list vertically"
            );
        }
    }

    #[gpui::test]
    fn the_model_picker_switches_the_next_turn(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = Arc::new(FakeProvider {
            requests: requests.clone(),
            events: vec![StreamEvent::Text("ok".to_owned()), finished()],
            title: None,
        });
        let (window, mut cx) = open_window_with(cx, Some(profile.clone()), Ok(provider));
        let root = window.root(&mut cx).expect("root view");

        send(&mut cx, &root, "first");

        let chip = cx
            .debug_bounds(selector(format!("composer-model-{DEFAULT_MODEL}")))
            .expect("the model chip");
        assert!(
            cx.debug_bounds("model-menu").is_none(),
            "the picker starts closed"
        );
        cx.simulate_click(chip.center(), Modifiers::default());
        assert!(
            cx.debug_bounds("model-menu").is_some(),
            "clicking the chip opens the picker"
        );

        let option = cx
            .debug_bounds(selector("model-option-deepseek/deepseek-v4-pro".to_owned()))
            .expect("the other model is offered");
        cx.simulate_click(option.center(), Modifiers::default());

        assert!(
            cx.debug_bounds("model-menu").is_none(),
            "choosing a model closes the picker"
        );
        assert_eq!(
            root.read_with(&cx, |root, _| root.model()),
            "deepseek-v4-pro"
        );

        send(&mut cx, &root, "second");

        let requests = requests.lock().expect("requests lock");
        let second = requests
            .iter()
            .find(|request| {
                request
                    .messages
                    .iter()
                    .any(|message| message.content == "second")
            })
            .expect("the second turn");
        assert_eq!(
            second.model, "deepseek-v4-pro",
            "the next request goes to the new model"
        );
        assert!(
            second.messages[0]
                .content
                .contains("- Model: deepseek-v4-pro"),
            "the prompt is rebuilt so it names the model it is talking to:\n{}",
            second.messages[0].content
        );

        let config = profile.config().expect("the saved config");
        assert_eq!(
            config.provider.model.as_deref(),
            Some("deepseek-v4-pro"),
            "the next thread starts on the model that was chosen last"
        );
    }

    #[gpui::test]
    fn the_model_menu_closes_when_clicking_elsewhere(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        let fallback = vec![StreamEvent::Text("ok".to_owned()), finished()];
        let (provider, _requests) = scripted(Vec::new(), fallback);
        let (window, mut cx) = open_window_with(cx, Some(profile), Ok(provider));
        let root = window.root(&mut cx).expect("root view");

        let chip = cx
            .debug_bounds(selector(format!("composer-model-{DEFAULT_MODEL}")))
            .expect("the model chip");
        cx.simulate_click(chip.center(), Modifiers::default());
        assert!(cx.debug_bounds("model-menu").is_some(), "the list is open");

        let timeline = cx.debug_bounds("timeline").expect("the timeline");
        cx.simulate_click(timeline.center(), Modifiers::default());

        assert!(
            cx.debug_bounds("model-menu").is_none(),
            "a press anywhere else dismisses the picker"
        );
        assert_eq!(
            root.read_with(&cx, |root, _| root.model()),
            DEFAULT_MODEL,
            "dismissing does not change the model"
        );
    }

    #[gpui::test]
    fn the_model_menu_answers_to_the_keyboard(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        let fallback = vec![StreamEvent::Text("ok".to_owned()), finished()];
        let (provider, _requests) = scripted(Vec::new(), fallback);
        let (window, mut cx) = open_window_with(cx, Some(profile), Ok(provider));
        let root = window.root(&mut cx).expect("root view");

        let chip = cx
            .debug_bounds(selector(format!("composer-model-{DEFAULT_MODEL}")))
            .expect("the model chip");
        cx.simulate_click(chip.center(), Modifiers::default());
        assert_eq!(
            root.read_with(&cx, |root, _| root.model_menu_highlight),
            0,
            "the open list starts on the current model"
        );

        cx.simulate_keystrokes("down");
        assert_eq!(
            root.read_with(&cx, |root, _| root.model_menu_highlight),
            1,
            "down moves the highlight"
        );
        cx.simulate_keystrokes("up");
        assert_eq!(
            root.read_with(&cx, |root, _| root.model_menu_highlight),
            0,
            "up moves it back"
        );
        cx.simulate_keystrokes("down");
        cx.simulate_keystrokes("enter");

        assert_eq!(
            root.read_with(&cx, |root, _| root.model()),
            "deepseek-v4-pro",
            "Enter takes the highlighted model"
        );
        assert!(
            !root.read_with(&cx, |root, _| root.model_menu_open),
            "and closes the list"
        );

        // Typing goes back to the draft once the list is closed.
        send(&mut cx, &root, "hello");
        assert_eq!(
            root.read_with(&cx, |root, _| root.turns.len()),
            1,
            "the composer takes input again"
        );

        // Escape closes without choosing anything. The key is not bound in this
        // window, so it reaches the list's own handler.
        let chip = cx
            .debug_bounds(selector("composer-model-deepseek-v4-pro".to_owned()))
            .expect("the chip for the new model");
        cx.simulate_click(chip.center(), Modifiers::default());
        assert!(
            root.read_with(&cx, |root, _| root.model_menu_open),
            "the list opens again"
        );

        cx.simulate_keystrokes("escape");
        assert!(
            !root.read_with(&cx, |root, _| root.model_menu_open),
            "Escape closes the list"
        );
        assert_eq!(
            root.read_with(&cx, |root, _| root.model()),
            "deepseek-v4-pro",
            "and leaves the model alone"
        );
    }

    #[gpui::test]
    fn the_model_menu_hangs_from_the_chip(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        let fallback = vec![StreamEvent::Text("ok".to_owned()), finished()];
        let (provider, _requests) = scripted(Vec::new(), fallback);
        let (_window, mut cx) = open_window_with(cx, Some(profile), Ok(provider));

        let chip = cx
            .debug_bounds(selector(format!("composer-model-{DEFAULT_MODEL}")))
            .expect("the model chip");
        cx.simulate_click(chip.center(), Modifiers::default());
        cx.update(|_, _| {});

        let menu = cx.debug_bounds("model-menu").expect("the model list");
        assert!(
            (menu.right() - chip.right()).abs() < px(1.),
            "the list hangs from the chip's right edge: {menu:?} / {chip:?}"
        );
        assert!(
            (menu.bottom() - (chip.top() - px(6.))).abs() < px(1.),
            "and sits just above it, not floating away from it: {menu:?} / {chip:?}"
        );
        assert!(
            menu.top() >= px(0.),
            "and stays inside the window: {menu:?}"
        );
    }

    #[gpui::test]
    fn a_thread_keeps_the_model_it_was_using(cx: &mut TestAppContext) {
        let (directory, workspace) = temp_workspace();
        let profile = Profile::at(directory.path().join("profile"));
        let workspace = workspace.to_string_lossy().into_owned();

        let store = Store::open(&profile.database_path()).expect("store");
        let pro = store
            .create_session(&workspace, Some("deepseek"), Some("deepseek-v4-pro"))
            .expect("a thread on the pro model");
        let flash = store
            .create_session(&workspace, Some("deepseek"), Some(DEFAULT_MODEL))
            .expect("a thread on the fast model");
        drop(store);

        let fallback = vec![StreamEvent::Text("ok".to_owned()), finished()];
        let (provider, _requests) = scripted(Vec::new(), fallback);
        let (window, mut cx) =
            open_window_in_workspace(cx, Some(profile.clone()), Ok(provider), workspace.as_ref());
        let root = window.root(&mut cx).expect("root view");

        assert_eq!(
            root.read_with(&cx, |root, _| root.session.as_ref().map(|s| s.id.clone())),
            Some(flash.id.clone()),
            "the most recent thread is the one that loads"
        );
        assert_eq!(root.read_with(&cx, |root, _| root.model()), DEFAULT_MODEL);

        let row = cx
            .debug_bounds(selector(format!("thread-{}", pro.id)))
            .expect("the other thread");
        cx.simulate_click(row.center(), Modifiers::default());

        assert_eq!(
            root.read_with(&cx, |root, _| root.model()),
            "deepseek-v4-pro",
            "opening a thread adopts the model it was using"
        );
    }

    #[gpui::test]
    fn threads_from_another_workspace_are_grouped_and_switch_when_opened(cx: &mut TestAppContext) {
        let (directory, workspace) = temp_workspace();
        let profile = Profile::at(directory.path().join("profile"));
        let default_workspace = profile.workspace_dir().expect("default workspace");
        let model = Some(DEFAULT_MODEL);

        // The list is ordered by `updated_at` and then `rowid`, descending, so the
        // thread created last is the one the window opens.
        let store = Store::open(&profile.database_path()).expect("store");
        let elsewhere = store
            .create_session(
                &default_workspace.to_string_lossy(),
                Some("deepseek"),
                model,
            )
            .expect("a thread in the default workspace");
        let here = store
            .create_session(&workspace.to_string_lossy(), Some("deepseek"), model)
            .expect("a thread in the pinned workspace");
        drop(store);

        let fallback = vec![StreamEvent::Text("ok".to_owned()), finished()];
        let (provider, _requests) = scripted(Vec::new(), fallback);
        let (window, mut cx) =
            open_window_in_workspace(cx, Some(profile), Ok(provider), &workspace);
        let root = window.root(&mut cx).expect("root view");

        for session in [&here, &elsewhere] {
            let row = selector(format!("thread-{}", session.id));
            assert!(
                cx.debug_bounds(row).is_some(),
                "{} should be listed even though it has its own workspace",
                session.id
            );
        }
        assert!(
            cx.debug_bounds("threads-elsewhere").is_some(),
            "the list separates threads that belong somewhere else"
        );
        assert_eq!(
            root.read_with(&cx, |root, _| root.workspace.clone()),
            workspace.to_string_lossy(),
            "the window stays in the workspace it was opened with"
        );

        let row = cx
            .debug_bounds(selector(format!("thread-{}", elsewhere.id)))
            .expect("the other workspace's row");
        cx.simulate_click(row.center(), Modifiers::default());

        assert_eq!(
            root.read_with(&cx, |root, _| root.workspace.clone()),
            default_workspace.to_string_lossy(),
            "opening a thread switches to the workspace it belongs to"
        );
        assert_eq!(
            root.read_with(&cx, |root, _| root.session.as_ref().map(|s| s.id.clone())),
            Some(elsewhere.id),
            "and opens that thread"
        );
    }

    /// A provider whose stream never produces anything, so a turn stays running
    /// until the user stops it. It still records what it was asked.
    #[derive(Default)]
    struct PendingProvider {
        requests: Arc<std::sync::Mutex<Vec<Request>>>,
    }

    impl Provider for PendingProvider {
        fn request(&self, messages: Vec<Message>) -> Request {
            Request::new(DEFAULT_MODEL, messages)
        }

        fn stream(&self, request: Request) -> EventStream {
            self.requests.lock().expect("requests lock").push(request);
            futures::stream::pending().boxed()
        }
    }

    #[gpui::test]
    fn a_thread_the_provider_refuses_summarizes_itself_before_the_next_message(
        cx: &mut TestAppContext,
    ) {
        // A provider that refuses a request over `fits` messages the way a real one
        // refuses a conversation that outgrew the model. The thread runs on the
        // catalogue's model, so the reported-size path can never fire here (no
        // usage is reported at all): the refusal is the only thing that can tell
        // this thread it is too big.
        struct Cramped {
            fits: usize,
            requests: Arc<std::sync::Mutex<Vec<Request>>>,
        }

        impl Provider for Cramped {
            fn request(&self, messages: Vec<Message>) -> Request {
                Request::new(DEFAULT_MODEL, messages)
            }

            fn stream(&self, request: Request) -> EventStream {
                let last = request
                    .messages
                    .last()
                    .map(|message| message.content.clone())
                    .unwrap_or_default();
                let too_long = request.messages.len() > self.fits;
                self.requests.lock().expect("requests lock").push(request);

                let events: Vec<anyhow::Result<StreamEvent>> = if last == THREAD_TITLE_PROMPT {
                    vec![Ok(text("a title")), Ok(finished())]
                } else if last == COMPACTION_PROMPT {
                    vec![Ok(text("what m1 through m3 were about")), Ok(finished())]
                } else if too_long {
                    let refused = anyhow::Error::from(ContextExceeded).context(
                        "DeepSeek request failed with 400 Bad Request: This model's maximum \
                         context length is 163840 tokens.",
                    );
                    vec![Err(refused)]
                } else {
                    vec![Ok(text("ok")), Ok(finished())]
                };
                futures::stream::iter(events).boxed()
            }
        }

        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = Arc::new(Cramped {
            // system + four turns of two messages, plus the message being sent.
            fits: 9,
            requests: requests.clone(),
        });
        let (window, mut cx) = open_window_with(cx, Some(profile), Ok(provider));
        let root = window.root(&mut cx).expect("root view");

        for message in ["m1", "m2", "m3", "m4"] {
            send(&mut cx, &root, message);
        }
        let last = root.read_with(&cx, |root, _| root.turns.len() - 1);
        assert!(
            root.read_with(&cx, |root, _| matches!(
                root.turns[last].state,
                TurnState::Done
            )),
            "four turns still fit"
        );

        // The fifth is refused, and the failure is the provider's own words.
        send(&mut cx, &root, "m5");
        let last = root.read_with(&cx, |root, _| root.turns.len() - 1);
        assert!(
            root.read_with(&cx, |root, _| {
                matches!(&root.turns[last].state, TurnState::Failed(message)
                    if message.contains("maximum context length"))
            }),
            "the person reads what the provider said"
        );
        assert!(
            root.read_with(&cx, |root, _| root.conversation.compaction_due()),
            "the conversation took the refusal as its cue"
        );

        // The sixth is summarized first, so it fits, and the older turns are gone
        // from what the model sees.
        send(&mut cx, &root, "m6");
        let last = root.read_with(&cx, |root, _| root.turns.len() - 1);
        assert!(
            root.read_with(&cx, |root, _| matches!(
                root.turns[last].state,
                TurnState::Done
            )),
            "the turn after the summary goes through"
        );
        assert!(
            cx.debug_bounds("summarized").is_some(),
            "and the timeline says history was summarized"
        );

        let requests = requests.lock().expect("requests lock");
        assert!(
            requests.iter().any(|request| request
                .messages
                .last()
                .is_some_and(|message| message.content == COMPACTION_PROMPT)),
            "a summary was asked for"
        );
        let sent: Vec<&str> = requests
            .last()
            .expect("the last request")
            .messages
            .iter()
            .map(|message| message.content.as_str())
            .collect();
        assert!(
            !sent
                .iter()
                .any(|content| ["m1", "m2", "m3", "m4"].contains(content)),
            "the summarized turns are left out: {sent:?}"
        );
        assert!(
            sent.contains(&"m5") && sent.contains(&"m6"),
            "the two turns the summary does not stand in for are sent in full: {sent:?}"
        );
        assert!(
            sent.iter()
                .any(|content| content.contains("what m1 through m3 were about")),
            "the summary stands in for them: {sent:?}"
        );
    }

    #[gpui::test]
    fn a_thread_that_is_too_big_summarizes_itself_before_the_next_turn(cx: &mut TestAppContext) {
        let (directory, workspace) = temp_workspace();
        let workspace = workspace.to_string_lossy().into_owned();
        let profile = Profile::at(directory.path().join("profile"));
        let store = Store::open(&profile.database_path()).expect("store");
        let session = store
            .create_session(&workspace, Some("deepseek"), Some(DEFAULT_MODEL))
            .expect("session");
        store
            .rename_session(&session.id, "A test thread")
            .expect("name it");
        drop(store);

        // A reply that reports a context already past 75% of the model's limit.
        let full = vec![
            StreamEvent::Text("ok".to_owned()),
            StreamEvent::Usage(humibird_providers::Usage {
                prompt_tokens: 800_000,
                completion_tokens: 10,
                total_tokens: 800_010,
                cache_hit_tokens: 0,
            }),
            finished(),
        ];
        let summary = vec![
            StreamEvent::Text("the first two turns in one page".to_owned()),
            finished(),
        ];
        let (provider, requests) = scripted(
            vec![
                full.clone(),
                full.clone(),
                full.clone(),
                summary,
                full.clone(),
            ],
            full,
        );
        let (window, mut cx) =
            open_window_in_workspace(cx, Some(profile.clone()), Ok(provider), workspace.as_ref());
        let root = window.root(&mut cx).expect("root view");

        for message in ["m1", "m2", "m3"] {
            send(&mut cx, &root, message);
        }
        assert_eq!(
            root.read_with(&cx, |root, _| root.conversation.context_tokens()),
            800_010,
            "the reported usage is what the thread measures itself with"
        );
        assert!(
            root.read_with(&cx, |root, _| root.compaction.is_none()),
            "three turns are not enough to summarize"
        );

        send(&mut cx, &root, "m4");

        let requests = requests.lock().expect("requests lock");
        assert_eq!(
            requests.len(),
            5,
            "three turns, the summary, then the fourth"
        );
        assert_eq!(
            requests[3].messages.last().map(|m| m.content.as_str()),
            Some(COMPACTION_PROMPT),
            "the summary is asked for before the turn is sent"
        );
        let contents: Vec<&str> = requests[4]
            .messages
            .iter()
            .map(|message| message.content.as_str())
            .collect();
        assert!(
            contents[1].contains("the first two turns in one page"),
            "the fourth request carries the summary: {contents:?}"
        );
        assert!(
            !contents
                .iter()
                .any(|content| *content == "m1" || *content == "m2"),
            "and leaves out what it stands in for: {contents:?}"
        );
        assert!(contents.contains(&"m3") && contents.contains(&"m4"));
        assert!(
            root.read_with(&cx, |root, _| root.compaction.is_some()),
            "the boundary was recorded"
        );
        assert!(
            cx.debug_bounds("summarized").is_some(),
            "the timeline says history was summarized"
        );

        // The transcript keeps everything: compaction is a boundary, not an edit.
        let store = Store::open(&profile.database_path()).expect("reopen");
        let stored = store.messages(&session.id).expect("messages");
        assert!(
            stored.iter().any(|message| message.content == "m1")
                && stored.iter().any(|message| message.content == "m4"),
            "the messages are untouched: {stored:?}"
        );
    }

    #[gpui::test]
    fn a_message_sent_during_a_turn_is_queued_and_sent_after_it_ends(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        let provider = Arc::new(PendingProvider::default());
        let requests = provider.requests.clone();
        let (window, mut cx) = open_window_with(cx, Some(profile), Ok(provider));
        let root = window.root(&mut cx).expect("root view");

        send(&mut cx, &root, "first");
        assert!(root.read_with(&cx, |root, _| root.turn_running()));

        // The turn is still running, so this one waits its turn instead of being
        // dropped.
        send(&mut cx, &root, "second");
        assert_eq!(
            root.read_with(&cx, |root, _| root.queued.len()),
            1,
            "the message is queued"
        );
        assert!(
            cx.debug_bounds("queued").is_some(),
            "and shown above the input"
        );
        assert_eq!(
            requests.lock().expect("requests lock").len(),
            1,
            "nothing was sent while the turn was busy"
        );

        let stop = cx.debug_bounds("stop").expect("the stop button");
        cx.simulate_click(stop.center(), Modifiers::default());
        cx.run_until_parked();

        assert!(
            root.read_with(&cx, |root, _| root.queued.is_empty()),
            "the queue was sent"
        );
        assert_eq!(
            root.read_with(&cx, |root, _| root.turns.len()),
            2,
            "the queued message is its own turn"
        );
        assert_eq!(
            root.read_with(&cx, |root, _| root.turns[1].user.clone()),
            "second"
        );
        assert_eq!(
            requests.lock().expect("requests lock").len(),
            2,
            "and it was sent"
        );
    }

    #[gpui::test]
    fn only_the_front_queued_message_can_steer(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        let provider: Arc<dyn Provider> = Arc::new(PendingProvider::default());
        let (window, mut cx) = open_window_with(cx, Some(profile), Ok(provider));
        let root = window.root(&mut cx).expect("root view");

        send(&mut cx, &root, "first");
        send(&mut cx, &root, "second");
        send(&mut cx, &root, "third");
        assert_eq!(root.read_with(&cx, |root, _| root.queued.len()), 2);

        // Steering the second message does nothing while the first is in front.
        let second = cx.debug_bounds("queued-steer-1").expect("the toggle");
        cx.simulate_click(second.center(), Modifiers::default());
        assert!(
            !root.read_with(&cx, |root, _| root.front_wants_steer()),
            "only the front message decides"
        );

        let first = cx.debug_bounds("queued-steer-0").expect("the toggle");
        cx.simulate_click(first.center(), Modifiers::default());
        assert!(root.read_with(&cx, |root, _| root.front_wants_steer()));

        // Removing the front promotes the second, which was already steering.
        let remove = cx
            .debug_bounds("queued-remove-0")
            .expect("the remove button");
        cx.simulate_click(remove.center(), Modifiers::default());
        assert_eq!(root.read_with(&cx, |root, _| root.queued.len()), 1);
        assert!(root.read_with(&cx, |root, _| root.front_wants_steer()));
    }

    #[gpui::test]
    fn a_new_thread_leaves_the_queue_behind(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        let provider: Arc<dyn Provider> = Arc::new(PendingProvider::default());
        let (window, mut cx) = open_window_with(cx, Some(profile), Ok(provider));
        let root = window.root(&mut cx).expect("root view");

        send(&mut cx, &root, "first");
        send(&mut cx, &root, "queued");
        assert_eq!(root.read_with(&cx, |root, _| root.queued.len()), 1);

        cx.simulate_keystrokes("cmd-n");
        assert_eq!(
            root.read_with(&cx, |root, _| root.queued.len()),
            0,
            "a queued message belongs to the thread it was written in"
        );
        assert!(cx.debug_bounds("queued").is_none(), "and the rows are gone");
    }

    #[gpui::test]
    fn a_steering_message_ends_the_turn_at_the_next_boundary(cx: &mut TestAppContext) {
        let (directory, workspace) = temp_workspace();
        std::fs::write(workspace.join("notes.md"), "hello\n").expect("write the file");
        let profile = Profile::at(directory.path().join("profile"));
        // A named session, so the turn does not also ask for a title.
        let store = Store::open(&profile.database_path()).expect("store");
        let session = store
            .create_session(
                &workspace.to_string_lossy(),
                Some("deepseek"),
                Some(DEFAULT_MODEL),
            )
            .expect("session");
        store
            .rename_session(&session.id, "A test thread")
            .expect("name it");
        drop(store);
        let call = vec![
            StreamEvent::ToolCall(ToolCall {
                id: "call_1".to_owned(),
                name: "read_file".to_owned(),
                arguments: "{\"path\":\"notes.md\"}".to_owned(),
            }),
            tool_calls(),
        ];
        let answer = vec![StreamEvent::Text("ok".to_owned()), finished()];
        let (provider, requests) = scripted(vec![call], answer);
        let (window, mut cx) =
            open_window_in_workspace(cx, Some(profile), Ok(provider), &workspace);
        let root = window.root(&mut cx).expect("root view");

        // A message that was waiting when the turn started, marked to steer. The
        // queue is what the composer writes to, so this is the same state the
        // user produces by typing while a turn runs.
        root.update_in(&mut cx, |root, _, cx| {
            root.queued.push(QueuedMessage {
                text: "actually do something else".to_owned(),
                attachments: Vec::new(),
                steer: true,
            });
            cx.notify();
        });

        send(&mut cx, &root, "run the command");
        cx.run_until_parked();

        let (steps, state) = root.read_with(&cx, |root, _| {
            (root.turns[0].tool_steps.len(), root.turns[0].state.clone())
        });
        assert_eq!(
            steps, 1,
            "the turn ended after the first boundary instead of asking for more"
        );
        assert_eq!(state, TurnState::Done, "and it ended normally");

        let requests = requests.lock().expect("requests lock");
        assert_eq!(
            requests.len(),
            2,
            "one round for the turn, one for the steering message"
        );
        assert_eq!(
            requests[1].messages.last().map(|m| m.content.as_str()),
            Some("actually do something else"),
            "the second request is the steering message, not a continuation"
        );
        assert!(
            root.read_with(&cx, |root, _| root
                .turns
                .iter()
                .any(|turn| turn.user == "actually do something else")),
            "the steering message became its own turn"
        );
    }

    #[gpui::test]
    fn stopping_a_turn_leaves_it_stopped_and_ready_for_the_next(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        let provider: Arc<dyn Provider> = Arc::new(PendingProvider::default());
        let (window, mut cx) = open_window_with(cx, Some(profile), Ok(provider));
        let root = window.root(&mut cx).expect("root view");

        send(&mut cx, &root, "hello");
        assert!(
            root.read_with(&cx, |root, _| root.turn_running()),
            "the turn waits for a model that never answers"
        );
        assert!(
            cx.debug_bounds("stop").is_some(),
            "the send button becomes a stop button"
        );
        assert!(cx.debug_bounds("send").is_none());

        let stop = cx.debug_bounds("stop").expect("the stop button");
        cx.simulate_click(stop.center(), Modifiers::default());

        assert!(
            !root.read_with(&cx, |root, _| root.turn_running()),
            "the turn is cancelled"
        );
        assert_eq!(
            root.read_with(&cx, |root, _| root.turns[0].state.clone()),
            TurnState::Stopped
        );
        assert!(
            cx.debug_bounds("send").is_some(),
            "and the composer is ready again"
        );

        send(&mut cx, &root, "again");
        assert_eq!(
            root.read_with(&cx, |root, _| root.turns.len()),
            2,
            "a new turn can start from the stopped one"
        );
    }

    /// A provider that answers from a script, one response per request. Once
    /// the script runs out it repeats `fallback`, which is how a looping model
    /// is simulated.
    struct ScriptedProvider {
        requests: Arc<std::sync::Mutex<Vec<Request>>>,
        responses: Arc<std::sync::Mutex<std::collections::VecDeque<Vec<StreamEvent>>>>,
        fallback: Vec<StreamEvent>,
    }

    impl Provider for ScriptedProvider {
        fn request(&self, messages: Vec<Message>) -> Request {
            Request::new("deepseek-flash", messages)
        }

        /// It stands in for a provider that catalogs the model it is asked about,
        /// which is what the compaction tests need: a window to measure against.
        fn context_limit(&self, _model: &str) -> Option<u32> {
            Some(1_000_000)
        }

        fn stream(&self, request: Request) -> EventStream {
            self.requests.lock().expect("requests lock").push(request);
            let events = self
                .responses
                .lock()
                .expect("responses lock")
                .pop_front()
                .unwrap_or_else(|| self.fallback.clone());
            futures::stream::iter(events.into_iter().map(Ok)).boxed()
        }
    }

    fn scripted(
        responses: Vec<Vec<StreamEvent>>,
        fallback: Vec<StreamEvent>,
    ) -> (Arc<ScriptedProvider>, Arc<std::sync::Mutex<Vec<Request>>>) {
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = Arc::new(ScriptedProvider {
            requests: requests.clone(),
            responses: Arc::new(std::sync::Mutex::new(responses.into())),
            fallback,
        });
        (provider, requests)
    }

    fn text(delta: &str) -> StreamEvent {
        StreamEvent::Text(delta.to_owned())
    }

    fn finished() -> StreamEvent {
        StreamEvent::Finished {
            reason: StopReason::Stop,
        }
    }

    /// A provider whose answer the test releases a stretch at a time, so it can
    /// look at the conversation, and scroll it, while the model is still talking.
    struct GatedProvider {
        requests: Arc<std::sync::Mutex<Vec<Request>>>,
        replies: Replies,
    }

    /// The test's end of each reply, in the order the answers were asked for.
    type Replies = Arc<std::sync::Mutex<Vec<futures::channel::mpsc::UnboundedSender<StreamEvent>>>>;

    impl GatedProvider {
        fn new() -> (Arc<Self>, Replies) {
            let replies = Replies::default();
            (
                Arc::new(Self {
                    requests: Arc::default(),
                    replies: replies.clone(),
                }),
                replies,
            )
        }
    }

    impl Provider for GatedProvider {
        fn request(&self, messages: Vec<Message>) -> Request {
            Request::new(DEFAULT_MODEL, messages)
        }

        fn stream(&self, request: Request) -> EventStream {
            let named = request
                .messages
                .last()
                .is_some_and(|message| message.content == THREAD_TITLE_PROMPT);
            self.requests.lock().expect("requests lock").push(request);
            if named {
                return futures::stream::iter(vec![
                    Ok(StreamEvent::Text("A thread".to_owned())),
                    Ok(finished()),
                ])
                .boxed();
            }
            let (sender, receiver) = futures::channel::mpsc::unbounded();
            self.replies.lock().expect("replies lock").push(sender);
            futures::stream::unfold(receiver, |mut receiver| async move {
                receiver.next().await.map(|event| (Ok(event), receiver))
            })
            .boxed()
        }
    }

    /// Releases a stretch of the `index`th answer.
    fn reply(replies: &Replies, index: usize, events: Vec<StreamEvent>) {
        let mut replies = replies.lock().expect("replies lock");
        let sender = replies
            .get_mut(index)
            .expect("the answer is being asked for");
        for event in events {
            sender
                .unbounded_send(event)
                .expect("the turn is still reading its reply");
        }
    }

    fn tool_calls() -> StreamEvent {
        StreamEvent::Finished {
            reason: StopReason::ToolCalls,
        }
    }

    fn title_requests(requests: &Arc<std::sync::Mutex<Vec<Request>>>) -> Vec<Request> {
        requests
            .lock()
            .expect("requests lock")
            .iter()
            .filter(|request| {
                request
                    .messages
                    .last()
                    .is_some_and(|message| message.content == THREAD_TITLE_PROMPT)
            })
            .cloned()
            .collect()
    }

    #[gpui::test]
    fn the_first_reply_names_the_session_once(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = Arc::new(FakeProvider {
            requests: requests.clone(),
            events: vec![
                StreamEvent::Text("here is the answer".to_owned()),
                StreamEvent::Finished {
                    reason: StopReason::Stop,
                },
            ],
            // The model adds a second line, which must not reach the sidebar.
            title: Some("Fix the flaky agent test\nHope that helps!".to_owned()),
        });
        let (window, mut cx) = open_window_with(cx, Some(profile.clone()), Ok(provider));
        let root = window.root(&mut cx).expect("root view");

        send(&mut cx, &root, "the agent test flakes sometimes");
        cx.update(|_, _| {});

        assert_eq!(
            root.read_with(&cx, |root, _| root
                .session
                .as_ref()
                .and_then(|session| session.title.clone())),
            Some("Fix the flaky agent test".to_owned())
        );
        let stored = Store::open(&profile.database_path()).expect("the store reopens");
        assert_eq!(
            stored.sessions().expect("sessions")[0].title.as_deref(),
            Some("Fix the flaky agent test")
        );

        let first = title_requests(&requests);
        assert_eq!(first.len(), 1, "one title request per session");
        // Truncating the answer with `max_tokens` starves a thinking model.
        assert_eq!(first[0].max_tokens, None);
        assert_eq!(first[0].temperature, Some(0.2));

        send(&mut cx, &root, "and the second question");
        cx.update(|_, _| {});
        assert_eq!(
            title_requests(&requests).len(),
            1,
            "a named session is not renamed on every reply"
        );
    }

    #[gpui::test]
    fn a_silent_title_request_falls_back_to_the_first_message(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        let provider = Arc::new(FakeProvider {
            requests: Arc::new(std::sync::Mutex::new(Vec::new())),
            events: vec![
                StreamEvent::Text("on it".to_owned()),
                StreamEvent::Finished {
                    reason: StopReason::Stop,
                },
            ],
            // Every token went into reasoning, which is not a title.
            title: Some(String::new()),
        });
        let (window, mut cx) = open_window_with(cx, Some(profile), Ok(provider));
        let root = window.root(&mut cx).expect("root view");

        send(&mut cx, &root, "fix the flaky agent test");
        cx.update(|_, _| {});

        assert_eq!(
            root.read_with(&cx, |root, _| root
                .session
                .as_ref()
                .and_then(|session| session.title.clone())),
            Some("fix the flaky agent test".to_owned())
        );
    }

    #[test]
    fn the_workspace_chip_shows_the_directory_name() {
        assert_eq!(workspace_name("/Users/me/code/humibird"), "humibird");
        assert_eq!(workspace_name(""), "");
    }

    /// Opens a window whose tools are pinned to `workspace`, so a test can let
    /// the model write files without touching the repository.
    fn open_window_in_workspace(
        cx: &mut TestAppContext,
        profile: Option<Profile>,
        provider: Result<Arc<dyn Provider>, String>,
        workspace: &std::path::Path,
    ) -> (WindowHandle<HumibirdRoot>, VisualTestContext) {
        cx.update(init);
        let workspace = workspace.to_string_lossy().into_owned();

        let window = cx.update(move |cx| {
            let bounds = Bounds::centered(None, size(px(1280.), px(820.)), cx);
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                move |_, cx| {
                    cx.new(move |cx| {
                        HumibirdRoot::new_with_workspace(
                            cx,
                            profile,
                            provider,
                            no_process_env(),
                            workspace,
                        )
                    })
                },
            )
            .expect("window opens")
        });

        let visual = VisualTestContext::from_window(window.into(), cx);
        (window, visual)
    }

    fn write_call(id: &str, path: &str, content: &str) -> Vec<StreamEvent> {
        vec![
            StreamEvent::ToolCall(ToolCall {
                id: id.to_owned(),
                name: "write_file".to_owned(),
                arguments: serde_json::json!({"path": path, "content": content}).to_string(),
            }),
            tool_calls(),
        ]
    }

    fn temp_workspace() -> (tempfile::TempDir, std::path::PathBuf) {
        let directory = tempfile::tempdir().expect("temp dir");
        let workspace = directory.path().join("project");
        std::fs::create_dir_all(&workspace).expect("create workspace");
        (directory, workspace)
    }

    #[gpui::test]
    fn a_write_call_waits_for_approval(cx: &mut TestAppContext) {
        let (directory, workspace) = temp_workspace();
        let profile = Profile::at(directory.path().join("profile"));
        let answer = vec![StreamEvent::Text("done".to_owned()), finished()];
        let (provider, requests) = scripted(
            vec![write_call("call_1", "notes.md", "hello"), answer.clone()],
            answer,
        );
        let (window, mut cx) =
            open_window_in_workspace(cx, Some(profile), Ok(provider), &workspace);
        let root = window.root(&mut cx).expect("root view");

        send(&mut cx, &root, "write the notes");
        cx.update(|_, _| {});

        assert!(cx.debug_bounds("approval").is_some(), "the call is waiting");
        assert!(
            cx.debug_bounds(selector("header-workspace-project".to_owned()))
                .is_some(),
            "the composer shows the workspace name"
        );
        let awaiting = root.read_with(&cx, |root, _| {
            root.turns[0]
                .tool_steps
                .last()
                .is_some_and(|step| step.state == StepState::Awaiting)
        });
        assert!(awaiting, "the prompt belongs to the waiting call");
        assert!(
            !workspace.join("notes.md").exists(),
            "nothing is written before the answer"
        );

        let approve = cx.debug_bounds("approve-once").expect("approve button");
        cx.simulate_click(approve.center(), Modifiers::default());
        cx.run_until_parked();
        cx.update(|_, _| {});

        assert_eq!(
            std::fs::read_to_string(workspace.join("notes.md")).expect("the file exists"),
            "hello"
        );
        assert_eq!(
            root.read_with(&cx, |root, _| root.turns[0].assistant_text.clone()),
            "done"
        );
        assert!(cx.debug_bounds("approval").is_none());

        let requests = requests.lock().expect("requests lock");
        assert!(
            requests[1].messages[3].content.contains("wrote 5 bytes"),
            "{}",
            requests[1].messages[3].content
        );
    }

    #[gpui::test]
    fn denying_a_call_tells_the_model_and_writes_nothing(cx: &mut TestAppContext) {
        let (directory, workspace) = temp_workspace();
        let profile = Profile::at(directory.path().join("profile"));
        let answer = vec![StreamEvent::Text("understood".to_owned()), finished()];
        let (provider, requests) = scripted(
            vec![write_call("call_1", "notes.md", "hello"), answer.clone()],
            answer,
        );
        let (window, mut cx) =
            open_window_in_workspace(cx, Some(profile), Ok(provider), &workspace);
        let root = window.root(&mut cx).expect("root view");

        send(&mut cx, &root, "write the notes");
        cx.update(|_, _| {});

        let deny = cx.debug_bounds("deny-call").expect("deny button");
        cx.simulate_click(deny.center(), Modifiers::default());
        cx.run_until_parked();
        cx.update(|_, _| {});

        assert!(
            !workspace.join("notes.md").exists(),
            "denied means no write"
        );
        let failed = root.read_with(&cx, |root, _| {
            root.turns[0].tool_steps[0].state == StepState::Done { failed: true }
        });
        assert!(failed, "the step is marked as failed");

        let requests = requests.lock().expect("requests lock");
        assert!(
            requests[1].messages[3].content.contains("denied"),
            "{}",
            requests[1].messages[3].content
        );
    }

    #[gpui::test]
    fn allowing_a_tool_for_the_session_skips_later_prompts(cx: &mut TestAppContext) {
        let (directory, workspace) = temp_workspace();
        let profile = Profile::at(directory.path().join("profile"));
        let answer = vec![StreamEvent::Text("both written".to_owned()), finished()];
        let (provider, _requests) = scripted(
            vec![
                write_call("call_1", "one.md", "one"),
                write_call("call_2", "two.md", "two"),
                answer.clone(),
            ],
            answer,
        );
        let (window, mut cx) =
            open_window_in_workspace(cx, Some(profile), Ok(provider), &workspace);
        let root = window.root(&mut cx).expect("root view");

        send(&mut cx, &root, "write both notes");
        cx.update(|_, _| {});

        let always = cx.debug_bounds("approve-always").expect("always button");
        cx.simulate_click(always.center(), Modifiers::default());
        cx.run_until_parked();
        cx.update(|_, _| {});

        assert_eq!(
            std::fs::read_to_string(workspace.join("one.md")).expect("one exists"),
            "one"
        );
        assert_eq!(
            std::fs::read_to_string(workspace.join("two.md")).expect("two exists"),
            "two"
        );
        assert!(
            cx.debug_bounds("approval").is_none(),
            "the second call did not ask again"
        );
        assert!(
            root.read_with(&cx, |root, _| root.approved_tools.contains("write_file")),
            "the session remembers the choice for later turns"
        );
        assert_eq!(
            root.read_with(&cx, |root, _| root.turns[0].tool_steps.len()),
            2
        );
    }

    #[gpui::test]
    fn two_calls_that_wait_for_consent_are_both_asked(cx: &mut TestAppContext) {
        let (window, mut cx) = open_window(cx);
        let root = window.root(&mut cx).expect("root view");

        // A turn and the subagent it started run at the same time, so both can be
        // waiting for consent at once.
        let mut decided = Vec::new();
        for (sink, summary) in [
            (Sink::Turn(0), "notes.md"),
            (Sink::Delegation { turn: 0, step: 0 }, "other.md"),
        ] {
            let (respond, answer) = oneshot::channel();
            decided.push(answer);
            let event = AgentEvent::Approval {
                index: 0,
                name: "write_file".to_owned(),
                key: format!("write_file {summary}"),
                summary: summary.to_owned(),
                respond,
            };
            cx.update(|_, cx| root.update(cx, |root, cx| root.apply(sink, event, cx)));
        }

        let waiting = |cx: &VisualTestContext| {
            root.read_with(cx, |root, _| {
                root.pending_approvals
                    .iter()
                    .map(|pending| pending.summary.clone())
                    .collect::<Vec<_>>()
            })
        };
        assert_eq!(
            waiting(&cx),
            ["notes.md", "other.md"],
            "neither call is dropped, and they are asked in order"
        );

        cx.update(|_, cx| root.update(cx, |root, cx| root.answer_approval(Decision::Always, cx)));
        assert_eq!(
            decided[0].try_recv().expect("the turn is still waiting"),
            Some(Decision::Always)
        );
        assert_eq!(waiting(&cx), ["other.md"], "the next call takes the card");

        cx.update(|_, cx| root.update(cx, |root, cx| root.answer_approval(Decision::Deny, cx)));
        assert_eq!(
            decided[1]
                .try_recv()
                .expect("the subagent is still waiting"),
            Some(Decision::Deny)
        );
        assert!(waiting(&cx).is_empty(), "nothing is left waiting");
        assert!(
            root.read_with(&cx, |root, _| root
                .approved_tools
                .contains("write_file notes.md")),
            "always covers the call that was approved, not the one after it"
        );
    }

    #[gpui::test]
    fn a_terminal_call_runs_after_approval(cx: &mut TestAppContext) {
        let (directory, workspace) = temp_workspace();
        let profile = Profile::at(directory.path().join("profile"));
        let answer = vec![StreamEvent::Text("done".to_owned()), finished()];
        let call = vec![
            StreamEvent::ToolCall(ToolCall {
                id: "call_1".to_owned(),
                name: "terminal".to_owned(),
                arguments: serde_json::json!({"command": "echo hi > made.txt"}).to_string(),
            }),
            tool_calls(),
        ];
        let (provider, requests) = scripted(vec![call, answer.clone()], answer);
        let (window, mut cx) =
            open_window_in_workspace(cx, Some(profile), Ok(provider), &workspace);
        let root = window.root(&mut cx).expect("root view");

        send(&mut cx, &root, "make the file");
        cx.update(|_, _| {});
        let approve = cx
            .debug_bounds("approve-once")
            .expect("the terminal call needs approval");
        cx.simulate_click(approve.center(), Modifiers::default());
        cx.run_until_parked();
        cx.update(|_, _| {});

        assert_eq!(
            std::fs::read_to_string(workspace.join("made.txt")).expect("the command ran"),
            "hi\n"
        );
        let (name, failed) = root.read_with(&cx, |root, _| {
            let step = &root.turns[0].tool_steps[0];
            (
                step.name.clone(),
                matches!(step.state, StepState::Done { failed: true }),
            )
        });
        assert_eq!(name, "terminal");
        assert!(!failed);

        let requests = requests.lock().expect("requests lock");
        assert_eq!(requests[1].messages[3].content, "(no output)");
    }

    #[gpui::test]
    fn the_session_starts_with_a_system_prompt(cx: &mut TestAppContext) {
        use humibird_providers::Role as MessageRole;

        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        std::fs::create_dir_all(profile.root()).expect("create the profile");
        std::fs::write(profile.soul_path(), "Prefer short answers.\n").expect("write SOUL.md");

        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = Arc::new(FakeProvider {
            requests: requests.clone(),
            events: vec![StreamEvent::Text("ok".to_owned()), finished()],
            title: None,
        });
        let (window, mut cx) = open_window_with(cx, Some(profile), Ok(provider));
        let root = window.root(&mut cx).expect("root view");

        send(&mut cx, &root, "hello");
        send(&mut cx, &root, "again");

        let requests = requests.lock().expect("requests lock");
        let first = requests
            .iter()
            .find(|request| {
                request
                    .messages
                    .iter()
                    .any(|message| message.content == "hello")
            })
            .expect("the first request");
        assert_eq!(first.messages[0].role, MessageRole::System);
        assert!(first.messages[0].content.contains("You are humibird"));
        assert!(first.messages[0].content.contains("Prefer short answers."));

        let second = requests
            .iter()
            .find(|request| {
                request
                    .messages
                    .iter()
                    .any(|message| message.content == "again")
            })
            .expect("the second request");
        assert_eq!(second.messages[0].role, MessageRole::System);
        assert_eq!(
            second.messages[0].content, first.messages[0].content,
            "the prefix must not change inside one session"
        );
    }

    #[gpui::test]
    fn a_new_thread_rebuilds_the_system_prompt(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        std::fs::create_dir_all(profile.root()).expect("create the profile");
        std::fs::write(profile.soul_path(), "First persona.\n").expect("write SOUL.md");

        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = Arc::new(FakeProvider {
            requests: requests.clone(),
            events: vec![StreamEvent::Text("ok".to_owned()), finished()],
            title: None,
        });
        let (window, mut cx) = open_window_with(cx, Some(profile.clone()), Ok(provider));
        let root = window.root(&mut cx).expect("root view");

        send(&mut cx, &root, "one");
        std::fs::write(profile.soul_path(), "Second persona.\n").expect("rewrite SOUL.md");
        cx.simulate_keystrokes("cmd-n");
        send(&mut cx, &root, "two");

        let requests = requests.lock().expect("requests lock");
        let prompt_for = |needle: &str| {
            requests
                .iter()
                .find(|request| {
                    request
                        .messages
                        .iter()
                        .any(|message| message.content == needle)
                })
                .map(|request| request.messages[0].content.clone())
                .expect(needle)
        };

        assert!(prompt_for("one").contains("First persona."));
        assert!(prompt_for("two").contains("Second persona."));
    }

    #[gpui::test]
    fn memory_is_read_once_and_frozen_for_the_session(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        let memories = MemoryPaths::in_profile(profile.root());
        std::fs::create_dir_all(memories.user.parent().expect("memories dir"))
            .expect("create the memories directory");
        std::fs::write(&memories.user, "Prefers short answers.\n").expect("write USER.md");
        std::fs::write(&memories.agent, "Keep notes in docs/.\n").expect("write MEMORY.md");

        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = Arc::new(FakeProvider {
            requests: requests.clone(),
            events: vec![StreamEvent::Text("ok".to_owned()), finished()],
            title: None,
        });
        let (window, mut cx) = open_window_with(cx, Some(profile), Ok(provider));
        let root = window.root(&mut cx).expect("root view");

        send(&mut cx, &root, "one");
        std::fs::write(&memories.user, "Prefers long answers.\n").expect("rewrite USER.md");
        send(&mut cx, &root, "two");
        cx.simulate_keystrokes("cmd-n");
        send(&mut cx, &root, "three");

        let requests = requests.lock().expect("requests lock");
        let prompt_for = |needle: &str| {
            requests
                .iter()
                .find(|request| {
                    request
                        .messages
                        .iter()
                        .any(|message| message.content == needle)
                })
                .map(|request| request.messages[0].content.clone())
                .expect(needle)
        };

        let first = prompt_for("one");
        for expected in [
            "## Memory",
            "### About the user\n\n1. Prefers short answers.",
            "### Your notes\n\n1. Keep notes in docs/.",
            "the `memory` tool",
        ] {
            assert!(
                first.contains(expected),
                "missing {expected:?} in:\n{first}"
            );
        }
        assert_eq!(
            first,
            prompt_for("two"),
            "editing the file mid-session must not change the prompt"
        );
        assert!(!first.contains("long"), "{first}");

        assert!(
            prompt_for("three").contains("1. Prefers long answers."),
            "a new session reads the file again"
        );
    }

    #[gpui::test]
    fn the_memory_tool_writes_without_approval_and_applies_next_session(cx: &mut TestAppContext) {
        let (directory, workspace) = temp_workspace();
        let profile = Profile::at(directory.path().join("profile"));
        let memories = MemoryPaths::in_profile(profile.root());
        let answer = vec![StreamEvent::Text("Noted.".to_owned()), finished()];
        let call = vec![
            StreamEvent::ToolCall(ToolCall {
                id: "call_1".to_owned(),
                name: "memory".to_owned(),
                arguments: serde_json::json!({
                    "operation": "add",
                    "target": "user",
                    "text": "Likes tables for comparisons."
                })
                .to_string(),
            }),
            tool_calls(),
        ];
        let (provider, requests) = scripted(vec![call, answer.clone()], answer);
        let (window, mut cx) =
            open_window_in_workspace(cx, Some(profile), Ok(provider), &workspace);
        let root = window.root(&mut cx).expect("root view");

        send(&mut cx, &root, "remember that I like tables");

        let saved = std::fs::read_to_string(&memories.user).expect("USER.md was written");
        assert!(saved.contains("Likes tables for comparisons."), "{saved}");
        let (name, summary, done) = root.read_with(&cx, |root, _| {
            let step = &root.turns[0].tool_steps[0];
            (
                step.name.clone(),
                step.summary.clone(),
                step.state == StepState::Done { failed: false },
            )
        });
        assert_eq!(name, "memory");
        assert_eq!(summary, "Likes tables for comparisons.");
        assert!(done, "the call ran without waiting for the user");
        assert!(
            cx.debug_bounds("approval").is_none(),
            "memory edits never ask for approval"
        );

        {
            let requests = requests.lock().expect("requests lock");
            assert!(
                !requests[1].messages[0]
                    .content
                    .contains("Likes tables for comparisons"),
                "the snapshot was taken before the call:\n{}",
                requests[1].messages[0].content
            );
        }

        cx.simulate_keystrokes("cmd-n");
        send(&mut cx, &root, "what do you know about me");

        let requests = requests.lock().expect("requests lock");
        let last = requests.last().expect("the new session asked");
        assert!(
            last.messages[0]
                .content
                .contains("1. Likes tables for comparisons."),
            "{}",
            last.messages[0].content
        );
    }

    /// Writes a skill into the profile the way an import would.
    fn install_skill(profile: &Profile, name: &str, description: &str, body: &str) -> PathBuf {
        let directory = profile.skills_dir().join(name);
        std::fs::create_dir_all(&directory).expect("create the skill directory");
        std::fs::write(
            directory.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\n\n{body}\n"),
        )
        .expect("write SKILL.md");
        directory
    }

    #[gpui::test]
    fn the_skill_index_is_read_once_and_frozen_for_the_session(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        install_skill(&profile, "card", "turns a JD into a card", "# Card");

        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = Arc::new(FakeProvider {
            requests: requests.clone(),
            events: vec![StreamEvent::Text("ok".to_owned()), finished()],
            title: None,
        });
        let (window, mut cx) = open_window_with(cx, Some(profile.clone()), Ok(provider));
        let root = window.root(&mut cx).expect("root view");

        send(&mut cx, &root, "one");
        install_skill(&profile, "review", "reviews a diff", "# Review");
        send(&mut cx, &root, "two");
        cx.simulate_keystrokes("cmd-n");
        send(&mut cx, &root, "three");

        let requests = requests.lock().expect("requests lock");
        let prompt_for = |needle: &str| {
            requests
                .iter()
                .find(|request| {
                    request
                        .messages
                        .iter()
                        .any(|message| message.content == needle)
                })
                .map(|request| request.messages[0].content.clone())
                .expect(needle)
        };

        let first = prompt_for("one");
        for expected in [
            "## Skills",
            "- `card`: turns a JD into a card",
            "`read_skill`",
        ] {
            assert!(
                first.contains(expected),
                "missing {expected:?} in:\n{first}"
            );
        }
        assert!(!first.contains("review"), "{first}");
        assert_eq!(
            first,
            prompt_for("two"),
            "installing a skill mid-session must not change the prompt"
        );

        let third = prompt_for("three");
        assert!(
            third.contains("- `review`: reviews a diff"),
            "a new session reads the skills again:\n{third}"
        );
    }

    #[gpui::test]
    fn a_profile_without_skills_gets_neither_the_section_nor_the_tool(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = Arc::new(FakeProvider {
            requests: requests.clone(),
            events: vec![StreamEvent::Text("ok".to_owned()), finished()],
            title: None,
        });
        let (window, mut cx) = open_window_with(cx, Some(profile), Ok(provider));
        let root = window.root(&mut cx).expect("root view");

        send(&mut cx, &root, "hello");

        let requests = requests.lock().expect("requests lock");
        assert!(!requests[0].messages[0].content.contains("## Skills"));
        let names: Vec<_> = requests[0]
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect();
        assert!(!names.contains(&"read_skill"), "{names:?}");
    }

    #[gpui::test]
    fn the_model_can_load_a_skill_before_answering(cx: &mut TestAppContext) {
        let (directory, workspace) = temp_workspace();
        let profile = Profile::at(directory.path().join("profile"));
        install_skill(
            &profile,
            "card",
            "turns a JD into a card",
            "# Card\n\nUse one page.",
        );
        let answer = vec![StreamEvent::Text("Done.".to_owned()), finished()];
        let call = vec![
            StreamEvent::ToolCall(ToolCall {
                id: "call_1".to_owned(),
                name: "read_skill".to_owned(),
                arguments: serde_json::json!({"name": "card"}).to_string(),
            }),
            tool_calls(),
        ];
        let (provider, requests) = scripted(vec![call, answer.clone()], answer);
        let (window, mut cx) =
            open_window_in_workspace(cx, Some(profile), Ok(provider), &workspace);
        let root = window.root(&mut cx).expect("root view");

        send(&mut cx, &root, "make me a card");

        let (name, summary, done) = root.read_with(&cx, |root, _| {
            let step = &root.turns[0].tool_steps[0];
            (
                step.name.clone(),
                step.summary.clone(),
                step.state == StepState::Done { failed: false },
            )
        });
        assert_eq!(name, "read_skill");
        assert_eq!(summary, "card");
        assert!(done, "reading a skill never asks for approval: {summary}");
        assert!(cx.debug_bounds("approval").is_none());

        let requests = requests.lock().expect("requests lock");
        let result = requests[1].messages.last().expect("the result");
        assert!(
            result.content.contains("Use one page."),
            "the model should see the body: {}",
            result.content
        );
    }

    /// The four pieces of an `ask_user` call: one question, some options, free
    /// text or not.
    fn ask_call(
        id: &str,
        question: &str,
        options: &[&str],
        allow_free_text: bool,
    ) -> Vec<StreamEvent> {
        vec![
            StreamEvent::ToolCall(ToolCall {
                id: id.to_owned(),
                name: "ask_user".to_owned(),
                arguments: serde_json::json!({
                    "question": question,
                    "options": options,
                    "allow_free_text": allow_free_text,
                })
                .to_string(),
            }),
            tool_calls(),
        ]
    }

    #[gpui::test]
    fn the_model_can_ask_a_question_and_a_clicked_option_is_the_answer(cx: &mut TestAppContext) {
        let (directory, workspace) = temp_workspace();
        let profile = Profile::at(directory.path().join("profile"));
        let answer = vec![
            StreamEvent::Text("Rewriting the parser.".to_owned()),
            finished(),
        ];
        let (provider, requests) = scripted(
            vec![
                ask_call(
                    "call_1",
                    "Which approach?",
                    &["Rewrite the parser", "Patch the caller"],
                    false,
                ),
                answer.clone(),
            ],
            answer,
        );
        let (window, mut cx) =
            open_window_in_workspace(cx, Some(profile), Ok(provider), &workspace);
        let root = window.root(&mut cx).expect("root view");

        send(&mut cx, &root, "fix the parser");

        assert!(
            cx.debug_bounds("question").is_some(),
            "the question waits for an answer"
        );
        {
            let timeline = cx.debug_bounds("timeline").expect("the timeline");
            let card = cx.debug_bounds("question").expect("the card");
            assert!(
                card.origin.y >= timeline.origin.y + timeline.size.height,
                "the card is pinned below the scrolling timeline"
            );
        }
        assert!(cx.debug_bounds("answer-option-0").is_some());
        assert!(cx.debug_bounds("answer-option-1").is_some());
        assert!(
            cx.debug_bounds("answer-input").is_none(),
            "no free text was offered"
        );
        let (summary, awaiting) = root.read_with(&cx, |root, _| {
            let step = &root.turns[0].tool_steps[0];
            (step.summary.clone(), step.state == StepState::Awaiting)
        });
        assert_eq!(summary, "Which approach?", "the step names the question");
        assert!(awaiting, "the step waits for the answer");

        let second = cx
            .debug_bounds("answer-option-1")
            .expect("the second option");
        cx.simulate_click(second.center(), Modifiers::default());
        cx.run_until_parked();

        {
            let requests = requests.lock().expect("requests lock");
            let result = requests[1].messages.last().expect("the result");
            assert_eq!(result.content, "The user selected: Patch the caller");
        }
        assert!(cx.debug_bounds("question").is_none(), "the card is gone");
        assert_eq!(
            root.read_with(&cx, |root, _| root.turns[0].tool_steps[0].state.clone()),
            StepState::Done { failed: false }
        );

        let recorded = root.read_with(&cx, |root, _| {
            let store = root.store.clone()?;
            let session = root.session.as_ref()?;
            store.tool_calls(&session.id).ok()
        });
        let recorded = recorded.expect("the stored tool calls");
        assert_eq!(recorded.len(), 1, "the question is in the transcript");
        assert_eq!(recorded[0].name, "ask_user");
        assert!(
            recorded[0].result.contains("Patch the caller"),
            "{}",
            recorded[0].result
        );
    }

    #[gpui::test]
    fn a_typed_answer_wins_over_the_options(cx: &mut TestAppContext) {
        let (directory, workspace) = temp_workspace();
        let profile = Profile::at(directory.path().join("profile"));
        let answer = vec![StreamEvent::Text("Then we drop it.".to_owned()), finished()];
        let (provider, requests) = scripted(
            vec![
                ask_call(
                    "call_1",
                    "Which approach?",
                    &["Rewrite the parser", "Patch the caller"],
                    true,
                ),
                answer.clone(),
            ],
            answer,
        );
        let (window, mut cx) =
            open_window_in_workspace(cx, Some(profile), Ok(provider), &workspace);
        let root = window.root(&mut cx).expect("root view");

        send(&mut cx, &root, "fix the parser");

        assert!(cx.debug_bounds("answer-input").is_some());
        cx.simulate_input("Neither: delete it");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();

        let requests = requests.lock().expect("requests lock");
        let result = requests[1].messages.last().expect("the result");
        assert_eq!(result.content, "The user selected: Neither: delete it");
    }

    #[gpui::test]
    fn skipping_a_question_tells_the_model_nobody_answered(cx: &mut TestAppContext) {
        let (directory, workspace) = temp_workspace();
        let profile = Profile::at(directory.path().join("profile"));
        let answer = vec![StreamEvent::Text("I will decide.".to_owned()), finished()];
        let (provider, requests) = scripted(
            vec![
                ask_call(
                    "call_1",
                    "Which approach?",
                    &["Rewrite the parser", "Patch the caller"],
                    false,
                ),
                answer.clone(),
            ],
            answer,
        );
        let (window, mut cx) =
            open_window_in_workspace(cx, Some(profile), Ok(provider), &workspace);
        let root = window.root(&mut cx).expect("root view");

        send(&mut cx, &root, "fix the parser");
        let skip = cx.debug_bounds("answer-decline").expect("the skip button");
        cx.simulate_click(skip.center(), Modifiers::default());
        cx.run_until_parked();

        {
            let requests = requests.lock().expect("requests lock");
            let result = requests[1].messages.last().expect("the result");
            assert_eq!(result.content, humibird_tools::DECLINED_TEXT);
        }
        assert_eq!(
            root.read_with(&cx, |root, _| root.turns[0].tool_steps[0].state.clone()),
            StepState::Done { failed: true },
            "a skipped question is not a success"
        );
        assert_eq!(
            root.read_with(&cx, |root, _| root.turns[0].state.clone()),
            TurnState::Done,
            "the turn still finishes"
        );
    }

    #[gpui::test]
    fn a_delegated_task_comes_back_as_the_tool_result(cx: &mut TestAppContext) {
        let (directory, workspace) = temp_workspace();
        let profile = Profile::at(directory.path().join("profile"));
        let answer = vec![StreamEvent::Text("three files".to_owned()), finished()];
        let (provider, requests) = scripted(
            vec![
                vec![
                    StreamEvent::ToolCall(ToolCall {
                        id: "call_1".to_owned(),
                        name: "delegate".to_owned(),
                        arguments: serde_json::json!({
                            "description": "count the files",
                            "prompt": "Count the files in the workspace.",
                        })
                        .to_string(),
                    }),
                    tool_calls(),
                ],
                vec![
                    StreamEvent::Text("there are three files".to_owned()),
                    finished(),
                ],
                answer.clone(),
            ],
            answer,
        );
        let (window, mut cx) =
            open_window_in_workspace(cx, Some(profile), Ok(provider), &workspace);
        let root = window.root(&mut cx).expect("root view");

        send(&mut cx, &root, "how many files?");

        let (text, steps) = root.read_with(&cx, |root, _| {
            let turn = &root.turns[0];
            (
                turn.assistant_text.clone(),
                turn.tool_steps
                    .iter()
                    .map(|step| (step.name.clone(), step.state.clone()))
                    .collect::<Vec<_>>(),
            )
        });
        assert_eq!(text, "three files");
        assert_eq!(
            steps,
            vec![("delegate".to_owned(), StepState::Done { failed: false })],
            "the delegation is one step that finished"
        );

        let requests = requests.lock().expect("requests lock");
        // The parent's first request, the subagent's own, the parent's second, and the
        // title an unnamed thread asks for once.
        assert_eq!(
            requests.len(),
            4,
            "{}",
            requests
                .iter()
                .map(|request| request.messages.len())
                .collect::<Vec<_>>()
                .len()
        );
        assert_eq!(
            requests[1].messages.len(),
            2,
            "the subagent starts from the system prompt and the task alone"
        );
        assert_eq!(
            requests[1].messages[1].content,
            "Count the files in the workspace."
        );
        let subagent_tools: Vec<_> = requests[1]
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect();
        assert!(
            !subagent_tools
                .iter()
                .any(|name| matches!(*name, "delegate" | "ask_user" | "memory")),
            "a subagent cannot delegate, ask, or write memory: {subagent_tools:?}"
        );
        assert_eq!(
            requests[2].messages.last().expect("the result").content,
            "there are three files",
            "the report is what the parent's model reads"
        );
    }

    #[gpui::test]
    fn a_delegated_task_shows_what_the_subagent_did(cx: &mut TestAppContext) {
        let (directory, workspace) = temp_workspace();
        std::fs::write(workspace.join("notes.md"), "hello\n").expect("write the file");
        let profile = Profile::at(directory.path().join("profile"));
        let answer = vec![StreamEvent::Text("it says hello".to_owned()), finished()];
        let (provider, _requests) = scripted(
            vec![
                vec![
                    StreamEvent::ToolCall(ToolCall {
                        id: "call_1".to_owned(),
                        name: "delegate".to_owned(),
                        arguments: serde_json::json!({
                            "description": "read the note",
                            "prompt": "Read notes.md and report what it says.",
                        })
                        .to_string(),
                    }),
                    tool_calls(),
                ],
                vec![
                    StreamEvent::ToolCall(ToolCall {
                        id: "call_2".to_owned(),
                        name: "read_file".to_owned(),
                        arguments: serde_json::json!({ "path": "notes.md" }).to_string(),
                    }),
                    tool_calls(),
                ],
                vec![
                    StreamEvent::Text("notes.md says hello".to_owned()),
                    finished(),
                ],
                answer.clone(),
            ],
            answer,
        );
        let (window, mut cx) =
            open_window_in_workspace(cx, Some(profile), Ok(provider), &workspace);
        let root = window.root(&mut cx).expect("root view");

        send(&mut cx, &root, "what does the note say?");

        // The delegation is a card of its own, not a line among the calls.
        assert!(
            cx.debug_bounds("tool-card-0-0").is_some(),
            "a delegated task gets a card"
        );
        assert!(
            cx.debug_bounds("subagent-body-0-0").is_none(),
            "and stays closed until it is opened"
        );

        let row = cx.debug_bounds("tool-step-0-0").expect("the delegate row");
        cx.simulate_click(row.center(), Modifiers::default());
        cx.update(|_, _| {});

        assert!(
            cx.debug_bounds("subagent-body-0-0").is_some(),
            "opening it shows the subagent's work"
        );
        assert!(
            cx.debug_bounds("subagent-call-0-0-0").is_some(),
            "including the call it made"
        );
        assert!(
            cx.debug_bounds("subagent-report-0-0").is_some(),
            "and the report it came back with"
        );
        let subagent = root.read_with(&cx, |root, _| {
            (
                root.subagents[0].work.user.clone(),
                root.subagents[0].work.tool_steps.len(),
                root.subagents[0].work.assistant_text.clone(),
            )
        });
        assert_eq!(subagent.0, "Read notes.md and report what it says.");
        assert_eq!(subagent.1, 1, "one call of its own");
        assert_eq!(subagent.2, "notes.md says hello");
    }

    #[gpui::test]
    fn a_restarted_thread_shows_what_its_subagent_did(cx: &mut TestAppContext) {
        let (directory, workspace) = temp_workspace();
        std::fs::write(workspace.join("notes.md"), "hello\n").expect("write the file");
        let profile = Profile::at(directory.path().join("profile"));
        let answer = vec![StreamEvent::Text("it says hello".to_owned()), finished()];
        let (provider, _requests) = scripted(
            vec![
                vec![
                    StreamEvent::ToolCall(ToolCall {
                        id: "call_1".to_owned(),
                        name: "delegate".to_owned(),
                        arguments: serde_json::json!({
                            "description": "read the note",
                            "prompt": "Read notes.md and report what it says.",
                        })
                        .to_string(),
                    }),
                    tool_calls(),
                ],
                vec![
                    StreamEvent::ToolCall(ToolCall {
                        id: "call_2".to_owned(),
                        name: "read_file".to_owned(),
                        arguments: serde_json::json!({ "path": "notes.md" }).to_string(),
                    }),
                    tool_calls(),
                ],
                vec![
                    StreamEvent::Text("notes.md says hello".to_owned()),
                    finished(),
                ],
                answer.clone(),
            ],
            answer,
        );
        let (window, mut cx) =
            open_window_in_workspace(cx, Some(profile), Ok(provider), &workspace);
        let root = window.root(&mut cx).expect("root view");

        send(&mut cx, &root, "what does the note say?");
        let session = root.read_with(&cx, |root, _| root.session.clone().expect("a session"));
        assert_eq!(
            root.read_with(&cx, |root, _| root.subagents.len()),
            1,
            "the delegation is live"
        );

        // A restart: another thread, then the one that delegated.
        cx.simulate_keystrokes("cmd-n");
        assert_eq!(root.read_with(&cx, |root, _| root.subagents.len()), 0);
        root.update_in(&mut cx, |root, window, cx| {
            root.open_session(&session.id, window, cx)
        });

        let restored = root.read_with(&cx, |root, _| {
            let subagent = root.subagents.first().expect("the card came back");
            (
                subagent.turn,
                subagent.step,
                subagent.work.user.clone(),
                subagent
                    .work
                    .tool_steps
                    .iter()
                    .map(|step| (step.name.clone(), step.result.clone()))
                    .collect::<Vec<_>>(),
                subagent.work.assistant_text.clone(),
            )
        });
        assert_eq!(restored.0, 0, "it is the first turn's step");
        assert_eq!(restored.1, 0);
        assert_eq!(restored.2, "Read notes.md and report what it says.");
        assert_eq!(restored.3.len(), 1, "the call it made came back");
        assert_eq!(restored.3[0].0, "read_file");
        assert!(restored.3[0].1.contains("hello"), "{:?}", restored.3);
        assert_eq!(restored.4, "notes.md says hello");

        let row = cx.debug_bounds("tool-step-0-0").expect("the delegate row");
        cx.simulate_click(row.center(), Modifiers::default());
        cx.update(|_, _| {});
        assert!(
            cx.debug_bounds("subagent-call-0-0-0").is_some(),
            "and the card still opens to it"
        );
        assert!(cx.debug_bounds("subagent-report-0-0").is_some());
    }

    #[gpui::test]
    fn stopping_a_subagent_lets_the_parent_carry_on(cx: &mut TestAppContext) {
        let (directory, workspace) = temp_workspace();
        let profile = Profile::at(directory.path().join("profile"));
        let (provider, replies) = GatedProvider::new();
        let (window, mut cx) =
            open_window_in_workspace(cx, Some(profile), Ok(provider), &workspace);
        let root = window.root(&mut cx).expect("root view");

        send(&mut cx, &root, "go and look");
        reply(
            &replies,
            0,
            vec![
                StreamEvent::ToolCall(ToolCall {
                    id: "call_1".to_owned(),
                    name: "delegate".to_owned(),
                    arguments: serde_json::json!({
                        "description": "search everything",
                        "prompt": "Search the whole tree for the flag.",
                    })
                    .to_string(),
                }),
                tool_calls(),
            ],
        );
        cx.run_until_parked();
        cx.update(|_, _| {});

        // The subagent is parked on its own stream: nothing has come back yet.
        let row = cx.debug_bounds("tool-step-0-0").expect("the delegate row");
        cx.simulate_click(row.center(), Modifiers::default());
        cx.update(|_, _| {});
        let stop = cx
            .debug_bounds("stop-subagent-0-0")
            .expect("a running subagent offers a stop");
        cx.simulate_click(stop.center(), Modifiers::default());
        cx.run_until_parked();

        // The parent is told the call was cancelled, and its turn goes on.
        reply(
            &replies,
            2,
            vec![StreamEvent::Text("moved on".to_owned()), finished()],
        );
        cx.run_until_parked();

        let (text, step) = root.read_with(&cx, |root, _| {
            (
                root.turns[0].assistant_text.clone(),
                root.turns[0].tool_steps[0].clone(),
            )
        });
        assert_eq!(text, "moved on", "the parent's turn continued");
        assert!(
            matches!(step.state, StepState::Done { failed: true }),
            "{:?}",
            step.state
        );
        assert_eq!(step.result, humibird_tools::DELEGATION_CANCELLED);
        assert!(
            cx.debug_bounds("stop-subagent-0-0").is_none(),
            "a stopped subagent no longer offers a stop"
        );
    }

    #[gpui::test]
    fn a_subagent_asks_the_parent_window_before_it_writes(cx: &mut TestAppContext) {
        let (directory, workspace) = temp_workspace();
        let profile = Profile::at(directory.path().join("profile"));
        let answer = vec![StreamEvent::Text("wrote it".to_owned()), finished()];
        let (provider, requests) = scripted(
            vec![
                vec![
                    StreamEvent::ToolCall(ToolCall {
                        id: "call_1".to_owned(),
                        name: "delegate".to_owned(),
                        arguments: serde_json::json!({
                            "description": "write the note",
                            "prompt": "Write hello into a.md.",
                        })
                        .to_string(),
                    }),
                    tool_calls(),
                ],
                vec![
                    StreamEvent::ToolCall(ToolCall {
                        id: "call_2".to_owned(),
                        name: "write_file".to_owned(),
                        arguments: serde_json::json!({
                            "path": "a.md",
                            "content": "hello\n",
                        })
                        .to_string(),
                    }),
                    tool_calls(),
                ],
                vec![StreamEvent::Text("a.md says hello".to_owned()), finished()],
                answer.clone(),
            ],
            answer,
        );
        let (window, mut cx) =
            open_window_in_workspace(cx, Some(profile), Ok(provider), &workspace);
        let root = window.root(&mut cx).expect("root view");

        send(&mut cx, &root, "write the note");
        // The subagent's call waits on the window the person is looking at.
        let approve = cx
            .debug_bounds("approve-once")
            .expect("the subagent's write asks the parent window");
        cx.simulate_click(approve.center(), Modifiers::default());
        cx.run_until_parked();
        cx.update(|_, _| {});

        assert_eq!(
            std::fs::read_to_string(workspace.join("a.md")).expect("a.md exists"),
            "hello\n",
            "the subagent wrote it after the person said yes"
        );
        let text = root.read_with(&cx, |root, _| root.turns[0].assistant_text.clone());
        assert_eq!(text, "wrote it", "the parent's answer follows the report");
        let requests = requests.lock().expect("requests lock");
        let report = &requests[3].messages.last().expect("the result").content;
        assert_eq!(report, "a.md says hello");
    }

    #[gpui::test]
    fn a_question_without_a_choice_or_free_text_never_shows_a_card(cx: &mut TestAppContext) {
        let (directory, workspace) = temp_workspace();
        let profile = Profile::at(directory.path().join("profile"));
        let answer = vec![StreamEvent::Text("Fine.".to_owned()), finished()];
        let (provider, requests) = scripted(
            vec![
                ask_call("call_1", "Go on?", &["Yes"], false),
                answer.clone(),
            ],
            answer,
        );
        let (window, mut cx) =
            open_window_in_workspace(cx, Some(profile), Ok(provider), &workspace);
        let root = window.root(&mut cx).expect("root view");

        send(&mut cx, &root, "fix the parser");

        assert!(
            cx.debug_bounds("question").is_none(),
            "a question nobody can answer is not shown"
        );
        let requests = requests.lock().expect("requests lock");
        let result = requests[1].messages.last().expect("the result");
        assert!(
            result.content.contains("at least two `options`"),
            "{}",
            result.content
        );
        assert_eq!(
            root.read_with(&cx, |root, _| root.turns[0].tool_steps[0].state.clone()),
            StepState::Done { failed: true }
        );
    }

    #[gpui::test]
    fn stopping_during_a_question_takes_the_card_away(cx: &mut TestAppContext) {
        let (directory, workspace) = temp_workspace();
        let profile = Profile::at(directory.path().join("profile"));
        let answer = vec![StreamEvent::Text("unused".to_owned()), finished()];
        let (provider, _requests) = scripted(
            vec![ask_call(
                "call_1",
                "Which approach?",
                &["Rewrite the parser", "Patch the caller"],
                true,
            )],
            answer,
        );
        let (window, mut cx) =
            open_window_in_workspace(cx, Some(profile), Ok(provider), &workspace);
        let root = window.root(&mut cx).expect("root view");

        send(&mut cx, &root, "fix the parser");
        assert!(cx.debug_bounds("question").is_some());

        let stop = cx.debug_bounds("stop").expect("the stop button");
        cx.simulate_click(stop.center(), Modifiers::default());
        cx.run_until_parked();

        assert!(cx.debug_bounds("question").is_none(), "the card is gone");
        assert_eq!(
            root.read_with(&cx, |root, _| root.turns[0].state.clone()),
            TurnState::Stopped
        );
    }

    #[gpui::test]
    fn new_output_follows_the_reader_only_at_the_bottom(cx: &mut TestAppContext) {
        let (directory, workspace) = temp_workspace();
        let profile = Profile::at(directory.path().join("profile"));
        let long_answer = (0..120)
            .map(|index| format!("line {index}\n\n"))
            .collect::<String>();
        let long_thought = (0..120)
            .map(|index| format!("thought {index}\n"))
            .collect::<String>();
        let (provider, replies) = GatedProvider::new();
        let (window, mut cx) =
            open_window_in_workspace(cx, Some(profile), Ok(provider), &workspace);
        let root = window.root(&mut cx).expect("root view");

        let scroll = |cx: &mut VisualTestContext, delta: f32| {
            let timeline = cx.debug_bounds("timeline").expect("the timeline");
            cx.simulate_mouse_move(timeline.center(), None, Modifiers::default());
            cx.simulate_event(ScrollEvent {
                position: timeline.center(),
                delta: ScrollDelta::Pixels(point(px(0.), px(delta))),
                ..Default::default()
            });
            cx.update(|_, _| {});
        };
        // How far the reader sits from the end of the conversation: zero at the
        // bottom, growing as they read back.
        let from_the_end = |cx: &mut VisualTestContext| {
            root.read_with(cx, |root, _| {
                root.scroll_handle.offset().y + root.scroll_handle.max_offset().y
            })
        };

        send(&mut cx, &root, "first");
        reply(
            &replies,
            0,
            vec![StreamEvent::Text(long_answer), finished()],
        );
        cx.run_until_parked();
        assert!(
            root.read_with(&cx, |root, _| root.scroll_handle.max_offset().y) > px(1_000.),
            "the answer is long enough to scroll"
        );
        assert!(
            from_the_end(&mut cx) <= px(8.),
            "an answer is followed while the reader is at the end, {:?} from it",
            from_the_end(&mut cx)
        );

        // The second turn's thinking arrives in two stretches, which is the moment
        // a reader wants to stop and read what the model said so far.
        send(&mut cx, &root, "second");
        reply(
            &replies,
            1,
            vec![StreamEvent::Reasoning(long_thought.clone())],
        );
        cx.run_until_parked();
        assert!(
            from_the_end(&mut cx) <= px(8.),
            "thinking is followed while the reader is at the end, {:?} from it",
            from_the_end(&mut cx)
        );

        // How far the streamed preview sits from its own newest line. It is
        // clipped, not scrolled by hand, so it keeps following on its own.
        let preview_end = |cx: &mut VisualTestContext| {
            root.read_with(cx, |root, _| {
                let scroll = &root.turns[1].thinking[0].scroll;
                scroll.offset().y + scroll.max_offset().y
            })
        };

        scroll(&mut cx, 600.);
        assert!(
            from_the_end(&mut cx) > px(100.),
            "the wheel moved the reader up, {:?} from the end",
            from_the_end(&mut cx)
        );
        let preview = cx
            .debug_bounds("thinking-body-1-0")
            .expect("the streamed preview");
        assert!(
            preview.size.height <= px(256.),
            "a streaming stretch is previewed, not shown whole: {:?}",
            preview.size.height
        );
        assert!(
            root.read_with(&cx, |root, _| root.turns[1].thinking[0]
                .scroll
                .max_offset()
                .y)
                > px(100.),
            "the preview is long enough to have been cut"
        );
        assert!(
            preview_end(&mut cx) <= px(8.),
            "the preview keeps its newest line when the wheel moves the conversation, {:?} from it",
            preview_end(&mut cx)
        );
        reply(&replies, 1, vec![StreamEvent::Reasoning(long_thought)]);
        cx.run_until_parked();
        assert!(
            from_the_end(&mut cx) > px(100.),
            "more thinking left the reader where they were, {:?} from the end",
            from_the_end(&mut cx)
        );
        assert!(
            preview_end(&mut cx) <= px(8.),
            "the preview still follows while the reader reads back, {:?} from its end",
            preview_end(&mut cx)
        );

        // Back at the end, the rest of the answer is followed again.
        scroll(&mut cx, -4_000.);
        assert!(
            from_the_end(&mut cx) <= px(8.),
            "the reader is back at the end, {:?} from it",
            from_the_end(&mut cx)
        );
        reply(
            &replies,
            1,
            vec![StreamEvent::Text("done".to_owned()), finished()],
        );
        cx.run_until_parked();
        assert!(
            from_the_end(&mut cx) <= px(8.),
            "output follows once the reader is back at the end, {:?} from it",
            from_the_end(&mut cx)
        );
    }

    #[gpui::test]
    fn the_model_thinking_is_shown_then_folded_away(cx: &mut TestAppContext) {
        let (directory, workspace) = temp_workspace();
        let profile = Profile::at(directory.path().join("profile"));
        let (provider, requests) = scripted(
            Vec::new(),
            vec![
                StreamEvent::Reasoning("The file says ".to_owned()),
                StreamEvent::Reasoning("nothing.".to_owned()),
                StreamEvent::Text("Done.".to_owned()),
                finished(),
            ],
        );
        let (window, mut cx) =
            open_window_in_workspace(cx, Some(profile), Ok(provider), &workspace);
        let root = window.root(&mut cx).expect("root view");

        send(&mut cx, &root, "look first");

        assert!(
            cx.debug_bounds("thinking-0-0").is_some(),
            "the thinking has a row"
        );
        assert!(
            cx.debug_bounds("thinking-body-0-0").is_none(),
            "and is folded away once the answer arrives"
        );

        let row = cx.debug_bounds("thinking-0-0").expect("the row");
        cx.simulate_click(row.center(), Modifiers::default());
        cx.update(|_, _| {});
        assert!(
            cx.debug_bounds("thinking-body-0-0").is_some(),
            "one click shows what the model thought"
        );

        // Thinking is context for the user, not conversation: it never goes back
        // to the model.
        send(&mut cx, &root, "and now?");
        let requests = requests.lock().expect("requests lock");
        assert!(
            requests
                .iter()
                .flat_map(|request| &request.messages)
                .all(|message| !message.content.contains("The file says")),
            "reasoning is not part of the request history"
        );
    }

    #[gpui::test]
    fn a_call_that_changes_something_is_a_card(cx: &mut TestAppContext) {
        let (directory, workspace) = temp_workspace();
        std::fs::write(workspace.join("notes.md"), "hello\n").expect("write the file");
        let profile = Profile::at(directory.path().join("profile"));
        let answer = vec![StreamEvent::Text("done".to_owned()), finished()];
        let (provider, _requests) = scripted(
            vec![
                vec![
                    StreamEvent::ToolCall(ToolCall {
                        id: "read_1".to_owned(),
                        name: "read_file".to_owned(),
                        arguments: "{\"path\":\"notes.md\"}".to_owned(),
                    }),
                    tool_calls(),
                ],
                answer.clone(),
                // The first reply names the thread; that request comes out of the
                // same script.
                vec![StreamEvent::Text("Reading a file".to_owned()), finished()],
                vec![
                    StreamEvent::ToolCall(ToolCall {
                        id: "write_1".to_owned(),
                        name: "write_file".to_owned(),
                        arguments: serde_json::json!({
                            "path": "server.py",
                            "content": "print(1)\n",
                        })
                        .to_string(),
                    }),
                    tool_calls(),
                ],
                answer.clone(),
            ],
            answer,
        );
        let (window, mut cx) =
            open_window_in_workspace(cx, Some(profile), Ok(provider), &workspace);
        let root = window.root(&mut cx).expect("root view");

        send(&mut cx, &root, "read it");
        assert!(
            cx.debug_bounds("tool-card-0-0").is_none(),
            "reading a file stays a line"
        );

        send(&mut cx, &root, "write it");
        let approve = cx
            .debug_bounds("approve-once")
            .expect("the write waits for approval");
        cx.simulate_click(approve.center(), Modifiers::default());
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("tool-card-1-0").is_some(),
            "writing a file is a card"
        );
    }

    #[gpui::test]
    fn thinking_renders_where_it_happened_and_opens_whole(cx: &mut TestAppContext) {
        let (directory, workspace) = temp_workspace();
        std::fs::write(workspace.join("notes.md"), "hello\n").expect("write the file");
        let profile = Profile::at(directory.path().join("profile"));
        let long_thought = (0..80)
            .map(|index| format!("line {index} of a long thought\n"))
            .collect::<String>();
        let answer = vec![StreamEvent::Text("Done.".to_owned()), finished()];
        let (provider, _requests) = scripted(
            vec![
                vec![
                    StreamEvent::Reasoning("first thought".to_owned()),
                    StreamEvent::ToolCall(ToolCall {
                        id: "call_1".to_owned(),
                        name: "read_file".to_owned(),
                        arguments: "{\"path\":\"notes.md\"}".to_owned(),
                    }),
                    tool_calls(),
                ],
                vec![
                    StreamEvent::Reasoning(long_thought),
                    StreamEvent::Text("Done.".to_owned()),
                    finished(),
                ],
            ],
            answer,
        );
        let (window, mut cx) =
            open_window_in_workspace(cx, Some(profile), Ok(provider), &workspace);
        let root = window.root(&mut cx).expect("root view");

        send(&mut cx, &root, "read it");

        let first = cx.debug_bounds("thinking-0-0").expect("first stretch");
        let step = cx.debug_bounds("tool-step-0-0").expect("the call");
        let second = cx.debug_bounds("thinking-0-1").expect("second stretch");
        assert!(
            first.origin.y < step.origin.y && step.origin.y < second.origin.y,
            "thinking stays where it happened: {first:?} {step:?} {second:?}"
        );

        // The long stretch is folded away at the end of the turn; opening it shows
        // the whole thought, since a finished stretch is only previewed while it
        // streams.
        assert!(cx.debug_bounds("thinking-body-0-1").is_none());
        let second = cx.debug_bounds("thinking-0-1").expect("second stretch");
        cx.simulate_click(second.center(), Modifiers::default());
        cx.update(|_, _| {});
        let body = cx
            .debug_bounds("thinking-body-0-1")
            .expect("the opened stretch");
        assert!(
            body.size.height > px(256.),
            "an opened stretch is not clipped: {:?}",
            body.size.height
        );
    }

    #[gpui::test]
    fn the_approval_card_is_pinned_below_the_timeline(cx: &mut TestAppContext) {
        let (directory, workspace) = temp_workspace();
        let profile = Profile::at(directory.path().join("profile"));
        // A long thought above the call: before the card was pinned, this pushed
        // the prompt out of the viewport.
        let long_thought = (0..200)
            .map(|index| format!("thought line {index}\n"))
            .collect::<String>();
        let (provider, _requests) = scripted(
            vec![vec![
                StreamEvent::Reasoning(long_thought),
                StreamEvent::ToolCall(ToolCall {
                    id: "call_1".to_owned(),
                    name: "write_file".to_owned(),
                    arguments: serde_json::json!({
                        "path": "server.py",
                        "content": "print(1)\n",
                    })
                    .to_string(),
                }),
                tool_calls(),
            ]],
            vec![finished()],
        );
        let (window, mut cx) =
            open_window_in_workspace(cx, Some(profile), Ok(provider), &workspace);
        let root = window.root(&mut cx).expect("root view");

        send(&mut cx, &root, "build a demo");

        let timeline = cx.debug_bounds("timeline").expect("the timeline");
        let approval = cx.debug_bounds("approval").expect("the prompt");
        assert!(
            approval.origin.y >= timeline.origin.y + timeline.size.height,
            "the prompt is pinned below the scrolling timeline: {approval:?} vs {timeline:?}"
        );
        assert!(
            approval.origin.y + approval.size.height <= px(820.),
            "and it stays inside the window: {approval:?}"
        );
    }

    #[gpui::test]
    fn choosing_a_workspace_starts_a_thread_there(cx: &mut TestAppContext) {
        let (directory, _pinned) = temp_workspace();
        let project = directory.path().join("other-project");
        std::fs::create_dir_all(&project).expect("create the project");
        let profile = Profile::at(directory.path().join("profile"));

        let fallback = vec![StreamEvent::Text("ok".to_owned()), finished()];
        let (provider, _requests) = scripted(Vec::new(), fallback);
        let (window, mut cx) = open_window_with(cx, Some(profile), Ok(provider));
        let root = window.root(&mut cx).expect("root view");

        let chip = cx
            .debug_bounds(selector("header-workspace-workspace".to_owned()))
            .expect("the workspace chip");
        cx.simulate_click(chip.center(), Modifiers::default());
        assert!(
            cx.did_prompt_for_paths(),
            "the platform is asked for a directory"
        );

        let chosen = project.clone();
        cx.simulate_path_prompt_response(move |_| Some(vec![chosen.clone()]));
        cx.run_until_parked();

        assert_eq!(
            root.read_with(&cx, |root, _| root.workspace.clone()),
            project.to_string_lossy(),
            "the window works in the chosen directory"
        );
        assert!(
            cx.debug_bounds(selector("header-workspace-other-project".to_owned()))
                .is_some(),
            "the chip names the new workspace"
        );
        assert_eq!(
            root.read_with(&cx, |root, _| root.turns.len()),
            0,
            "a different workspace starts a new thread"
        );
        assert!(
            !root.read_with(&cx, |root, _| root.conversation.tools().is_empty()),
            "the tools are rooted in the new workspace"
        );
        assert!(
            cx.debug_bounds("workspace-notice").is_none(),
            "nothing to complain about"
        );
    }

    #[gpui::test]
    fn a_refused_workspace_is_reported_and_not_switched(cx: &mut TestAppContext) {
        let (directory, _pinned) = temp_workspace();
        let profile = Profile::at(directory.path().join("profile"));
        let fallback = vec![StreamEvent::Text("ok".to_owned()), finished()];
        let (provider, _requests) = scripted(Vec::new(), fallback);
        let (window, mut cx) = open_window_with(cx, Some(profile), Ok(provider));
        let root = window.root(&mut cx).expect("root view");
        let before = root.read_with(&cx, |root, _| root.workspace.clone());

        let chip = cx
            .debug_bounds(selector("header-workspace-workspace".to_owned()))
            .expect("the workspace chip");
        cx.simulate_click(chip.center(), Modifiers::default());
        cx.simulate_path_prompt_response(|_| Some(vec![PathBuf::from("/")]));
        cx.run_until_parked();

        assert_eq!(
            root.read_with(&cx, |root, _| root.workspace.clone()),
            before,
            "a refused directory leaves the workspace alone"
        );
        assert!(
            cx.debug_bounds("workspace-notice").is_some(),
            "and the refusal is on screen"
        );
    }

    #[gpui::test]
    fn a_workspace_the_profile_refuses_disables_the_file_tools(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        // A session stored before a workspace was validated, pointing at the
        // filesystem root.
        let store = Store::open(&profile.database_path()).expect("store");
        store
            .create_session("/", Some("deepseek"), Some("deepseek-flash"))
            .expect("create the session");
        drop(store);

        let answer = vec![StreamEvent::Text("I cannot.".to_owned()), finished()];
        let call = vec![
            StreamEvent::ToolCall(ToolCall {
                id: "call_1".to_owned(),
                name: "read_file".to_owned(),
                arguments: "{\"path\":\"etc/passwd\"}".to_owned(),
            }),
            tool_calls(),
        ];
        let (provider, requests) = scripted(vec![call, answer.clone()], answer);
        let (window, mut cx) = open_window_with(cx, Some(profile), Ok(provider));
        let root = window.root(&mut cx).expect("root view");

        send(&mut cx, &root, "read the password file");

        let (name, failed) = root.read_with(&cx, |root, _| {
            let step = &root.turns[0].tool_steps[0];
            (
                step.name.clone(),
                matches!(step.state, StepState::Done { failed: true }),
            )
        });
        assert_eq!(name, "read_file");
        assert!(failed, "the call cannot run without file tools");

        let requests = requests.lock().expect("requests lock");
        let names: Vec<_> = requests[0]
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect();
        assert_eq!(
            names,
            ["fetch", "memory", "ask_user", "delegate", "session_search"],
            "only the tools that do not touch the workspace survive"
        );
    }

    #[gpui::test]
    fn a_reopened_session_shows_the_tool_steps_that_ran(cx: &mut TestAppContext) {
        let (directory, workspace) = temp_workspace();
        std::fs::write(workspace.join("notes.md"), "hello\n").expect("write the file");
        let profile = Profile::at(directory.path().join("profile"));
        let answer = vec![StreamEvent::Text("done".to_owned()), finished()];
        let calls = vec![
            StreamEvent::ToolCall(ToolCall {
                id: "call_1".to_owned(),
                name: "read_file".to_owned(),
                arguments: "{\"path\":\"notes.md\"}".to_owned(),
            }),
            StreamEvent::ToolCall(ToolCall {
                id: "call_2".to_owned(),
                name: "read_file".to_owned(),
                arguments: "{\"path\":\"missing.md\"}".to_owned(),
            }),
            tool_calls(),
        ];
        let (provider, requests) = scripted(
            vec![calls, answer.clone(), vec![text("again"), finished()]],
            answer.clone(),
        );
        let (window, mut cx) =
            open_window_in_workspace(cx, Some(profile), Ok(provider), &workspace);
        let root = window.root(&mut cx).expect("root view");

        send(&mut cx, &root, "read both");

        let recorded = root.read_with(&cx, |root, _| {
            root.turns[0]
                .tool_steps
                .iter()
                .map(|step| {
                    (
                        step.name.clone(),
                        step.summary.clone(),
                        matches!(step.state, StepState::Done { failed: true }),
                    )
                })
                .collect::<Vec<_>>()
        });
        assert_eq!(
            recorded,
            vec![
                ("read_file".to_owned(), "notes.md".to_owned(), false),
                ("read_file".to_owned(), "missing.md".to_owned(), true),
            ]
        );

        let session_id = root.read_with(&cx, |root, _| {
            root.session.as_ref().expect("a session").id.clone()
        });
        cx.simulate_keystrokes("cmd-n");
        assert_eq!(root.read_with(&cx, |root, _| root.turns.len()), 0);

        root.update_in(&mut cx, |root, window, cx| {
            root.open_session(&session_id, window, cx)
        });

        let restored = root.read_with(&cx, |root, _| {
            root.turns[0]
                .tool_steps
                .iter()
                .map(|step| {
                    (
                        step.name.clone(),
                        step.summary.clone(),
                        matches!(step.state, StepState::Done { failed: true }),
                    )
                })
                .collect::<Vec<_>>()
        });
        assert_eq!(restored, recorded, "the steps survive a reopen");

        // What a call returned is in the store too, so a reopened step opens.
        let result = root.read_with(&cx, |root, _| root.turns[0].tool_steps[0].result.clone());
        assert!(result.contains("hello"), "{result}");
        let row = cx
            .debug_bounds("tool-step-0-0")
            .expect("the first step is laid out");
        cx.simulate_click(row.center(), Modifiers::default());
        cx.update(|_, _| {});
        assert!(
            cx.debug_bounds("step-detail").is_some(),
            "opening a restored step shows the call"
        );

        // And the model sees the conversation it had, not just the new message.
        send(&mut cx, &root, "and again");
        let requests = requests.lock().expect("requests lock");
        let sent = requests.last().expect("the request").messages.clone();
        assert_eq!(
            sent.iter()
                .map(|message| (message.role, message.content.as_str()))
                .collect::<Vec<_>>()[1..],
            [
                (humibird_providers::Role::User, "read both"),
                (humibird_providers::Role::Assistant, "done"),
                (humibird_providers::Role::User, "and again"),
            ],
            "a reopened thread sends its history"
        );
    }

    #[gpui::test]
    fn a_tool_step_opens_and_closes(cx: &mut TestAppContext) {
        let (directory, workspace) = temp_workspace();
        std::fs::write(workspace.join("notes.md"), "hello\n").expect("write the file");
        let profile = Profile::at(directory.path().join("profile"));
        let answer = vec![StreamEvent::Text("done".to_owned()), finished()];
        let (provider, _requests) = scripted(
            vec![
                vec![
                    StreamEvent::ToolCall(ToolCall {
                        id: "call_1".to_owned(),
                        name: "read_file".to_owned(),
                        arguments: "{\"path\":\"notes.md\"}".to_owned(),
                    }),
                    tool_calls(),
                ],
                answer.clone(),
            ],
            answer,
        );
        let (window, mut cx) =
            open_window_in_workspace(cx, Some(profile), Ok(provider), &workspace);
        let root = window.root(&mut cx).expect("root view");

        send(&mut cx, &root, "read it");

        assert!(
            cx.debug_bounds("step-detail").is_none(),
            "a step starts closed"
        );

        let row = cx.debug_bounds("tool-step-0-0").expect("the step row");
        cx.simulate_click(row.center(), Modifiers::default());
        cx.update(|_, _| {});
        assert!(
            cx.debug_bounds("step-detail").is_some(),
            "clicking the triangle opens the call"
        );

        let row = cx.debug_bounds("tool-step-0-0").expect("the step row");
        cx.simulate_click(row.center(), Modifiers::default());
        cx.update(|_, _| {});
        assert!(
            cx.debug_bounds("step-detail").is_none(),
            "and clicking it again closes it"
        );
    }

    #[gpui::test]
    fn the_model_can_read_a_file_before_answering(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        let workspace = profile.workspace_dir().expect("default workspace");
        std::fs::write(workspace.join("Cargo.toml"), "[package]\nname = \"demo\"\n")
            .expect("write the manifest");
        let answer = vec![
            StreamEvent::Text("It is the crate manifest.".to_owned()),
            finished(),
        ];
        let call = vec![
            StreamEvent::ToolCall(ToolCall {
                id: "call_1".to_owned(),
                name: "read_file".to_owned(),
                arguments: "{\"path\":\"Cargo.toml\"}".to_owned(),
            }),
            tool_calls(),
        ];
        let (provider, requests) = scripted(vec![call, answer.clone()], answer);
        let (window, mut cx) = open_window_with(cx, Some(profile), Ok(provider));
        let root = window.root(&mut cx).expect("root view");

        assert_eq!(
            root.read_with(&cx, |root, _| root.workspace.clone()),
            workspace.to_string_lossy(),
            "a new thread starts in the profile's own workspace"
        );

        send(&mut cx, &root, "what package is this?");

        let (text, steps) = root.read_with(&cx, |root, _| {
            let turn = &root.turns[0];
            (
                turn.assistant_text.clone(),
                turn.tool_steps
                    .iter()
                    .map(|step| {
                        (
                            step.name.clone(),
                            step.summary.clone(),
                            matches!(step.state, StepState::Done { failed: true }),
                        )
                    })
                    .collect::<Vec<_>>(),
            )
        });
        assert_eq!(text, "It is the crate manifest.");
        assert_eq!(
            steps,
            vec![("read_file".to_owned(), "Cargo.toml".to_owned(), false)]
        );

        let requests = requests.lock().expect("requests lock");
        let names: Vec<_> = requests[0]
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect();
        assert_eq!(names.len(), 14, "the tools are offered: {names:?}");
        assert!(names.contains(&"session_search"), "{names:?}");
        assert!(names.contains(&"memory"), "{names:?}");
        assert!(names.contains(&"ask_user"), "{names:?}");
        assert!(names.contains(&"delegate"), "{names:?}");
        let second = &requests[1].messages;
        assert_eq!(
            second.len(),
            4,
            "the prompt, the question, the call, and its result"
        );
        assert_eq!(second[2].tool_calls[0].name, "read_file");
        assert_eq!(second[3].tool_call_id.as_deref(), Some("call_1"));
        assert!(
            second[3].content.contains("[package]"),
            "the model should see the file: {}",
            second[3].content
        );
    }

    #[gpui::test]
    fn an_unknown_tool_is_reported_back_to_the_model(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        let answer = vec![
            StreamEvent::Text("I cannot do that.".to_owned()),
            finished(),
        ];
        let call = vec![
            StreamEvent::ToolCall(ToolCall {
                id: "call_9".to_owned(),
                name: "delete_everything".to_owned(),
                arguments: "{}".to_owned(),
            }),
            tool_calls(),
        ];
        let (provider, requests) = scripted(vec![call, answer.clone()], answer);
        let (window, mut cx) = open_window_with(cx, Some(profile), Ok(provider));
        let root = window.root(&mut cx).expect("root view");

        send(&mut cx, &root, "delete it all");

        let (name, failed) = root.read_with(&cx, |root, _| {
            let step = &root.turns[0].tool_steps[0];
            (
                step.name.clone(),
                matches!(step.state, StepState::Done { failed: true }),
            )
        });
        assert_eq!(name, "delete_everything");
        assert!(failed, "an unknown tool marks the step as failed");

        let requests = requests.lock().expect("requests lock");
        let result = &requests[1].messages[3];
        assert!(
            result.content.starts_with("error:") && result.content.contains("delete_everything"),
            "{}",
            result.content
        );
    }

    #[gpui::test]
    fn a_model_that_keeps_calling_tools_is_stopped(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        let forever = vec![
            StreamEvent::ToolCall(ToolCall {
                id: "call_1".to_owned(),
                name: "list_directory".to_owned(),
                arguments: "{}".to_owned(),
            }),
            tool_calls(),
        ];
        let (provider, _requests) = scripted(Vec::new(), forever);
        let (window, mut cx) = open_window_with(cx, Some(profile), Ok(provider));
        let root = window.root(&mut cx).expect("root view");

        send(&mut cx, &root, "loop forever");

        let state = root.read_with(&cx, |root, _| root.turns[0].state.clone());
        assert!(
            matches!(&state, TurnState::Failed(message) if message.contains("kept asking")),
            "{state:?}"
        );
    }

    #[gpui::test]
    fn a_finished_reply_is_persisted_and_restored_on_launch(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        let provider = Arc::new(FakeProvider {
            requests: Arc::new(std::sync::Mutex::new(Vec::new())),
            events: vec![
                StreamEvent::Text("remembered".to_owned()),
                StreamEvent::Finished {
                    reason: StopReason::Stop,
                },
            ],
            title: None,
        });
        let (window, mut cx) = open_window_with(cx, Some(profile.clone()), Ok(provider));
        let root = window.root(&mut cx).expect("root view");
        send(&mut cx, &root, "remember me");

        let store = Store::open(&profile.database_path()).expect("reopen the store");
        let sessions = store.sessions().expect("sessions");
        assert_eq!(sessions.len(), 1);
        let messages = store.messages(&sessions[0].id).expect("messages");
        assert_eq!(messages.len(), 2, "the question and the complete reply");
        assert_eq!(messages[0].role, Role::User);
        assert_eq!(messages[1].role, Role::Assistant);
        assert_eq!(messages[1].content, "remembered");

        // Opening the app again shows the newest session's transcript.
        let restored =
            cx.new(|cx| HumibirdRoot::new_with(cx, Some(profile), Err("offline".into())));
        let turns = restored.read_with(&cx, |root, _| {
            root.turns
                .iter()
                .map(|turn| (turn.user.clone(), turn.assistant_text.clone()))
                .collect::<Vec<_>>()
        });
        assert_eq!(
            turns,
            vec![("remember me".to_owned(), "remembered".to_owned())]
        );
    }

    #[gpui::test]
    fn searching_threads_matches_titles_and_workspace_names(cx: &mut TestAppContext) {
        let (directory, workspace) = temp_workspace();
        let profile = Profile::at(directory.path().join("profile"));
        let other_workspace = directory.path().join("search-project");
        std::fs::create_dir_all(&other_workspace).expect("other workspace");
        let store = Store::open(&profile.database_path()).expect("store");
        let other = store
            .create_session(
                &other_workspace.to_string_lossy(),
                Some("deepseek"),
                Some(DEFAULT_MODEL),
            )
            .expect("other thread");
        store
            .rename_session(&other.id, "会话搜索 GPUI")
            .expect("title");
        store
            .append_message(&other.id, Role::User, "body-only-needle")
            .expect("message");
        let current = store
            .create_session(
                &workspace.to_string_lossy(),
                Some("deepseek"),
                Some(DEFAULT_MODEL),
            )
            .expect("current thread");
        store
            .rename_session(&current.id, "Unrelated title")
            .expect("title");
        let (provider, _) = scripted(Vec::new(), vec![finished()]);
        let (window, mut cx) =
            open_window_in_workspace(cx, Some(profile), Ok(provider), &workspace);
        let root = window.root(&mut cx).expect("root");
        cx.update(|_, _| {});
        let search_bounds = cx.debug_bounds("session-search").expect("search field");
        cx.simulate_click(search_bounds.center(), Modifiers::default());
        cx.simulate_input("搜索 gpui");
        cx.update(|_, _| {});
        assert!(
            cx.debug_bounds(selector(format!("thread-{}", other.id)))
                .is_some()
        );
        assert!(
            cx.debug_bounds(selector(format!("thread-{}", current.id)))
                .is_none()
        );
        // Filtering never switches the active conversation.
        assert_eq!(
            root.read_with(&cx, |root, _| root.session.as_ref().unwrap().id.clone()),
            current.id
        );
        cx.simulate_keystrokes("cmd-a");
        cx.simulate_input("SEARCH-PROJECT");
        cx.update(|_, _| {});
        let result = cx
            .debug_bounds(selector(format!("thread-{}", other.id)))
            .expect("workspace match");
        cx.simulate_click(result.center(), Modifiers::default());
        assert_eq!(
            root.read_with(&cx, |root, _| root.session.as_ref().unwrap().id.clone()),
            other.id
        );
        // A new search in the same field does not match stored message bodies.
        let search_bounds = cx.debug_bounds("session-search").unwrap();
        cx.simulate_click(search_bounds.center(), Modifiers::default());
        cx.simulate_keystrokes("cmd-a");
        cx.simulate_input("body-only-needle");
        cx.update(|_, _| {});
        assert!(
            cx.debug_bounds(selector(format!("thread-{}", other.id)))
                .is_none()
        );
        assert!(cx.debug_bounds("threads-empty").is_some());
        let clear = cx
            .debug_bounds("clear-session-search")
            .expect("clear button");
        cx.simulate_click(clear.center(), Modifiers::default());
        cx.update(|_, _| {});
        assert!(
            cx.debug_bounds(selector(format!("thread-{}", other.id)))
                .is_some()
        );
        assert!(
            cx.debug_bounds(selector(format!("thread-{}", current.id)))
                .is_some()
        );
        cx.simulate_click(search_bounds.center(), Modifiers::default());
        cx.simulate_input("no-match");
        cx.simulate_keystrokes("escape");
        cx.update(|_, _| {});
        assert_eq!(
            root.read_with(&cx, |root, cx| root
                .session_search
                .read(cx)
                .value()
                .to_owned()),
            ""
        );
    }

    #[gpui::test]
    fn clicking_a_session_in_the_sidebar_opens_it(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        let provider = Arc::new(FakeProvider {
            requests: Arc::new(std::sync::Mutex::new(Vec::new())),
            events: vec![StreamEvent::Finished {
                reason: StopReason::Stop,
            }],
            title: None,
        });
        let (window, mut cx) = open_window_with(cx, Some(profile), Ok(provider));
        let root = window.root(&mut cx).expect("root view");

        send(&mut cx, &root, "first question");
        let first = root
            .read_with(&cx, |root, _| root.session.clone())
            .expect("session");
        cx.simulate_keystrokes("cmd-n");
        send(&mut cx, &root, "second question");
        let second = root
            .read_with(&cx, |root, _| root.session.clone())
            .expect("session");
        assert_ne!(first.id, second.id);

        // The newest session is the current one; the older row switches back.
        cx.update(|_, _| {});
        let older_row = cx
            .debug_bounds(selector(format!("thread-{}", first.id)))
            .expect("older session row");
        cx.simulate_click(older_row.center(), Modifiers::default());
        cx.update(|_, _| {});

        let turns = root.read_with(&cx, |root, _| {
            root.turns
                .iter()
                .map(|turn| turn.user.clone())
                .collect::<Vec<_>>()
        });
        assert_eq!(turns, vec!["first question"]);
        assert_eq!(
            root.read_with(&cx, |root, _| root
                .session
                .clone()
                .map(|session| session.id)),
            Some(first.id)
        );
    }

    #[gpui::test]
    async fn grammars_for_common_code_fences_are_registered(cx: &mut TestAppContext) {
        let registry = cx.update(|cx| syntax::language_registry(cx));

        // Mirrors how the markdown crate resolves a code fence label.
        for fence in ["rust", "python", "typescript", "tsx", "json", "bash"] {
            assert!(
                registry.language_for_name_or_extension(fence).await.is_ok(),
                "```{fence} should resolve to a registered grammar"
            );
        }
    }

    #[gpui::test]
    fn streamed_code_fence_resolves_to_a_grammar(cx: &mut TestAppContext) {
        let provider = Arc::new(FakeProvider {
            requests: Arc::new(std::sync::Mutex::new(Vec::new())),
            events: vec![
                StreamEvent::Text("```rust\nfn main() {}\n```\n".to_owned()),
                StreamEvent::Finished {
                    reason: StopReason::Stop,
                },
            ],
            title: None,
        });
        let (window, mut cx) = open_window_with(cx, None, Ok(provider));
        let root = window.root(&mut cx).expect("root view");
        send(&mut cx, &root, "show me rust");

        let language = root.read_with(&cx, |root, cx| {
            root.turns[0]
                .assistant
                .read(cx)
                .first_code_block_language()
                .map(|language| language.name().to_string())
        });

        assert_eq!(
            language.as_deref(),
            Some("Rust"),
            "the rust fence should resolve to the Rust grammar"
        );
    }

    #[gpui::test]
    fn a_submission_without_a_provider_keeps_the_draft_and_opens_the_list(cx: &mut TestAppContext) {
        let (window, mut cx) = open_window(cx);
        let root = window.root(&mut cx).expect("root view");
        let input = root.read_with(&cx, |root, _| root.composer_input.clone());

        input.update_in(&mut cx, |input, window, cx| {
            window.focus(&input.focus_handle(cx), cx);
        });
        cx.simulate_input("hello there");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        // Opening the window's own list is deferred out of the key dispatch.
        cx.update(|_, _| {});

        assert_eq!(
            input.read_with(&cx, |input, _| input.value().to_owned()),
            "hello there",
            "the draft is not lost to a turn that cannot run"
        );
        assert_eq!(
            root.read_with(&cx, |root, _| root.turns.len()),
            0,
            "no provider means no turn, not a failed one"
        );
        assert!(
            cx.debug_bounds("model-menu").is_some(),
            "the list that can fix it is open"
        );
    }

    struct FakeProvider {
        requests: Arc<std::sync::Mutex<Vec<Request>>>,
        events: Vec<StreamEvent>,
        /// What it answers when it is asked for a thread title.
        title: Option<String>,
    }

    impl Provider for FakeProvider {
        fn request(&self, messages: Vec<Message>) -> Request {
            Request::new("deepseek-flash", messages)
        }

        fn stream(&self, request: Request) -> EventStream {
            let is_title_request = request
                .messages
                .last()
                .is_some_and(|message| message.content == THREAD_TITLE_PROMPT);
            self.requests.lock().expect("requests lock").push(request);

            let events = match (&self.title, is_title_request) {
                (Some(title), true) => vec![
                    StreamEvent::Text(title.clone()),
                    StreamEvent::Finished {
                        reason: StopReason::Stop,
                    },
                ],
                _ => self.events.clone(),
            };
            futures::stream::iter(events.into_iter().map(Ok)).boxed()
        }
    }

    #[gpui::test]
    fn clicking_send_submits_the_draft(cx: &mut TestAppContext) {
        let (provider, _requests) = scripted(Vec::new(), vec![finished()]);
        let (window, mut cx) = open_window_with(cx, None, Ok(provider));
        let root = window.root(&mut cx).expect("root view");
        let input = root.read_with(&cx, |root, _| root.composer_input.clone());

        input.update_in(&mut cx, |input, window, cx| {
            window.focus(&input.focus_handle(cx), cx);
        });
        cx.simulate_input("send me");

        let send = cx.debug_bounds("send").expect("send button laid out");
        cx.simulate_click(send.center(), Modifiers::default());

        assert_eq!(
            input.read_with(&cx, |input, _| input.value().to_owned()),
            ""
        );
        assert_eq!(root.read_with(&cx, |root, _| root.turns.len()), 1);
        assert_eq!(
            root.read_with(&cx, |root, _| root.turns[0].user.clone()),
            "send me"
        );
    }

    #[gpui::test]
    fn sending_streams_the_reply_into_a_turn(cx: &mut TestAppContext) {
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = Arc::new(FakeProvider {
            requests: requests.clone(),
            events: vec![
                StreamEvent::Text("Hello".to_owned()),
                StreamEvent::Text(" there".to_owned()),
                StreamEvent::Finished {
                    reason: StopReason::Stop,
                },
            ],
            title: None,
        });
        let (window, mut cx) = open_window_with(cx, None, Ok(provider));

        let root = window.root(&mut cx).expect("root view");
        let input = root.read_with(&cx, |root, _| root.composer_input.clone());
        input.update_in(&mut cx, |input, window, cx| {
            window.focus(&input.focus_handle(cx), cx);
        });
        cx.simulate_input("hi");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();

        let (turns, source, state) = root.read_with(&cx, |root, cx| {
            let turn = &root.turns[0];
            (
                root.turns.len(),
                turn.assistant.read(cx).source().to_string(),
                turn.state.clone(),
            )
        });

        assert_eq!(turns, 1);
        assert_eq!(source, "Hello there");
        assert_eq!(state, TurnState::Done);

        let requests = requests.lock().expect("requests lock");
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].messages.last(),
            Some(&Message::user("hi")),
            "the question is the last message"
        );
        assert_eq!(
            requests[0].messages[0].role,
            humibird_providers::Role::System,
            "the session starts with its prompt"
        );
    }

    #[gpui::test]
    fn light_is_the_default_and_the_toggle_switches_to_dark(cx: &mut TestAppContext) {
        let (_, mut cx) = open_window(cx);

        let toggle = cx
            .debug_bounds("theme-light")
            .expect("light theme toggle is laid out");
        assert!(cx.debug_bounds("theme-dark").is_none());
        let light_title_bar = cx.update(|_, cx| cx.theme().colors().title_bar_background);

        cx.simulate_click(toggle.center(), Modifiers::default());

        assert!(cx.debug_bounds("theme-dark").is_some());
        assert!(cx.debug_bounds("theme-light").is_none());

        let dark_title_bar = cx.update(|_, cx| cx.theme().colors().title_bar_background);
        assert_ne!(light_title_bar, dark_title_bar);
    }

    #[gpui::test]
    fn a_dark_config_starts_in_dark(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        profile
            .save_config(&Config {
                theme: Theme::Dark,
                ..Default::default()
            })
            .expect("write config");

        let (_, mut cx) = open_window_with(cx, Some(profile), Err("not configured".to_owned()));

        assert!(cx.debug_bounds("theme-dark").is_some());
        assert!(cx.debug_bounds("theme-light").is_none());
    }

    #[gpui::test]
    fn toggling_the_theme_is_saved_to_the_config(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        let (_, mut cx) =
            open_window_with(cx, Some(profile.clone()), Err("not configured".to_owned()));

        let toggle = cx
            .debug_bounds("theme-light")
            .expect("light theme toggle is laid out");
        cx.simulate_click(toggle.center(), Modifiers::default());

        assert_eq!(
            profile.config().expect("the config is written").theme,
            Theme::Dark
        );

        // The next launch reads it back.
        let restored =
            cx.new(|cx| HumibirdRoot::new_with(cx, Some(profile), Err("offline".to_owned())));
        assert_eq!(
            restored.read_with(&cx, |root, _| root.appearance),
            Appearance::Dark
        );
    }

    #[test]
    fn a_profile_resolves_the_provider_it_names() {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));

        // Ollama needs no key, so this resolves with nothing on disk.
        let ollama = Config {
            provider: ProviderConfig {
                id: "ollama".to_owned(),
                model: Some("qwen3".to_owned()),
                ..ProviderConfig::default()
            },
            ..Config::default()
        };
        assert!(
            default_provider(&Some(profile.clone()), &ollama, &no_process_env()).is_ok(),
            "a provider without a key has to resolve"
        );
    }

    #[test]
    fn an_anthropic_profile_resolves_with_its_own_key() {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        profile
            .set_secret("ANTHROPIC_API_KEY", "sk-ant-test")
            .expect("write .env");
        let config = Config {
            provider: ProviderConfig {
                id: "anthropic".to_owned(),
                model: Some("claude-sonnet-4-5".to_owned()),
                ..ProviderConfig::default()
            },
            ..Config::default()
        };

        assert!(
            default_provider(&Some(profile), &config, &no_process_env()).is_ok(),
            "Anthropic has its own adapter, and the resolver has to reach it"
        );
    }

    #[test]
    fn a_provider_that_cannot_work_says_what_to_fix() {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Some(Profile::at(directory.path().join("profile")));

        let resolve = |provider: ProviderConfig| -> String {
            match default_provider(
                &profile,
                &Config {
                    provider,
                    ..Config::default()
                },
                &no_process_env(),
            ) {
                Ok(_) => panic!("this provider cannot be built"),
                Err(error) => error,
            }
        };

        let unknown = resolve(ProviderConfig {
            id: "somewhere".to_owned(),
            ..ProviderConfig::default()
        });
        assert!(unknown.contains("Unknown provider"), "{unknown}");
        assert!(unknown.contains("deepseek"), "{unknown}");

        // The endpoint of a custom provider is only known to the person.
        let no_endpoint = resolve(ProviderConfig {
            id: "custom".to_owned(),
            model: Some("local".to_owned()),
            ..ProviderConfig::default()
        });
        assert!(no_endpoint.contains("provider.base_url"), "{no_endpoint}");

        // A variable that is not set is named, provider and variable both.
        let unset_key = "HUMIBIRD_TEST_KEY_THAT_IS_NOT_SET";
        let no_key = resolve(ProviderConfig {
            id: "custom".to_owned(),
            base_url: Some("https://gateway.example/v1".to_owned()),
            model: Some("local".to_owned()),
            key_env: Some(unset_key.to_owned()),
            ..ProviderConfig::default()
        });
        assert!(no_key.contains("Custom endpoint"), "{no_key}");
        assert!(no_key.contains(unset_key), "{no_key}");

        // A provider with no catalogue of its own needs a model named for it.
        let no_model = resolve(ProviderConfig {
            id: "custom".to_owned(),
            base_url: Some("http://localhost:1234/v1".to_owned()),
            ..ProviderConfig::default()
        });
        assert!(no_model.contains("provider.model"), "{no_model}");
    }

    #[gpui::test]
    fn a_provider_switch_records_the_new_model_on_the_thread(cx: &mut TestAppContext) {
        let (directory, workspace) = temp_workspace();
        let profile = Profile::at(directory.path().join("profile"));
        let answer = vec![StreamEvent::Text("ok".to_owned()), finished()];
        let (provider, _requests) = scripted(vec![answer.clone()], answer);
        let (window, mut cx) =
            open_window_in_workspace(cx, Some(profile.clone()), Ok(provider), &workspace);
        let root = window.root(&mut cx).expect("root view");

        send(&mut cx, &root, "hello");
        cx.run_until_parked();
        cx.update(|_, _| {});

        // What the settings window writes when the profile moves to another
        // provider and names its model.
        profile
            .save_config(&Config {
                provider: ProviderConfig {
                    id: "ollama".to_owned(),
                    model: Some("qwen3".to_owned()),
                    ..ProviderConfig::default()
                },
                ..Config::default()
            })
            .expect("write config");
        cx.update(|_, cx| root.update(cx, |root, cx| root.refresh_provider(cx)));
        cx.update(|_, _| {});

        assert_eq!(root.read_with(&cx, |root, _| root.model()), "qwen3");
        let store = Store::open(&profile.database_path()).expect("the store opens");
        let sessions = store.sessions().expect("the sessions");
        assert_eq!(
            sessions[0].model.as_deref(),
            Some("qwen3"),
            "a restart reads the model back from the thread, so it has to say the new one"
        );
    }

    #[gpui::test]
    fn a_thread_from_another_provider_follows_the_profile_and_says_so(cx: &mut TestAppContext) {
        let (directory, workspace) = temp_workspace();
        let profile = Profile::at(directory.path().join("profile"));
        let store = Store::open(&profile.database_path()).expect("store");
        let session = store
            .create_session(
                &workspace.to_string_lossy(),
                Some("openrouter"),
                Some("a-model-openrouter-had"),
            )
            .expect("a thread from another provider");
        store
            .rename_session(&session.id, "Old thread")
            .expect("name it");
        drop(store);

        let answer = vec![StreamEvent::Text("ok".to_owned()), finished()];
        let (provider, _requests) = scripted(vec![answer.clone()], answer);
        let (window, mut cx) =
            open_window_in_workspace(cx, Some(profile.clone()), Ok(provider), &workspace);
        let root = window.root(&mut cx).expect("root view");

        assert_eq!(
            root.read_with(&cx, |root, _| root.model()),
            DEFAULT_MODEL,
            "the thread cannot keep a model the provider in use may not know"
        );
        let notice = root
            .read_with(&cx, |root, _| root.workspace_notice.clone())
            .expect("a notice explaining the move");
        assert!(notice.contains("OpenRouter"), "{notice}");
        assert!(notice.contains("DeepSeek"), "{notice}");

        // The row now says what the thread really talks to, so opening it again is
        // quiet.
        let store = Store::open(&profile.database_path()).expect("store");
        let sessions = store.sessions().expect("sessions");
        assert_eq!(sessions[0].provider.as_deref(), Some("deepseek"));
        assert_eq!(sessions[0].model.as_deref(), Some(DEFAULT_MODEL));
    }

    #[gpui::test]
    fn the_thinking_switch_reaches_the_request_and_the_profile(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        let (provider, requests) = scripted(vec![vec![text("sure"), finished()]], Vec::new());
        let (window, mut cx) = open_window_with(cx, Some(profile.clone()), Ok(provider));
        let root = window.root(&mut cx).expect("root view");

        let toggle = cx
            .debug_bounds("thinking-toggle")
            .expect("DeepSeek lets the switch be offered");
        cx.simulate_click(toggle.center(), Modifiers::default());
        cx.update(|_, _| {});
        assert!(
            profile
                .config()
                .expect("the choice is written")
                .thinking
                .enabled,
            "turning it on is kept for the next launch"
        );

        let chip = cx
            .debug_bounds("thinking-effort")
            .expect("the effort chip appears with the switch");
        cx.simulate_click(chip.center(), Modifiers::default());
        cx.update(|_, _| {});

        let menu = cx
            .debug_bounds("thinking-menu")
            .expect("the chip opens the levels");
        assert!(
            (menu.left() - chip.left()).abs() < px(1.),
            "the levels hang from the chip's left edge: {menu:?} / {chip:?}"
        );
        assert!(
            (menu.bottom() - (chip.top() - px(6.))).abs() < px(1.),
            "and sit just above it: {menu:?} / {chip:?}"
        );

        // A press on the panel itself is not a click outside it.
        cx.simulate_click(
            point(menu.origin.x + px(2.), menu.origin.y + px(2.)),
            Modifiers::default(),
        );
        cx.update(|_, _| {});
        assert!(
            cx.debug_bounds("thinking-menu").is_some(),
            "a press on the menu does not dismiss it"
        );

        let high = cx
            .debug_bounds("thinking-effort-high")
            .expect("the level is offered while the menu is open");
        cx.simulate_click(high.center(), Modifiers::default());
        cx.update(|_, _| {});
        assert!(
            cx.debug_bounds("thinking-menu").is_none(),
            "picking a level puts the menu away"
        );
        assert_eq!(
            profile
                .config()
                .expect("the level is written")
                .thinking
                .effort,
            ProfileEffort::High
        );

        send(&mut cx, &root, "hello");
        cx.run_until_parked();

        let sent = requests
            .lock()
            .expect("requests")
            .iter()
            .find(|request| {
                request
                    .messages
                    .iter()
                    .any(|message| message.content == "hello")
            })
            .cloned()
            .expect("the turn's request");
        assert_eq!(
            sent.thinking,
            Some(humibird_providers::Thinking {
                enabled: true,
                effort: Effort::High,
            }),
            "the request carries what the composer shows"
        );
    }

    #[gpui::test]
    fn the_composer_menus_take_turns(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        let (provider, _requests) = scripted(vec![vec![text("sure"), finished()]], Vec::new());
        let (window, mut cx) = open_window_with(cx, Some(profile), Ok(provider));
        let root = window.root(&mut cx).expect("root view");

        let toggle = cx
            .debug_bounds("thinking-toggle")
            .expect("DeepSeek lets the switch be offered");
        cx.simulate_click(toggle.center(), Modifiers::default());
        cx.update(|_, _| {});

        let effort = cx
            .debug_bounds("thinking-effort")
            .expect("the effort chip appears with the switch");
        cx.simulate_click(effort.center(), Modifiers::default());
        cx.update(|_, _| {});
        assert!(
            cx.debug_bounds("thinking-menu").is_some(),
            "the chip opens the levels"
        );

        let chip = cx
            .debug_bounds(selector(format!("composer-model-{DEFAULT_MODEL}")))
            .expect("the model chip");
        cx.simulate_click(chip.center(), Modifiers::default());
        cx.update(|_, _| {});
        assert!(
            root.read_with(&cx, |root, _| root.model_menu_open),
            "the model chip opens its own list"
        );
        assert!(
            cx.debug_bounds("thinking-menu").is_none(),
            "and the levels close behind it"
        );
    }

    #[gpui::test]
    fn a_provider_without_a_thinking_switch_does_not_offer_one(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        profile
            .save_config(&Config {
                provider: humibird_profile::ProviderConfig {
                    id: "anthropic".to_owned(),
                    ..Default::default()
                },
                ..Config::default()
            })
            .expect("write config");
        let (provider, _requests) = scripted(Vec::new(), Vec::new());
        let (_window, mut cx) = open_window_with(cx, Some(profile), Ok(provider));

        assert!(
            cx.debug_bounds("thinking-toggle").is_none(),
            "Anthropic's wire has no switch this app can use, so none is shown"
        );
    }

    #[gpui::test]
    fn the_configured_model_reaches_the_session_and_the_provider(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        profile
            .save_config(&Config {
                theme: Theme::Light,
                provider: ProviderConfig {
                    id: "deepseek".to_owned(),
                    base_url: None,
                    model: Some("deepseek-reasoner".to_owned()),
                    key_env: None,
                    ..ProviderConfig::default()
                },
                ..Config::default()
            })
            .expect("write config");

        let provider = Arc::new(FakeProvider {
            requests: Arc::new(std::sync::Mutex::new(Vec::new())),
            events: vec![StreamEvent::Finished {
                reason: StopReason::Stop,
            }],
            title: None,
        });
        let (window, mut cx) = open_window_with(cx, Some(profile), Ok(provider));
        let root = window.root(&mut cx).expect("root view");

        assert_eq!(
            root.read_with(&cx, |root, _| root.model()),
            "deepseek-reasoner"
        );
        assert!(
            cx.debug_bounds(selector("composer-model-deepseek-reasoner".to_owned()))
                .is_some(),
            "the composer shows the configured model"
        );

        send(&mut cx, &root, "hello");

        let sessions = root.read_with(&cx, |root, _| root.sessions.clone());
        assert_eq!(sessions[0].model.as_deref(), Some("deepseek-reasoner"));
    }

    #[gpui::test]
    fn the_assistant_message_renders_as_markdown(cx: &mut TestAppContext) {
        let provider = Arc::new(FakeProvider {
            requests: Arc::new(std::sync::Mutex::new(Vec::new())),
            events: vec![
                StreamEvent::Text("# Heading\n\nSome **bold** text.\n".to_owned()),
                StreamEvent::Finished {
                    reason: StopReason::Stop,
                },
            ],
            title: None,
        });
        let (window, mut cx) = open_window_with(cx, None, Ok(provider));
        let root = window.root(&mut cx).expect("root view");
        send(&mut cx, &root, "markdown please");
        cx.run_until_parked();
        cx.update(|_, _| {});

        let timeline = cx.debug_bounds("timeline").expect("timeline laid out");
        assert!(timeline.size.height > px(0.));
    }

    #[gpui::test]
    fn new_thread_clears_the_conversation(cx: &mut TestAppContext) {
        let (provider, _requests) = scripted(Vec::new(), vec![finished()]);
        let (window, mut cx) = open_window_with(cx, None, Ok(provider));
        let root = window.root(&mut cx).expect("root view");
        send(&mut cx, &root, "hi");
        assert_eq!(root.read_with(&cx, |root, _| root.turns.len()), 1);

        let new_thread = cx.debug_bounds("new-thread").expect("new thread laid out");
        cx.simulate_click(new_thread.center(), Modifiers::default());

        assert_eq!(root.read_with(&cx, |root, _| root.turns.len()), 0);
        assert_eq!(
            root.read_with(&cx, |root, cx| root
                .composer_input
                .read(cx)
                .value()
                .to_owned()),
            ""
        );
    }

    #[gpui::test]
    fn deleting_a_thread_asks_once_and_then_removes_it(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        let provider = Arc::new(FakeProvider {
            requests: Arc::new(std::sync::Mutex::new(Vec::new())),
            events: vec![StreamEvent::Text("sure".to_owned()), finished()],
            title: None,
        });
        let (window, mut cx) = open_window_with(cx, Some(profile), Ok(provider));
        let root = window.root(&mut cx).expect("root view");
        send(&mut cx, &root, "hello");
        cx.update(|_, _| {});

        let session = root
            .read_with(&cx, |root, _| root.session.clone())
            .expect("the thread the message started");
        let control = selector(format!("thread-delete-{}", session.id));

        // The control is on screen only while its row is hovered.
        assert_eq!(
            root.read_with(&cx, |root, _| root.hovered_thread.clone()),
            None
        );
        hover_row(&mut cx, &session.id);

        // The first press only asks: deleting a transcript has no undo.
        let delete = cx
            .debug_bounds(control)
            .expect("the row has a delete control");
        cx.simulate_click(delete.center(), Modifiers::default());
        cx.update(|_, _| {});
        assert_eq!(
            root.read_with(&cx, |root, _| root.pending_delete.clone()),
            Some(session.id.clone()),
            "the row asks before it deletes"
        );
        assert!(
            root.read_with(&cx, |root, _| root.session.is_some()),
            "the thread is still open"
        );
        assert_eq!(root.read_with(&cx, |root, _| root.turns.len()), 1);

        // The second press deletes it, and the window falls back to a new thread.
        let confirm = cx.debug_bounds(control).expect("the control stays put");
        cx.simulate_click(confirm.center(), Modifiers::default());
        cx.update(|_, _| {});

        assert_eq!(
            root.read_with(&cx, |root, _| root.pending_delete.clone()),
            None
        );
        assert!(
            root.read_with(&cx, |root, _| root.session.is_none()),
            "the open thread is gone, so the window starts a new one"
        );
        assert_eq!(root.read_with(&cx, |root, _| root.turns.len()), 0);
        assert!(root.read_with(&cx, |root, _| root.sessions.is_empty()));
        assert!(
            root.read_with(&cx, |root, _| root
                .store
                .as_ref()
                .expect("a store")
                .sessions()
                .expect("threads")
                .is_empty()),
            "the store keeps nothing of it"
        );
        assert!(cx.debug_bounds("threads-empty").is_some());
    }

    #[gpui::test]
    fn deleting_another_thread_keeps_the_open_one(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        let provider = Arc::new(FakeProvider {
            requests: Arc::new(std::sync::Mutex::new(Vec::new())),
            events: vec![StreamEvent::Text("sure".to_owned()), finished()],
            title: None,
        });
        let (window, mut cx) = open_window_with(cx, Some(profile), Ok(provider));
        let root = window.root(&mut cx).expect("root view");
        send(&mut cx, &root, "first");
        cx.update(|_, _| {});
        let first = root
            .read_with(&cx, |root, _| root.session.clone())
            .expect("the first thread");

        let new_thread = cx.debug_bounds("new-thread").expect("new thread laid out");
        cx.simulate_click(new_thread.center(), Modifiers::default());
        send(&mut cx, &root, "second");
        cx.update(|_, _| {});
        let second = root
            .read_with(&cx, |root, _| root.session.clone())
            .expect("the second thread");
        assert_ne!(first.id, second.id);

        // Pressing the control and then clicking away cancels: nothing is deleted.
        hover_row(&mut cx, &first.id);
        let delete = cx
            .debug_bounds(selector(format!("thread-delete-{}", first.id)))
            .expect("the row has a delete control");
        cx.simulate_click(delete.center(), Modifiers::default());
        cx.update(|_, _| {});
        let elsewhere = cx
            .debug_bounds(selector(format!("thread-{}", second.id)))
            .expect("the open row");
        cx.simulate_click(elsewhere.center(), Modifiers::default());
        cx.update(|_, _| {});
        assert_eq!(
            root.read_with(&cx, |root, _| root.pending_delete.clone()),
            None,
            "clicking away cancels the question"
        );
        assert_eq!(root.read_with(&cx, |root, _| root.sessions.len()), 2);

        // Deleting the thread that is not open leaves the open one alone.
        hover_row(&mut cx, &first.id);
        let delete = cx
            .debug_bounds(selector(format!("thread-delete-{}", first.id)))
            .expect("the delete control");
        cx.simulate_click(delete.center(), Modifiers::default());
        cx.update(|_, _| {});
        let confirm = cx
            .debug_bounds(selector(format!("thread-delete-{}", first.id)))
            .expect("the control stays put");
        cx.simulate_click(confirm.center(), Modifiers::default());
        cx.update(|_, _| {});

        assert_eq!(
            root.read_with(&cx, |root, _| root
                .session
                .as_ref()
                .map(|session| session.id.clone())),
            Some(second.id.clone()),
            "the open thread is untouched"
        );
        assert_eq!(root.read_with(&cx, |root, _| root.turns.len()), 1);
        let threads = root.read_with(&cx, |root, _| root.sessions.clone());
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].id, second.id);
    }

    #[gpui::test]
    fn a_window_without_a_key_asks_for_one(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        let (window, mut cx) = open_window_with(cx, Some(profile), Err("no key".to_owned()));
        let _root = window.root(&mut cx).expect("root view");

        assert!(
            cx.debug_bounds("setup-key").is_some(),
            "the empty page asks for the key before anything is sent"
        );
        assert!(
            cx.debug_bounds("empty-ready").is_none(),
            "and does not invite a conversation it cannot have"
        );

        let button = cx
            .debug_bounds("setup-open-settings")
            .expect("the button that opens settings");
        cx.simulate_click(button.center(), Modifiers::default());
        cx.update(|_, _| {});

        let (_, mut settings) =
            settings_window(&mut cx).expect("settings opens from the empty page");
        settings.update(|_, _| {});
        assert!(settings.debug_bounds("settings").is_some());
        assert!(
            settings.debug_bounds("settings-key-deepseek").is_some(),
            "with the key field, not a card that says one is configured"
        );
    }

    #[gpui::test]
    fn a_window_without_a_key_offers_no_model(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        let (_, mut cx) = open_window_with(
            cx,
            Some(profile),
            Err("No DeepSeek API key yet.".to_owned()),
        );

        assert!(
            cx.debug_bounds(selector(format!("composer-model-{DEFAULT_MODEL}")))
                .is_none(),
            "a model that cannot answer is not offered"
        );
    }

    #[gpui::test]
    fn a_configured_window_invites_a_conversation(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        let provider = Arc::new(FakeProvider {
            requests: Arc::new(std::sync::Mutex::new(Vec::new())),
            events: vec![StreamEvent::Text("ok".to_owned()), finished()],
            title: None,
        });
        let (window, mut cx) = open_window_with(cx, Some(profile), Ok(provider));
        let _root = window.root(&mut cx).expect("root view");

        assert!(cx.debug_bounds("empty-ready").is_some());
        assert!(cx.debug_bounds("setup-key").is_none());
    }

    #[gpui::test]
    fn saving_a_key_writes_the_dotenv_and_rebuilds_the_provider(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        let (window, mut cx) =
            open_window_with(cx, Some(profile), Err("not configured".to_owned()));
        let root = window.root(&mut cx).expect("root view");

        // The row and the shortcut open the same window.
        let row = cx
            .debug_bounds("open-settings")
            .expect("the profile row is laid out");
        cx.simulate_click(row.center(), Modifiers::default());
        cx.update(|_, _| {});
        let (_, mut settings) = settings_window(&mut cx).expect("the settings window opens");
        settings.update(|_, _| {});
        assert!(settings.debug_bounds("settings").is_some());
        assert!(settings.debug_bounds("settings-key-deepseek").is_some());
        assert!(
            settings
                .debug_bounds("settings-configured-deepseek")
                .is_none()
        );

        // Opening settings focuses the key field.
        settings.simulate_input("sk-test");
        settings.simulate_keystrokes("enter");
        settings.update(|_, _| {});

        let env = std::fs::read_to_string(directory.path().join("profile/.env"))
            .expect("the key was written");
        assert!(env.contains("DEEPSEEK_API_KEY=sk-test"), "{env}");
        assert!(root.read_with(&settings, |root, _| root.provider.is_ok()));

        // A saved key replaced the field with the configured row.
        assert!(
            settings
                .debug_bounds("settings-configured-deepseek")
                .is_some()
        );
        assert!(settings.debug_bounds("settings-key-deepseek").is_none());
    }

    #[gpui::test]
    fn the_providers_page_offers_every_provider(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        let (_, mut cx) = open_window_with(cx, Some(profile), Err("not configured".to_owned()));

        let mut settings = open_settings(&mut cx);

        for preset in humibird_providers::PRESETS {
            assert!(
                settings
                    .debug_bounds(selector(format!("provider-row-{}", preset.id)))
                    .is_some(),
                "{} is not listed",
                preset.id
            );
        }
        // The provider in use is chosen in the composer's model picker, not here.
    }

    #[gpui::test]
    fn a_provider_without_a_key_still_leaves_the_picker_reachable(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        // DeepSeek has a key, but the profile is on OpenAI, which does not.
        profile
            .set_secret("DEEPSEEK_API_KEY", "sk-test")
            .expect("write .env");
        profile
            .save_config(&Config {
                provider: ProviderConfig {
                    id: "openai".to_owned(),
                    ..ProviderConfig::default()
                },
                ..Config::default()
            })
            .expect("write config");
        let (window, mut cx) = open_window_with(
            cx,
            Some(profile.clone()),
            Err("No OpenAI API key yet".to_owned()),
        );
        let root = window.root(&mut cx).expect("root view");

        // The chip is the only way out of a provider that cannot answer, so it is
        // offered even when there is nothing to name.
        let chip = cx
            .debug_bounds("composer-model-chip")
            .expect("the model chip");
        cx.simulate_click(chip.center(), Modifiers::default());
        cx.update(|_, _| {});
        let row = cx
            .debug_bounds(selector(format!("model-option-deepseek/{DEFAULT_MODEL}")))
            .expect("another provider's model");
        cx.simulate_click(row.center(), Modifiers::default());
        cx.run_until_parked();

        assert_eq!(
            profile.config().expect("the choice is written").provider.id,
            "deepseek"
        );
        assert_eq!(
            root.read_with(&cx, |root, _| root.model.clone()),
            DEFAULT_MODEL
        );
    }

    #[gpui::test]
    fn the_model_list_leads_to_provider_settings(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        let (window, mut cx) =
            open_window_with(cx, Some(profile), Err("not configured".to_owned()));
        let _root = window.root(&mut cx).expect("root view");

        let chip = cx
            .debug_bounds("composer-model-chip")
            .expect("the model chip");
        cx.simulate_click(chip.center(), Modifiers::default());
        cx.update(|_, _| {});

        let configure = cx
            .debug_bounds("model-configure-providers")
            .expect("the way into the keys");
        cx.simulate_click(configure.center(), Modifiers::default());
        cx.update(|_, _| {});

        let (_, mut settings) = settings_window(&mut cx).expect("settings open from the list");
        settings.update(|_, _| {});
        assert!(
            settings.debug_bounds("settings-key-deepseek").is_some(),
            "the list leads to the page that holds the keys"
        );
    }

    #[gpui::test]
    fn the_picker_lists_nothing_that_cannot_answer(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        let (window, mut cx) =
            open_window_with(cx, Some(profile), Err("not configured".to_owned()));
        let _root = window.root(&mut cx).expect("root view");

        let chip = cx
            .debug_bounds("composer-model-chip")
            .expect("the model chip");
        cx.simulate_click(chip.center(), Modifiers::default());
        cx.update(|_, _| {});

        assert!(
            cx.debug_bounds(selector(format!("model-option-deepseek/{DEFAULT_MODEL}")))
                .is_none(),
            "a catalogue is not offered for a provider with no key"
        );
        assert!(
            cx.debug_bounds("model-switch-ollama").is_none(),
            "a provider with nothing to offer is not a row"
        );
        assert!(
            cx.debug_bounds("model-configure-providers").is_some(),
            "the way to the keys is still there"
        );
    }

    #[gpui::test]
    fn the_search_tool_follows_the_deepseek_key(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        let (window, mut cx) =
            open_window_with(cx, Some(profile.clone()), Err("not configured".to_owned()));
        let root = window.root(&mut cx).expect("root view");
        let names = |root: &HumibirdRoot| {
            root.conversation
                .tools()
                .iter()
                .map(|tool| tool.name().to_owned())
                .collect::<Vec<_>>()
        };

        assert!(
            !root
                .read_with(&cx, |root, _| names(root))
                .contains(&"web_search".to_owned()),
            "no DeepSeek key, no DeepSeek search"
        );

        // A saved key is what offers it: the same rebuild that takes the new key for
        // the chat takes it for the tool.
        profile
            .set_secret("DEEPSEEK_API_KEY", "sk-test")
            .expect("write .env");
        cx.update(|_, cx| root.update(cx, |root, cx| root.refresh_provider(cx)));

        assert!(
            root.read_with(&cx, |root, _| names(root))
                .contains(&"web_search".to_owned()),
            "the key offers the tool"
        );

        profile
            .remove_secret("DEEPSEEK_API_KEY")
            .expect("remove the key");
        cx.update(|_, cx| root.update(cx, |root, cx| root.refresh_provider(cx)));

        assert!(
            !root
                .read_with(&cx, |root, _| names(root))
                .contains(&"web_search".to_owned()),
            "a reset key takes the tool away"
        );
    }

    #[gpui::test]
    fn switching_the_provider_writes_the_profile_and_the_chat_follows(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        profile
            .set_secret("OPENROUTER_API_KEY", "sk-or-test")
            .expect("write .env");
        let (window, mut cx) = open_window_with(
            cx,
            Some(profile.clone()),
            Ok(Arc::new(ListingProvider::new())),
        );
        let root = window.root(&mut cx).expect("root view");
        // The window holds one provider of its own; every other one is asked
        // through this factory, which the test answers without a request.
        cx.update(|_, cx| {
            root.update(cx, |root, _| {
                root.listing_provider = Rc::new(|preset| {
                    if preset.id == "openrouter" {
                        Ok(Arc::new(ListingProvider::new()) as Arc<dyn Provider>)
                    } else {
                        Err("not in this test".to_owned())
                    }
                });
            });
        });

        // The picker lists the models of every provider that can answer, so
        // another provider's model is offered without switching to it first.
        let chip = cx
            .debug_bounds(selector(format!("composer-model-{DEFAULT_MODEL}")))
            .expect("the model chip");
        cx.simulate_click(chip.center(), Modifiers::default());
        cx.run_until_parked();
        let row = cx
            .debug_bounds("model-option-openrouter/provider-model-one")
            .expect("another provider's model");
        cx.simulate_click(row.center(), Modifiers::default());
        cx.run_until_parked();

        let config = profile.config().expect("the choice is written");
        assert_eq!(config.provider.id, "openrouter");
        assert_eq!(config.provider.model.as_deref(), Some("provider-model-one"));
        assert_eq!(
            root.read_with(&cx, |root, _| root.model.clone()),
            "provider-model-one"
        );
        assert_eq!(
            root.read_with(&cx, |root, _| root.conversation.model().to_owned()),
            "provider-model-one",
            "the open thread follows the pick"
        );
        assert!(
            root.read_with(&cx, |root, _| root.provider.is_ok()),
            "the new provider is built from the profile"
        );
    }

    #[gpui::test]
    fn a_custom_endpoint_names_its_model_and_measures_the_window(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        let provider = Arc::new(FakeProvider {
            requests: Arc::new(std::sync::Mutex::new(Vec::new())),
            events: vec![StreamEvent::Text("ok".to_owned()), finished()],
            title: None,
        });
        let (window, mut cx) = open_window_with(cx, Some(profile.clone()), Ok(provider));
        let root = window.root(&mut cx).expect("root view");

        // The endpoint, model and window are named in Settings without switching.
        let mut settings = open_settings(&mut cx);
        settings.simulate_resize(size(px(760.), px(1200.)));
        settings.update(|_, _| {});
        let endpoint = settings
            .debug_bounds("settings-base-url")
            .expect("the endpoint field");
        settings.simulate_click(endpoint.center(), Modifiers::default());
        settings.simulate_input("https://example.com/v1");

        let model = settings
            .debug_bounds("settings-model")
            .expect("the model field");
        settings.simulate_click(model.center(), Modifiers::default());
        settings.simulate_input("local-model");

        let field = settings
            .debug_bounds("settings-context-window")
            .expect("the window field");
        settings.simulate_click(field.center(), Modifiers::default());
        settings.simulate_input("200000");
        let save = settings
            .debug_bounds("settings-save")
            .expect("save laid out");
        settings.simulate_click(save.center(), Modifiers::default());
        settings.update(|_, _| {});

        let config = profile.config().expect("written");
        let custom = config.overrides("custom");
        assert_eq!(custom.base_url.as_deref(), Some("https://example.com/v1"));
        assert_eq!(custom.model.as_deref(), Some("local-model"));
        assert_eq!(custom.context_window, Some(200_000));
        assert_eq!(
            config.provider.id, "deepseek",
            "naming an endpoint is not a switch"
        );

        settings.simulate_keystrokes("escape");

        // Its model then appears like any other, and picking it is the switch. The
        // chip asks for a model because no provider resolved here.
        let chip = cx
            .debug_bounds("composer-model-chip")
            .expect("the model chip");
        cx.simulate_click(chip.center(), Modifiers::default());
        cx.update(|_, _| {});
        let row = cx
            .debug_bounds("model-option-custom/local-model")
            .expect("the custom endpoint's model");
        cx.simulate_click(row.center(), Modifiers::default());
        cx.run_until_parked();

        let config = profile.config().expect("written");
        assert_eq!(config.provider.id, "custom");
        assert_eq!(config.provider.model.as_deref(), Some("local-model"));
        assert_eq!(
            root.read_with(&cx, |root, _| root.conversation.context_window()),
            Some(200_000),
            "the thread measures against the window the record names"
        );

        // A word is not a number: the save is refused and the old value stands.
        let mut settings = open_settings(&mut cx);
        let field = settings
            .debug_bounds("settings-context-window")
            .expect("the window field");
        settings.simulate_click(field.center(), Modifiers::default());
        settings.simulate_input("lots");
        let save = settings
            .debug_bounds("settings-save")
            .expect("save laid out");
        settings.simulate_click(save.center(), Modifiers::default());
        settings.update(|_, _| {});

        assert_eq!(
            profile
                .config()
                .expect("unchanged")
                .overrides("custom")
                .context_window,
            Some(200_000)
        );
        assert!(
            settings.debug_bounds("settings-status").is_some(),
            "the refusal is on screen"
        );
    }

    #[gpui::test]
    fn a_custom_endpoint_is_configured_without_switching_to_it(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        let provider = Arc::new(FakeProvider {
            requests: Arc::new(std::sync::Mutex::new(Vec::new())),
            events: vec![StreamEvent::Text("ok".to_owned()), finished()],
            title: None,
        });
        let (window, mut cx) = open_window_with(cx, Some(profile.clone()), Ok(provider));
        let _root = window.root(&mut cx).expect("root view");

        // The profile is on DeepSeek; the custom section still names the endpoint,
        // and saving it leaves the thread where it is. The window is opened taller
        // than the real one so the form and its save button are on screen at once.
        let mut settings = open_settings(&mut cx);
        // The page is taller than this window: the test opens it taller so the form
        // and its save button are on screen at once.
        settings.simulate_resize(size(px(760.), px(1200.)));
        settings.update(|_, _| {});
        let endpoint = settings
            .debug_bounds("settings-base-url")
            .expect("the endpoint field is there without switching");
        settings.simulate_click(endpoint.center(), Modifiers::default());
        settings.simulate_input("https://example.com/v1");
        let save = settings
            .debug_bounds("settings-save")
            .expect("save laid out");
        settings.simulate_click(save.center(), Modifiers::default());
        settings.update(|_, _| {});

        let config = profile.config().expect("written");
        assert_eq!(
            config.overrides("custom").base_url.as_deref(),
            Some("https://example.com/v1")
        );
        assert_eq!(
            config.provider.id, "deepseek",
            "saving an endpoint is not a switch"
        );
        assert_eq!(config.provider.model, None);
    }

    #[gpui::test]
    fn a_local_providers_address_is_named_in_settings(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        let (window, mut cx) = open_window_with(
            cx,
            Some(profile.clone()),
            Ok(Arc::new(ListingProvider::new())),
        );
        let _root = window.root(&mut cx).expect("root view");

        let mut settings = open_settings(&mut cx);
        let field = settings
            .debug_bounds("settings-endpoint-ollama")
            .expect("the Ollama endpoint field");
        settings.simulate_click(field.center(), Modifiers::default());
        settings.simulate_input("http://localhost:11434/v1");
        settings.simulate_keystrokes("enter");
        settings.update(|_, _| {});

        let config = profile.config().expect("written");
        assert_eq!(
            config.overrides("ollama").base_url.as_deref(),
            Some("http://localhost:11434/v1")
        );
    }

    #[gpui::test]
    fn a_key_can_be_added_for_a_provider_that_is_not_in_use(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        let (_, mut cx) =
            open_window_with(cx, Some(profile.clone()), Err("not configured".to_owned()));
        let mut settings = open_settings(&mut cx);

        // Every provider that takes a key has its own field, so a key is entered
        // where it belongs instead of first switching the profile to that provider.
        let field = settings
            .debug_bounds("settings-key-openai")
            .expect("OpenAI's own key field");
        settings.simulate_click(field.center(), Modifiers::default());
        settings.simulate_input("sk-openai");
        settings.simulate_keystrokes("enter");
        settings.update(|_, _| {});

        let env = std::fs::read_to_string(directory.path().join("profile/.env"))
            .expect("the key was written");
        assert!(env.contains("OPENAI_API_KEY=sk-openai"), "{env}");
        assert_eq!(
            profile.config().expect("unchanged").provider.id,
            "deepseek",
            "adding a key does not switch the provider"
        );
        assert!(
            settings
                .debug_bounds("settings-configured-openai")
                .is_some()
        );
    }

    #[gpui::test]
    fn enter_in_the_key_field_saves_the_key(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        let (_, mut cx) = open_window_with(cx, Some(profile), Err("not configured".to_owned()));

        let mut settings = open_settings(&mut cx);
        settings.simulate_input("sk-entered");
        settings.simulate_keystrokes("enter");
        settings.update(|_, _| {});

        let env = std::fs::read_to_string(directory.path().join("profile/.env"))
            .expect("the key was written");
        assert!(env.contains("DEEPSEEK_API_KEY=sk-entered"), "{env}");
        assert!(
            settings
                .debug_bounds("settings-configured-deepseek")
                .is_some()
        );
        assert!(settings.debug_bounds("settings-key-deepseek").is_none());
    }

    #[gpui::test]
    fn a_configured_key_is_a_card_and_resetting_it_brings_the_input_back(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        profile
            .set_secret("DEEPSEEK_API_KEY", "sk-stored")
            .expect("write .env");
        let (_, mut cx) = open_window_with(cx, Some(profile), Err("not configured".to_owned()));

        let mut settings = open_settings(&mut cx);
        assert!(
            settings
                .debug_bounds("settings-configured-deepseek")
                .is_some()
        );
        assert!(settings.debug_bounds("settings-key-deepseek").is_none());

        let reset = settings
            .debug_bounds("settings-reset-deepseek")
            .expect("reset laid out");
        settings.simulate_click(reset.center(), Modifiers::default());
        settings.update(|_, _| {});

        let env = std::fs::read_to_string(directory.path().join("profile/.env"))
            .expect("the .env is still there");
        assert!(!env.contains("DEEPSEEK_API_KEY"), "{env}");
        assert!(settings.debug_bounds("settings-key-deepseek").is_some());
        assert!(
            settings
                .debug_bounds("settings-configured-deepseek")
                .is_none()
        );
    }

    #[gpui::test]
    fn a_key_from_the_environment_can_still_be_stored_in_the_profile(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        let env: EnvLookup =
            Rc::new(|name: &str| (name == "DEEPSEEK_API_KEY").then(|| "sk-shell".to_owned()));
        let (_, mut cx) =
            open_window_with_env(cx, Some(profile), Err("not configured".to_owned()), env);

        let mut settings = open_settings(&mut cx);
        // The environment wins while it is set, but the profile can still hold a key
        // for the launches that do not see it, so the field stays available.
        let field = settings
            .debug_bounds("settings-key-deepseek")
            .expect("the field for a key stored in the profile");
        settings.simulate_click(field.center(), Modifiers::default());
        settings.simulate_input("sk-profile");
        settings.simulate_keystrokes("enter");
        settings.update(|_, _| {});

        let env_file = std::fs::read_to_string(directory.path().join("profile/.env"))
            .expect("the key was written");
        assert!(
            env_file.contains("DEEPSEEK_API_KEY=sk-profile"),
            "{env_file}"
        );

        // Stored now, so the row and its Reset appear even though the environment
        // still supplies the key that requests use.
        assert!(
            settings
                .debug_bounds("settings-configured-deepseek")
                .is_some()
        );
        let reset = settings
            .debug_bounds("settings-reset-deepseek")
            .expect("reset laid out");
        settings.simulate_click(reset.center(), Modifiers::default());
        settings.update(|_, _| {});

        let env_file = std::fs::read_to_string(directory.path().join("profile/.env"))
            .expect("the .env is still there");
        assert!(!env_file.contains("DEEPSEEK_API_KEY"), "{env_file}");
        assert!(settings.debug_bounds("settings-key-deepseek").is_some());
    }

    #[gpui::test]
    fn resetting_a_key_rebuilds_the_provider_the_conversation_holds(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        profile
            .set_secret("DEEPSEEK_API_KEY", "sk-stored")
            .expect("write .env");
        let provider = Arc::new(FakeProvider {
            requests: Arc::new(std::sync::Mutex::new(Vec::new())),
            events: vec![StreamEvent::Text("ok".to_owned()), finished()],
            title: None,
        });
        let (window, mut cx) = open_window_with(cx, Some(profile), Ok(provider));
        let root = window.root(&mut cx).expect("root view");
        assert!(root.read_with(&cx, |root, _| root.conversation.has_provider()));

        let mut settings = open_settings(&mut cx);
        let reset = settings
            .debug_bounds("settings-reset-deepseek")
            .expect("reset laid out");
        settings.simulate_click(reset.center(), Modifiers::default());
        settings.update(|_, _| {});

        assert!(
            root.read_with(&settings, |root, _| root.provider.is_err()),
            "the window has no provider once the key is gone"
        );
        assert!(
            !root.read_with(&settings, |root, _| root.conversation.has_provider()),
            "the thread must not keep the provider that held the old key"
        );
    }

    #[gpui::test]
    fn cmd_comma_opens_settings_and_escape_closes_them(cx: &mut TestAppContext) {
        let (_, mut cx) = open_window(cx);
        assert!(settings_window(&mut cx).is_none());

        let mut settings = open_settings(&mut cx);
        assert!(settings.debug_bounds("settings").is_some());
        assert!(settings.debug_bounds("settings-nav-providers").is_some());

        settings.simulate_keystrokes("escape");

        // Closing is deferred out of the key dispatch, and the window is gone
        // by the time `simulate_keystrokes` returns.
        assert!(settings_window(&mut cx).is_none());
        // The chat window is untouched by the closing.
        assert!(cx.debug_bounds("composer").is_some());
    }

    #[gpui::test]
    fn cmd_n_starts_a_new_thread(cx: &mut TestAppContext) {
        let (provider, _requests) = scripted(Vec::new(), vec![finished()]);
        let (window, mut cx) = open_window_with(cx, None, Ok(provider));
        let root = window.root(&mut cx).expect("root view");
        send(&mut cx, &root, "hi");
        assert_eq!(root.read_with(&cx, |root, _| root.turns.len()), 1);

        cx.simulate_keystrokes("cmd-n");

        assert_eq!(root.read_with(&cx, |root, _| root.turns.len()), 0);
    }

    fn home_env(home: &std::path::Path) -> EnvLookup {
        let home = home.to_string_lossy().into_owned();
        Rc::new(move |name: &str| (name == "HOME").then(|| home.clone()))
    }

    fn open_skills_page(settings: &mut VisualTestContext) {
        let nav = settings
            .debug_bounds("settings-nav-skills")
            .expect("the skills nav item");
        settings.simulate_click(nav.center(), Modifiers::default());
        settings.update(|_, _| {});
    }

    fn open_context_servers_page(settings: &mut VisualTestContext) {
        let nav = settings
            .debug_bounds("settings-nav-context-servers")
            .expect("the context servers nav item");
        settings.simulate_click(nav.center(), Modifiers::default());
        settings.update(|_, _| {});
    }

    #[gpui::test]
    fn the_context_servers_page_says_what_became_of_each_server(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        std::fs::create_dir_all(profile.root()).expect("profile directory");
        std::fs::write(
            profile.mcp_path(),
            r#"{"servers": {"notes": {"command": "/nonexistent/mcp-server"}, "spare": {"command": "x", "enabled": false}}}"#,
        )
        .expect("write mcp.json");
        let (provider, _requests) = scripted(Vec::new(), Vec::new());
        let (_window, mut cx) = open_window_with(cx, Some(profile), Ok(provider));
        cx.run_until_parked();

        let mut settings = open_settings(&mut cx);
        open_context_servers_page(&mut settings);
        // Both configured servers are listed, one that failed to start and one that
        // is switched off; the page is about the file as much as about the run.
        assert!(
            settings.debug_bounds("settings-mcp-server-notes").is_some(),
            "the server that did not start"
        );
        assert!(
            settings.debug_bounds("settings-mcp-server-spare").is_some(),
            "the server that is switched off"
        );
    }

    #[gpui::test]
    fn the_skills_page_lists_what_is_installed_and_imports_one(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("temp dir");
        let home = directory.path().join("home");
        let profile = Profile::at(directory.path().join("profile"));
        install_skill(&profile, "card", "turns a JD into a card", "# Card");
        let source = humibird_profile::agents_skills_dir(&home).join("review");
        std::fs::create_dir_all(&source).expect("create the skill");
        std::fs::write(
            source.join("SKILL.md"),
            "---\nname: review\ndescription: reviews a diff\n---\n\n# Review\n",
        )
        .expect("write SKILL.md");

        let (_, mut cx) = open_window_with_env(
            cx,
            Some(profile.clone()),
            Err("not configured".to_owned()),
            home_env(&home),
        );

        let mut settings = open_settings(&mut cx);
        assert!(
            settings.debug_bounds("skills-show-folder").is_none(),
            "settings open on the providers page"
        );

        open_skills_page(&mut settings);

        assert!(settings.debug_bounds("skills-installed-card").is_some());
        assert!(settings.debug_bounds("skills-installed-review").is_none());

        let import = settings
            .debug_bounds("skills-import-review")
            .expect("the import button");
        settings.simulate_click(import.center(), Modifiers::default());
        settings.update(|_, _| {});

        assert!(
            profile.skills_dir().join("review/SKILL.md").is_file(),
            "the import copied the directory"
        );
        assert!(settings.debug_bounds("skills-installed-review").is_some());
        assert!(
            settings.debug_bounds("skills-import-review").is_none(),
            "an imported skill is no longer offered"
        );
        assert!(settings.debug_bounds("settings-skills-status").is_some());
    }

    #[gpui::test]
    fn a_long_skill_description_wraps_inside_the_page(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("temp dir");
        let home = directory.path().join("home");
        let profile = Profile::at(directory.path().join("profile"));
        // The index cuts a description at 200 characters, so this is the longest
        // one the page can be given.
        let description =
            "A long procedure that explains every step of the review in order, ".repeat(3);
        install_skill(&profile, "card", &description, "# Card");
        let source = humibird_profile::agents_skills_dir(&home).join("review");
        std::fs::create_dir_all(&source).expect("create the skill");
        std::fs::write(
            source.join("SKILL.md"),
            format!("---\nname: review\ndescription: {description}\n---\n\n# Review\n"),
        )
        .expect("write SKILL.md");

        let (_, mut cx) = open_window_with_env(
            cx,
            Some(profile),
            Err("not configured".to_owned()),
            home_env(&home),
        );
        let mut settings = open_settings(&mut cx);
        open_skills_page(&mut settings);

        let page = settings.debug_bounds("settings-page").expect("the page");
        for name in ["card", "review"] {
            let bounds = settings
                .debug_bounds(selector(format!("skills-description-{name}")))
                .unwrap_or_else(|| panic!("{name}'s description is laid out"));

            assert!(
                bounds.right() <= page.right(),
                "{name}'s description runs past the page: {} > {}",
                bounds.right(),
                page.right()
            );
            assert!(
                bounds.size.height > px(20.),
                "{name}'s description wrapped onto more than one line: {:?}",
                bounds.size.height
            );
        }
    }

    #[gpui::test]
    fn show_folder_opens_the_profiles_skills_directory(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        let (_, mut cx) =
            open_window_with(cx, Some(profile.clone()), Err("not configured".to_owned()));

        let mut settings = open_settings(&mut cx);
        open_skills_page(&mut settings);

        let button = settings
            .debug_bounds("skills-show-folder")
            .expect("the show folder button");
        settings.simulate_click(button.center(), Modifiers::default());
        settings.update(|_, _| {});

        let url = settings.opened_url().expect("the folder was opened");
        assert!(url.starts_with("file://"), "{url}");
        assert!(url.ends_with("/profile/skills"), "{url}");
        assert!(
            profile.skills_dir().is_dir(),
            "the folder exists so it can be revealed"
        );
    }
}
