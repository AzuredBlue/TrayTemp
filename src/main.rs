#![cfg_attr(windows, windows_subsystem = "windows")]

mod platform;
#[allow(dead_code)]
mod sensor;
#[allow(dead_code)]
mod settings;
#[allow(dead_code)]
mod stats;
#[allow(dead_code)]
mod tooltip;

fn main() {
    platform::run()
}
