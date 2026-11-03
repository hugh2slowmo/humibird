use gpui::{
    Anchor, AnyElement, App, Bounds, BoxShadow, Context, Element, ElementId, Entity, FocusHandle,
    FontWeight, GlobalElementId, IntoElement, KeyDownEvent, LayoutId, MouseButton, MouseDownEvent,
    ParentElement, Pixels, Style, Styled, Window, anchored, deferred, div, hsla, point, prelude::*,
    px, rgb,
};

use ui::CircularProgress;

use crate::{HumibirdRoot, Palette, QueuedMessage, TextInput, display_model, widgets};
use humibird_providers::Attachment;

/// The open `@` file menu. The first entry is what Enter takes.
#[derive(Clone, Copy)]
pub(crate) struct FileMenu<'a> {
    pub(crate) choices: &'a [String],
}

/// The open state of the picker, borrowed from the window root for one render.
#[derive(Clone, Copy)]
pub(crate) struct ModelMenu<'a> {
    pub(crate) focus: &'a FocusHandle,
    pub(crate) highlight: usize,
}

/// What the composer shows, all of it owned by the window root.
#[derive(Clone, Copy)]
pub(crate) struct ComposerState<'a> {
    pub(crate) model: &'a str,
    /// Which provider the thread is on, so its model is the one marked.
    pub(crate) provider_id: &'a str,
    /// Whether a provider resolved. Without one no model applies, so the chip asks
    /// for one instead of naming a model that cannot answer.
    pub(crate) provider_ready: bool,
    /// Every provider this profile can reach, grouped, so a model from any of them
    /// can be picked without switching providers first.
    pub(crate) groups: &'a [crate::ModelGroup],
    /// How full the model's window is, when the provider reported a window.
    pub(crate) context: Option<ContextUsage>,
    /// Whether the pointer is over the meter, which swaps in the token numbers.
    pub(crate) context_hovered: bool,
    /// The thinking switch: what the profile asks for, and whether this
    /// provider's wire has a way to ask at all.
    pub(crate) thinking: ThinkingChoice,
    /// Files and images waiting to go with the message being written.
    pub(crate) attachments: &'a [Attachment],
    /// The `@` menu, when the text ends in a mention.
    pub(crate) files: Option<FileMenu<'a>>,
    pub(crate) menu: Option<ModelMenu<'a>>,
    pub(crate) notice: Option<&'a str>,
    /// A turn is running, so the send button is a stop button.
    pub(crate) running: bool,
    /// Messages written while the turn was busy.
    pub(crate) queued: &'a [QueuedMessage],
}

/// How full the model's window is. Both numbers are what the provider actually
/// reported: `used` is the last reply's prompt plus completion, and `limit` is
/// the provider's window for this model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ContextUsage {
    pub(crate) used: u32,
    pub(crate) limit: u32,
}

/// The thinking switch, as the composer needs it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ThinkingChoice {
    pub(crate) enabled: bool,
    pub(crate) effort: humibird_profile::Effort,
    /// Whether the effort menu is showing.
    pub(crate) menu_open: bool,
    /// Whether this provider's wire carries a thinking switch. Anthropic's does
    /// not here (its thinking blocks would have to be replayed, and they are
    /// never stored), and neither do the providers it cannot know.
    pub(crate) available: bool,
}

pub(crate) fn effort_label(effort: humibird_profile::Effort) -> &'static str {
    match effort {
        humibird_profile::Effort::Low => "Low",
        humibird_profile::Effort::Medium => "Medium",
        humibird_profile::Effort::High => "High",
    }
}

/// The share of the window in use, clamped: a provider that reports more than its
/// own limit still fills the bar rather than painting past it.
pub(crate) fn context_fraction(used: u32, limit: u32) -> f32 {
    if limit == 0 {
        return 0.0;
    }
    (used as f32 / limit as f32).clamp(0.0, 1.0)
}

pub(crate) fn context_percent(used: u32, limit: u32) -> u32 {
    (context_fraction(used, limit) * 100.0).round() as u32
}

/// The share of the model's window the thread has used, as a ring. The numbers
/// live behind the ring rather than beside it: the composer only has room for the
/// one glanceable thing, and pointing at it explains it.
fn context_meter(
    theme: Palette,
    usage: ContextUsage,
    hovered: bool,
    cx: &mut Context<HumibirdRoot>,
) -> impl IntoElement {
    let fraction = context_fraction(usage.used, usage.limit);
    let past_threshold = fraction >= humibird_agent::AUTO_COMPACT_AT as f32;
    let color = if past_threshold {
        theme.warning
    } else {
        theme.accent
    };

    let ring = div()
        .id("context-meter")
        .debug_selector(|| "context-meter".into())
        .flex_none()
        .on_hover(cx.listener(|this, hovered, _, cx| {
            if this.context_meter_hovered != *hovered {
                this.context_meter_hovered = *hovered;
                cx.notify();
            }
        }))
        .child(
            CircularProgress::new(usage.used as f32, usage.limit.max(1) as f32, px(16.), cx)
                .stroke_width(px(2.))
                .bg_color(rgb(theme.border).into())
                .progress_color(rgb(color).into()),
        );

    ChipMenu::new("composer-context".into(), MenuEdge::End, ring)
        .menu(hovered.then(|| context_tooltip(theme, usage)))
}

/// What the ring means: the percentage, and the numbers it came from.
fn context_tooltip(theme: Palette, usage: ContextUsage) -> AnyElement {
    let percent = context_percent(usage.used, usage.limit);
    let past_threshold =
        context_fraction(usage.used, usage.limit) >= humibird_agent::AUTO_COMPACT_AT as f32;

    div()
        .debug_selector(|| "context-tooltip".into())
        .w(px(170.))
        .px(px(10.))
        .py(px(7.))
        .rounded_md()
        .border_1()
        .border_color(rgb(theme.border))
        .bg(rgb(theme.panel_bg))
        .shadow(vec![BoxShadow {
            color: hsla(0., 0., 0., 0.14),
            offset: point(px(0.), px(4.)),
            blur_radius: px(12.),
            spread_radius: px(0.),
            inset: false,
        }])
        .flex()
        .flex_col()
        .gap(px(3.))
        .child(
            div()
                .text_size(px(10.))
                .text_color(rgb(theme.text_muted))
                .child("Context"),
        )
        .child(
            div()
                .text_size(px(12.5))
                .text_color(rgb(if past_threshold {
                    theme.warning
                } else {
                    theme.text
                }))
                .child(format!(
                    "{percent}% · {} / {}",
                    tokens_label(usage.used),
                    tokens_label(usage.limit)
                )),
        )
        .into_any_element()
}

/// Token counts are read at a glance, so they stay short: 950, 1.5k, 12k, 1M.
pub(crate) fn tokens_label(tokens: u32) -> String {
    let tokens = f64::from(tokens);
    if tokens >= 1_000_000.0 {
        let millions = tokens / 1_000_000.0;
        // A whole window reads as `1M`, not `1.0M`; a fraction keeps its digit.
        if millions.fract() == 0.0 {
            format!("{millions:.0}M")
        } else {
            format!("{millions:.1}M")
        }
    } else if tokens >= 10_000.0 {
        format!("{:.0}k", tokens / 1_000.0)
    } else if tokens >= 1_000.0 {
        format!("{:.1}k", tokens / 1_000.0)
    } else {
        format!("{tokens:.0}")
    }
}

pub fn render(
    theme: Palette,
    input: Entity<TextInput>,
    state: ComposerState<'_>,
    cx: &mut Context<HumibirdRoot>,
) -> impl IntoElement {
    div()
        .id("composer")
        .debug_selector(|| "composer".into())
        .flex_none()
        .px(px(20.))
        .pt(px(6.))
        .pb(px(16.))
        .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
            if event.keystroke.key != "escape" {
                return;
            }
            if this.file_menu_open() {
                this.dismiss_file_menu(cx);
                cx.stop_propagation();
            } else if this.is_thinking_menu_open() {
                this.close_thinking_menu(cx);
                cx.stop_propagation();
            }
        }))
        .child(
            div()
                .debug_selector(|| "composer-card".into())
                .max_w(px(720.))
                .mx_auto()
                .w_full()
                .rounded_lg()
                .border_1()
                .border_color(rgb(theme.border_strong))
                .bg(rgb(theme.panel_bg))
                .p(px(12.))
                .flex()
                .flex_col()
                .gap(px(10.))
                .when(!state.queued.is_empty(), |this| {
                    this.child(queued_rows(theme, state.queued, cx))
                })
                .when(!state.attachments.is_empty(), |this| {
                    this.child(attachment_rows(theme, state.attachments, cx))
                })
                .when_some(state.files, |this, menu| {
                    this.child(file_menu(theme, menu, cx))
                })
                .child(
                    div()
                        .h(px(52.))
                        .pt(px(2.))
                        .text_size(px(13.))
                        .line_height(px(18.))
                        .text_color(rgb(theme.text))
                        .child(input),
                )
                .when_some(state.notice, |this, notice| {
                    this.child(
                        div()
                            .debug_selector(|| "workspace-notice".into())
                            .text_size(px(11.))
                            .line_height(px(15.))
                            .text_color(rgb(theme.warning))
                            .child(notice.to_owned()),
                    )
                })
                .child(bottom_row(theme, state, cx)),
        )
}

fn bottom_row(
    theme: Palette,
    state: ComposerState<'_>,
    cx: &mut Context<HumibirdRoot>,
) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .justify_between()
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .child(add_attachment_button(theme, cx))
                .child(thinking_controls(theme, state.thinking, cx)),
        )
        .child(
            div()
                .flex()
                .flex_none()
                .items_center()
                .gap(px(8.))
                .when_some(state.context, |this, usage| {
                    this.child(context_meter(theme, usage, state.context_hovered, cx))
                })
                // The chip is the only way out of a provider that cannot answer,
                // so it is offered even when there is no provider to name.
                .child(model_selector(
                    theme,
                    state.model,
                    state.provider_id,
                    state.provider_ready,
                    state.groups,
                    state.menu,
                    cx,
                ))
                .when(state.running, |this| this.child(stop_button(theme, cx)))
                .when(!state.running, |this| this.child(send_button(theme, cx))),
        )
}

fn add_attachment_button(theme: Palette, cx: &mut Context<HumibirdRoot>) -> impl IntoElement {
    div()
        .id("attach")
        .debug_selector(|| "attach".into())
        .flex_none()
        .size(px(20.))
        .rounded_md()
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(14.))
        .text_color(rgb(theme.text_muted))
        .cursor_pointer()
        .hover(move |this| this.bg(rgb(theme.hover_bg)).text_color(rgb(theme.text)))
        .on_click(cx.listener(|this, _, window, cx| this.choose_attachments(window, cx)))
        .child("+")
}

fn file_menu(theme: Palette, menu: FileMenu<'_>, cx: &mut Context<HumibirdRoot>) -> AnyElement {
    if menu.choices.is_empty() {
        return div()
            .debug_selector(|| "file-menu".into())
            .px(px(8.))
            .py(px(4.))
            .text_size(px(11.))
            .text_color(rgb(theme.text_dim))
            .child("No file matches.")
            .into_any_element();
    }

    let rows = menu.choices.iter().enumerate().map(|(index, path)| {
        let selector = format!("file-option-{index}");
        let chosen = path.clone();
        let first = index == 0;

        div()
            .id(selector.clone())
            .debug_selector(move || selector.clone())
            .flex()
            .items_center()
            .justify_between()
            .gap(px(8.))
            .px(px(8.))
            .py(px(4.))
            .rounded_sm()
            .text_size(px(11.))
            .text_color(rgb(if first {
                theme.text
            } else {
                theme.text_secondary
            }))
            .cursor_pointer()
            .hover(move |this| this.bg(rgb(theme.hover_bg)).text_color(rgb(theme.text)))
            .on_mouse_down(
                MouseButton::Left,
                |_: &MouseDownEvent, _: &mut gpui::Window, cx: &mut gpui::App| {
                    cx.stop_propagation()
                },
            )
            .on_click(cx.listener(move |this, _, _, cx| this.accept_file(chosen.clone(), cx)))
            .child(div().min_w_0().truncate().child(path.clone()))
            .child(
                div()
                    .flex_none()
                    .text_size(px(9.5))
                    .text_color(rgb(theme.text_dim))
                    .child(if first { "↵" } else { "" }),
            )
    });

    div()
        .debug_selector(|| "file-menu".into())
        .flex()
        .flex_col()
        .gap(px(2.))
        .children(rows)
        .into_any_element()
}

/// What is waiting to be sent with the message: the name, what kind it is, and
/// a control to take it back off.
fn attachment_rows(
    theme: Palette,
    attachments: &[Attachment],
    cx: &mut Context<HumibirdRoot>,
) -> impl IntoElement {
    let rows = attachments.iter().enumerate().map(|(index, attachment)| {
        let row_selector = format!("attachment-{index}");
        let remove_selector = format!("attachment-remove-{index}");
        let kind = if attachment.is_image() {
            "image"
        } else {
            "file"
        };

        div()
            .id(row_selector.clone())
            .debug_selector(move || row_selector.clone())
            .flex()
            .items_center()
            .gap(px(8.))
            .px(px(8.))
            .py(px(4.))
            .rounded_sm()
            .bg(rgb(theme.window_bg))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(11.))
                    .text_color(rgb(theme.text_secondary))
                    .child(format!("{kind} · {}", attachment.name())),
            )
            .child(
                div()
                    .id(remove_selector.clone())
                    .debug_selector(move || remove_selector.clone())
                    .px(px(4.))
                    .text_size(px(11.))
                    .text_color(rgb(theme.text_dim))
                    .cursor_pointer()
                    .hover(move |this| this.text_color(rgb(theme.text)))
                    .on_click(cx.listener(move |this, _, _, cx| this.remove_attachment(index, cx)))
                    .child("×"),
            )
    });

    div()
        .debug_selector(|| "attachments".into())
        .flex()
        .flex_col()
        .gap(px(4.))
        .children(rows)
}

/// Which side of the chip the menu is flush with.
#[derive(Clone, Copy)]
enum MenuEdge {
    /// The chip's left edge; the menu grows to the right of it.
    Start,
    /// The chip's right edge; the menu grows to the left of it.
    End,
}

/// A control and the menu it opens.
///
/// The menu is not part of the composer's own layout. It is measured against the
/// chip and drawn in an overlay layer on top of the window, the way Zed's
/// popovers are (`deferred(anchored(..))`): it hangs from the chip's top edge and
/// floats over whatever is behind it. Positioned with `absolute` inside the card,
/// the card's border crossed its shadow and the draft's text showed around it.
struct ChipMenu {
    id: ElementId,
    trigger: Option<AnyElement>,
    menu: Option<AnyElement>,
    edge: MenuEdge,
}

/// The gap between the chip and the menu above it.
const MENU_GAP: f32 = 6.;

/// Where the chip was in the frame before. The menu hangs from the chip's own
/// corner, so it lines up with it however the row is laid out. Reading it a frame
/// late is enough: the chip is on screen before its menu can be opened.
#[derive(Clone, Copy, Default)]
struct ChipBounds(Option<Bounds<Pixels>>);

struct ChipMenuLayout {
    trigger: Option<AnyElement>,
    menu: Option<AnyElement>,
    trigger_layout: Option<LayoutId>,
}

impl ChipMenu {
    fn new(id: ElementId, edge: MenuEdge, trigger: impl IntoElement) -> Self {
        ChipMenu {
            id,
            trigger: Some(trigger.into_any_element()),
            menu: None,
            edge,
        }
    }

    fn menu(mut self, menu: Option<AnyElement>) -> Self {
        self.menu = menu;
        self
    }
}

impl Element for ChipMenu {
    type RequestLayoutState = ChipMenuLayout;
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let chip_bounds = global_id
            .map(|global_id| {
                window.with_element_state(global_id, |state: Option<ChipBounds>, _| {
                    let bounds = state.unwrap_or_default();
                    (bounds, bounds)
                })
            })
            .unwrap_or_default();

        let (chip_corner, menu_corner) = match self.edge {
            MenuEdge::Start => (Anchor::TopLeft, Anchor::BottomLeft),
            MenuEdge::End => (Anchor::TopRight, Anchor::BottomRight),
        };
        let menu = self.menu.take().map(|menu| {
            let mut placement = anchored()
                .anchor(menu_corner)
                .offset(point(px(0.), px(-MENU_GAP)))
                .snap_to_window_with_margin(px(8.));
            if let Some(bounds) = chip_bounds.0 {
                placement = placement.position(bounds.corner(chip_corner));
            }
            let mut menu = deferred(placement.child(menu)).with_priority(1).into_any();
            let layout = menu.request_layout(window, cx);
            (layout, menu)
        });
        let menu_layout = menu.as_ref().map(|(layout, _)| *layout);
        let menu = menu.map(|(_, menu)| menu);

        let mut trigger = self.trigger.take();
        let trigger_layout = trigger
            .as_mut()
            .map(|trigger| trigger.request_layout(window, cx));

        let style = Style {
            flex_shrink: 0.,
            ..Style::default()
        };
        let layout =
            window.request_layout(style, trigger_layout.into_iter().chain(menu_layout), cx);

        (
            layout,
            ChipMenuLayout {
                trigger,
                menu,
                trigger_layout,
            },
        )
    }

    fn prepaint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        _bounds: Bounds<Pixels>,
        request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) {
        if let Some(trigger) = request_layout.trigger.as_mut() {
            trigger.prepaint(window, cx);
        }
        if let Some(menu) = request_layout.menu.as_mut() {
            menu.prepaint(window, cx);
        }

        if let (Some(global_id), Some(layout)) = (global_id, request_layout.trigger_layout) {
            let bounds = window.layout_bounds(layout);
            window.with_element_state(global_id, |_: Option<ChipBounds>, _| {
                ((), ChipBounds(Some(bounds)))
            });
        }
    }

    fn paint(
        &mut self,
        _global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        _bounds: Bounds<Pixels>,
        request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        if let Some(mut trigger) = request_layout.trigger.take() {
            trigger.paint(window, cx);
        }
        if let Some(mut menu) = request_layout.menu.take() {
            menu.paint(window, cx);
        }
    }
}

impl IntoElement for ChipMenu {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

/// The thinking switch: one control to ask for it, and, while it is on, the
/// level. Hidden entirely on a provider whose wire has no such field, because a
/// switch that cannot do anything is worse than no switch.
fn thinking_controls(
    theme: Palette,
    choice: ThinkingChoice,
    cx: &mut Context<HumibirdRoot>,
) -> AnyElement {
    if !choice.available {
        return div().into_any_element();
    }

    let enabled = choice.enabled;
    div()
        .flex()
        .items_center()
        .gap(px(6.))
        .child(
            div()
                .id("thinking-toggle")
                .debug_selector(|| "thinking-toggle".into())
                .flex()
                .items_center()
                .gap(px(4.))
                .px(px(7.))
                .py(px(3.))
                .rounded_md()
                .text_size(px(10.))
                .cursor_pointer()
                .text_color(rgb(if enabled {
                    theme.accent
                } else {
                    theme.text_muted
                }))
                .when(enabled, |this| this.bg(rgb(theme.accent_soft)))
                .when(!enabled, |this| {
                    this.hover(move |this| this.text_color(rgb(theme.text)))
                })
                .on_click(cx.listener(|this, _, _, cx| this.toggle_thinking_mode(cx)))
                .child("✧")
                .child("Think"),
        )
        .when(enabled, |this| {
            this.child(effort_selector(theme, choice, cx))
        })
        .into_any_element()
}

/// The effort chip, shaped like the model chip at the other end of the row: the
/// current level, a chevron, and the three levels behind it. Three bare words
/// read as three buttons; one chip reads as one choice.
fn effort_selector(
    theme: Palette,
    choice: ThinkingChoice,
    cx: &mut Context<HumibirdRoot>,
) -> impl IntoElement {
    let chip = div()
        .id("thinking-effort")
        .debug_selector(|| "thinking-effort".into())
        .cursor_pointer()
        // Presses on the chip belong to the chip: without this the window root
        // would read the press as "outside" and close the menu before the click
        // could toggle it.
        .on_mouse_down(
            MouseButton::Left,
            |_: &MouseDownEvent, _: &mut gpui::Window, cx: &mut gpui::App| cx.stop_propagation(),
        )
        .on_click(cx.listener(|this, _, window, cx| this.toggle_thinking_menu(window, cx)))
        .child(effort_chip(
            theme,
            effort_label(choice.effort),
            choice.menu_open,
        ));

    let menu = choice
        .menu_open
        .then(|| effort_menu(theme, choice.effort, cx));

    ChipMenu::new("composer-effort-menu".into(), MenuEdge::Start, chip).menu(menu)
}

/// Open, the chip takes the accent: the menu hangs from it, and it says so.
fn effort_chip(theme: Palette, label: &str, open: bool) -> impl IntoElement {
    div()
        .flex_none()
        .px(px(8.))
        .py(px(4.))
        .rounded_md()
        .text_size(px(10.5))
        .when(open, |this| {
            this.bg(rgb(theme.accent_soft))
                .text_color(rgb(theme.accent))
        })
        .when(!open, |this| {
            this.bg(rgb(theme.hover_bg))
                .text_color(rgb(theme.text_secondary))
        })
        .child(label.to_owned())
}

fn effort_menu(
    theme: Palette,
    current: humibird_profile::Effort,
    cx: &mut Context<HumibirdRoot>,
) -> AnyElement {
    let rows = [
        humibird_profile::Effort::Low,
        humibird_profile::Effort::Medium,
        humibird_profile::Effort::High,
    ]
    .map(|effort| effort_row(theme, effort, current, cx));

    // Narrower than the model list, which has to hold whole model names.
    menu_panel(
        theme,
        px(180.),
        "thinking-menu",
        Some("Change thinking effort"),
        None,
        rows,
    )
}

fn effort_row(
    theme: Palette,
    effort: humibird_profile::Effort,
    current: humibird_profile::Effort,
    cx: &mut Context<HumibirdRoot>,
) -> AnyElement {
    let selected = effort == current;
    let selector = format!("thinking-effort-{}", effort_label(effort).to_lowercase());

    div()
        .id(selector.clone())
        .debug_selector(move || selector.clone())
        .w_full()
        .px(px(8.))
        .py(px(5.))
        .rounded_sm()
        .flex()
        .items_center()
        .justify_between()
        .gap(px(8.))
        .text_size(px(11.))
        .text_color(rgb(theme.text_secondary))
        .cursor_pointer()
        .hover(move |this| this.bg(rgb(theme.hover_bg)).text_color(rgb(theme.text)))
        .on_mouse_down(
            MouseButton::Left,
            |_: &MouseDownEvent, _: &mut gpui::Window, cx: &mut gpui::App| cx.stop_propagation(),
        )
        .on_click(cx.listener(move |this, _, _, cx| this.set_thinking_effort(effort, cx)))
        .child(effort_label(effort))
        .child(
            div()
                .flex_none()
                .w(px(12.))
                .text_color(rgb(theme.accent))
                .child(if selected { "✓" } else { "" }),
        )
        .into_any_element()
}

/// The panel both menus are drawn on: a floating card with a shadow, flush with
/// the chip it hangs from. It occludes the window beneath it, so a press inside
/// the menu cannot reach the composer it covers. Zed's effort popover names
/// itself at the top; the model list does not, since its rows say what they are.
fn menu_panel(
    theme: Palette,
    width: Pixels,
    debug_selector: &'static str,
    title: Option<&'static str>,
    max_height: Option<Pixels>,
    rows: impl IntoIterator<Item = AnyElement>,
) -> AnyElement {
    div()
        .id(debug_selector)
        .debug_selector(move || debug_selector.into())
        .occlude()
        // A press on the panel is the menu's, not a click outside it: the window
        // root closes menus on a press it sees.
        .on_mouse_down(
            MouseButton::Left,
            |_: &MouseDownEvent, _: &mut gpui::Window, cx: &mut gpui::App| cx.stop_propagation(),
        )
        .w(width)
        .p(px(4.))
        .rounded_md()
        .border_1()
        .border_color(rgb(theme.border_strong))
        .bg(rgb(theme.panel_bg))
        .shadow(vec![BoxShadow {
            color: hsla(0., 0., 0., 0.16),
            offset: point(px(0.), px(6.)),
            blur_radius: px(16.),
            spread_radius: px(0.),
            inset: false,
        }])
        .when_some(title, |this, title| {
            this.child(
                div()
                    .px(px(8.))
                    .py(px(4.))
                    .mb(px(4.))
                    .border_b_1()
                    .border_color(rgb(theme.border))
                    .text_size(px(10.))
                    .text_color(rgb(theme.text_muted))
                    .child(title),
            )
        })
        .child(
            // A provider can name more models than fit above the composer; the
            // list scrolls rather than growing past the top of the window.
            div()
                .id(format!("{debug_selector}-rows"))
                .flex()
                .flex_col()
                .when_some(max_height, |this, height| {
                    this.max_h(height).overflow_y_scroll()
                })
                .children(rows),
        )
        .into_any_element()
}

/// The model chip and, above it, the list of models the provider offers. Both
/// floats are overlay layers, so the list never squeezes the composer.
fn model_selector(
    theme: Palette,
    model: &str,
    provider_id: &str,
    provider_ready: bool,
    groups: &[crate::ModelGroup],
    menu: Option<ModelMenu<'_>>,
    cx: &mut Context<HumibirdRoot>,
) -> impl IntoElement {
    // A provider that cannot answer names no model, so the chip asks for one rather
    // than repeating a name that does not apply.
    let label = if provider_ready {
        display_model(model)
    } else {
        "Choose model".to_owned()
    };
    let selector = if provider_ready {
        format!("composer-model-{model}")
    } else {
        "composer-model-chip".to_owned()
    };
    let chip = div()
        .id("model-chip")
        .debug_selector(move || selector.clone())
        .cursor_pointer()
        // Presses on the chip belong to the chip: without this the window
        // root would read the press as "outside" and close the list before
        // the click could toggle it.
        .on_mouse_down(
            MouseButton::Left,
            |_: &MouseDownEvent, _: &mut gpui::Window, cx: &mut gpui::App| cx.stop_propagation(),
        )
        .on_click(cx.listener(|this, _, window, cx| this.toggle_model_menu(window, cx)))
        .child(widgets::model_chip(&label, theme));

    let menu = menu.map(|menu| model_menu(theme, model, provider_id, groups, menu, cx));

    ChipMenu::new("composer-model-menu".into(), MenuEdge::End, chip).menu(menu)
}

fn model_menu(
    theme: Palette,
    current: &str,
    current_provider: &str,
    groups: &[crate::ModelGroup],
    menu: ModelMenu<'_>,
    cx: &mut Context<HumibirdRoot>,
) -> AnyElement {
    let mut children: Vec<AnyElement> = Vec::new();
    let mut index = 0usize;
    for group in groups {
        if children.is_empty() {
            // No divider above the first group.
        } else {
            children.push(
                div()
                    .h(px(1.))
                    .mx(px(4.))
                    .bg(rgb(theme.border))
                    .into_any_element(),
            );
        }
        children.push(
            div()
                .px(px(8.))
                .pt(px(6.))
                .pb(px(2.))
                .text_size(px(10.5))
                .text_color(rgb(theme.text_dim))
                .child(group.provider.display_name)
                .into_any_element(),
        );
        for (id, label) in &group.models {
            let selected = id == current && group.provider.id == current_provider;
            children.push(model_row(
                format!("model-option-{}/{id}", group.provider.id),
                group.provider.id.to_owned(),
                id.clone(),
                label.clone(),
                selected,
                index == menu.highlight,
                theme,
                cx,
            ));
            index += 1;
        }
    }

    // The list is the only place the provider is decided, so it ends with the way
    // to the keys it needs — and an empty list still has a door.
    if !children.is_empty() {
        children.push(
            div()
                .h(px(1.))
                .mx(px(4.))
                .bg(rgb(theme.border))
                .into_any_element(),
        );
    }
    children.push(
        div()
            .id("model-configure-providers")
            .debug_selector(|| "model-configure-providers".into())
            .w_full()
            .px(px(8.))
            .py(px(5.))
            .rounded_sm()
            .text_size(px(11.))
            .text_color(rgb(theme.text_secondary))
            .cursor_pointer()
            .hover(move |this| this.bg(rgb(theme.hover_bg)).text_color(rgb(theme.text)))
            .on_click(cx.listener(|this, _, window, cx| {
                this.close_model_menu(window, cx);
                this.open_settings(cx);
            }))
            .child("Configure providers…")
            .into_any_element(),
    );

    div()
        .id("model-menu")
        .track_focus(menu.focus)
        .on_mouse_down(
            MouseButton::Left,
            |_: &MouseDownEvent, _: &mut gpui::Window, cx: &mut gpui::App| cx.stop_propagation(),
        )
        .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
            match event.keystroke.key.as_str() {
                "down" => this.move_model_highlight(1, cx),
                "up" => this.move_model_highlight(-1, cx),
                "enter" => this.accept_model_highlight(window, cx),
                "escape" => this.close_model_menu(window, cx),
                _ => {}
            }
        }))
        .child(menu_panel(
            theme,
            px(240.),
            "model-menu",
            None,
            Some(px(320.)),
            children,
        ))
        .into_any_element()
}

#[allow(clippy::too_many_arguments)]
fn model_row(
    selector: String,
    provider: String,
    id: String,
    label: String,
    selected: bool,
    highlighted: bool,
    theme: Palette,
    cx: &mut Context<HumibirdRoot>,
) -> AnyElement {
    let chosen = id.clone();

    div()
        .id(selector.clone())
        .debug_selector(move || selector.clone())
        .w_full()
        .px(px(8.))
        .py(px(5.))
        .rounded_sm()
        .flex()
        .items_center()
        .justify_between()
        .gap(px(8.))
        .text_size(px(11.))
        .text_color(rgb(if highlighted {
            theme.text
        } else {
            theme.text_secondary
        }))
        .cursor_pointer()
        .when(highlighted, |this| this.bg(rgb(theme.hover_bg)))
        .on_mouse_down(
            MouseButton::Left,
            |_: &MouseDownEvent, _: &mut gpui::Window, cx: &mut gpui::App| cx.stop_propagation(),
        )
        .on_click(cx.listener(move |this, _, window, cx| {
            this.set_model(provider.clone(), chosen.clone(), window, cx)
        }))
        .child(div().min_w_0().truncate().child(label))
        .child(
            div()
                .flex_none()
                .w(px(12.))
                .text_color(rgb(theme.accent))
                .child(if selected { "✓" } else { "" }),
        )
        .into_any_element()
}

fn send_button(theme: Palette, cx: &mut Context<HumibirdRoot>) -> impl IntoElement {
    div()
        .id("send")
        .debug_selector(|| "send".into())
        .flex_none()
        .size(px(26.))
        .rounded_full()
        .bg(rgb(theme.accent))
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(13.))
        .font_weight(FontWeight::MEDIUM)
        .text_color(rgb(theme.text_on_accent))
        .cursor_pointer()
        .hover(move |this| this.opacity(0.88))
        .on_click(cx.listener(|this, _, _, cx| this.submit_draft(cx)))
        .child("↑")
}

/// Replaces the send button while a turn runs: the only useful action then is
/// stopping it.
fn stop_button(theme: Palette, cx: &mut Context<HumibirdRoot>) -> impl IntoElement {
    div()
        .id("stop")
        .debug_selector(|| "stop".into())
        .flex_none()
        .size(px(26.))
        .rounded_full()
        .bg(rgb(theme.accent))
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .hover(move |this| this.opacity(0.88))
        .on_click(cx.listener(|this, _, _, cx| this.stop_turn(cx)))
        .child(
            div()
                .size(px(8.))
                .rounded_sm()
                .bg(rgb(theme.text_on_accent)),
        )
}

/// What is waiting while the turn finishes. The front one decides whether the
/// turn should end early, so its "Steer" toggle is the one that does something.
fn queued_rows(
    theme: Palette,
    queued: &[QueuedMessage],
    cx: &mut Context<HumibirdRoot>,
) -> impl IntoElement {
    let rows = queued.iter().enumerate().map(|(index, message)| {
        let row_selector = format!("queued-{index}");
        let steer_selector = format!("queued-steer-{index}");
        let remove_selector = format!("queued-remove-{index}");
        let steering = message.steer;

        div()
            .id(row_selector.clone())
            .debug_selector(move || row_selector.clone())
            .flex()
            .items_center()
            .gap(px(8.))
            .px(px(8.))
            .py(px(4.))
            .rounded_sm()
            .bg(rgb(theme.window_bg))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(11.))
                    .text_color(rgb(theme.text_secondary))
                    .child(message.text.clone()),
            )
            .child(
                div()
                    .id(steer_selector.clone())
                    .debug_selector(move || steer_selector.clone())
                    .px(px(6.))
                    .py(px(2.))
                    .rounded_sm()
                    .text_size(px(10.))
                    .cursor_pointer()
                    .when(steering, |this| {
                        this.bg(rgb(theme.accent_soft))
                            .text_color(rgb(theme.accent))
                    })
                    .when(!steering, |this| {
                        this.text_color(rgb(theme.text_dim))
                            .hover(move |this| this.text_color(rgb(theme.text)))
                    })
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.toggle_queued_steer(index, cx)),
                    )
                    .child("Steer"),
            )
            .child(
                div()
                    .id(remove_selector.clone())
                    .debug_selector(move || remove_selector.clone())
                    .px(px(4.))
                    .text_size(px(11.))
                    .text_color(rgb(theme.text_dim))
                    .cursor_pointer()
                    .hover(move |this| this.text_color(rgb(theme.text)))
                    .on_click(cx.listener(move |this, _, _, cx| this.remove_queued(index, cx)))
                    .child("×"),
            )
    });

    div()
        .debug_selector(|| "queued".into())
        .flex()
        .flex_col()
        .gap(px(4.))
        .children(rows)
}
