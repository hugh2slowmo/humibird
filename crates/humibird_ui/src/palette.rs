//! Application palettes.
//!
//! Deliberately local rather than Zed's theme registry: the shell must render
//! before any settings store or theme is initialized, and the conversation
//! surface is small enough to keep an explicit palette.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Appearance {
    Dark,
    Light,
}

impl Appearance {
    pub fn toggled(self) -> Self {
        match self {
            Self::Dark => Self::Light,
            Self::Light => Self::Dark,
        }
    }

    /// A stable debug selector, so tests can assert which palette is active.
    pub fn selector(self) -> &'static str {
        match self {
            Self::Dark => "theme-dark",
            Self::Light => "theme-light",
        }
    }
}

#[derive(Clone, Copy)]
pub struct Palette {
    pub window_bg: u32,
    pub sidebar_bg: u32,
    pub panel_bg: u32,
    pub hover_bg: u32,
    pub selected_bg: u32,
    pub user_bubble: u32,
    pub border: u32,
    pub border_strong: u32,
    pub code_bg: u32,
    pub code_text: u32,

    pub text: u32,
    pub text_secondary: u32,
    pub text_muted: u32,
    pub text_dim: u32,
    pub text_on_accent: u32,

    pub accent: u32,
    pub accent_soft: u32,
    pub success: u32,
    pub warning: u32,
    pub warning_soft: u32,
    pub warning_border: u32,

    pub syntax_keyword: u32,
    pub syntax_string: u32,
    pub syntax_function: u32,
    pub syntax_type: u32,
    pub syntax_number: u32,
    pub syntax_comment: u32,
}

pub const DARK: Palette = Palette {
    window_bg: 0x0e0e12,
    sidebar_bg: 0x131318,
    panel_bg: 0x16161c,
    hover_bg: 0x1e1e26,
    selected_bg: 0x222230,
    user_bubble: 0x24242e,
    border: 0x24242c,
    border_strong: 0x33333e,
    code_bg: 0x0b0b0f,
    code_text: 0xc8c8d6,

    text: 0xe8e8f0,
    text_secondary: 0xa4a4b4,
    text_muted: 0x7c7c8c,
    text_dim: 0x5c5c6a,
    text_on_accent: 0x0a1020,

    accent: 0x6aa6ff,
    accent_soft: 0x1a2438,
    success: 0x55c79c,
    warning: 0xe6b567,
    warning_soft: 0x2a2114,
    warning_border: 0x5c4520,

    syntax_keyword: 0xc792ea,
    syntax_string: 0x9ece6a,
    syntax_function: 0x82aaff,
    syntax_type: 0xe5c07b,
    syntax_number: 0xf78c6c,
    syntax_comment: 0x5f6672,
};

pub const LIGHT: Palette = Palette {
    window_bg: 0xf7f7fa,
    sidebar_bg: 0xf0f0f5,
    panel_bg: 0xfbfbfe,
    hover_bg: 0xe8e8f0,
    selected_bg: 0xe3e3ee,
    user_bubble: 0xeaeaf3,
    border: 0xe2e2ea,
    border_strong: 0xcbcbd8,
    code_bg: 0xf4f4f8,
    code_text: 0x33333f,

    text: 0x1c1c24,
    text_secondary: 0x4c4c5a,
    text_muted: 0x6d6d7d,
    text_dim: 0x9797a7,
    text_on_accent: 0xffffff,

    accent: 0x2f6fd0,
    accent_soft: 0xdfe9fb,
    success: 0x1f8a63,
    warning: 0xa8700f,
    warning_soft: 0xfdf4e4,
    warning_border: 0xe6cfa2,

    syntax_keyword: 0x7a3e9d,
    syntax_string: 0x2f7d32,
    syntax_function: 0x2f6fd0,
    syntax_type: 0x8a5a00,
    syntax_number: 0xb45309,
    syntax_comment: 0x8a8a99,
};

impl Palette {
    pub fn for_appearance(appearance: Appearance) -> Self {
        match appearance {
            Appearance::Dark => DARK,
            Appearance::Light => LIGHT,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appearances_toggle_between_palettes() {
        assert_eq!(Appearance::Dark.toggled(), Appearance::Light);
        assert_eq!(Appearance::Light.toggled(), Appearance::Dark);
    }

    #[test]
    fn the_two_palettes_are_inverted() {
        let dark = Palette::for_appearance(Appearance::Dark);
        let light = Palette::for_appearance(Appearance::Light);

        assert_ne!(dark.window_bg, light.window_bg);
        assert_ne!(dark.text, light.text);
        assert_ne!(dark.sidebar_bg, light.sidebar_bg);
        assert_ne!(dark.accent, light.accent);
    }
}
