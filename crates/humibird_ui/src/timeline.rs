use std::sync::Arc;

use gpui::{
    AnyElement, Context, Entity, FontWeight, Hsla, IntoElement, ParentElement, ScrollHandle,
    SharedString, Styled, div, linear_color_stop, linear_gradient, prelude::*, px, rgb,
};
use humibird_agent::{Answer, Decision, StepState, ToolStep};
use humibird_providers::Attachment;
use humibird_tools::{CallKind, CallLabel, Tool};
use markdown::MarkdownElement;
use ui::{
    Color, Disclosure, Icon, IconName, IconSize, LabelCommon as _, LabelSize, SpinnerLabel,
    VisibleOnHover,
};

use crate::{
    HumibirdRoot, Palette, PendingApproval, PendingQuestion, Subagent, TextInput, Thinking, Turn,
    TurnState, markdown_style,
};

/// How tall a streaming stretch may get before the preview clips it. Zed previews
/// reasoning the same way: a bounded block that shows the newest lines instead of
/// pushing the conversation around.
const THINKING_PREVIEW_HEIGHT: f32 = 256.0;

pub fn render(
    theme: Palette,
    turns: &[Turn],
    notice: Notice<'_>,
    scroll_handle: &ScrollHandle,
    tools: &[Arc<dyn Tool>],
    subagents: &[Subagent],
    cx: &mut Context<HumibirdRoot>,
) -> impl IntoElement {
    let summarized = notice.summarized;
    let mut views = Vec::new();
    for (index, turn) in turns.iter().enumerate() {
        views.push(turn_view(theme, turn, index, tools, subagents, cx));
    }

    div()
        .id("timeline")
        .debug_selector(|| "timeline".into())
        .flex_1()
        .min_h_0()
        .overflow_y_scroll()
        .track_scroll(scroll_handle)
        .px(px(20.))
        .py(px(18.))
        .on_scroll(cx.listener(|this, _: &gpui::ScrollEvent, _, cx| this.scrolled(cx)))
        .child(
            div()
                .w_full()
                .max_w(px(720.))
                .min_w_0()
                .mx_auto()
                .flex()
                .flex_col()
                .gap(px(22.))
                .when(turns.is_empty(), |this| {
                    this.child(empty_state(theme, notice.without_provider, cx))
                })
                .when(summarized > 0, |this| {
                    this.child(
                        div()
                            .debug_selector(|| "summarized".into())
                            .text_size(px(11.))
                            .text_color(rgb(theme.text_dim))
                            .child(format!(
                                "The first {summarized} turn(s) are summarized for the model; the \
                                 full history stays here and in the transcript."
                            )),
                    )
                })
                .children(views),
        )
}

/// What the page says around the turns: how much of the history a summary stands
/// in for, and why nothing can be sent yet.
#[derive(Clone, Copy)]
pub struct Notice<'a> {
    pub summarized: usize,
    /// The reason the window cannot send, when it cannot: without a provider the
    /// empty page is where the key is asked for (Zed does the same by listing no
    /// models for a provider that is not authenticated).
    pub without_provider: Option<&'a str>,
}

fn empty_state(
    theme: Palette,
    without_provider: Option<&str>,
    cx: &mut Context<HumibirdRoot>,
) -> impl IntoElement {
    let setup = without_provider;

    div()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(10.))
        .py(px(96.))
        .when(setup.is_none(), |this| {
            this.child(
                div()
                    .debug_selector(|| "empty-ready".into())
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(6.))
                    .child(
                        div()
                            .text_size(px(15.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(theme.text))
                            .child("Start a conversation"),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(theme.text_muted))
                            .child(
                                "Replies stream here. Every thread keeps its own history and \
                                 its own workspace.",
                            ),
                    ),
            )
        })
        .when_some(setup, |this, reason| {
            this.child(
                div()
                    .debug_selector(|| "setup-key".into())
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(10.))
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(theme.text_muted))
                            .child(reason.to_owned()),
                    )
                    .child(
                        div()
                            .id("setup-open-settings")
                            .debug_selector(|| "setup-open-settings".into())
                            .px(px(10.))
                            .py(px(4.))
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(theme.border_strong))
                            .text_size(px(11.))
                            .text_color(rgb(theme.text_secondary))
                            .cursor_pointer()
                            .hover(move |this| {
                                this.bg(rgb(theme.hover_bg)).text_color(rgb(theme.text))
                            })
                            .on_click(cx.listener(|this, _, _, cx| this.open_settings(cx)))
                            .child("Open Settings"),
                    ),
            )
        })
}

fn turn_view(
    theme: Palette,
    turn: &Turn,
    index: usize,
    tools: &[Arc<dyn Tool>],
    subagents: &[Subagent],
    cx: &mut Context<HumibirdRoot>,
) -> impl IntoElement + 'static {
    let notice = match &turn.state {
        TurnState::Failed(message) => {
            Some((message.clone(), theme.warning_border, theme.warning_soft))
        }
        TurnState::Stopped => Some(("Stopped.".to_owned(), theme.border, theme.hover_bg)),
        TurnState::Streaming | TurnState::Done => None,
    };

    div()
        .flex()
        .flex_col()
        .gap(px(10.))
        .child(user_bubble(theme, &turn.user, &turn.attachments))
        .when(
            matches!(turn.state, TurnState::Streaming)
                && turn.assistant_text.is_empty()
                && turn.thinking.is_empty(),
            |this| {
                this.child(
                    div()
                        .debug_selector(|| "waiting".into())
                        .text_size(px(11.))
                        .text_color(rgb(theme.text_dim))
                        .child("Waiting for the model…"),
                )
            },
        )
        .child(work(theme, turn, index, tools, subagents, cx))
        .child(div().min_w_0().flex_shrink_0().child(MarkdownElement::new(
            turn.assistant.clone(),
            markdown_style::style(theme),
        )))
        .when_some(notice, |this, (message, border, background)| {
            this.child(
                div()
                    .debug_selector(|| "turn-notice".into())
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(border))
                    .bg(rgb(background))
                    .p(px(10.))
                    .text_size(px(11.5))
                    .line_height(px(17.))
                    .text_color(rgb(theme.text_secondary))
                    .child(message),
            )
        })
}

/// What the turn did, in the order it did it: a stretch of thinking, then the
/// calls that followed it, then the next stretch. The prompt a call is waiting
/// for comes last, because it is the thing the user has to act on.
fn work(
    theme: Palette,
    turn: &Turn,
    turn_index: usize,
    tools: &[Arc<dyn Tool>],
    subagents: &[Subagent],
    cx: &mut Context<HumibirdRoot>,
) -> impl IntoElement + 'static {
    let mut blocks: Vec<AnyElement> = Vec::new();
    for step_index in 0..=turn.tool_steps.len() {
        for (chunk_index, chunk) in turn.thinking.iter().enumerate() {
            if chunk.after_steps == step_index {
                blocks.push(
                    thinking_block(theme, chunk, turn_index, chunk_index, cx).into_any_element(),
                );
            }
        }
        if let Some(step) = turn.tool_steps.get(step_index) {
            // The card of a task this step delegated, if it did.
            let subagent = subagents
                .iter()
                .find(|subagent| subagent.turn == turn_index && subagent.step == step_index);
            blocks.push(
                step_row(
                    theme,
                    step,
                    StepCard {
                        turn: turn_index,
                        index: step_index,
                        open: turn.expanded_steps.contains(&step_index),
                        subagent,
                    },
                    tools,
                    cx,
                )
                .into_any_element(),
            );
        }
    }

    div().flex().flex_col().gap(px(6.)).children(blocks)
}

/// One call: a line the user can open to see what it was given and what it
/// returned. The icon and the wording come from the tool's kind and label, so the
/// window never matches on tool names.
/// Where a step sits in the timeline, and what else belongs to it.
struct StepCard<'a> {
    turn: usize,
    index: usize,
    open: bool,
    /// The task this step delegated, when it delegated one.
    subagent: Option<&'a Subagent>,
}

fn step_row(
    theme: Palette,
    step: &ToolStep,
    card: StepCard<'_>,
    tools: &[Arc<dyn Tool>],
    cx: &mut Context<HumibirdRoot>,
) -> impl IntoElement + 'static {
    let StepCard {
        turn,
        index,
        open,
        subagent,
    } = card;
    let (kind, label) = describe(tools, step);
    let failed = matches!(step.state, StepState::Done { failed: true });
    let color = match step.state {
        StepState::Awaiting => theme.warning,
        StepState::Running => theme.text_muted,
        StepState::Done { failed: false } => theme.text_muted,
        StepState::Done { failed: true } => theme.warning,
    };
    let selector = format!("tool-step-{turn}-{index}");
    let group = SharedString::from(format!("tool-step-group-{turn}-{index}"));
    let disclosure_group = group.clone();
    // A call that changes something gets a card; a read stays a line, so the two
    // cannot be mistaken for each other. A delegated task is a card too:
    // its body is a whole other agent's work.
    let card = subagent.is_some() || changes_something(kind);
    let card_selector = format!("tool-card-{turn}-{index}");
    // A subagent that is still working spins where the kind icon sits, so the card
    // says "alive" without being opened.
    let working =
        subagent.is_some_and(|subagent| matches!(subagent.work.state, TurnState::Streaming));

    let header = div()
        .id(SharedString::from(selector.clone()))
        .debug_selector(move || selector.clone())
        .group(group)
        .flex()
        .items_center()
        .gap(px(6.))
        .py(px(2.))
        .text_size(px(12.))
        .line_height(px(18.))
        .text_color(rgb(color))
        .cursor_pointer()
        .rounded_sm()
        .hover(move |this| this.bg(rgb(theme.hover_bg)).text_color(rgb(theme.text)))
        .on_click(cx.listener(move |this, _, _, cx| this.toggle_step(turn, index, cx)))
        .when(card, |this| this.px(px(8.)))
        // The column keeps its width either way, so the label does not jump when a
        // subagent stops spinning.
        .child(div().w(px(14.)).flex().justify_center().child(if working {
            SpinnerLabel::new()
                .size(LabelSize::XSmall)
                .into_any_element()
        } else {
            Icon::new(icon_for(kind))
                .size(IconSize::XSmall)
                .color(Color::Muted)
                .into_any_element()
        }))
        .child(label_element(theme, step, label))
        .child(div().flex_1())
        .when(failed, |this| {
            this.child(
                Icon::new(IconName::XCircle)
                    .size(IconSize::XSmall)
                    .color(Color::Error),
            )
        })
        .child(
            Disclosure::new(
                (
                    SharedString::from(format!("tool-step-disclosure-{turn}")),
                    index,
                ),
                open,
            )
            .opened_icon(IconName::ChevronDown)
            .closed_icon(IconName::ChevronRight)
            .visible_on_hover(disclosure_group)
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_step(turn, index, cx))),
        );

    div()
        .flex()
        .flex_col()
        .gap(px(6.))
        .when(card, |this| {
            this.debug_selector(move || card_selector.clone())
                .my(px(4.))
                .rounded_md()
                .border_1()
                .border_color(rgb(theme.border))
                .bg(rgb(theme.panel_bg))
                .overflow_hidden()
                .when(failed, |this| this.border_dashed())
        })
        .child(header)
        .when(open, |this| {
            this.child(
                div()
                    .when(card, |this| this.px(px(8.)).pb(px(6.)))
                    .child(match subagent {
                        Some(subagent) => subagent_body(theme, subagent, turn, index, tools, cx)
                            .into_any_element(),
                        None => step_detail(theme, step, failed).into_any_element(),
                    }),
            )
        })
}

/// What a subagent has done, in the card of the step that delegated it: the task it
/// was given, the calls it made, and its report. The parent's model never
/// sees any of this — only the report — so this is the person's view of the work.
fn subagent_body(
    theme: Palette,
    subagent: &Subagent,
    turn: usize,
    step: usize,
    tools: &[Arc<dyn Tool>],
    cx: &mut Context<HumibirdRoot>,
) -> impl IntoElement + 'static {
    let work = &subagent.work;
    let running = matches!(work.state, TurnState::Streaming);
    let notice = match &work.state {
        TurnState::Failed(message) => Some(message.clone()),
        TurnState::Stopped => Some("Stopped.".to_owned()),
        TurnState::Streaming | TurnState::Done => None,
    };
    let stop = format!("stop-subagent-{turn}-{step}");
    let body = format!("subagent-body-{turn}-{step}");
    let report = format!("subagent-report-{turn}-{step}");
    let mut calls = Vec::new();
    for (index, call) in work.tool_steps.iter().enumerate() {
        let (kind, label) = describe(tools, call);
        let failed = matches!(call.state, StepState::Done { failed: true });
        let line = format!("subagent-call-{turn}-{step}-{index}");
        calls.push(
            div()
                .id(SharedString::from(line.clone()))
                .debug_selector(move || line.clone())
                .flex()
                .items_center()
                .gap(px(6.))
                .min_w_0()
                .text_size(px(11.5))
                .line_height(px(17.))
                .text_color(rgb(theme.text_dim))
                .child(
                    Icon::new(icon_for(kind))
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                )
                .child(label_element(theme, call, label))
                .when(failed, |this| {
                    this.child(
                        Icon::new(IconName::XCircle)
                            .size(IconSize::XSmall)
                            .color(Color::Error),
                    )
                }),
        );
    }

    div()
        .id(SharedString::from(body.clone()))
        .debug_selector(move || body.clone())
        .flex()
        .flex_col()
        .gap(px(8.))
        .child(
            div()
                .text_size(px(11.5))
                .line_height(px(17.))
                .text_color(rgb(theme.text_dim))
                .child(work.user.clone()),
        )
        .when(!calls.is_empty(), |this| {
            this.child(div().flex().flex_col().gap(px(2.)).children(calls))
        })
        .when(!work.assistant_text.is_empty(), |this| {
            this.child(
                div()
                    .id(SharedString::from(report.clone()))
                    .debug_selector(move || report.clone())
                    .min_w_0()
                    .text_size(px(11.5))
                    .line_height(px(17.))
                    .child(MarkdownElement::new(
                        work.assistant.clone(),
                        markdown_style::style(theme),
                    )),
            )
        })
        .when_some(notice, |this, message| {
            this.child(
                div()
                    .debug_selector(|| "subagent-notice".into())
                    .text_size(px(11.))
                    .line_height(px(17.))
                    .text_color(rgb(theme.warning))
                    .child(message),
            )
        })
        .when(running, |this| {
            this.child(
                div().flex().items_center().gap(px(6.)).child(
                    div()
                        .id(SharedString::from(stop.clone()))
                        .debug_selector(move || stop.clone())
                        .cursor_pointer()
                        .rounded_sm()
                        .px(px(6.))
                        .text_size(px(11.))
                        .line_height(px(16.))
                        .text_color(rgb(theme.warning))
                        .hover(move |this| this.bg(rgb(theme.hover_bg)))
                        .child("Stop subagent")
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.stop_subagent(turn, step, cx)),
                        ),
                ),
            )
        })
}

/// One stretch of thinking, where it happened: open while the model thinks, and
/// folded away when it acts. A streaming stretch is previewed — clipped to its
/// newest lines so it cannot push the conversation around; once it ends, opening
/// it shows the whole thought.
fn thinking_block(
    theme: Palette,
    chunk: &Thinking,
    turn: usize,
    index: usize,
    cx: &mut Context<HumibirdRoot>,
) -> impl IntoElement + 'static {
    let label = if chunk.streaming {
        "Thinking…"
    } else {
        "Thought process"
    };
    let selector = format!("thinking-{turn}-{index}");
    let body = format!("thinking-body-{turn}-{index}");
    let scroll = chunk.scroll.clone();
    let group = SharedString::from(format!("thinking-group-{turn}-{index}"));
    let disclosure_group = group.clone();
    // A stretch that is still streaming shows its newest lines: the preview is
    // clipped rather than scrollable, so the wheel over it moves the conversation
    // instead of fighting the stream for the same offset. The whole thought is one
    // click away.
    let preview = chunk.expanded && chunk.streaming;
    if preview {
        scroll.scroll_to_bottom();
    }

    div()
        .flex()
        .flex_col()
        .gap(px(6.))
        .child(
            div()
                .id(SharedString::from(selector.clone()))
                .debug_selector(move || selector.clone())
                .group(group)
                .flex()
                .items_center()
                .gap(px(6.))
                .py(px(2.))
                .text_size(px(12.))
                .line_height(px(18.))
                .text_color(rgb(theme.text_muted))
                .cursor_pointer()
                .rounded_sm()
                .hover(move |this| this.bg(rgb(theme.hover_bg)).text_color(rgb(theme.text)))
                .on_click(cx.listener(move |this, _, _, cx| this.toggle_thinking(turn, index, cx)))
                .child(
                    Icon::new(IconName::ToolThink)
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                )
                .child(div().child(label))
                .child(div().flex_1())
                .child(
                    Disclosure::new(
                        (
                            SharedString::from(format!("thinking-disclosure-{turn}")),
                            index,
                        ),
                        chunk.expanded,
                    )
                    .opened_icon(IconName::ChevronDown)
                    .closed_icon(IconName::ChevronRight)
                    .visible_on_hover(disclosure_group)
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.toggle_thinking(turn, index, cx)),
                    ),
                ),
        )
        .when(chunk.expanded, |this| {
            this.child(
                div()
                    .relative()
                    .child(
                        div()
                            .id(SharedString::from(body.clone()))
                            .debug_selector(move || body.clone())
                            .when(preview, |this| {
                                this.max_h(px(THINKING_PREVIEW_HEIGHT))
                                    .track_scroll(&scroll)
                            })
                            .overflow_hidden()
                            .border_l_2()
                            .border_color(rgb(theme.border))
                            .pl(px(10.))
                            .text_size(px(11.5))
                            .line_height(px(17.))
                            .text_color(rgb(theme.text_dim))
                            .child(chunk.text.clone()),
                    )
                    .when(preview, |this| {
                        // The newest line sits at the bottom, so the cut is at the
                        // top: fade it rather than end it with a hard edge, the way
                        // Zed's constrained thinking block does.
                        this.child(div().absolute().inset_0().bg(linear_gradient(
                            180.,
                            linear_color_stop(Hsla::from(rgb(theme.window_bg)).opacity(0.9), 0.),
                            linear_color_stop(Hsla::from(rgb(theme.window_bg)).opacity(0.), 0.12),
                        )))
                    }),
            )
        })
}

/// What the window knows about a call without matching on tool names: the kind
/// (icon, card) and the label (verb and target).
fn describe(tools: &[Arc<dyn Tool>], step: &ToolStep) -> (CallKind, Option<CallLabel>) {
    let Some(tool) = tools.iter().find(|tool| tool.name() == step.name) else {
        return (CallKind::Read, None);
    };
    let label = serde_json::from_str(&step.arguments)
        .ok()
        .and_then(|input| tool.label(&input));
    (tool.kind(), label)
}

/// Whether a call can leave the workspace different from how it found it. Those
/// get card chrome in the timeline; reads, searches and questions stay rows.
fn changes_something(kind: CallKind) -> bool {
    matches!(
        kind,
        CallKind::Write
            | CallKind::Edit
            | CallKind::Delete
            | CallKind::Move
            | CallKind::Execute
            // A context server's tool can do whatever its server can, which is
            // why it also asks before it runs.
            | CallKind::Mcp
    )
}

fn icon_for(kind: CallKind) -> IconName {
    match kind {
        CallKind::Read => IconName::ToolSearch,
        CallKind::List => IconName::Folder,
        CallKind::Search => IconName::ToolSearch,
        CallKind::Write | CallKind::Edit => IconName::ToolPencil,
        CallKind::Delete => IconName::ToolDeleteFile,
        CallKind::Move => IconName::ArrowRightLeft,
        CallKind::Execute => IconName::ToolTerminal,
        CallKind::Fetch => IconName::ToolWeb,
        CallKind::Remember => IconName::Book,
        CallKind::Skill => IconName::Book,
        CallKind::Ask => IconName::Chat,
        CallKind::Delegate => IconName::Sparkle,
        CallKind::Mcp => IconName::Server,
    }
}

/// The verb, and the one thing it acted on as a code chip. A tool without a label
/// falls back to its name and the argument summary the step was built with.
fn label_element(theme: Palette, step: &ToolStep, label: Option<CallLabel>) -> AnyElement {
    let Some(label) = label else {
        let text = if step.summary.is_empty() {
            step.name.clone()
        } else {
            format!("{} {}", step.name, step.summary)
        };
        return div().min_w_0().truncate().child(text).into_any_element();
    };

    div()
        .flex()
        .items_center()
        .gap(px(6.))
        .min_w_0()
        .child(div().flex_none().child(label.verb))
        .when_some(label.target, |this, target| {
            this.child(
                div()
                    .min_w_0()
                    .truncate()
                    .px(px(4.))
                    .rounded_sm()
                    .bg(rgb(theme.code_bg))
                    .font_family("Menlo")
                    .text_size(px(11.))
                    .text_color(rgb(theme.text_secondary))
                    .child(target),
            )
        })
        .into_any_element()
}

/// What a call was given and what it returned. Long results are cut: the whole
/// transcript is in the store, and a step is a summary, not a log viewer.
fn step_detail(theme: Palette, step: &ToolStep, failed: bool) -> impl IntoElement + 'static {
    let border = if failed {
        theme.warning_border
    } else {
        theme.border
    };

    div()
        .debug_selector(|| "step-detail".into())
        .flex()
        .flex_col()
        .gap(px(8.))
        .pl(px(10.))
        .border_l_2()
        .border_color(rgb(border))
        .child(detail_block(
            theme,
            "Arguments",
            &display(&pretty_json(&step.arguments)),
            theme.text_secondary,
        ))
        .when(!step.result.is_empty(), |this| {
            let color = if failed {
                theme.warning
            } else {
                theme.text_secondary
            };
            this.child(detail_block(
                theme,
                "Result",
                &display(&shorten(&step.result)),
                color,
            ))
        })
}

/// One labelled block of detail. Monospaced: it is output, not prose.
fn detail_block(theme: Palette, label: &'static str, text: &str, color: u32) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(px(2.))
        .child(
            div()
                .text_size(px(10.))
                .text_color(rgb(theme.text_dim))
                .child(label),
        )
        .child(
            div()
                .font_family("Menlo")
                .text_size(px(11.))
                .line_height(px(16.))
                .text_color(rgb(color))
                .child(text.to_owned()),
        )
}

/// Tool output is written for the model, so it uses tabs (`read_file` aligns its
/// line numbers with them). The window shows spaces, which lay out predictably.
fn display(text: &str) -> String {
    text.replace('\t', "    ")
}

/// JSON arguments get one key per line; anything that is not JSON is shown as it
/// arrived.
fn pretty_json(arguments: &str) -> String {
    serde_json::from_str::<serde_json::Value>(arguments)
        .ok()
        .and_then(|value| serde_json::to_string_pretty(&value).ok())
        .unwrap_or_else(|| arguments.to_owned())
}

fn shorten(text: &str) -> String {
    const MAX: usize = 2_000;

    if text.chars().count() <= MAX {
        return text.to_owned();
    }
    let mut short: String = text.chars().take(MAX).collect();
    short.push_str("\n… (cut; the whole result is in the transcript)");
    short
}

/// The prompt the turn is blocked on. It is rendered above the composer rather
/// than inside the timeline, so neither a long answer nor a long stretch of
/// thinking can scroll it out of reach.
pub fn prompt(
    theme: Palette,
    approval: Option<&PendingApproval>,
    question: Option<&PendingQuestion>,
    cx: &mut Context<HumibirdRoot>,
) -> Option<AnyElement> {
    if let Some(approval) = approval {
        return Some(approval_controls(theme, approval, cx).into_any_element());
    }

    question.map(|question| question_card(theme, question, cx).into_any_element())
}

fn approval_controls(
    theme: Palette,
    pending: &PendingApproval,
    cx: &mut Context<HumibirdRoot>,
) -> impl IntoElement + 'static {
    let label = if pending.summary.is_empty() {
        pending.name.clone()
    } else {
        format!("{} {}", pending.name, pending.summary)
    };
    let button = |id: &'static str, text: &'static str, primary: bool| {
        div()
            .id(id)
            .debug_selector(move || id.to_owned())
            .px(px(10.))
            .py(px(5.))
            .rounded_md()
            .when(primary, |this| {
                this.bg(rgb(theme.accent))
                    .text_color(rgb(theme.text_on_accent))
            })
            .when(!primary, |this| {
                this.border_1()
                    .border_color(rgb(theme.border))
                    .text_color(rgb(theme.text_secondary))
            })
            .text_size(px(11.))
            .cursor_pointer()
            .hover(move |this| this.opacity(0.88))
            .child(text)
    };

    div()
        .debug_selector(|| "approval".into())
        .flex()
        .flex_col()
        .gap(px(8.))
        .rounded_lg()
        .border_1()
        .border_color(rgb(theme.warning_border))
        .bg(rgb(theme.warning_soft))
        .p(px(12.))
        .child(
            div()
                .text_size(px(12.))
                .text_color(rgb(theme.text))
                .child(format!("Allow `{label}`?")),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .child(button("approve-once", "Allow once", true).on_click(
                    cx.listener(|this, _, _, cx| this.answer_approval(Decision::Once, cx)),
                ))
                .child(
                    button("approve-always", "Always this session", false).on_click(
                        cx.listener(|this, _, _, cx| this.answer_approval(Decision::Always, cx)),
                    ),
                )
                .child(button("deny-call", "Deny", false).on_click(
                    cx.listener(|this, _, _, cx| this.answer_approval(Decision::Deny, cx)),
                )),
        )
}

/// The model's question, asked where the call sits: one button per option, an
/// optional field for an answer of the user's own, and a way out that tells the
/// model nobody answered.
fn question_card(
    theme: Palette,
    pending: &PendingQuestion,
    cx: &mut Context<HumibirdRoot>,
) -> impl IntoElement + 'static {
    let mut options = Vec::with_capacity(pending.options.len());
    for (index, option) in pending.options.iter().enumerate() {
        let label = option.clone();
        let answer = label.clone();
        options.push(
            div()
                .id(SharedString::from(format!("answer-option-{index}")))
                .debug_selector(move || format!("answer-option-{index}"))
                .px(px(10.))
                .py(px(5.))
                .rounded_md()
                .border_1()
                .border_color(rgb(theme.border_strong))
                .bg(rgb(theme.panel_bg))
                .text_size(px(11.5))
                .text_color(rgb(theme.text))
                .cursor_pointer()
                .hover(move |this| this.bg(rgb(theme.hover_bg)))
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.answer_question(Answer::Selected(answer.clone()), cx)
                })),
        );
    }

    let input: Entity<TextInput> = pending.input.clone();

    div()
        .debug_selector(|| "question".into())
        .flex()
        .flex_col()
        .gap(px(8.))
        .rounded_lg()
        .border_1()
        .border_color(rgb(theme.border_strong))
        .bg(rgb(theme.hover_bg))
        .p(px(12.))
        .child(
            div()
                .text_size(px(12.5))
                .line_height(px(18.))
                .text_color(rgb(theme.text))
                .child(pending.question.clone()),
        )
        .child(div().flex().flex_wrap().gap(px(6.)).children(options))
        .when(pending.allow_free_text, |this| {
            this.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .id("answer-input")
                            .debug_selector(|| "answer-input".into())
                            .flex_1()
                            .min_w_0()
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(theme.border_strong))
                            .bg(rgb(theme.window_bg))
                            .px(px(8.))
                            .py(px(5.))
                            .text_size(px(12.))
                            .line_height(px(18.))
                            .child(input),
                    )
                    .child(
                        div()
                            .id("answer-submit")
                            .debug_selector(|| "answer-submit".into())
                            .px(px(10.))
                            .py(px(5.))
                            .rounded_md()
                            .bg(rgb(theme.accent))
                            .text_size(px(11.))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(rgb(theme.text_on_accent))
                            .cursor_pointer()
                            .hover(move |this| this.opacity(0.88))
                            .on_click(cx.listener(|this, _, _, cx| this.submit_answer(cx)))
                            .child("Answer"),
                    ),
            )
        })
        .child(
            div()
                .id("answer-decline")
                .debug_selector(|| "answer-decline".into())
                .text_size(px(11.))
                .text_color(rgb(theme.text_muted))
                .cursor_pointer()
                .hover(move |this| this.text_color(rgb(theme.text)))
                .on_click(cx.listener(|this, _, _, cx| this.answer_question(Answer::Declined, cx)))
                .child("Skip"),
        )
}

fn user_bubble(theme: Palette, text: &str, attachments: &[Attachment]) -> impl IntoElement {
    div().flex().flex_col().items_end().child(
        div()
            .max_w(px(560.))
            .bg(rgb(theme.user_bubble))
            .rounded_lg()
            .px(px(14.))
            .py(px(10.))
            .text_size(px(13.))
            .line_height(px(20.))
            .child(text.to_owned())
            .when(!attachments.is_empty(), |this| {
                this.child(
                    div()
                        .debug_selector(|| "turn-attachments".into())
                        .mt(px(6.))
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .text_size(px(10.5))
                        .text_color(rgb(theme.text_muted))
                        .children(attachments.iter().map(|attachment| {
                            let kind = if attachment.is_image() {
                                "image"
                            } else {
                                "file"
                            };
                            div().child(format!("{kind} · {}", attachment.name()))
                        })),
                )
            }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::AssetSource as _;

    #[test]
    fn a_context_server_call_reads_as_a_change() {
        assert!(changes_something(CallKind::Mcp));
        assert_eq!(icon_for(CallKind::Mcp), IconName::Server);

        for read_only in [
            CallKind::Read,
            CallKind::List,
            CallKind::Search,
            CallKind::Fetch,
            CallKind::Remember,
            CallKind::Skill,
            CallKind::Ask,
        ] {
            assert!(
                !changes_something(read_only),
                "{read_only:?} leaves no mark and stays a row"
            );
        }
    }

    /// `ui` resolves an icon to a path and asks the app's asset source for the
    /// bytes; a name nobody vendored draws nothing at all, silently. This is the
    /// only place that can notice, so it walks every icon the transcript and the
    /// composer ask for.
    #[test]
    fn every_icon_the_window_asks_for_is_vendored() {
        let kinds = [
            CallKind::Read,
            CallKind::List,
            CallKind::Search,
            CallKind::Write,
            CallKind::Edit,
            CallKind::Delete,
            CallKind::Move,
            CallKind::Execute,
            CallKind::Fetch,
            CallKind::Remember,
            CallKind::Skill,
            CallKind::Ask,
            CallKind::Delegate,
            CallKind::Mcp,
        ];
        let icons = kinds.into_iter().map(icon_for).chain([
            IconName::ToolThink,
            IconName::XCircle,
            IconName::ChevronDown,
            IconName::ChevronRight,
        ]);

        for icon in icons {
            let path = icon.path();
            assert!(
                crate::Assets
                    .load(&path)
                    .expect("an icon path is not an I/O error")
                    .is_some(),
                "{icon:?} is asked for but {path} is not in resources/icons"
            );
        }
    }
}
