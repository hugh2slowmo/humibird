//! Builds a `MarkdownStyle` from humibird's palette.
//!
//! Zed's own `MarkdownStyle::themed` reads colors from Zed's `ThemeColors`,
//! which would ignore humibird's light palette. Everything structural is kept
//! at the defaults the markdown crate ships with; only colors, fonts and
//! spacing are set here.

use gpui::{
    AbsoluteLength, BorderStyle, DefiniteLength, EdgesRefinement, FontWeight, Length,
    StyleRefinement, TextStyle, TextStyleRefinement, UnderlineStyle, point, px, relative, rems,
    rgb,
};
use markdown::{BlockQuoteKindColors, HeadingLevelStyles, MarkdownStyle};

use crate::Palette;

pub fn style(theme: Palette) -> MarkdownStyle {
    let mut code_block = StyleRefinement {
        padding: EdgesRefinement {
            top: Some(DefiniteLength::Absolute(AbsoluteLength::Pixels(px(10.)))),
            left: Some(DefiniteLength::Absolute(AbsoluteLength::Pixels(px(10.)))),
            right: Some(DefiniteLength::Absolute(AbsoluteLength::Pixels(px(10.)))),
            bottom: Some(DefiniteLength::Absolute(AbsoluteLength::Pixels(px(10.)))),
        },
        margin: EdgesRefinement {
            top: Some(Length::Definite(px(8.).into())),
            left: Some(Length::Definite(px(0.).into())),
            right: Some(Length::Definite(px(0.).into())),
            bottom: Some(Length::Definite(px(12.).into())),
        },
        border_style: Some(BorderStyle::Solid),
        border_widths: EdgesRefinement {
            top: Some(AbsoluteLength::Pixels(px(1.))),
            left: Some(AbsoluteLength::Pixels(px(1.))),
            right: Some(AbsoluteLength::Pixels(px(1.))),
            bottom: Some(AbsoluteLength::Pixels(px(1.))),
        },
        border_color: Some(rgb(theme.border).into()),
        background: Some(rgb(theme.code_bg).into()),
        text: TextStyleRefinement {
            font_family: Some("Menlo".into()),
            font_size: Some(px(12.).into()),
            line_height: Some(relative(1.5)),
            color: Some(rgb(theme.code_text).into()),
            ..Default::default()
        },
        ..Default::default()
    };
    let radius = Some(AbsoluteLength::Pixels(px(6.)));
    code_block.corner_radii.top_left = radius;
    code_block.corner_radii.top_right = radius;
    code_block.corner_radii.bottom_left = radius;
    code_block.corner_radii.bottom_right = radius;

    let mut style = MarkdownStyle {
        base_text_style: TextStyle {
            font_family: ".SystemUIFont".into(),
            font_size: px(13.).into(),
            line_height: px(21.).into(),
            color: rgb(theme.text).into(),
            ..Default::default()
        },
        syntax: crate::syntax::syntax_theme(theme),
        selection_background_color: rgb(theme.accent_soft).into(),
        rule_color: rgb(theme.border).into(),
        block_quote_border_color: rgb(theme.border_strong).into(),
        block_quote_kind_colors: BlockQuoteKindColors {
            note: rgb(theme.accent).into(),
            tip: rgb(theme.success).into(),
            important: rgb(theme.accent).into(),
            warning: rgb(theme.warning).into(),
            caution: rgb(0xe06c6c).into(),
        },
        code_block_overflow_x_scroll: true,
        code_block,
        inline_code: TextStyleRefinement {
            font_family: Some("Menlo".into()),
            font_size: Some(px(12.).into()),
            color: Some(rgb(theme.text).into()),
            background_color: Some(rgb(theme.hover_bg).into()),
            ..Default::default()
        },
        link: TextStyleRefinement {
            color: Some(rgb(theme.accent).into()),
            underline: Some(UnderlineStyle {
                color: Some(rgb(theme.accent).into()),
                thickness: px(1.),
                ..Default::default()
            }),
            ..Default::default()
        },
        heading_level_styles: Some(HeadingLevelStyles {
            h1: Some(heading(1.2)),
            h2: Some(heading(1.1)),
            h3: Some(heading(1.05)),
            ..Default::default()
        }),
        paragraph_spacing: px(10.),
        paragraph_line_height: px(21.).into(),
        list_spacing: px(6.),
        table_cell_padding: point(px(8.), px(4.)),
        soft_break_as_hard_break: true,
        ..Default::default()
    };
    style.heading.text.color = Some(rgb(theme.text).into());
    style
}

fn heading(scale: f32) -> TextStyleRefinement {
    TextStyleRefinement {
        font_size: Some(rems(scale).into()),
        font_weight: Some(FontWeight::SEMIBOLD),
        line_height: Some(relative(1.3)),
        ..Default::default()
    }
}
