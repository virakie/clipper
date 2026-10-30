// Embeds assets/clipper.ico as the exe icon (Explorer, taskbar, Alt+Tab).
fn main() {
    embed_resource::compile("clipper.rc", embed_resource::NONE).manifest_optional().unwrap();
}
