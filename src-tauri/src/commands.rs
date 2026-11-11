//! Comandos Tauri (IPC) — camada fina sobre o `cloneforge-core`.
//!
//! Cada operação destrutiva segue o mesmo roteiro:
//!   trava global (`jobs`) → validação fail-closed (`plan_*`) → registro
//!   `running` no histórico → execução (`exec`) → resultado no histórico +
//!   evento final para a UI.
//!
//! Não há nenhum `cfg` de plataforma aqui: tudo que depende do SO está no core.

use std::sync::Arc;
use tauri::{AppHandle, Manager};

use cloneforge_core::clone::{plan_clone, CloneRequest};
use cloneforge_core::disk::DiskInfo;
use cloneforge_core::exec::{self, SharedProgress};
use cloneforge_core::image::{
    plan_create, plan_restore, ImageHeaderInfo, ImageRequest, RestoreRequest,
};
use cloneforge_core::safety::human;

use crate::db::{self, DbState};
use crate::jobs::{self, emit_progress, JobReport, JobState};

/// Pedido barrado na validação: nada foi tocado, mas a tentativa fica no log.
fn rejected(kind: &str, source: &str, target: &str, err: String) -> String {
    tracing::warn!("{kind} recusada na validação ({source} → {target}): {err}");
    err
}

fn progress_to_ui(app: &AppHandle) -> SharedProgress {
    let app = app.clone();
    Arc::new(move |p| emit_progress(&app, p))
}

#[tauri::command]
pub async fn list_disks() -> Result<Vec<DiskInfo>, String> {
    cloneforge_core::disk::list_disks().await
}

#[tauri::command]
pub fn read_image_header(path: String) -> Result<ImageHeaderInfo, String> {
    cloneforge_core::image::read_image_header(&path)
}

/// Inicia a clonagem e retorna imediatamente o id do job; progresso e
/// resultado chegam pelo evento `clone://progress`.
#[tauri::command]
pub async fn start_clone(app: AppHandle, req: CloneRequest) -> Result<String, String> {
    // Uma operação por vez: duas gravações simultâneas no mesmo disco seriam desastrosas.
    let job = app.state::<Arc<JobState>>().begin()?;
    let plan = plan_clone(&req).await.map_err(|e| rejected("clonagem", &req.source, &req.target, e))?;

    let db_id = db::job_start(&app.state::<DbState>(), "clone", &plan.source.path, &plan.target.path);
    let job_id = blake3::hash(
        format!("{}→{}@{}", plan.source.path, plan.target.path, chrono::Utc::now().to_rfc3339())
            .as_bytes(),
    )
    .to_hex()
    .to_string();

    let app2 = app.clone();
    tokio::spawn(async move {
        let total = plan.source.size;
        let result = exec::clone(&req, plan, job.cancel.clone(), progress_to_ui(&app2)).await;
        // O resultado chega à UI pelo evento final (o comando já retornou).
        let _ = jobs::conclude(
            &app2,
            db_id,
            total,
            result.map(|o| JobReport {
                bytes: o.bytes,
                message: format!(
                    "clonagem OK{} — BLAKE3 {}",
                    if o.verified { " (verificada)" } else { " (sem verificação)" },
                    o.hash
                ),
                hash: o.hash,
                verified: Some(o.verified),
            }),
        );
        drop(job); // libera a trava global só depois de registrar o resultado
    });

    Ok(job_id)
}

#[tauri::command]
pub async fn create_image(app: AppHandle, req: ImageRequest) -> Result<String, String> {
    let job = app.state::<Arc<JobState>>().begin()?;
    let base = crate::image_base_dir(&app);
    let plan = plan_create(&req, base.as_deref())
        .await
        .map_err(|e| rejected("imagem", &req.source, &req.output, e))?;
    let total = plan.source.size;
    let db_id = db::job_start(&app.state::<DbState>(), "image", &plan.source.path, &plan.output);

    let result = exec::create_image(&req, plan, job.cancel.clone(), progress_to_ui(&app)).await;
    if let Ok(o) = &result {
        db::record_image(&app.state::<DbState>(), &o.output, &o.format, o.image_size, &o.hash);
    }
    let outcome = jobs::conclude(
        &app,
        db_id,
        total,
        result.map(|o| {
            let ratio = if o.logical > 0 {
                o.image_size as f64 / o.logical as f64 * 100.0
            } else {
                0.0
            };
            JobReport {
                bytes: o.logical,
                message: format!(
                    "imagem OK — {} → {} ({ratio:.1}%) em {} · BLAKE3 {}",
                    human(o.logical),
                    human(o.image_size),
                    o.output,
                    o.hash
                ),
                hash: o.hash,
                verified: None,
            }
        }),
    );
    drop(job);
    outcome
}

#[tauri::command]
pub async fn restore_image(app: AppHandle, req: RestoreRequest) -> Result<String, String> {
    let job = app.state::<Arc<JobState>>().begin()?;
    let plan = plan_restore(&req).await.map_err(|e| rejected("restauração", &req.image, &req.target, e))?;
    let total = plan.header.as_ref().map(|h| h.source_size).unwrap_or(0);
    let db_id = db::job_start(&app.state::<DbState>(), "restore", &plan.image, &plan.target.path);

    let result = exec::restore_image(&req, plan, job.cancel.clone(), progress_to_ui(&app)).await;
    let outcome = jobs::conclude(
        &app,
        db_id,
        total,
        result.map(|o| JobReport {
            bytes: o.written,
            message: format!(
                "restore OK{} — BLAKE3 {}",
                if o.verified {
                    " (verificado por releitura do destino)"
                } else {
                    " (imagem íntegra; destino não relido)"
                },
                o.hash
            ),
            hash: o.hash,
            verified: Some(o.verified),
        }),
    );
    drop(job);
    outcome
}
