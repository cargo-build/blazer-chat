//! rchat-blazar — application entry point.
//!
//! M0 wire-up: [`service`] owns the compio executor (a tako endpoint plus the
//! client loop that proves it serves), [`ui`] owns the GPUI window.
//! `async-channel` carries events and requests across the boundary — channels,
//! not nested executors.

mod service;
mod ui;

fn main() {
    ui::run();
}
