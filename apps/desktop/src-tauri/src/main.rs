fn main() {
    // WebKitGTK's DMA-BUF renderer crashes on Wayland with NVIDIA drivers.
    // Set this before Tauri starts any threads; an explicit value still wins.
    if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
        std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
    }

    opendam_desktop_lib::run();
}
