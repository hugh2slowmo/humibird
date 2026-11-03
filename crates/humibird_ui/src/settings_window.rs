//! The settings window.
//!
//! Zed opens its settings UI in a window of its own with a section nav down the
//! left, and this follows that shape: the conversation stays untouched behind
//! it, and more sections can be added without crowding the chat.
//!
//! A key that is not configured yet and a key that is show different pages,
//! also as in Zed: the empty state is an input, the configured state is a card
//! whose only action is resetting the key. The key is never displayed again.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::{
    AnyElement, App, Bounds, Context, Div, Entity, Focusable, FontWeight, IntoElement,
    ParentElement, Render, ScrollHandle, SharedString, Stateful, Styled, Subscription,
    TitlebarOptions, WeakEntity, Window, WindowBounds, WindowOptions, div, point, prelude::*, px,
    rgb, size,
};
use humibird_agent::McpServers;
use humibird_profile::{
    Config, McpConfig, McpServer, McpTransport, Profile, ProviderOverride, agents_skills_dir,
};
use humibird_providers::Preset;
use humibird_skills::{Skill, SkillIndex};
use ui::{ScrollAxes, Scrollbars, WithScrollbar};
use util::ResultExt as _;

use crate::{Appearance, EnvLookup, HumibirdRoot, Palette, TextInput, TextInputEvent};

/// The preset whose endpoint and model the person names themselves. Its fields are
/// the only ones on the page that are not the preset's to decide.
const CUSTOM: &str = "custom";

#[derive(Clone, Copy, PartialEq, Eq)]
enum SettingsPage {
    Providers,
    ContextServers,
    Skills,
}

impl SettingsPage {
    fn label(self) -> &'static str {
        match self {
            Self::Providers => "Providers",
            Self::ContextServers => "Context servers",
            Self::Skills => "Skills",
        }
    }

    fn selector(self) -> &'static str {
        match self {
            Self::Providers => "settings-nav-providers",
            Self::ContextServers => "settings-nav-context-servers",
            Self::Skills => "settings-nav-skills",
        }
    }
}

pub struct SettingsWindow {
    root: WeakEntity<HumibirdRoot>,
    profile: Option<Profile>,
    appearance: Appearance,
    env: EnvLookup,
    /// The profile's configuration as this window last read or wrote it. The chat
    /// window reloads it when this one says something changed.
    config: Config,
    key_inputs: Vec<(&'static str, Entity<TextInput>)>,
    /// One field per provider that names its own address but takes no key. Saving it
    /// writes the provider's record, which is what tells the picker to ask it.
    endpoint_inputs: Vec<(&'static str, Entity<TextInput>)>,
    base_url_input: Entity<TextInput>,
    model_input: Entity<TextInput>,
    context_window_input: Entity<TextInput>,
    status: Option<String>,
    page: SettingsPage,
    /// The page is taller than the window once every provider is on it. The handle is
    /// what the scrollbar tracks, so the fact that there is more below is visible.
    page_scroll: ScrollHandle,
    /// `~/.agents/skills`, the only place an import copies from.
    source: Option<PathBuf>,
    /// The context servers the profile names, as the file had them when this window
    /// opened, and what the chat window managed to connect.
    mcp_config: Result<McpConfig, String>,
    mcp: Option<Arc<McpServers>>,
    /// The installed skills, snapped when the window opens and after an import.
    /// The chat's index is frozen separately, at the start of its session.
    skills: Vec<Skill>,
    importable: Vec<Skill>,
    skills_status: Option<String>,
    _base_url_subscription: Subscription,
    _model_subscription: Subscription,
    _context_window_subscription: Subscription,
    _key_subscriptions: Vec<Subscription>,
}

impl SettingsWindow {
    /// Opens the settings window, or brings the existing one forward.
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        cx: &mut App,
        profile: Option<Profile>,
        root: WeakEntity<HumibirdRoot>,
        appearance: Appearance,
        env: EnvLookup,
        mcp: Option<Arc<McpServers>>,
    ) {
        let existing = cx
            .windows()
            .into_iter()
            .filter_map(|window| window.downcast::<Self>())
            .next();
        if let Some(existing) = existing {
            existing
                .update(cx, |_, window, _| window.activate_window())
                .log_err();
            return;
        }

        // Tall enough for a provider list and the form under it: the pages scroll,
        // but a settings window that opens on a cut-off button is not a settings
        // window.
        let bounds = Bounds::centered(None, size(px(760.), px(780.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(560.), px(320.))),
                titlebar: Some(TitlebarOptions {
                    title: Some("humibird — Settings".into()),
                    appears_transparent: true,
                    traffic_light_position: Some(point(px(14.), px(18.))),
                }),
                focus: true,
                ..Default::default()
            },
            move |window, cx| {
                cx.new(|cx| {
                    Self::new(
                        profile.clone(),
                        root.clone(),
                        appearance,
                        env,
                        mcp.clone(),
                        window,
                        cx,
                    )
                })
            },
        )
        .log_err();
    }

    fn new(
        profile: Option<Profile>,
        root: WeakEntity<HumibirdRoot>,
        appearance: Appearance,
        env: EnvLookup,
        mcp: Option<Arc<McpServers>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let config = profile
            .as_ref()
            .and_then(|profile| {
                profile
                    .config()
                    .inspect_err(|error| log::warn!("settings open without the file: {error:#}"))
                    .ok()
            })
            .unwrap_or_default();
        // One field per provider that takes a key, so a key can be entered without
        // switching the profile to that provider. Enter saves, as in Zed.
        let mut key_inputs: Vec<(&'static str, Entity<TextInput>)> = Vec::new();
        let mut key_subscriptions: Vec<Subscription> = Vec::new();
        for preset in humibird_providers::PRESETS {
            if preset.key_env.is_none() {
                continue;
            }
            let input = cx.new(|cx| TextInput::new("xxxxxxxxxxxxxxxxxxxxxxxx", cx));
            key_subscriptions.push(cx.subscribe(
                &input,
                move |this, _input, event: &TextInputEvent, cx| {
                    if matches!(event, TextInputEvent::Submit) {
                        this.save_key(preset.id, cx);
                    }
                },
            ));
            key_inputs.push((preset.id, input));
        }
        // One field per local provider that has a default address but takes no key,
        // so pointing one somewhere else is a settings decision like any other.
        let mut endpoint_inputs: Vec<(&'static str, Entity<TextInput>)> = Vec::new();
        for preset in humibird_providers::PRESETS {
            if preset.key_env.is_some() || preset.base_url.is_empty() {
                continue;
            }
            let input = cx.new(|cx| TextInput::new(preset.base_url, cx));
            key_subscriptions.push(cx.subscribe(
                &input,
                move |this, _input, event: &TextInputEvent, cx| {
                    if matches!(event, TextInputEvent::Submit) {
                        this.save_endpoint(preset.id, cx);
                    }
                },
            ));
            endpoint_inputs.push((preset.id, input));
        }
        let base_url_input = cx.new(|cx| TextInput::new("https://...", cx));
        let model_input = cx.new(|cx| TextInput::new("model id", cx));
        let context_window_input = cx.new(|cx| TextInput::new("tokens (optional)", cx));
        // Enter in any of the fields saves the provider, the way Zed's provider
        // settings do.
        let _base_url_subscription = cx.subscribe(
            &base_url_input,
            |this, _input, event: &TextInputEvent, cx| {
                if matches!(event, TextInputEvent::Submit) {
                    this.save_provider(cx);
                }
            },
        );
        let _model_subscription =
            cx.subscribe(&model_input, |this, _input, event: &TextInputEvent, cx| {
                if matches!(event, TextInputEvent::Submit) {
                    this.save_provider(cx);
                }
            });
        let _context_window_subscription = cx.subscribe(
            &context_window_input,
            |this, _input, event: &TextInputEvent, cx| {
                if matches!(event, TextInputEvent::Submit) {
                    this.save_provider(cx);
                }
            },
        );
        let mcp_config = profile
            .as_ref()
            .map(|profile| profile.mcp_config().map_err(|error| format!("{error:#}")))
            .unwrap_or_else(|| Ok(McpConfig::default()));
        let source = (env)("HOME").map(|home| agents_skills_dir(Path::new(&home)));
        let (skills, importable) = skills_snapshot(profile.as_ref(), source.as_deref());

        let mut settings = Self {
            root,
            profile,
            appearance,
            env,
            config,
            key_inputs,
            endpoint_inputs,
            base_url_input,
            model_input,
            context_window_input,
            status: None,
            page: SettingsPage::Providers,
            page_scroll: ScrollHandle::new(),
            source,
            mcp_config,
            mcp,
            skills,
            importable,
            skills_status: None,
            _base_url_subscription,
            _model_subscription,
            _context_window_subscription,
            _key_subscriptions: key_subscriptions,
        };
        settings.reset_provider_form(cx);
        if settings.key_source() == "not configured"
            && settings.key_variable().is_some()
            && let Some(input) = settings.key_input_for(&settings.config.provider.id)
        {
            let focus = input.read(cx).focus_handle(cx);
            window.focus(&focus, cx);
        }
        settings
    }

    /// Keeps an open settings window in step with the chat's appearance toggle.
    pub(crate) fn set_appearance(&mut self, appearance: Appearance, cx: &mut Context<Self>) {
        if self.appearance != appearance {
            self.appearance = appearance;
            cx.notify();
        }
    }

    fn active_preset(&self) -> Option<&'static Preset> {
        humibird_providers::preset(&self.config.provider.id)
    }

    /// The variable the active provider's key lives in, when it wants one. The
    /// provider's record wins, which is how an endpoint without a key of its own
    /// gets one.
    fn key_variable(&self) -> Option<String> {
        self.config
            .overrides(&self.config.provider.id)
            .key_env
            .or_else(|| {
                self.active_preset()
                    .and_then(|preset| preset.key_env.map(str::to_owned))
            })
    }

    fn key_source_for(&self, variable: &str) -> &'static str {
        // The profile's file wins, matching `Profile::secret_with`.
        if self.profile.as_ref().is_some_and(|profile| {
            profile
                .dotenv()
                .get(variable)
                .is_some_and(|value| !value.trim().is_empty())
        }) {
            ".env"
        } else if (self.env)(variable).is_some_and(|value| !value.trim().is_empty()) {
            "environment"
        } else {
            "not configured"
        }
    }

    fn key_source(&self) -> &'static str {
        match self.key_variable() {
            Some(variable) => self.key_source_for(&variable),
            None => "not configured",
        }
    }

    /// Writes the custom endpoint's fields into its own record. The record belongs
    /// to the provider, so it can be filled in while another provider is in use; a
    /// picker row appears once it answers.
    fn save_provider(&mut self, cx: &mut Context<Self>) {
        let Some(profile) = self.profile.clone() else {
            self.status = Some("No profile directory is available.".to_owned());
            cx.notify();
            return;
        };

        let endpoint = self.base_url_input.read(cx).value().trim().to_owned();
        let model = self.model_input.read(cx).value().trim().to_owned();
        let window = self.context_window_input.read(cx).value().trim().to_owned();
        let window = match window.parse::<u32>() {
            Ok(tokens) if tokens > 0 => Some(tokens),
            Ok(_) => {
                self.status = Some("A context window has to be more than 0 tokens.".to_owned());
                cx.notify();
                return;
            }
            Err(_) if !window.is_empty() => {
                self.status = Some("A context window has to be a number of tokens.".to_owned());
                cx.notify();
                return;
            }
            Err(_) => None,
        };
        let record = self.config.providers.entry(CUSTOM.to_owned()).or_default();
        record.base_url = (!endpoint.is_empty()).then_some(endpoint);
        record.model = (!model.is_empty()).then_some(model);
        record.context_window = window;
        if self.config.provider.id == CUSTOM {
            // The model of the provider in use is the one this thread talks to.
            self.config.provider.model = record.model.clone();
        }

        match profile.save_config(&self.config) {
            Ok(()) => self.status = Some("Saved the custom endpoint.".to_owned()),
            Err(error) => {
                self.status = Some(format!("Could not save the provider: {error}"));
                cx.notify();
                return;
            }
        }
        self.notify_root(cx);
        cx.notify();
    }

    fn reset_provider_form(&mut self, cx: &mut Context<Self>) {
        let custom = self.config.overrides(CUSTOM);
        let endpoint = custom.base_url.unwrap_or_default();
        let model = custom.model.unwrap_or_default();
        let window = custom
            .context_window
            .map(|tokens| tokens.to_string())
            .unwrap_or_default();

        self.base_url_input.update(cx, |input, cx| {
            input.set_placeholder("https://...", cx);
            input.set_value(endpoint, cx);
        });
        self.model_input.update(cx, |input, cx| {
            input.set_placeholder("model id", cx);
            input.set_value(model, cx);
        });
        self.context_window_input.update(cx, |input, cx| {
            input.set_value(window, cx);
        });
        for (_, input) in &self.key_inputs {
            input.update(cx, |input, cx| input.set_value("", cx));
        }
        for (preset, input) in &self.endpoint_inputs {
            let endpoint = self.config.overrides(preset).base_url.unwrap_or_default();
            input.update(cx, |input, cx| input.set_value(endpoint, cx));
        }
    }

    fn remove_key(&mut self, variable: String, cx: &mut Context<Self>) {
        let Some(profile) = self.profile.clone() else {
            self.status = Some("No profile directory is available.".to_owned());
            cx.notify();
            return;
        };

        match profile.remove_secret(&variable) {
            Ok(()) => {
                self.status = Some(format!("Removed {variable} from .env."));
                self.notify_root(cx);
            }
            Err(error) => {
                self.status = Some(format!("Could not remove the key: {error}"));
            }
        }

        // The field came back, so put the cursor where the user has to type.
        if self.key_variable().as_deref() == Some(variable.as_str())
            && self.key_source() == "not configured"
            && let Some(input) = self.key_input_for(&self.config.provider.id)
        {
            let focus = input.read(cx).focus_handle(cx);
            cx.defer(move |cx| {
                if let Some(window) = cx.active_window() {
                    window
                        .update(cx, |_, window, cx| window.focus(&focus, cx))
                        .log_err();
                }
            });
        }
        cx.notify();
    }

    fn key_input_for(&self, id: &str) -> Option<Entity<TextInput>> {
        self.key_inputs
            .iter()
            .find(|(preset, _)| *preset == id)
            .map(|(_, input)| input.clone())
    }

    fn endpoint_input_for(&self, id: &str) -> Option<Entity<TextInput>> {
        self.endpoint_inputs
            .iter()
            .find(|(preset, _)| *preset == id)
            .map(|(_, input)| input.clone())
    }

    /// Writes a local provider's address into its own record. The record is what makes
    /// the picker ask that address; leaving the field empty stops the asking.
    fn save_endpoint(&mut self, id: &'static str, cx: &mut Context<Self>) {
        let Some(profile) = self.profile.clone() else {
            self.status = Some("No profile directory is available.".to_owned());
            cx.notify();
            return;
        };
        let Some(input) = self.endpoint_input_for(id) else {
            return;
        };
        let endpoint = input.read(cx).value().trim().to_owned();

        let record = self.config.providers.entry(id.to_owned()).or_default();
        record.base_url = (!endpoint.is_empty()).then_some(endpoint);
        if record == &ProviderOverride::default() {
            self.config.providers.remove(id);
        }

        match profile.save_config(&self.config) {
            Ok(()) => self.status = Some("Saved the endpoint.".to_owned()),
            Err(error) => {
                self.status = Some(format!("Could not save the endpoint: {error}"));
                cx.notify();
                return;
            }
        }
        self.notify_root(cx);
        cx.notify();
    }

    /// Writes the key typed into a provider's own field. The profile's selected
    /// provider does not change: a key belongs to the section it was typed in.
    fn save_key(&mut self, id: &'static str, cx: &mut Context<Self>) {
        let Some(preset) = humibird_providers::preset(id) else {
            return;
        };
        let Some(variable) = preset.key_env else {
            return;
        };
        let Some(input) = self.key_input_for(id) else {
            return;
        };
        let Some(profile) = self.profile.clone() else {
            self.status = Some("No profile directory is available.".to_owned());
            cx.notify();
            return;
        };
        let key = input.read(cx).value().trim().to_owned();
        if key.is_empty() {
            self.status = Some("Type a key first.".to_owned());
            cx.notify();
            return;
        }

        match profile.set_secret(variable, &key) {
            Ok(()) => {
                input.update(cx, |input, cx| input.set_value("", cx));
                self.status = Some(format!("Saved {variable} to .env."));
                self.notify_root(cx);
            }
            Err(error) => self.status = Some(format!("Could not save the key: {error}")),
        }
        cx.notify();
    }

    fn notify_root(&mut self, cx: &mut Context<Self>) {
        if let Err(error) = self.root.update(cx, |root, cx| root.refresh_provider(cx)) {
            log::warn!("the chat window is gone: {error}");
        }
    }

    fn set_page(&mut self, page: SettingsPage, cx: &mut Context<Self>) {
        if self.page != page {
            self.page = page;
            self.page_scroll.set_offset(point(px(0.), px(0.)));
            cx.notify();
        }
    }

    fn refresh_skills(&mut self) {
        (self.skills, self.importable) =
            skills_snapshot(self.profile.as_ref(), self.source.as_deref());
    }

    fn import_skill(&mut self, name: &str, cx: &mut Context<Self>) {
        let Some(profile) = self.profile.clone() else {
            self.skills_status = Some("No profile directory is available.".to_owned());
            cx.notify();
            return;
        };
        let Some(skill) = self
            .importable
            .iter()
            .find(|skill| skill.name == name)
            .cloned()
        else {
            return;
        };

        self.skills_status = match humibird_skills::import(&profile.skills_dir(), skill.root()) {
            Ok(skill) => Some(format!(
                "Imported {}. It joins the next conversation, not this one.",
                skill.name
            )),
            Err(error) => Some(format!("Could not import {name}: {error}")),
        };
        self.refresh_skills();
        cx.notify();
    }

    /// Reveals the profile's skills directory rather than editing or deleting
    /// inside it: a skill is a directory, and the user's file manager is the
    /// editor for directories.
    fn show_skills_folder(&mut self, cx: &mut Context<Self>) {
        let Some(profile) = self.profile.clone() else {
            self.skills_status = Some("No profile directory is available.".to_owned());
            cx.notify();
            return;
        };

        let path = profile.skills_dir();
        match std::fs::create_dir_all(&path) {
            Ok(()) => cx.open_url(&file_url(&path)),
            Err(error) => {
                self.skills_status = Some(format!("Could not create {}: {error}", path.display()));
                cx.notify();
            }
        }
    }

    /// One section per provider, laid out the way Zed's LLM Providers page does
    /// it: the name and a rule, then the key with its explanation on the left and
    /// the field on the right. A configured key collapses to a single row.
    fn providers_page(&self, theme: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        // The provider in use comes first, so its controls are where the page starts
        // and a longer list never pushes them below the fold.
        let mut presets: Vec<&'static Preset> = humibird_providers::PRESETS.iter().collect();
        presets.sort_by_key(|preset| preset.id != self.config.provider.id);
        let sections: Vec<AnyElement> = presets
            .into_iter()
            .map(|preset| self.provider_section(theme, preset, cx))
            .collect();

        div()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .pb(px(8.))
                    .child(page_heading(
                        theme,
                        "Providers",
                        "Keys belong to this profile and live in its .env.",
                    ))
                    .when(self.active_preset().is_none(), |this| {
                        this.child(note(
                            theme,
                            format!(
                                "`{}` is not a provider this build knows. Choose one below.",
                                self.config.provider.id
                            ),
                        ))
                    })
                    .children(self.status.as_deref().map(|status| {
                        div()
                            .debug_selector(|| "settings-status".into())
                            .text_size(px(11.))
                            .text_color(rgb(theme.text_muted))
                            .child(status.to_owned())
                    })),
            )
            .children(sections)
    }

    /// A provider's section: its name, how to switch to it, and the one control it
    /// needs. `custom` is the only preset that has to be told where to go.
    fn provider_section(
        &self,
        theme: Palette,
        preset: &'static Preset,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // Which provider is in use is decided in the composer's model picker, where
        // the model names the provider. This page holds the keys, and the endpoint
        // the person has to name for `custom`.
        let variable = preset.key_env;
        let key_state = variable
            .map(|variable| self.key_source_for(variable))
            .unwrap_or("none");
        // The environment and the profile's `.env` are two sources, not one state:
        // a key written here must be resettable even while the shell supplies one.
        let stored = variable.is_some_and(|variable| {
            self.profile.as_ref().is_some_and(|profile| {
                profile
                    .dotenv()
                    .get(variable)
                    .is_some_and(|value| !value.trim().is_empty())
            })
        });

        let header = div()
            .debug_selector(move || format!("provider-row-{}", preset.id))
            .flex()
            .items_center()
            .gap(px(10.))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(13.))
                    .text_color(rgb(theme.text_muted))
                    .child(preset.display_name),
            );

        let mut body = div().flex().flex_col().gap(px(8.));
        if preset.key_env.is_some() {
            body = body.child(key_block(
                theme,
                preset,
                key_state,
                stored,
                self.key_input_for(preset.id),
                cx,
            ));
        }
        if preset.id == CUSTOM {
            // The endpoint belongs to the provider, not to the profile's current
            // choice: it is filled in here whether or not it is the one in use.
            body = body.child(self.custom_block(theme, cx));
        } else if preset.key_env.is_none() {
            if let Some(input) = self.endpoint_input_for(preset.id) {
                body = body.child(endpoint_field(theme, preset, input));
            }
            body = body.child(note(
                theme,
                "Models on this machine. Naming the address here is what makes the composer ask it.",
            ));
        }

        div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .py(px(10.))
            .border_b_1()
            .border_color(rgb(theme.border))
            .child(header)
            .child(body)
            .into_any_element()
    }

    /// The address, model and window a custom endpoint has to name itself.
    fn custom_block(&self, theme: Palette, cx: &mut Context<Self>) -> AnyElement {
        let fields = div()
            .flex()
            .flex_col()
            .gap(px(12.))
            .child(
                div()
                    .flex()
                    .gap(px(12.))
                    .child(div().flex_1().min_w_0().child(text_field(
                        theme,
                        "settings-base-url",
                        "Endpoint",
                        self.base_url_input.clone(),
                    )))
                    .child(div().flex_1().min_w_0().child(text_field(
                        theme,
                        "settings-model",
                        "Model",
                        self.model_input.clone(),
                    ))),
            )
            .child(
                div()
                    .flex()
                    .gap(px(12.))
                    .child(div().flex_1().min_w_0().child(text_field(
                        theme,
                        "settings-context-window",
                        "Context window",
                        self.context_window_input.clone(),
                    ))),
            )
            .child(div().flex().child(save_button(theme, cx)));

        div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(fields)
            .into_any_element()
    }

    /// The context servers the profile names, and whether the window managed to
    /// connect each one. Read-only: the file is edited by hand, and a change needs a
    /// restart, so there is nothing here to type into.
    fn context_servers_page(&self, theme: Palette) -> impl IntoElement {
        let mut rows: Vec<AnyElement> = Vec::new();
        match &self.mcp_config {
            Err(error) => rows.push(note(theme, error.clone()).into_any_element()),
            Ok(config) if config.servers.is_empty() => rows.push(
                note(
                    theme,
                    "No context servers yet. Add them to mcp.json in the profile directory.",
                )
                .into_any_element(),
            ),
            Ok(config) => {
                for (name, server) in &config.servers {
                    rows.push(
                        server_row(theme, name, server, self.status_of(name)).into_any_element(),
                    );
                }
            }
        }

        let path = self
            .profile
            .as_ref()
            .map(|profile| profile.mcp_path().display().to_string())
            .unwrap_or_else(|| "mcp.json".to_owned());

        div()
            .flex()
            .flex_col()
            .gap(px(16.))
            .child(page_heading(
                theme,
                "Context servers",
                "Tools from MCP servers, added to every conversation. Every call asks for approval.",
            ))
            .child(
                div()
                    .debug_selector(|| "settings-mcp-card".into())
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .rounded_lg()
                    .border_1()
                    .border_color(rgb(theme.border))
                    .bg(rgb(theme.panel_bg))
                    .p(px(16.))
                    .children(rows),
            )
            .child(note(
                theme,
                format!("Configured in {path}. Editing it takes effect on the next launch."),
            ))
    }

    /// What became of one server: how many tools it offered, why it did not come
    /// up, or that it is still on its way.
    fn status_of(&self, name: &str) -> String {
        let Some(mcp) = &self.mcp else {
            return "Starting…".to_owned();
        };
        if let Some(failure) = mcp
            .failures()
            .iter()
            .find(|failure| failure.starts_with(&format!("{name}:")))
        {
            return failure.clone();
        }
        match mcp.connected().iter().find(|(server, _)| server == name) {
            Some((_, count)) => match count {
                1 => "1 tool".to_owned(),
                count => format!("{count} tools"),
            },
            None => "Not connected.".to_owned(),
        }
    }

    fn skills_page(&self, theme: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap(px(16.))
            .child(page_heading(
                theme,
                "Skills",
                "Procedures this profile can load. A change joins the next conversation, not one already running.",
            ))
            .child(self.installed_card(theme, cx))
            .child(self.import_card(theme, cx))
            .children(self.skills_status.as_deref().map(|status| {
                div()
                    .debug_selector(|| "settings-skills-status".into())
                    .text_size(px(11.))
                    .text_color(rgb(theme.text_muted))
                    .child(status.to_owned())
            }))
    }

    fn installed_card(&self, theme: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let show_folder = cx.listener(|this, _, _, cx| this.show_skills_folder(cx));

        div()
            .debug_selector(|| "skills-installed-card".into())
            .flex()
            .flex_col()
            .gap(px(8.))
            .rounded_lg()
            .border_1()
            .border_color(rgb(theme.border))
            .bg(rgb(theme.panel_bg))
            .p(px(16.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(13.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Installed"),
                    )
                    .when(self.profile.is_some(), |this| {
                        this.child(
                            small_button(theme, "skills-show-folder", "Show folder")
                                .on_click(show_folder),
                        )
                    }),
            )
            .children(self.skills.iter().map(|skill| skill_row(theme, skill)))
            .when(self.skills.is_empty(), |this| {
                this.child(note(
                    theme,
                    "No skills yet. Import one below, or add a directory to the skills folder.",
                ))
            })
    }

    fn import_card(&self, theme: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let hint = match &self.source {
            Some(source) => format!(
                "Copied from {}, which is never written to.",
                source.display()
            ),
            None => {
                "No home directory is available, so there is nothing to import from.".to_owned()
            }
        };
        let mut rows = Vec::with_capacity(self.importable.len());
        for skill in &self.importable {
            let name = skill.name.clone();
            let button = small_button(theme, &format!("skills-import-{}", skill.name), "Import")
                .on_click(cx.listener(move |this, _, _, cx| this.import_skill(&name, cx)));
            rows.push(import_row(theme, skill, button));
        }

        div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .rounded_lg()
            .border_1()
            .border_color(rgb(theme.border))
            .bg(rgb(theme.panel_bg))
            .p(px(16.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(3.))
                    .child(
                        div()
                            .text_size(px(13.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("From ~/.agents/skills"),
                    )
                    .child(note(theme, hint)),
            )
            .children(rows)
            .when(
                self.importable.is_empty() && self.source.is_some(),
                |this| this.child(note(theme, "Every skill there is already installed.")),
            )
    }
}

impl Render for SettingsWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Palette::for_appearance(self.appearance);
        let page = match self.page {
            SettingsPage::Providers => self.providers_page(theme, cx).into_any_element(),
            SettingsPage::ContextServers => self.context_servers_page(theme).into_any_element(),
            SettingsPage::Skills => self.skills_page(theme, cx).into_any_element(),
        };
        let page_scroll = self.page_scroll.clone();

        div()
            .id("settings")
            .debug_selector(|| "settings".into())
            .key_context("SettingsWindow")
            .flex()
            .size_full()
            .bg(rgb(theme.window_bg))
            .text_color(rgb(theme.text))
            .text_size(px(13.))
            .child(nav(theme, self.page, cx))
            .child(
                div()
                    .id("settings-page")
                    .debug_selector(|| "settings-page".into())
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .overflow_y_scroll()
                    .track_scroll(&page_scroll)
                    .px(px(24.))
                    .py(px(18.))
                    .child(
                        div()
                            .w_full()
                            .max_w(px(560.))
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(16.))
                            .child(page),
                    )
                    // Always visible: a page that ends below the fold has to say so.
                    .custom_scrollbars(
                        Scrollbars::always_visible(ScrollAxes::Vertical)
                            .tracked_scroll_handle(&page_scroll),
                        window,
                        cx,
                    ),
            )
    }
}

fn page_heading(theme: Palette, title: &'static str, subtitle: &'static str) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(px(3.))
        .child(
            div()
                .text_size(px(15.))
                .font_weight(FontWeight::SEMIBOLD)
                .child(title),
        )
        .child(
            div()
                .text_size(px(11.))
                .text_color(rgb(theme.text_muted))
                .child(subtitle),
        )
}

fn nav(
    theme: Palette,
    current: SettingsPage,
    cx: &mut Context<SettingsWindow>,
) -> impl IntoElement {
    let to_providers = cx.listener(|this, _, _, cx| this.set_page(SettingsPage::Providers, cx));
    let to_context_servers =
        cx.listener(|this, _, _, cx| this.set_page(SettingsPage::ContextServers, cx));
    let to_skills = cx.listener(|this, _, _, cx| this.set_page(SettingsPage::Skills, cx));

    div()
        .id("settings-nav")
        .debug_selector(|| "settings-nav".into())
        .flex_none()
        .w(px(200.))
        .h_full()
        .flex()
        .flex_col()
        .gap(px(2.))
        .bg(rgb(theme.sidebar_bg))
        .border_r_1()
        .border_color(rgb(theme.border))
        .pt(px(42.))
        .px(px(8.))
        .child(
            div()
                .px(px(6.))
                .pb(px(8.))
                .text_size(px(10.))
                .text_color(rgb(theme.text_dim))
                .child("Settings"),
        )
        .child(nav_item(theme, SettingsPage::Providers, current).on_click(to_providers))
        .child(nav_item(theme, SettingsPage::ContextServers, current).on_click(to_context_servers))
        .child(nav_item(theme, SettingsPage::Skills, current).on_click(to_skills))
}

fn nav_item(theme: Palette, page: SettingsPage, current: SettingsPage) -> Stateful<Div> {
    let item = div()
        .id(page.selector())
        .debug_selector(move || page.selector().to_owned())
        .px(px(7.))
        .py(px(5.))
        .rounded_md()
        .text_size(px(12.))
        .child(page.label());

    if page == current {
        item.bg(rgb(theme.selected_bg)).text_color(rgb(theme.text))
    } else {
        item.text_color(rgb(theme.text_secondary))
            .cursor_pointer()
            .hover(move |this| this.bg(rgb(theme.hover_bg)).text_color(rgb(theme.text)))
    }
}

/// One row of the provider list: what it is, whether it can be used, and how to
/// switch to it. Zed's settings list providers the same way, with the model list
/// below the one that is in use.
/// The API key block, the way Zed's provider settings lay it out: the explanation
/// on the left, the field or the configured row on the right.
fn key_block(
    theme: Palette,
    preset: &'static Preset,
    key_state: &str,
    stored: bool,
    input: Option<Entity<TextInput>>,
    cx: &mut Context<SettingsWindow>,
) -> impl IntoElement {
    let variable = preset.key_env.unwrap_or_default();

    let description = if stored {
        "The key in this profile's .env is the one in use; a variable in your shell does not override it."
            .to_owned()
    } else if key_state == "environment" {
        format!(
            "No key is stored here. {variable} is set in your environment and is used until you store one, which then wins."
        )
    } else {
        format!(
            "Set {variable}, or paste a key from the {} dashboard and press Enter.",
            preset.display_name
        )
    };

    let explanation = div()
        .flex()
        .flex_col()
        .gap(px(3.))
        .flex_1()
        .min_w_0()
        .child(
            div()
                .text_size(px(13.))
                .font_weight(FontWeight::SEMIBOLD)
                .child("API key"),
        )
        .child(
            div()
                .text_size(px(11.))
                .line_height(px(16.))
                .text_color(rgb(theme.text_muted))
                .child(description),
        );

    // A key in `.env` is this profile's to remove, whether or not the environment
    // also supplies one; the field only comes back when there is nothing stored.
    let control = if stored {
        configured_row(theme, preset, cx).into_any_element()
    } else {
        match input {
            Some(input) => key_field(theme, preset, input).into_any_element(),
            None => div().into_any_element(),
        }
    };

    div()
        .flex()
        .items_start()
        .gap(px(32.))
        .child(explanation)
        .child(div().w(px(320.)).flex_none().child(control))
}

fn configured_row(
    theme: Palette,
    preset: &'static Preset,
    cx: &mut Context<SettingsWindow>,
) -> impl IntoElement {
    let mut row = div()
        .debug_selector(move || format!("settings-configured-{}", preset.id))
        .flex()
        .items_center()
        .justify_between()
        .gap(px(12.))
        .w_full()
        .rounded_md()
        .border_1()
        .border_color(rgb(theme.border))
        .bg(rgb(theme.hover_bg))
        .px(px(12.))
        .py(px(10.))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(rgb(theme.success))
                        .child("✓"),
                )
                .child(div().text_size(px(13.)).child("API key configured")),
        );
    let variable = preset.key_env.unwrap_or_default().to_owned();
    let selector = format!("settings-reset-{}", preset.id);
    row = row.child(
        div()
            .id(SharedString::from(selector.clone()))
            .debug_selector(move || selector.clone())
            .text_size(px(12.))
            .text_color(rgb(theme.text_secondary))
            .cursor_pointer()
            .hover(move |this| this.text_color(rgb(theme.text)))
            .on_click(cx.listener(move |this, _, _, cx| this.remove_key(variable.clone(), cx)))
            .child("Reset key"),
    );
    row
}

fn key_field(
    theme: Palette,
    preset: &'static Preset,
    input: Entity<TextInput>,
) -> impl IntoElement {
    let selector = format!("settings-key-{}", preset.id);
    div()
        .id(SharedString::from(selector.clone()))
        .debug_selector(move || selector.clone())
        .rounded_md()
        .border_1()
        .border_color(rgb(theme.border_strong))
        .px(px(10.))
        .py(px(8.))
        .text_size(px(13.))
        .line_height(px(18.))
        .text_color(rgb(theme.text))
        .child(input)
}

fn endpoint_field(
    theme: Palette,
    preset: &'static Preset,
    input: Entity<TextInput>,
) -> impl IntoElement {
    let selector = format!("settings-endpoint-{}", preset.id);
    div()
        .flex()
        .flex_col()
        .gap(px(6.))
        .child(
            div()
                .text_size(px(11.))
                .text_color(rgb(theme.text_muted))
                .child("Endpoint"),
        )
        .child(
            div()
                .id(SharedString::from(selector.clone()))
                .debug_selector(move || selector.clone())
                .rounded_md()
                .border_1()
                .border_color(rgb(theme.border_strong))
                .px(px(10.))
                .py(px(8.))
                .text_size(px(13.))
                .line_height(px(18.))
                .text_color(rgb(theme.text))
                .child(input),
        )
}

fn text_field(
    theme: Palette,
    selector: &'static str,
    label: &'static str,
    input: Entity<TextInput>,
) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(px(6.))
        .child(
            div()
                .text_size(px(11.))
                .text_color(rgb(theme.text_muted))
                .child(label),
        )
        .child(
            div()
                .id(selector)
                .debug_selector(move || selector.to_owned())
                .rounded_md()
                .border_1()
                .border_color(rgb(theme.border_strong))
                .px(px(10.))
                .py(px(8.))
                .text_size(px(13.))
                .line_height(px(18.))
                .text_color(rgb(theme.text))
                .child(input),
        )
}

fn save_button(theme: Palette, cx: &mut Context<SettingsWindow>) -> impl IntoElement {
    div()
        .id("settings-save")
        .debug_selector(|| "settings-save".into())
        .px(px(10.))
        .py(px(5.))
        .rounded_md()
        .bg(rgb(theme.accent))
        .text_size(px(11.))
        .font_weight(FontWeight::MEDIUM)
        .text_color(rgb(theme.text_on_accent))
        .cursor_pointer()
        .hover(move |this| this.opacity(0.88))
        .on_click(cx.listener(|this, _, _, cx| this.save_provider(cx)))
        .child("Save")
}

/// The empty state: nothing is configured, so the key has to be typed in.
/// What this profile has installed and what an import would add.
fn skills_snapshot(profile: Option<&Profile>, source: Option<&Path>) -> (Vec<Skill>, Vec<Skill>) {
    let Some(profile) = profile else {
        return (Vec::new(), Vec::new());
    };

    let installed = SkillIndex::load(&profile.skills_dir());
    let importable = source
        .map(|source| humibird_skills::importable(source, &installed))
        .unwrap_or_default();
    (installed.skills().to_vec(), importable)
}

/// A `file://` URL the platform can open. The path needs encoding: a home
/// directory can hold a space, and a profile name may hold anything.
fn file_url(path: &Path) -> String {
    let mut url = String::from("file://");
    for byte in path.to_string_lossy().bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                url.push(byte as char);
            }
            _ => url.push_str(&format!("%{byte:02X}")),
        }
    }
    url
}

fn skill_row(theme: Palette, skill: &Skill) -> impl IntoElement {
    let id = format!("skills-installed-{}", skill.name);
    let selector = id.clone();
    let description = format!("skills-description-{}", skill.name);

    div()
        .id(SharedString::from(id))
        .debug_selector(move || selector)
        .flex()
        .flex_col()
        .gap(px(2.))
        .px(px(10.))
        .py(px(8.))
        .rounded_md()
        .border_1()
        .border_color(rgb(theme.border))
        .child(
            div()
                .text_size(px(12.))
                .font_weight(FontWeight::MEDIUM)
                .child(skill.name.clone()),
        )
        .child(
            div()
                .debug_selector(move || description.clone())
                .text_size(px(11.))
                .text_color(rgb(theme.text_muted))
                .child(skill.description.clone()),
        )
}

fn import_row(theme: Palette, skill: &Skill, button: impl IntoElement) -> impl IntoElement {
    let description = format!("skills-description-{}", skill.name);

    div()
        .flex()
        .items_start()
        .justify_between()
        .gap(px(12.))
        .px(px(10.))
        .py(px(8.))
        .rounded_md()
        .border_1()
        .border_color(rgb(theme.border))
        .child(
            // The column has to be told to shrink: a flex item's minimum width is
            // its longest line otherwise, and a 200-character description would
            // run out of the card instead of wrapping inside it.
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .gap(px(2.))
                .child(
                    div()
                        .text_size(px(12.))
                        .font_weight(FontWeight::MEDIUM)
                        .child(skill.name.clone()),
                )
                .child(
                    div()
                        .debug_selector(move || description.clone())
                        .text_size(px(11.))
                        .text_color(rgb(theme.text_muted))
                        .child(skill.description.clone()),
                ),
        )
        .child(button)
}

fn small_button(theme: Palette, id: &str, label: &str) -> Stateful<Div> {
    let selector = id.to_owned();
    let label = label.to_owned();

    div()
        .id(SharedString::from(id.to_owned()))
        .debug_selector(move || selector)
        .flex_none()
        .px(px(10.))
        .py(px(5.))
        .rounded_md()
        .border_1()
        .border_color(rgb(theme.border))
        .text_size(px(11.))
        .text_color(rgb(theme.text_secondary))
        .cursor_pointer()
        .hover(move |this| this.bg(rgb(theme.hover_bg)).text_color(rgb(theme.text)))
        .child(label)
}

/// One configured server: what it is, and what became of it.
fn server_row(theme: Palette, name: &str, server: &McpServer, status: String) -> impl IntoElement {
    let container = format!("settings-mcp-server-{name}");
    let status_selector = format!("settings-mcp-status-{name}");
    let where_it_is = match &server.transport {
        McpTransport::Stdio { command, args, .. } => {
            let mut line = command.clone();
            for argument in args {
                line.push(' ');
                line.push_str(argument);
            }
            line
        }
        McpTransport::Http { url, .. } => url.clone(),
    };
    let trouble = !server.enabled || !status.contains("tool");

    div()
        .id(container.clone())
        .debug_selector(move || container.clone())
        .flex()
        .items_center()
        .justify_between()
        .gap(px(12.))
        .py(px(4.))
        .child(
            div()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(2.))
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_size(px(12.))
                        .text_color(rgb(theme.text))
                        .child(name.to_owned()),
                )
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_size(px(10.5))
                        .text_color(rgb(theme.text_muted))
                        .child(where_it_is),
                ),
        )
        .child(
            div()
                .debug_selector(move || status_selector.clone())
                .flex_none()
                .text_size(px(10.5))
                .text_color(rgb(if trouble {
                    theme.warning
                } else {
                    theme.text_secondary
                }))
                .child(if server.enabled {
                    status
                } else {
                    "Switched off".to_owned()
                }),
        )
}

fn note(theme: Palette, text: impl Into<String>) -> impl IntoElement {
    div()
        .text_size(px(11.))
        .line_height(px(16.))
        .text_color(rgb(theme.text_dim))
        .child(text.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_becomes_an_openable_file_url() {
        assert_eq!(
            file_url(Path::new("/Users/me/My Projects/skills")),
            "file:///Users/me/My%20Projects/skills"
        );
        assert_eq!(file_url(Path::new("/tmp/a#b")), "file:///tmp/a%23b");
    }
}
