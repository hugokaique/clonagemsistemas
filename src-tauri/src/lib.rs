//! CloneForge — núcleo da aplicação Tauri v2.
//!
//! Módulos:
//! - `disk`:  enumeração de discos físicos/partições por SO, com marcação de disco de sistema.
//! - `clone`: motor assíncrono de cópia em blocos + verificação BLAKE3 + progresso via eventos.
//! - `db`:    SQLite local (WAL) — histórico de clonagens, catálogo de imagens e auditoria.

pub mod clone;
mod db;
mod disk;
pub mod image;
mod privilege;
mod smart;
mod vss;

use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tracing_subscriber::fmt::init();

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .setup(|app| {
            let dir = app
                .path()
                .app_data_dir()
                .expect("não foi possível resolver o app_data_dir");
            std::fs::create_dir_all(&dir).ok();
            let conn = db::init(&dir.join("cloneforge.db")).expect("falha ao abrir o SQLite");
            app.manage(db::DbState(std::sync::Mutex::new(conn)));
            tracing::info!("banco local em {}", dir.display());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            disk::list_disks,
            clone::start_clone,
            db::list_history,
            db::list_images,
            image::create_image,
            image::restore_image,
            image::read_image_header,
        ])
        .run(tauri::generate_context!())
        .expect("erro fatal ao iniciar o CloneForge");
}
