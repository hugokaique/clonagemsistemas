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

/// 8 MiB — otimizado para NVMe/SSD; mínimo forçado de 1 MiB
pub const DEFAULT_BLOCK_SIZE: usize = 8 * 1024 * 1024;

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
    /// "copying" | "flushing" | "verifying" | "compressing" | "restoring" | "done" | "error"
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

#[tauri::command]
pub async fn start_clone(app: AppHandle, req: CloneRequest) -> Result<String, String> {
    // ── Revalidações de segurança (nunca confie só na UI) ────────────────
    if req.source == req.target {
        return Err("origem e destino são o mesmo dispositivo".into());
    }
    if req.source_size == 0 {
        return Err("tamanho da origem inválido (0 bytes)".into());
    }
    let disks = crate::disk::list_disks().await.unwrap_or_default();
    if let Some(dst) = disks.iter().find(|d| d.path == req.target) {
        if dst.is_system {
            return Err("BLOQUEADO: o destino é o disco do sistema em execução".into());
        }
        if dst.size_bytes < req.source_size {
            return Err(format!(
                "destino ({:.1} GiB) menor que a origem ({:.1} GiB)",
                dst.size_bytes as f64 / 2f64.powi(30),
                req.source_size as f64 / 2f64.powi(30)
            ));
        }
    }

    let job_id = blake3::hash(
        format!("{}→{}@{}", req.source, req.target, chrono::Utc::now().to_rfc3339()).as_bytes(),
    )
    .to_hex()
    .to_string();

    let app2 = app.clone();
    let jid = job_id.clone();
    tokio::spawn(async move {
        let result = execute_clone(&app2, &req).await;
        let (status, verify_ok, hash) = match &result {
            Ok(h) => ("ok", Some(req.verify), Some(h.clone())),
            Err(e) => {
                tracing::error!("clonagem falhou: {e}");
                ("error", Some(false), None)
            }
        };
        // Persistência no SQLite — histórico/auditoria
        let state = app2.state::<crate::db::DbState>();
        crate::db::record_job(
            &state,
            &req.source,
            &req.target,
            result.as_ref().map(|_| req.source_size).unwrap_or(0),
            hash.as_deref(),
            verify_ok,
            status,
        );
        let done = result.is_ok();
        let msg = match &result {
            Ok(h) => Some(format!("verificação OK — BLAKE3 {h}")),
            Err(e) => Some(e.clone()),
        };
        emit(
            &app2,
            &CloneProgress {
                phase: if done { "done".into() } else { "error".into() },
                bytes_done: if done { req.source_size } else { 0 },
                total_bytes: req.source_size,
                percent: if done { 100.0 } else { 0.0 },
                speed_mbps: 0.0,
                eta_seconds: 0.0,
                message: msg,
            },
        );
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
        return Ok(v["hash"].as_str().unwrap_or_default().to_string());
    }
    run_clone(req, |p| emit(app, p)).await
}

/// Núcleo da cópia raw. `on_progress` é chamado no máximo a cada 200 ms
/// (além das mudanças de fase), desacoplando o motor do canal de eventos.
pub async fn run_clone<F>(req: &CloneRequest, on_progress: F) -> Result<String, String>
where
    F: Fn(&CloneProgress) + Send,
{
    let block = req
        .block_size
        .unwrap_or(DEFAULT_BLOCK_SIZE)
        .max(1024 * 1024);
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
        let mut vh = blake3::Hasher::new();
        let mut vdone: u64 = 0;
        loop {
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
                on_progress(&CloneProgress::new("verifying", vdone, done, started, None));
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
