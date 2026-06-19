fn main() {
    if let Err(e) = cutter::gui::run() {
        eprintln!("cutter-gui error: {e}");
        std::process::exit(1);
    }
}
