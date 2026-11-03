//! The window's icons.
//!
//! Zed's `ui` crate names an icon (`IconName::ChevronDown`) and resolves it to a
//! path like `icons/chevron_down.svg`; the bytes come from the app's
//! [`AssetSource`]. Without one installed, every `Icon` in the window is silently
//! skipped — the transcript's tool cards, the disclosure chevrons and the failure
//! cross all drew nothing, which is why the arrows were text glyphs instead.
//!
//! The files are embedded with `include_bytes!` rather than read from a directory,
//! so a bundle needs nothing beside the binary and a debug build behaves like a
//! release one. That is also how the sidebar's mark is loaded.

use std::borrow::Cow;

use gpui::{AssetSource, Result, SharedString};

/// Every icon the window asks for, copied from Zed's `assets/icons`; the
/// `LICENSES` file beside them covers the set (Lucide's ISC, and Zed's own).
const ICONS: &[(&str, &[u8])] = &[
    (
        "icons/arrow_right_left.svg",
        include_bytes!("../resources/icons/arrow_right_left.svg"),
    ),
    (
        "icons/book.svg",
        include_bytes!("../resources/icons/book.svg"),
    ),
    (
        "icons/chat.svg",
        include_bytes!("../resources/icons/chat.svg"),
    ),
    (
        "icons/chevron_down.svg",
        include_bytes!("../resources/icons/chevron_down.svg"),
    ),
    (
        "icons/chevron_right.svg",
        include_bytes!("../resources/icons/chevron_right.svg"),
    ),
    (
        "icons/server.svg",
        include_bytes!("../resources/icons/server.svg"),
    ),
    (
        "icons/folder.svg",
        include_bytes!("../resources/icons/folder.svg"),
    ),
    (
        "icons/sparkle.svg",
        include_bytes!("../resources/icons/sparkle.svg"),
    ),
    (
        "icons/tool_delete_file.svg",
        include_bytes!("../resources/icons/tool_delete_file.svg"),
    ),
    (
        "icons/tool_pencil.svg",
        include_bytes!("../resources/icons/tool_pencil.svg"),
    ),
    (
        "icons/tool_search.svg",
        include_bytes!("../resources/icons/tool_search.svg"),
    ),
    (
        "icons/tool_terminal.svg",
        include_bytes!("../resources/icons/tool_terminal.svg"),
    ),
    (
        "icons/tool_think.svg",
        include_bytes!("../resources/icons/tool_think.svg"),
    ),
    (
        "icons/tool_web.svg",
        include_bytes!("../resources/icons/tool_web.svg"),
    ),
    (
        "icons/x_circle.svg",
        include_bytes!("../resources/icons/x_circle.svg"),
    ),
];

/// What the window draws its icons from.
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        Ok(ICONS
            .iter()
            .find(|(name, _)| *name == path)
            .map(|(_, bytes)| Cow::Borrowed(*bytes)))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(ICONS
            .iter()
            .filter(|(name, _)| name.starts_with(path))
            .map(|(name, _)| SharedString::from(*name))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_embedded_icon_can_be_loaded() {
        for (name, _) in ICONS {
            let bytes = Assets
                .load(name)
                .expect("an embedded icon cannot fail to load");
            assert!(bytes.is_some_and(|bytes| !bytes.is_empty()), "{name}");
        }
    }

    #[test]
    fn a_path_nobody_embedded_is_not_an_error() {
        assert!(
            Assets
                .load("icons/no_such_icon.svg")
                .expect("no I/O")
                .is_none()
        );
        assert!(Assets.load("mark.png").expect("no I/O").is_none());
    }

    #[test]
    fn listing_names_the_icons_under_a_prefix() {
        let icons = Assets.list("icons/").expect("no I/O");
        assert_eq!(icons.len(), ICONS.len());
        assert!(icons.iter().any(|name| name == "icons/chevron_down.svg"));
        assert!(Assets.list("nope/").expect("no I/O").is_empty());
    }
}
