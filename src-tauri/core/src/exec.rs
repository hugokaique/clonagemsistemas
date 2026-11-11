//! Roteador de execução: decide ONDE cada operação já validada roda.
//!
//! - Linux sem root: delega ao helper elevado via pkexec (Polkit), que refaz
//!   a validação com privilégio e devolve progresso/resultado em JSON-lines.
//! - Demais casos (Windows elevado, root, macOS): executa localmente numa
//!   thread de I/O bloqueante.
//!
//! Fica no core para que o app Tauri não tenha nenhum `cfg` de plataforma.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use crate::clone::{CloneOutcome, ClonePlan, CloneRequest};
use crate::image::{ImageOutcome, ImagePlan, ImageRequest, RestoreOutcome, RestorePlan, RestoreRequest};
use crate::progress::CloneProgress;

/// Callback de progresso compartilhável entre threads/tarefas.
pub type SharedProgress = Arc<dyn Fn(&CloneProgress) + Send + Sync>;

async fn blocking<T, F>(f: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, String> + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| format!("task: {e}"))?
}

#[cfg(target_os = "linux")]
async fn delegate<T: serde::de::DeserializeOwned>(
    job: &str,
    request: serde_json::Value,
    cancel: Arc<AtomicBool>,
    on_progress: SharedProgress,
) -> Result<T, String> {
    tracing::info!("sem root — delegando `{job}` ao helper via pkexec");
    let v = crate::privilege::run_elevated(
        serde_json::json!({"job": job, "request": request}),
        cancel,
        on_progress,
    )
    .await?;
    serde_json::from_value(v).map_err(|e| format!("resposta inválida do helper: {e}"))
}

pub async fn clone(
    req: &CloneRequest,
    plan: ClonePlan,
    cancel: Arc<AtomicBool>,
    on_progress: SharedProgress,
) -> Result<CloneOutcome, String> {
    #[cfg(target_os = "linux")]
    if !crate::privilege::is_root() {
        // O helper refaz a validação como root; enviamos caminhos já canônicos.
        let mut elevated = req.clone();
        elevated.source = plan.source.path.clone();
        elevated.target = plan.target.path.clone();
        let request = serde_json::to_value(elevated).map_err(|e| e.to_string())?;
        return delegate("clone", request, cancel, on_progress).await;
    }
    let _ = req;
    blocking(move || crate::clone::run_clone(&plan, &cancel, &*on_progress)).await
}

pub async fn create_image(
    req: &ImageRequest,
    plan: ImagePlan,
    cancel: Arc<AtomicBool>,
    on_progress: SharedProgress,
) -> Result<ImageOutcome, String> {
    #[cfg(target_os = "linux")]
    if !crate::privilege::is_root() {
        // O helper roda em outro diretório (e como root): caminhos absolutos.
        let mut elevated = req.clone();
        elevated.source = plan.source.path.clone();
        elevated.output = plan.output.clone();
        elevated.fs = plan.fs.clone();
        let request = serde_json::to_value(elevated).map_err(|e| e.to_string())?;
        return delegate("create_image", request, cancel, on_progress).await;
    }
    let _ = req;
    blocking(move || crate::image::run_create(&plan, &cancel, &*on_progress)).await
}

pub async fn restore_image(
    req: &RestoreRequest,
    plan: RestorePlan,
    cancel: Arc<AtomicBool>,
    on_progress: SharedProgress,
) -> Result<RestoreOutcome, String> {
    #[cfg(target_os = "linux")]
    if !crate::privilege::is_root() {
        let mut elevated = req.clone();
        elevated.image = plan.image.clone();
        elevated.target = plan.target.path.clone();
        let request = serde_json::to_value(elevated).map_err(|e| e.to_string())?;
        return delegate("restore", request, cancel, on_progress).await;
    }
    let _ = req;
    blocking(move || crate::image::run_restore(&plan, &cancel, &*on_progress)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clone::tests::{noise, TempDir};
    use std::sync::Mutex;

    /// Fluxo completo como o app o executa (plan → exec), em arquivos.
    /// No Linux sem root o `exec` delegaria ao pkexec, então lá este teste
    /// só roda como root (CI em contêiner).
    #[test]
    fn fluxo_completo_clone_imagem_restore() {
        #[cfg(target_os = "linux")]
        if !crate::privilege::is_root() {
            eprintln!("ignorado: requer root no Linux (delegaria ao pkexec)");
            return;
        }
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let dir = TempDir::new("exec");
            let data = noise(3 * 1024 * 1024 + 99, 21);
            std::fs::write(dir.path("src.bin"), &data).unwrap();
            let phases = Arc::new(Mutex::new(Vec::<String>::new()));
            let seen = phases.clone();
            let progress: SharedProgress = Arc::new(move |p: &CloneProgress| {
                let mut ph = seen.lock().unwrap();
                if ph.last() != Some(&p.phase) {
                    ph.push(p.phase.clone());
                }
            });
            let cancel = || Arc::new(AtomicBool::new(false));

            let creq = CloneRequest {
                source: dir.path("src.bin"),
                target: dir.path("clone.bin"),
                source_size: 0,
                block_size: None,
                verify: true,
            };
            let plan = crate::clone::plan_clone(&creq).await.unwrap();
            let out = clone(&creq, plan, cancel(), progress.clone()).await.unwrap();
            assert!(out.verified);
            assert_eq!(std::fs::read(dir.path("clone.bin")).unwrap(), data);

            let ireq = ImageRequest {
                source: dir.path("src.bin"),
                source_size: 0,
                output: "img.cfimg".into(), // relativo → pasta base
                level: None,
                smart: false,
                fs: None,
                live: None,
            };
            let plan = crate::image::plan_create(&ireq, Some(&dir.0)).await.unwrap();
            let img = create_image(&ireq, plan, cancel(), progress.clone()).await.unwrap();
            assert_eq!(img.output, dir.path("img.cfimg"));
            assert_eq!(img.hash, out.hash);

            let rreq = RestoreRequest { image: img.output.clone(), target: dir.path("volta.bin"), verify: true };
            let plan = crate::image::plan_restore(&rreq).await.unwrap();
            let r = restore_image(&rreq, plan, cancel(), progress.clone()).await.unwrap();
            assert!(r.verified);
            assert_eq!(std::fs::read(dir.path("volta.bin")).unwrap(), data);

            let ph = phases.lock().unwrap().clone();
            for expected in ["preparing", "copying", "verifying", "compressing", "restoring"] {
                assert!(ph.iter().any(|p| p == expected), "fase {expected} ausente em {ph:?}");
            }
        });
    }
}
