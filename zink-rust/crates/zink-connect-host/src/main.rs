#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[path = "../../zink-app/src/zink_connect_runtime.rs"]
mod zink_connect_runtime;

fn main() -> wry::Result<()> {
    zink_connect_runtime::run()
}
