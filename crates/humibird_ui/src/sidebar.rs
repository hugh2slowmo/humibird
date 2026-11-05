use gpui::{
    AnyElement, Context, FontWeight, Image, ImageFormat, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, Styled, div, img, prelude::*, px, rgb,
};
use gpui::{HighlightStyle, StyledText};
use humibird_store::Session;
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;

use crate::{HumibirdRoot, Palette, TextInput};

/// Which rows the list should draw differently: the open thread, the one asking
/// whether to delete, and the one under the pointer, which is the only row whose
/// delete control is on screen.
pub struct Rows<'a> {
    pub active: Option<&'a str>,
    pub confirming: Option<&'a str>,
    pub hovered: Option<&'a str>,
}

/// Continuous substring matching, like Zed's thread archive. Byte ranges stay
/// on character boundaries so Chinese text can also be highlighted safely.
fn match_range(text: &str, query: &str) -> Option<Range<usize>> {
    if query.is_empty() {
        return Some(0..0);
    }
    let query: Vec<_> = query.chars().collect();
    let chars: Vec<_> = text.char_indices().collect();
    chars.windows(query.len()).find_map(|window| {
        window
            .iter()
            .zip(&query)
            .all(|((_, actual), expected)| actual.eq_ignore_ascii_case(expected))
            .then(|| {
                let (last, ch) = window[window.len() - 1];
                window[0].0..last + ch.len_utf8()
            })
    })
}

fn matches_session(session: &Session, query: &str) -> bool {
    match_range(session.title.as_deref().unwrap_or("New thread"), query).is_some()
        || Path::new(&session.workspace)
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| match_range(name, query).is_some())
}

fn highlighted_text(text: String, query: &str, theme: Palette) -> StyledText {
    let range = match_range(&text, query).filter(|range| !range.is_empty());
    StyledText::new(text).with_highlights(range.map(|range| {
        (
            range,
            HighlightStyle {
                color: Some(rgb(theme.text).into()),
                font_weight: Some(FontWeight::BOLD),
                ..Default::default()
            },
        )
    }))
}

#[cfg(test)]
mod search_tests {
    use super::*;

    #[test]
    fn matches_contiguous_text_and_returns_utf8_ranges() {
        assert_eq!(match_range("Build GPUI search", "gpui"), Some(6..10));
        assert_eq!(match_range("会话搜索功能", "搜索"), Some(6..12));
        assert_eq!(match_range("Build GPUI search", "bgs"), None);
        assert_eq!(match_range("", "longer"), None);
        assert_eq!(match_range("anything", ""), Some(0..0));
    }
}

/// The thread list. Threads are grouped by whether they belong to the workspace
/// this window is working in; a thread from elsewhere is shown with its
/// workspace and opening it switches to that workspace.
pub fn render(
    sessions: &[Session],
    search: gpui::Entity<TextInput>,
    rows: Rows<'_>,
    workspace: &str,
    theme: Palette,
    cx: &mut Context<HumibirdRoot>,
) -> impl IntoElement {
    let query = search.read(cx).value().trim().to_owned();
    let now = now_millis();
    let (here, elsewhere): (Vec<&Session>, Vec<&Session>) = sessions
        .iter()
        .filter(|session| matches_session(session, &query))
        .partition(|session| session.workspace == workspace);

    let here_rows: Vec<AnyElement> = here
        .iter()
        .map(|session| thread_row(session, &rows, &query, now, theme, cx).into_any_element())
        .collect();
    let elsewhere_rows: Vec<AnyElement> = elsewhere
        .iter()
        .map(|session| thread_row(session, &rows, &query, now, theme, cx).into_any_element())
        .collect();

    div()
        .id("sidebar")
        .debug_selector(|| "sidebar".into())
        .flex_none()
        .w(px(264.))
        .h_full()
        .flex()
        .flex_col()
        .bg(rgb(theme.sidebar_bg))
        .border_r_1()
        .border_color(rgb(theme.border))
        .pt(px(44.))
        .pb(px(10.))
        .child(brand())
        .child(new_thread_row(theme, cx))
        .child(
            div()
                .id("session-search")
                .debug_selector(|| "session-search".into())
                .mx(px(14.))
                .mt(px(10.))
                .px(px(8.))
                .py(px(6.))
                .rounded_md()
                .border_1()
                .border_color(rgb(theme.border))
                .text_size(px(12.))
                .flex()
                .items_center()
                .gap(px(6.))
                .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                    if event.keystroke.key == "escape" {
                        this.session_search
                            .update(cx, |input, cx| input.clear(window, cx));
                        cx.stop_propagation();
                    }
                }))
                .child(div().flex_1().min_w_0().child(search))
                .when(!query.is_empty(), |this| {
                    this.child(
                        div()
                            .id("clear-session-search")
                            .debug_selector(|| "clear-session-search".into())
                            .cursor_pointer()
                            .child("×")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.session_search
                                    .update(cx, |input, cx| input.clear(window, cx));
                            })),
                    )
                }),
        )
        .child(section_label("This workspace", "threads-here", theme))
        .when(here.is_empty(), |this| {
            this.child(empty_row(
                if !query.is_empty() && elsewhere.is_empty() {
                    "No threads match your search."
                } else if sessions.is_empty() {
                    "Nothing yet. Send a message to start one."
                } else {
                    "No threads in this workspace yet."
                },
                theme,
            ))
        })
        .children(here_rows)
        .when(!elsewhere.is_empty(), |this| {
            this.child(section_label(
                "Other workspaces",
                "threads-elsewhere",
                theme,
            ))
            .children(elsewhere_rows)
        })
        .child(div().flex_1())
        .child(settings_row(theme, cx))
}

fn brand() -> impl IntoElement {
    div().flex().items_center().h(px(30.)).px(px(14.)).child(
        div()
            .flex()
            .items_center()
            .gap(px(8.))
            .child(
                img(mark())
                    .debug_selector(|| "brand-mark".into())
                    .flex_none()
                    .size(px(18.)),
            )
            .child(
                div()
                    .text_size(px(13.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("humibird"),
            ),
    )
}

/// The app icon's tile, cut to the artwork's edges by `script/make-icon`.
///
/// This app installs no `AssetSource`, so `img("resources/…")` would paint
/// nothing; the bytes are handed over directly. Decoded once and then reused.
fn mark() -> Arc<Image> {
    static MARK: std::sync::OnceLock<Arc<Image>> = std::sync::OnceLock::new();
    MARK.get_or_init(|| {
        Arc::new(Image::from_bytes(
            ImageFormat::Png,
            include_bytes!("../resources/mark.png").to_vec(),
        ))
    })
    .clone()
}

fn new_thread_row(theme: Palette, cx: &mut Context<HumibirdRoot>) -> impl IntoElement + 'static {
    div()
        .id("new-thread")
        .debug_selector(|| "new-thread".into())
        .mx(px(8.))
        .mt(px(10.))
        .px(px(8.))
        .h(px(28.))
        .rounded_md()
        .flex()
        .items_center()
        .gap(px(8.))
        .text_size(px(12.))
        .text_color(rgb(theme.text_secondary))
        .cursor_pointer()
        .hover(move |this| this.bg(rgb(theme.hover_bg)).text_color(rgb(theme.text)))
        .on_click(cx.listener(|this, _, window, cx| this.new_thread(window, cx)))
        .child(div().text_size(px(14.)).child("+"))
        .child("New thread")
        .child(div().flex_1())
        .child(
            div()
                .text_size(px(10.))
                .text_color(rgb(theme.text_dim))
                .child("⌘N"),
        )
}

fn section_label(label: &'static str, selector: &'static str, theme: Palette) -> impl IntoElement {
    div()
        .debug_selector(move || selector.to_owned())
        .px(px(14.))
        .pt(px(18.))
        .pb(px(6.))
        .text_size(px(10.))
        .text_color(rgb(theme.text_dim))
        .child(label)
}

fn empty_row(text: &'static str, theme: Palette) -> impl IntoElement {
    div()
        .debug_selector(|| "threads-empty".into())
        .px(px(14.))
        .py(px(6.))
        .text_size(px(11.))
        .text_color(rgb(theme.text_dim))
        .child(text)
}

fn thread_row(
    session: &Session,
    rows: &Rows<'_>,
    query: &str,
    now: i64,
    theme: Palette,
    cx: &mut Context<HumibirdRoot>,
) -> impl IntoElement + 'static {
    let id = session.id.clone();
    let selector = format!("thread-{}", session.id);
    let delete_selector = format!("thread-delete-{}", session.id);
    let selected = rows.active == Some(session.id.as_str());
    let confirming = rows.confirming == Some(session.id.as_str());
    // A hidden element is not under the pointer as far as the mouse is concerned,
    // so the control is only clickable while its row is the one being hovered.
    let hovering = rows.hovered == Some(session.id.as_str());
    let title = session
        .title
        .clone()
        .unwrap_or_else(|| "New thread".to_owned());
    let preview = crate::display_workspace(&session.workspace);
    let age = relative_age(session.updated_at, now);
    let title = highlighted_text(title, query, theme);
    let preview = highlighted_text(preview, query, theme);

    let open_id = id.clone();
    let delete_id = id.clone();
    let hover_id = id.clone();

    div()
        .id(selector.clone())
        .debug_selector(move || selector.clone())
        .mx(px(8.))
        .px(px(8.))
        .py(px(7.))
        .rounded_md()
        .flex()
        .items_center()
        .gap(px(8.))
        .cursor_pointer()
        .when(selected, |this| this.bg(rgb(theme.selected_bg)))
        .when(!selected, move |this| {
            this.hover(move |this| this.bg(rgb(theme.hover_bg)))
        })
        .on_click(cx.listener(move |this, _, window, cx| this.open_session(&open_id, window, cx)))
        .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
            this.set_hovered_thread(&hover_id, *hovered, cx)
        }))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(2.))
                .child(div().text_size(px(12.)).truncate().child(title))
                // The question takes the second line, where the workspace was: the
                // first line is how the row is known, and swapping the two made an
                // untitled thread read "Delete this thread? / New thread".
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(rgb(if confirming {
                            theme.warning
                        } else {
                            theme.text_muted
                        }))
                        .truncate()
                        .when(confirming, |this| this.child("Delete this thread?"))
                        .when(!confirming, |this| this.child(preview)),
                ),
        )
        // The age and the delete control share one fixed box, so revealing the
        // control on hover does not move the row.
        .child(
            div()
                .relative()
                .flex_none()
                .w(px(20.))
                .h(px(16.))
                .child(
                    div()
                        .absolute()
                        .inset_0()
                        .flex()
                        .items_center()
                        .justify_end()
                        .text_size(px(10.))
                        .text_color(rgb(theme.text_dim))
                        .when(confirming || hovering, |this| this.invisible())
                        .child(age),
                )
                .child(
                    div()
                        .id(delete_selector.clone())
                        .debug_selector(move || delete_selector.clone())
                        .absolute()
                        .inset_0()
                        .flex()
                        .items_center()
                        .justify_end()
                        .cursor_pointer()
                        .when(confirming || hovering, |this| this.visible())
                        .when(!confirming && !hovering, |this| this.invisible())
                        .on_mouse_down(
                            MouseButton::Left,
                            |_: &MouseDownEvent, _: &mut gpui::Window, cx: &mut gpui::App| {
                                cx.stop_propagation()
                            },
                        )
                        .when(confirming, |this| {
                            this.text_size(px(12.))
                                .text_color(rgb(theme.accent))
                                .child("✓")
                        })
                        .when(!confirming, |this| {
                            this.text_size(px(11.))
                                .text_color(rgb(theme.text_dim))
                                .hover(move |this| this.text_color(rgb(theme.text)))
                                .child("✕")
                        })
                        .on_click(cx.listener(move |this, _, window, cx| {
                            if confirming {
                                this.confirm_delete_thread(&delete_id, window, cx);
                            } else {
                                this.arm_delete_thread(&delete_id, cx);
                            }
                            // The row itself opens the thread on a click; this one
                            // is the control's, not the row's.
                            cx.stop_propagation();
                        })),
                ),
        )
}

/// The profile this window belongs to. One profile is reachable today, so the row
/// opens settings rather than switching between profiles: the switcher arrives
/// with multi-profile support, and until then the row does not look
/// like something it is not.
fn settings_row(theme: Palette, cx: &mut Context<HumibirdRoot>) -> impl IntoElement + 'static {
    div()
        .mx(px(10.))
        .border_t_1()
        .border_color(rgb(theme.border))
        .pt(px(10.))
        .child(
            div()
                .id("open-settings")
                .debug_selector(|| "open-settings".into())
                .px(px(6.))
                .py(px(6.))
                .rounded_md()
                .flex()
                .items_center()
                .gap(px(8.))
                .cursor_pointer()
                .hover(move |this| this.bg(rgb(theme.hover_bg)))
                .on_click(cx.listener(|this, _, _, cx| this.open_settings(cx)))
                .child(
                    div()
                        .flex_none()
                        .text_size(px(12.))
                        .text_color(rgb(theme.text_muted))
                        .child("⚙"),
                )
                .child(div().text_size(px(12.)).child("Settings")),
        )
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or_default()
}

fn relative_age(updated_at: i64, now: i64) -> String {
    let seconds = (now - updated_at).max(0) / 1000;
    match seconds {
        0..=59 => "now".to_owned(),
        60..=3599 => format!("{}m", seconds / 60),
        3600..=86_399 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::relative_age;

    #[test]
    fn ages_are_reported_in_the_coarsest_unit() {
        let now = 1_000_000_000;

        assert_eq!(relative_age(now, now), "now");
        assert_eq!(relative_age(now - 59_000, now), "now");
        assert_eq!(relative_age(now - 60_000, now), "1m");
        assert_eq!(relative_age(now - 3_600_000, now), "1h");
        assert_eq!(relative_age(now - 3 * 86_400_000, now), "3d");
    }
}
