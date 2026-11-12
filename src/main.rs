// A GUI program: no console window in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() -> eframe::Result {
    // `playr <file-or-url>`
    let initial = std::env::args().nth(1).filter(|a| !a.trim().is_empty());
    playr::ui::run(initial)
}
