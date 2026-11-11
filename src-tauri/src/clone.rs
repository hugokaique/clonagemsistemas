//! Motor de clonagem assíncrono em blocos + verificação de integridade.
//!
//! Pipeline single-pass na origem:
//!   read(src) ──> update(BLAKE3) ──> write(dst)
//!
//! - Buffer padrão de 8 MiB: bom equilíbrio entre throughput em NVMe/SSD
//!   (IOPS de fila profunda) e pressão de memória.
//! - Progresso via callback (a UI emite `clone://progress` a cada 200 ms;
//!   o helper Linux emite JSON-lines no stdout para o processo pai reemitir).
//! - Verificação pós-clone: relê APENAS o destino e compara o hash BLAKE3
//!   com o calculado durante a escrita — a origem nunca é relida.
//! - Ao final, grava o resultado no SQLite (histórico + auditoria).
//!
//! TODO (produção):
//! - Windows: abrir com FILE_FLAG_NO_BUFFERING | FILE_FLAG_WRITE_THROUGH e
//!   buffers alinhados a 4096 (SetFilePointerEx + DeviceIoControl FSCTL_LOCK_VOLUME).
//! - Linux: open() com O_DIRECT + BLKGETSIZE64 para tamanho real do dispositivo.
//! - macOS: /dev/rdiskX já é raw; avaliar F_NOCACHE via fcntl.

use serde::{Deserialize, Serialize};
use std::time::Instant;
use tauri::{AppHandle, Emitter, Manager};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// 8 MiB — otimizado para NVMe/SSD
pub const DEFAULT_BLOCK_SIZE: usize = 8 * 1024 * 1024;
/// Limites do bloco de I/O aceito do cliente (o valor vira um `Vec` alocado).
const MIN_BLOCK_SIZE: usize = 1024 * 1024;
const MAX_BLOCK_SIZE: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CloneRequest {
    /// ex: `\\.\PhysicalDrive1` | `/dev/sdb` | `/dev/rdisk2`
    pub source: String,
    pub target: String,
    pub source_size: u64,
    pub block_size: Option<usize>,
    pub verify: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CloneProgress {
    /// "copying" | "flushing" | "verifying" | "compressing" | "restoring" | "done" | "error" | "cancelled"
    pub phase: String,
    pub bytes_done: u64,
    pub total_bytes: u64,
    pub percent: f64,
    pub speed_mbps: f64,
    pub eta_seconds: f64,
    pub message: Option<String>,
}

impl CloneProgress {
    pub(crate) fn new(phase: &str, done: u64, total: u64, started: Instant, message: Option<String>) -> Self {
        let elapsed = started.elapsed().as_secs_f64().max(0.001);
        let speed = done as f64 / elapsed / 1_048_576.0; // MiB/s ≈ MB/s p/ UI
        let remaining = total.saturating_sub(done) as f64;
        let eta = if speed > 0.0 {
            remaining / (speed * 1_048_576.0)
        } else {
            f64::INFINITY
        };
        Self {
            phase: phase.into(),
            bytes_done: done,
            total_bytes: total,
            percent: if total > 0 {
                (done as f64 / total as f64 * 100.0).min(100.0)
            } else {
                0.0
            },
            speed_mbps: speed,
            eta_seconds: eta,
            message,
        }
    }
}

pub(crate) fn emit(app: &AppHandle, p: &CloneProgress) {
    if let Err(e) = app.emit("clone://progress", p) {
        tracing::warn!("falha ao emitir progresso: {e}");
    }
}

/// Evento final de uma operação que não concluiu: `error` ou `cancelled`.
pub(crate) fn emit_failure(app: &AppHandle, total: u64, error: &str, cancel_note: &str) {
    let kind = crate::job::failure_kind(error);
    let message = if kind == "cancelled" {
        format!("{} — {cancel_note}", crate::job::CANCELLED)
    } else {
        error.to_string()
    };
    emit(
        app,
        &CloneProgress {
            phase: kind.into(),
            bytes_done: 0,
            total_bytes: total,
            percent: 0.0,
            speed_mbps: 0.0,
            eta_seconds: 0.0,
            message: Some(message),
        },
    );
}

/// Pede o cancelamento da operação em andamento (clonagem, imagem ou
/// restore). Sem operação rodando, não faz nada.
#[tauri::command]
pub async fn cancel_job() -> Result<(), String> {
    if !crate::job::is_running() {
        return Ok(());
    }
    crate::job::global().cancel();
    // No Linux sem root o trabalho está no helper elevado: repassa o pedido.
    #[cfg(target_os = "linux")]
    crate::privilege::forward_cancel().await;
    Ok(())
}

#[tauri::command]
pub async fn start_clone(app: AppHandle, req: CloneRequest) -> Result<String, String> {
    // Um job por vez: a posse acompanha a tarefa até ela terminar.
    let job = crate::job::begin()?;

    // ── Revalidações de segurança (nunca confie só na UI) ────────────────
    if req.source == req.target {
        return Err("origem e destino são o mesmo dispositivo".into());
    }
    if req.source_size == 0 {
        return Err("tamanho da origem inválido (0 bytes)".into());
    }
    // FAIL-CLOSED: sem enumeração confiável nada é escrito, e o destino tem
    // de ser exatamente um dos discos enumerados (ver `guard`).
    let disks = crate::disk::list_disks().await.map_err(|e| {
        format!("BLOQUEADO: falha ao enumerar os discos para validar o destino: {e}")
    })?;
    let (_, dst) = crate::guard::check_clone(&disks, &req.source, &req.target, req.source_size)?;
    // Quando o trabalho vai para o helper elevado, é ele quem desmonta.
    if !crate::elevated::delegates_to_helper() {
        crate::unmount::release_target(dst).await?;
    }

    let job_id = blake3::hash(
        format!("{}→{}@{}", req.source, req.target, chrono::Utc::now().to_rfc3339()).as_bytes(),
    )
    .to_hex()
    .to_string();

    let app2 = app.clone();
    let jid = job_id.clone();
    tokio::spawn(async move {
        let _job = job; // liberada quando a tarefa termina
        let row = crate::db::begin_job(
            &app2.state::<crate::db::DbState>(),
            &req.source,
            &req.target,
        );

        let result = execute_clone(&app2, &req).await;
        let state = app2.state::<crate::db::DbState>();
        match &result {
            Ok(hash) => {
                // `verify_ok` só é verdadeiro se a verificação realmente rodou.
                crate::db::finish_job(
                    &state,
                    row,
                    &req.source,
                    &req.target,
                    req.source_size,
                    Some(hash),
                    req.verify.then_some(true),
                    "ok",
                );
                let message = if req.verify {
                    format!("verificação OK — BLAKE3 {hash}")
                } else {
                    format!("cópia concluída SEM verificação — BLAKE3 da origem {hash}")
                };
                emit(
                    &app2,
                    &CloneProgress {
                        phase: "done".into(),
                        bytes_done: req.source_size,
                        total_bytes: req.source_size,
                        percent: 100.0,
                        speed_mbps: 0.0,
                        eta_seconds: 0.0,
                        message: Some(message),
                    },
                );
            }
            Err(e) => {
                tracing::error!("clonagem não concluída: {e}");
                crate::db::finish_job(
                    &state,
                    row,
                    &req.source,
                    &req.target,
                    0,
                    None,
                    None,
                    crate::job::failure_kind(e),
                );
                emit_failure(&app2, req.source_size, e, "o destino ficou incompleto");
            }
        }
    });

    Ok(jid)
}

/// Roteador de execução: no Linux sem root, delega ao helper elevado
/// via pkexec (Polkit); nos demais casos executa localmente.
pub async fn execute_clone(app: &AppHandle, req: &CloneRequest) -> Result<String, String> {
    #[cfg(target_os = "linux")]
    if !crate::privilege::is_root() {
        tracing::info!("sem root — delegando clonagem ao helper via pkexec");
        let v = crate::privilege::run_elevated(app, serde_json::json!({"job": "clone", "request": req})).await?;
        // Sem hash não há como afirmar que a cópia terminou: nunca é sucesso.
        return match v["hash"].as_str() {
            Some(hash) if !hash.is_empty() => Ok(hash.to_string()),
            _ => Err("o helper terminou sem informar o hash da cópia".into()),
        };
    }
    run_clone(req, |p| emit(app, p)).await
}

/// Descarta do page cache o que já está no disco, para a releitura de
/// verificação vir do dispositivo e não da memória. Só é necessário no
/// Linux: no macOS `/dev/rdiskN` não passa por cache, e no Windows o acesso
/// a `\\.\PhysicalDriveN` não usa o cache de arquivos.
#[cfg(target_os = "linux")]
pub(crate) fn drop_page_cache(file: &impl std::os::fd::AsRawFd) {
    // SAFETY: chamada simples sobre um descritor válido; o retorno é só um
    // conselho ao kernel e pode ser ignorado.
    unsafe {
        libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED);
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn drop_page_cache<T>(_file: &T) {}

/// Núcleo da cópia raw. `on_progress` é chamado no máximo a cada 200 ms
/// (além das mudanças de fase), desacoplando o motor do canal de eventos.
/// Cancelável pelo token do processo (`job::global`).
pub async fn run_clone<F>(req: &CloneRequest, on_progress: F) -> Result<String, String>
where
    F: Fn(&CloneProgress) + Send,
{
    run_clone_cancellable(req, &crate::job::global(), on_progress).await
}

pub async fn run_clone_cancellable<F>(
    req: &CloneRequest,
    cancel: &crate::job::CancelToken,
    on_progress: F,
) -> Result<String, String>
where
    F: Fn(&CloneProgress) + Send,
{
    let block = req
        .block_size
        .unwrap_or(DEFAULT_BLOCK_SIZE)
        .clamp(MIN_BLOCK_SIZE, MAX_BLOCK_SIZE);
    let total = req.source_size;

    let mut src = tokio::fs::File::open(&req.source)
        .await
        .map_err(|e| format!("abrir origem `{}`: {e} (permissão de administrador/root?)", req.source))?;
    // Destinos de bloco (\\.\PhysicalDriveN, /dev/sdX) já existem; alvos
    // arquivo (testes, imagens .raw) precisam ser criados.
    let target_is_device = req.target.starts_with("\\\\.\\") || req.target.starts_with("/dev/");
    let mut dst_opts = tokio::fs::OpenOptions::new();
    dst_opts.write(true);
    if !target_is_device {
        dst_opts.create(true).truncate(true);
    }
    let mut dst = dst_opts
        .open(&req.target)
        .await
        .map_err(|e| format!("abrir destino `{}`: {e} (disco desmontado/travado?)", req.target))?;

    let mut buf = vec![0u8; block];
    let mut hasher = blake3::Hasher::new();
    let mut done: u64 = 0;
    let started = Instant::now();
    let mut last_emit = Instant::now();

    // ── Fase 1: cópia raw bloco a bloco, hash on-the-fly ─────────────────
    loop {
        cancel.check()?;
        let n = src
            .read(&mut buf)
            .await
            .map_err(|e| format!("leitura da origem @ offset {done}: {e}"))?;
        if n == 0 {
            break;
        }
        dst.write_all(&buf[..n])
            .await
            .map_err(|e| format!("escrita no destino @ offset {done}: {e}"))?;
        hasher.update(&buf[..n]);
        done += n as u64;

        if last_emit.elapsed().as_millis() >= 200 {
            on_progress(&CloneProgress::new("copying", done, total, started, None));
            last_emit = Instant::now();
        }
    }

    // ── Fase 2: flush — garante que os dados saíram do cache p/ o device ─
    on_progress(&CloneProgress::new("flushing", done, total, started, None));
    // O `File` do tokio escreve em segundo plano: o erro da ÚLTIMA escrita só
    // aparece no flush. Sem ele, um destino com falha podia passar por OK.
    dst.flush()
        .await
        .map_err(|e| format!("escrita no destino @ offset {done}: {e}"))?;
    dst.sync_all()
        .await
        .map_err(|e| format!("fsync do destino: {e}"))?;
    drop(dst);

    let src_hash = hasher.finalize().to_hex().to_string();

    // ── Fase 3: verificação — relê só o destino e compara o BLAKE3 ───────
    if req.verify {
        let mut rd = tokio::fs::File::open(&req.target)
            .await
            .map_err(|e| format!("reabrir destino p/ verificação: {e}"))?;
        drop_page_cache(&rd);
        let mut vh = blake3::Hasher::new();
        let mut vdone: u64 = 0;
        // Cronômetro próprio: velocidade e ETA desta fase, não da cópia.
        let vstarted = Instant::now();
        loop {
            cancel.check()?;
            let n = rd
                .read(&mut buf)
                .await
                .map_err(|e| format!("verificação @ offset {vdone}: {e}"))?;
            if n == 0 || vdone >= done {
                break;
            }
            // Nunca hash além do que foi escrito (destino pode ser maior)
            let take = (n as u64).min(done - vdone) as usize;
            vh.update(&buf[..take]);
            vdone += take as u64;
            if last_emit.elapsed().as_millis() >= 200 {
                on_progress(&CloneProgress::new("verifying", vdone, done, vstarted, None));
                last_emit = Instant::now();
            }
        }
        let dst_hash = vh.finalize().to_hex().to_string();
        if dst_hash != src_hash {
            return Err(format!(
                "VERIFICAÇÃO FALHOU — origem {src_hash} ≠ destino {dst_hash}. NÃO use este clone."
            ));
        }
    }

    Ok(src_hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nada(_: &CloneProgress) {}

    fn req(dir: &std::path::Path, verify: bool, block_size: Option<usize>) -> CloneRequest {
        CloneRequest {
            source: dir.join("src.bin").to_string_lossy().into_owned(),
            target: dir.join("dst.bin").to_string_lossy().into_owned(),
            source_size: 0,
            block_size,
            verify,
        }
    }

    fn tmp(nome: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("cloneforge-clone-{}-{nome}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn clone_copia_verifica_e_limita_o_bloco() {
        let dir = tmp("ok");
        let dados: Vec<u8> = (0..2_500_000u32).map(|i| (i % 239) as u8).collect();
        std::fs::write(dir.join("src.bin"), &dados).unwrap();
        let token = crate::job::CancelToken::new();

        // bloco absurdo vindo do cliente é limitado, não alocado
        let hash = run_clone_cancellable(&req(&dir, true, Some(usize::MAX)), &token, nada)
            .await
            .unwrap();
        assert_eq!(hash, blake3::hash(&dados).to_hex().to_string());
        assert_eq!(std::fs::read(dir.join("dst.bin")).unwrap(), dados);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn clone_cancelado_para_antes_de_copiar() {
        let dir = tmp("cancelado");
        std::fs::write(dir.join("src.bin"), vec![1u8; 4096]).unwrap();
        let token = crate::job::CancelToken::new();
        token.cancel();
        let err = run_clone_cancellable(&req(&dir, true, None), &token, nada)
            .await
            .unwrap_err();
        assert_eq!(err, crate::job::CANCELLED);
        assert_eq!(std::fs::metadata(dir.join("dst.bin")).unwrap().len(), 0);
        std::fs::remove_dir_all(&dir).ok();
    }
}
