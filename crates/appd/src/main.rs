mod desktop;
mod fileindex;
mod font;
mod gitscan;
mod icon;
mod matcher;
mod mounts;
mod recents;
mod render;
mod socket;
mod state;
mod watcher;
mod wayland;

use std::process::ExitCode;

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    log::info!("appd starting");

    if let Err(e) = wayland::run() {
        log::error!("wayland: {e}");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}
