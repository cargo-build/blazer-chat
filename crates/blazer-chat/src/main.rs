//! blazer-chat — application entry point.
//!
//! Layers, one owner each: [`http`] carries bytes, [`rc`] speaks the
//! Rocket.Chat REST protocol, [`app`] owns the command/event boundary and
//! the compio thread, [`ui`] owns the GPUI window. `async-channel` carries
//! events and requests across the boundary — channels, not nested executors.

mod app;
mod control;
mod http;
mod rc;
mod settings;
mod store;
mod tasks;
mod ui;

fn main() {
    let settings = match settings::AppSettings::load() {
        Ok(settings) => settings,
        Err(error) => {
            eprintln!("blazer-chat: {error}");
            std::process::exit(1);
        }
    };
    ui::run(settings);
}
