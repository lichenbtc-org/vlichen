//! The vlichen wallet as an app: vlichen-gui's page in a webview, its API a
//! command rather than HTTP on 127.0.0.1. Its wallets and settings in the
//! app's data directory.

use std::sync::Arc;

use vlichen_gui::app::App;
use tauri::Manager;

struct Wallet(Arc<App>);

/// The page's API call: what vlichen-gui serves at `url` (`/api/...`, with
/// its query), its response's body; an error's text.
#[tauri::command]
async fn api(state: tauri::State<'_, Wallet>, method: String, url: String, body: Option<String>) -> Result<String, String> {
    let app = state.0.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let (path, query) = url.split_once('?').unwrap_or((&url, ""));
        vlichen_gui::api::route(&app, &method, path, query, body.as_deref().unwrap_or("")).map(|(v, _)| v).map_err(|e| format!("{e:#}"))
    })
    .await
    .map_err(|e| e.to_string())?
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    vlichen_gui::app::tune_malloc();
    tauri::Builder::default()
        // a backup saved, and one put back, through the system's file picker
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .setup(|t| {
            let dir = t.path().app_data_dir()?;
            let app = App::new(dir)?;
            // reconnecting waits on the network: not before the window
            let a = app.clone();
            std::thread::spawn(move || vlichen_gui::app::launch(&a));
            t.manage(Wallet(app));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![api])
        .run(tauri::generate_context!())
        .expect("the vlichen app");
}
