//! CloneForge — núcleo da aplicação Tauri v2.
//!
//! Módulos:
//! - `disk`:  enumeração de discos físicos/partições por SO, com marcação de disco de sistema.
//! - `clone`: motor assíncrono de cópia em blocos + verificação BLAKE3 + progresso via eventos.
//! - `db`:    SQLite local (WAL) — histórico de clonagens, catálogo de imagens e auditoria.
//! - `guard`: travas fail-closed do destino de escrita.
//! - `elevated`: execução validada de jobs dentro do helper privilegiado (Linux/pkexec).
//! - `job`:   um job por vez + cancelamento.
//! - `unmount`: desmontagem dos volumes do destino antes da escrita.

pub mod clone;
mod db;
mod disk;
pub mod elevated;
mod guard;
pub mod image;
pub mod job;
mod privilege;
mod smart;
mod unmount;
mod vss;

use tauri::{Emitter, Manager};

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tracing_subscriber::fmt::init();

    tauri::Builder::default()
        // Fechar a janela no meio de uma operação deixaria o destino pela
        // metade sem aviso: o fechamento é barrado e a interface é avisada.
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if job::is_running() {
                    api.prevent_close();
                    let _ = window.emit("app://close-blocked", ());
                }
            }
        })
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
            clone::cancel_job,
            db::list_history,
            db::list_images,
            image::create_image,
            image::restore_image,
            image::read_image_header,
            image::output_exists,
        ])
        .run(tauri::generate_context!())
        .expect("erro fatal ao iniciar o CloneForge");
}
