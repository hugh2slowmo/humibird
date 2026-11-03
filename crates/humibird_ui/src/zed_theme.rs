//! Keeps Zed's `GlobalTheme` in sync with humibird's palette.
//!
//! The markdown crate reads a handful of colors straight from the active Zed
//! theme — table borders, the header background, odd-row stripes, and the
//! hover chrome on code blocks. The default theme is dark, so without this the
//! light palette would still draw dark tables. We clone the default theme and
//! remap the roles the shell and the markdown renderer actually use.

use std::sync::Arc;

use gpui::{App, WindowBackgroundAppearance, hsla, rgb};
use theme::{Appearance as ZedAppearance, DEFAULT_DARK_THEME, GlobalTheme, Theme, ThemeRegistry};

use crate::{Appearance, Palette};

pub fn apply(cx: &mut App, appearance: Appearance, palette: Palette) {
    let Ok(base) = ThemeRegistry::global(cx).get(DEFAULT_DARK_THEME) else {
        return;
    };

    let mut theme: Theme = (*base).clone();
    theme.appearance = match appearance {
        Appearance::Dark => ZedAppearance::Dark,
        Appearance::Light => ZedAppearance::Light,
    };
    theme.styles.window_background_appearance = WindowBackgroundAppearance::Opaque;

    let colors = &mut theme.styles.colors;
    let transparent = hsla(0., 0., 0., 0.);

    colors.border = rgb(palette.border).into();
    colors.border_variant = rgb(palette.border).into();
    colors.border_focused = rgb(palette.accent).into();
    colors.border_selected = rgb(palette.accent).into();
    colors.border_disabled = rgb(palette.border).into();

    colors.elevated_surface_background = rgb(palette.panel_bg).into();
    colors.surface_background = rgb(palette.panel_bg).into();

    colors.element_background = rgb(palette.panel_bg).into();
    colors.element_hover = rgb(palette.hover_bg).into();
    colors.element_active = rgb(palette.selected_bg).into();
    colors.element_selected = rgb(palette.selected_bg).into();
    colors.element_selection_background = rgb(palette.accent_soft).into();
    colors.element_disabled = rgb(palette.panel_bg).into();

    colors.ghost_element_background = transparent;
    colors.ghost_element_hover = rgb(palette.hover_bg).into();
    colors.ghost_element_active = rgb(palette.selected_bg).into();
    colors.ghost_element_selected = rgb(palette.selected_bg).into();
    colors.ghost_element_disabled = transparent;

    colors.text = rgb(palette.text).into();
    colors.text_muted = rgb(palette.text_muted).into();
    colors.text_placeholder = rgb(palette.text_dim).into();
    colors.text_disabled = rgb(palette.text_dim).into();
    colors.text_accent = rgb(palette.accent).into();

    colors.icon = rgb(palette.text_secondary).into();
    colors.icon_muted = rgb(palette.text_muted).into();
    colors.icon_placeholder = rgb(palette.text_dim).into();
    colors.icon_disabled = rgb(palette.text_dim).into();
    colors.icon_accent = rgb(palette.accent).into();

    colors.title_bar_background = rgb(palette.sidebar_bg).into();
    colors.toolbar_background = rgb(palette.sidebar_bg).into();
    colors.tab_bar_background = rgb(palette.sidebar_bg).into();
    colors.panel_background = rgb(palette.sidebar_bg).into();

    colors.editor_background = rgb(palette.code_bg).into();
    colors.editor_foreground = rgb(palette.text).into();

    colors.scrollbar_thumb_background = rgb(palette.border_strong).into();
    colors.scrollbar_thumb_hover_background = rgb(palette.text_dim).into();
    colors.scrollbar_track_background = transparent;

    GlobalTheme::update_theme(cx, Arc::new(theme));
}
