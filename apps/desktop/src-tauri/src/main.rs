fn main() {
    // Pick the WebKitGTK rendering default (FORCE_SHM on NVIDIA, where the DMA-BUF renderer
    // crashes on Wayland) before Tauri starts any threads. A variable you set yourself still wins.
    let render_choice = opendam_desktop_lib::render::apply_render_env();

    opendam_desktop_lib::run(render_choice);
}
