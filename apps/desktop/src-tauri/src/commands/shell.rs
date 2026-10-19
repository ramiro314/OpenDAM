use std::path::Path;

type Result<T> = std::result::Result<T, String>;

/// Shows the file selected in the file manager (FileManager1 over D-Bus, which
/// Dolphin and Nautilus implement). If that fails, for example because the file
/// is gone, opens its parent folder instead.
#[tauri::command]
pub async fn reveal_in_file_manager(file_path: String) -> Result<()> {
    // The reveal is a blocking D-Bus call that can wait for the bus timeout, so
    // keep it off the async runtime's workers. A sync command would block the
    // main thread instead.
    tauri::async_runtime::spawn_blocking(move || {
        let Err(e) = tauri_plugin_opener::reveal_item_in_dir(&file_path) else {
            return Ok(());
        };
        tracing::warn!("Reveal of {file_path} failed ({e}), opening its parent folder instead");
        let parent = Path::new(&file_path)
            .parent()
            .ok_or("File has no parent folder")?;
        tauri_plugin_opener::open_path(parent, None::<&str>).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn open_with_default(file_path: String) -> Result<()> {
    tauri_plugin_opener::open_path(&file_path, None::<&str>).map_err(|e| e.to_string())
}

/// Opens a pack's source URL in the browser. Only a web address opens, so a stored URL can't
/// start a program or open a local file.
#[tauri::command]
pub async fn open_url(url: String) -> Result<()> {
    if !crate::commands::folder::is_web_url(&url) {
        return Err("Only http:// and https:// addresses open".into());
    }
    tauri_plugin_opener::open_url(&url, None::<&str>).map_err(|e| e.to_string())
}
