//! CloneForge — aplicação Tauri v2 (GUI).
//!
//! O motor (enumeração, validação de segurança, clonagem, imagens) vive no
//! crate `cloneforge-core`; aqui ficam só a cola com a interface:
//! - `commands`: comandos IPC chamados pelo frontend.
//! - `jobs`:     uma operação por vez + cancelamento + evento final.
//! - `db`:       SQLite local (WAL) — histórico, catálogo de imagens e auditoria.

mod commands;
mod db;
mod jobs;

use std::path::Path;
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter, Manager};

const LOG_FILE: &str = "cloneforge.log";
const LOG_MAX_BYTES: u64 = 5 * 1024 * 1024;

/// Log em arquivo no diretório de dados do app. No Windows o binário de
/// release não tem console (`windows_subsystem = "windows"`): sem isto os
/// logs do motor simplesmente não existiam para diagnóstico.
fn init_logging(dir: &Path) {
    let path = dir.join(LOG_FILE);
    if std::fs::metadata(&path).map(|m| m.len() > LOG_MAX_BYTES).unwrap_or(false) {
        let _ = std::fs::rename(&path, dir.join(format!("{LOG_FILE}.old")));
    }
    let file = std::fs::OpenOptions::new().create(true).append(true).open(&path);
    let result = match file {
        Ok(f) => tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(Mutex::new(f))
            .try_init(),
        Err(_) => tracing_subscriber::fmt().try_init(),
    };
    if let Err(e) = result {
        eprintln!("log não inicializado: {e}");
    }
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct AppInfo {
    /// "windows" | "linux" | "macos"
    os: &'static str,
    version: String,
    log_path: String,
}

#[tauri::command]
fn app_info(app: AppHandle) -> AppInfo {
    AppInfo {
        os: std::env::consts::OS,
        version: app.package_info().version.to_string(),
        log_path: app
            .path()
            .app_data_dir()
            .map(|d| d.join(LOG_FILE).to_string_lossy().into_owned())
            .unwrap_or_default(),
    }
}

/// Pasta base para imagens: Documentos do usuário (o diretório atual de um
/// app elevado costuma ser System32 ou Program Files).
pub(crate) fn image_base_dir(app: &AppHandle) -> Option<std::path::PathBuf> {
    app.path().document_dir().ok().or_else(|| app.path().home_dir().ok())
}

/// Sugestão de arquivo de saída: `Documentos/cloneforge-AAAAMMDD-HHMM.cfimg`
#[tauri::command]
fn suggest_image_path(app: AppHandle) -> String {
    let name = format!("cloneforge-{}.cfimg", chrono::Local::now().format("%Y%m%d-%H%M"));
    match image_base_dir(&app) {
        Some(dir) => dir.join(name).to_string_lossy().into_owned(),
        None => name,
    }
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct OutputPath {
    /// Caminho absoluto que o backend vai usar
    path: String,
    exists: bool,
}

/// Resolve o caminho de saída digitado (relativo → Documentos) e informa se
/// já existe, para a UI pedir confirmação antes de sobrescrever.
#[tauri::command]
fn resolve_output(app: AppHandle, path: String) -> OutputPath {
    let abs = cloneforge_core::io::absolute(path.trim(), image_base_dir(&app).as_deref());
    OutputPath { exists: Path::new(&abs).exists(), path: abs }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(Arc::new(jobs::JobState::default()))
        .setup(|app| {
            let dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&dir)?;
            init_logging(&dir);
            let conn = db::open_or_recover(&dir.join("cloneforge.db"));
            app.manage(db::DbState(Mutex::new(conn)));
            tracing::info!("CloneForge {} — dados em {}", app.package_info().version, dir.display());
            Ok(())
        })
        // Fechar a janela no meio de uma gravação deixaria o destino pela
        // metade sem aviso: a UI mostra o alerta e o usuário cancela antes.
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if window.state::<Arc<jobs::JobState>>().is_busy() {
                    api.prevent_close();
                    let _ = window.emit("app://close-blocked", ());
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            app_info,
            suggest_image_path,
            resolve_output,
            commands::list_disks,
            commands::start_clone,
            commands::create_image,
            commands::restore_image,
            commands::read_image_header,
            jobs::cancel_operation,
            db::list_history,
            db::list_images,
            db::forget_image,
        ])
        .run(tauri::generate_context!())
        .expect("erro fatal ao iniciar o CloneForge");
}
