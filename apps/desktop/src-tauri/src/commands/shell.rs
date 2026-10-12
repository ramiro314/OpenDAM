use std::path::Path;

type Result<T> = std::result::Result<T, String>;

/// Shows the file selected in the file manager (FileManager1 over D-Bus, which
/// Dolphin and Nautilus implement). If that fails, for example because the file
/// is gone, opens its parent folder instead.
#[tauri::command]
pub async fn reveal_in_file_manager(file_path: String) -> Result<()> {
    if tauri_plugin_opener::reveal_item_in_dir(&file_path).is_ok() {
        return Ok(());
    }
    let parent = Path::new(&file_path).parent().ok_or("File has no parent folder")?;
    tauri_plugin_opener::open_path(parent, None::<&str>).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn open_with_default(file_path: String) -> Result<()> {
    tauri_plugin_opener::open_path(&file_path, None::<&str>).map_err(|e| e.to_string())
}
