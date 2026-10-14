//! Nuiter.exe: the tray app, or (with "extract-service") one of its reader helpers.

#![windows_subsystem = "windows"]

mod app;
mod search_window;
mod settings_window;
mod shell;
mod ui;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a| a == "extract-service") {
        std::process::exit(nuiter_core::extract::service::run());
    }
    std::process::exit(app::run(args));
}
