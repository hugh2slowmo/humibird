use gpui::{
    App, AppContext, Bounds, QuitMode, TitlebarOptions, WindowBounds, WindowOptions, point, px,
    size,
};
use gpui_platform::application;
use humibird_ui::HumibirdRoot;

mod logging;
mod shell_env;

fn main() {
    // Asked by the login shell during startup: print and leave before any window
    // machinery runs.
    if std::env::args().nth(1).as_deref() == Some(shell_env::PRINT_ENV) {
        shell_env::print_environment();
        return;
    }

    logging::init();
    // A bundle started from Finder has launchd's environment, which has no
    // toolchain in it; the `terminal` tool needs the one the person's shell has.
    shell_env::adopt_login_shell();

    // One window: closing it is the app being done, not a window being put away.
    // macOS would otherwise keep a windowless process running that ⌘Q has to find.
    application()
        // The window's icons come from here: `ui` asks for a path, not a file.
        .with_assets(humibird_ui::Assets)
        .with_quit_mode(QuitMode::LastWindowClosed)
        .run(|cx: &mut App| {
            humibird_ui::init(cx);

            let bounds = Bounds::centered(None, size(px(1280.), px(820.)), cx);

            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    window_min_size: Some(size(px(960.), px(600.))),
                    titlebar: Some(TitlebarOptions {
                        title: Some("humibird".into()),
                        appears_transparent: true,
                        traffic_light_position: Some(point(px(14.), px(18.))),
                    }),
                    ..Default::default()
                },
                |_, cx| cx.new(HumibirdRoot::new),
            )
            .expect("failed to open the main window");

            cx.activate(true);
        });
}
