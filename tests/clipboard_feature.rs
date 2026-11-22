//! Pasting a link into the URL field needs eframe's `clipboard` feature (arboard on Windows);
//! without it Ctrl+V silently does nothing. The window itself cannot be driven headless, so
//! this checks the build configuration that turns pasting on.

fn read(name: &str) -> String {
    std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(name)).expect("read")
}

#[test]
fn eframe_is_built_with_the_clipboard_feature() {
    let manifest = read("Cargo.toml");
    let eframe = manifest.lines().find(|l| l.trim_start().starts_with("eframe")).expect("eframe dependency");

    let (_, features) = eframe.split_once(" features = [").expect("eframe lists its features");
    let features: Vec<&str> = features.split(']').next().unwrap_or("").split(',').map(|f| f.trim().trim_matches('"')).collect();
    assert!(features.contains(&"clipboard"), "{eframe}");
}

#[test]
fn the_lock_file_resolves_arboard() {
    let lock = read("Cargo.lock");

    assert!(lock.lines().any(|l| l.trim() == "name = \"arboard\""), "arboard is not in Cargo.lock");
}
