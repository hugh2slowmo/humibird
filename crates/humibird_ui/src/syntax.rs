//! Tree-sitter grammars for code fences in the conversation.
//!
//! Only languages that show up in agent output are registered. Adding one means
//! adding the grammar crate and a line in `language_registry`.

use std::sync::Arc;

use gpui::{App, FontStyle, HighlightStyle, rgb};
use language::{Language, LanguageRegistry};
use theme::SyntaxTheme;

use crate::Palette;

pub fn language_registry(cx: &App) -> Arc<LanguageRegistry> {
    let registry = LanguageRegistry::new(cx.background_executor().clone());

    macro_rules! register {
        ($name:literal, $grammar:expr) => {{
            let config = grammars::load_config($name);
            let queries = grammars::load_queries($name);
            match Language::new(config, Some($grammar.into())).with_queries(queries) {
                Ok(language) => registry.add(Arc::new(language)),
                Err(error) => {
                    log::error!("failed to load the {} grammar: {error:#}", $name)
                }
            }
        }};
    }

    register!("rust", tree_sitter_rust::LANGUAGE);
    register!("python", tree_sitter_python::LANGUAGE);
    register!("typescript", tree_sitter_typescript::LANGUAGE_TYPESCRIPT);
    register!("tsx", tree_sitter_typescript::LANGUAGE_TSX);
    register!("json", tree_sitter_json::LANGUAGE);
    register!("bash", tree_sitter_bash::LANGUAGE);

    Arc::new(registry)
}

/// Syntax colors for fenced code blocks.
///
/// Zed's own syntax themes come from a full editor theme; humibird only needs
/// the scopes tree-sitter queries emit, so the map is explicit and follows the
/// active palette.
pub fn syntax_theme(palette: Palette) -> Arc<SyntaxTheme> {
    let color = |hex: u32| HighlightStyle {
        color: Some(rgb(hex).into()),
        ..Default::default()
    };

    let mut highlights = vec![
        ("keyword".to_string(), color(palette.syntax_keyword)),
        ("string".to_string(), color(palette.syntax_string)),
        ("function".to_string(), color(palette.syntax_function)),
        (
            "function.method".to_string(),
            color(palette.syntax_function),
        ),
        ("type".to_string(), color(palette.syntax_type)),
        ("constructor".to_string(), color(palette.syntax_type)),
        ("number".to_string(), color(palette.syntax_number)),
        ("constant".to_string(), color(palette.syntax_number)),
        ("escape".to_string(), color(palette.syntax_number)),
        ("variable".to_string(), color(palette.text)),
        ("property".to_string(), color(palette.text_secondary)),
        ("operator".to_string(), color(palette.text_secondary)),
        ("punctuation".to_string(), color(palette.text_muted)),
        ("tag".to_string(), color(palette.accent)),
        ("attribute".to_string(), color(palette.syntax_type)),
        ("label".to_string(), color(palette.syntax_function)),
    ];

    highlights.push((
        "comment".to_string(),
        HighlightStyle {
            color: Some(rgb(palette.syntax_comment).into()),
            font_style: Some(FontStyle::Italic),
            ..Default::default()
        },
    ));

    Arc::new(SyntaxTheme::new(highlights))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Appearance;

    #[test]
    fn common_scopes_have_colors() {
        let theme = syntax_theme(Palette::for_appearance(Appearance::Light));

        for scope in ["keyword", "string", "function", "type", "number", "comment"] {
            assert!(theme.style_for_name(scope).is_some(), "{scope} is unstyled");
        }
    }

    #[test]
    fn dotted_scopes_fall_back_to_their_prefix() {
        let theme = syntax_theme(Palette::for_appearance(Appearance::Dark));

        assert!(theme.highlight_id("string.escape").is_some());
        assert!(theme.highlight_id("keyword.control").is_some());
    }
}
