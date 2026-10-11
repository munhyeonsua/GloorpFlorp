// No console window in release builds on Windows.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod engine;
mod settings;
mod ui;

fn main() -> anyhow::Result<()> {
    todo!()
}
