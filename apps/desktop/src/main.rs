#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod app;

use std::path::PathBuf;

use clap::Parser;
use gpui_kit::*;
use wrongmemory_desktop::preferences::{Appearance, Preferences};
use wrongmemory_desktop::service;
use wrongmemory_ipc::DataRoot;

#[derive(Parser)]
#[command(version, about = "WrongMemory native knowledge workbench")]
struct Args {
    #[arg(long)]
    data_root: Option<PathBuf>,
    /// Installed WrongMemory CLI used only by the explicit Start service action.
    #[arg(long)]
    cli: Option<PathBuf>,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let root = DataRoot::resolve(args.data_root.as_deref())?;
    let client = service::Service::new(root.clone())?;
    let cli = service::find_cli(args.cli);
    let (preferences, settings_error) = match Preferences::load(&root) {
        Ok(p) => (p, None),
        Err(e) => (Preferences::default(), Some(e.to_string())),
    };
    application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            let mode = match preferences.appearance {
                Appearance::Dark => gpui_kit::component::ThemeMode::Dark,
                Appearance::Light => gpui_kit::component::ThemeMode::Light,
                Appearance::System => gpui_kit::component::ThemeMode::from(cx.window_appearance()),
            };
            gpui_kit::component::Theme::change(mode, None, cx);
            app::bind_keys(cx);
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
            gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                        None,
                        size(px(1280.), px(800.)),
                        cx,
                    ))),
                    window_min_size: Some(size(px(940.), px(600.))),
                    titlebar: Some(TitlebarOptions {
                        title: Some("WrongMemory".into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                cx,
                move |window, cx| {
                    cx.new(|cx| {
                        app::Workbench::new(
                            root,
                            cli,
                            client,
                            preferences,
                            settings_error,
                            window,
                            cx,
                        )
                    })
                },
            )
            .expect("open WrongMemory desktop");
            cx.activate(true);
        });
    Ok(())
}
