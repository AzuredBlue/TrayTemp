#![cfg_attr(windows, windows_subsystem = "windows")]

mod platform;
#[allow(dead_code)]
mod stats;

fn main() {
    platform::run()
}
