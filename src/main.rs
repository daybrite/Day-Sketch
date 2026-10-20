fn main() {
    day::launch(
        day::WindowOptions::default()
            .title("Day Sketch")
            // A desktop-appropriate default size; mobile fills the screen regardless.
            .size(960.0, 640.0),
        dayapp::root,
    );
}
