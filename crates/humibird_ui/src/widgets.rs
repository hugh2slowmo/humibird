use gpui::{App, ClickEvent, IntoElement, ParentElement, Styled, Window, div, prelude::*, px, rgb};

use crate::{Appearance, Palette};

/// A model indicator: a status dot plus the model name.
pub fn model_chip(label: &str, theme: Palette) -> impl IntoElement {
    div()
        .flex_none()
        .flex()
        .items_center()
        .px(px(8.))
        .py(px(4.))
        .rounded_md()
        .bg(rgb(theme.hover_bg))
        .text_size(px(10.5))
        .text_color(rgb(theme.text_secondary))
        .child(label.to_owned())
}

pub fn theme_toggle(
    appearance: Appearance,
    theme: Palette,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id("theme-toggle")
        .debug_selector(move || appearance.selector().to_string())
        .flex_none()
        .size(px(24.))
        .rounded_md()
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(12.))
        .text_color(rgb(theme.text_secondary))
        .cursor_pointer()
        .hover(move |this| this.bg(rgb(theme.hover_bg)).text_color(rgb(theme.text)))
        .on_click(on_click)
        .child(match appearance {
            Appearance::Dark => "☀",
            Appearance::Light => "☾",
        })
}
