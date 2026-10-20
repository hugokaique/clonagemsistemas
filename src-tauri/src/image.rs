//! Imagens `.cfimg` — stream zstd + BLAKE3, com catálogo no SQLite.
//!
//! Layout do arquivo:
//! ```text
//! [ header 128 B ][ frame(s) zstd ][ footer 32 B = BLAKE3 do stream descomprimido ]
//! ```
//!
//! Header (little-endian):
//!   0..8   magic "CFIMG001"
//!   8..12  versão (u32 = 1)
//!  12..16  modo (u32): 0 = raw, 1 = smart-bitmap (Win), 2 = partclone (Linux)
//!  16..24  tamanho lógico da origem (u64)
//!  24..28  tamanho do bloco de I/O (u32)
//!  28..32  nível zstd (i32)
//!  32..36  algoritmo de hash (u32): 1 = BLAKE3
//!  36..128 reservado (zeros)
//!
//! Como gaps livres são sintetizados como zeros (Windows) ou o partclone já
//! produz stream sparse-aware (Linux), o restore é sempre um único loop de
//! decompress→write, idêntico para raw e smart.
//!
//! Live copy (Windows): com `live = true`, o volume de origem é congelado via
//! snapshot VSS (Win32_ShadowCopy) e a leitura acontece do dispositivo de
//! shadow copy — permite imagem consistente do volume do sistema em execução.
//!
//! A pipeline roda em `spawn_blocking`: compressão zstd é CPU-bound e as
//! chamadas Win32 (DeviceIoControl) são síncronas por natureza.

use serde::{Deserialize, Serialize};
use std::io::{Read, Seek, SeekFrom, Write};
use std::time::Instant;
use tauri::{AppHandle, Manager};

use crate::clone::{emit, CloneProgress};

pub const CFIMG_MAGIC: &[u8; 8] = b"CFIMG001";
pub const HEADER_SIZE: usize = 128;
pub const FOOTER_SIZE: usize = 32; // BLAKE3
const IO_BUF: usize = 8 * 1024 * 1024;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageHeaderInfo {
    pub valid: bool,
    pub mode: u32,
    pub source_size: u64,
    pub block_size: u32,
    pub level: i32,
    pub hash_hex: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageRequest {
    /// disco (`\\.\PhysicalDrive1`), volume (`\\.\C:`) ou partição (`/dev/sdb1`, `/dev/disk4s2`)
    pub source: String,
    pub source_size: u64,
    /// caminho de saída do .cfimg (ou .dmg no macOS smart)
    pub output: String,
    /// nível zstd 1..19 (default 3 — sweet spot ratio/velocidade)
    pub level: Option<i32>,
    /// true = copiar só blocos alocados
    pub smart: bool,
    /// filesystem p/ partclone no Linux (ext4, xfs, btrfs, ntfs, fat32, exfat)
    #[allow(dead_code)] // lido apenas no build Linux
    pub fs: Option<String>,
    /// Windows: criar snapshot VSS antes de ler (origem em uso / volume do sistema)
    pub live: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreRequest {
    pub image: String,
    pub target: String,
    pub verify: bool,
}

fn header_bytes(mode: u32, source_size: u64, level: i32) -> [u8; HEADER_SIZE] {
    let mut h = [0u8; HEADER_SIZE];
    h[0..8].copy_from_slice(CFIMG_MAGIC);
    h[8..12].copy_from_slice(&1u32.to_le_bytes());
    h[12..16].copy_from_slice(&mode.to_le_bytes());
    h[16..24].copy_from_slice(&source_size.to_le_bytes());
    h[24..28].copy_from_slice(&(IO_BUF as u32).to_le_bytes());
    h[28..32].copy_from_slice(&level.to_le_bytes());
    h[32..36].copy_from_slice(&1u32.to_le_bytes()); // 1 = BLAKE3
    h
}

#[tauri::command]
pub fn read_image_header(path: String) -> Result<ImageHeaderInfo, String> {
    let mut f = std::fs::File::open(&path).map_err(|e| format!("abrir {path}: {e}"))?;
    let mut h = [0u8; HEADER_SIZE];
    f.read_exact(&mut h).map_err(|e| format!("header: {e}"))?;
    if &h[0..8] != CFIMG_MAGIC {
        return Err("não é um arquivo .cfimg (magic inválido)".into());
    }
    f.seek(SeekFrom::End(-(FOOTER_SIZE as i64)))
        .map_err(|e| e.to_string())?;
    let mut foot = [0u8; FOOTER_SIZE];
    f.read_exact(&mut foot).map_err(|e| format!("footer: {e}"))?;
    Ok(ImageHeaderInfo {
        valid: true,
        mode: u32::from_le_bytes(h[12..16].try_into().unwrap()),
        source_size: u64::from_le_bytes(h[16..24].try_into().unwrap()),
        block_size: u32::from_le_bytes(h[24..28].try_into().unwrap()),
        level: i32::from_le_bytes(h[28..32].try_into().unwrap()),
        hash_hex: blake3::Hash::from_bytes(foot).to_hex().to_string(),
    })
}

// ─────────────────────────── Criar imagem ──────────────────────────────

#[tauri::command]
pub async fn create_image(app: AppHandle, req: ImageRequest) -> Result<String, String> {
    if req.source_size == 0 {
        return Err("tamanho da origem inválido".into());
    }
    let out_path = req.output.clone();
    let total = req.source_size;
    let res = execute_create(&app, &req).await;

    match &res {
        Ok((logical, hash, img_size, format)) => {
            let state = app.state::<crate::db::DbState>();
            crate::db::record_image(&state, &out_path, format, *img_size, hash);
            let ratio = if *logical > 0 {
                *img_size as f64 / *logical as f64 * 100.0
            } else {
                0.0
            };
            emit(
                &app,
                &CloneProgress {
                    phase: "done".into(),
                    bytes_done: *logical,
                    total_bytes: *logical,
                    percent: 100.0,
                    speed_mbps: 0.0,
                    eta_seconds: 0.0,
                    message: Some(format!(
                        "imagem OK — {logical} B lógicos → {img_size} B ({ratio:.1}%) · BLAKE3 {hash}"
                    )),
                },
            );
            Ok(hash.clone())
        }
        Err(e) => {
            emit(
                &app,
                &CloneProgress {
                    phase: "error".into(),
                    bytes_done: 0,
                    total_bytes: total,
                    percent: 0.0,
                    speed_mbps: 0.0,
                    eta_seconds: 0.0,
                    message: Some(e.clone()),
                },
            );
            Err(e.clone())
        }
    }
}

/// Roteador de execução: no Linux sem root, delega ao helper via pkexec.
async fn execute_create(
    app: &AppHandle,
    req: &ImageRequest,
) -> Result<(u64, String, u64, String), String> {
    #[cfg(target_os = "linux")]
    if !crate::privilege::is_root() {
        tracing::info!("sem root — delegando criação de imagem ao helper via pkexec");
        let v = crate::privilege::run_elevated(
            app,
            serde_json::json!({"job": "create_image", "request": req}),
        )
        .await?;
        return Ok((
            v["logical"].as_u64().unwrap_or(0),
            v["hash"].as_str().unwrap_or_default().to_string(),
            v["image_size"].as_u64().unwrap_or(0),
            v["format"].as_str().unwrap_or("cfimg").to_string(),
        ));
    }
    let app2 = app.clone();
    let req2 = req.clone();
    tokio::task::spawn_blocking(move || run_create(&req2, move |p| emit(&app2, p)))
        .await
        .map_err(|e| format!("task: {e}"))?
}

/// Retorna (bytes lógicos lidos, hash BLAKE3, tamanho do arquivo, formato).
pub fn run_create<F>(req: &ImageRequest, on_progress: F) -> Result<(u64, String, u64, String), String>
where
    F: Fn(&CloneProgress) + Send,
{
    let level = req.level.unwrap_or(3).clamp(1, 19);
    let started = Instant::now();

    // macOS smart: delega ao hdiutil (.dmg UDZO) — único formato que o asr
    // restaura corretamente em containers APFS (volumes selados).
    #[cfg(target_os = "macos")]
    if req.smart {
        crate::smart::platform::create_dmg(&req.source, &req.output)?;
        let hash = crate::smart::platform::hash_file(&req.output)?;
        let size = std::fs::metadata(&req.output).map(|m| m.len()).unwrap_or(0);
        return Ok((size, hash, size, "dmg".to_string()));
    }

    #[allow(unused_mut)]
    let mut mode = 0u32;
    #[allow(unused_mut)]
    let mut reader: Box<dyn Read + Send>;

    // Windows: com live=true, congela o volume via VSS e lê do shadow copy
    #[cfg(windows)]
    let mut _shadow_guard: Option<crate::vss::ShadowGuard> = None;

    #[cfg(windows)]
    {
        reader = if req.smart {
            mode = 1;
            let mut vol = req.source.clone();
            if req.live.unwrap_or(false) {
                // Win32_ShadowCopy.Create espera "C:\"; nossa fonte é "\\.\C:"
                let letter = vol.trim_start_matches("\\\\.\\").to_string();
                let sc = crate::vss::create(&format!("{letter}\\"))?;
                tracing::info!("VSS snapshot {} → {}", sc.id, sc.device_object);
                vol = sc.device_object.clone();
                _shadow_guard = Some(crate::vss::ShadowGuard(sc));
            }
            Box::new(crate::smart::platform::SparseVolumeReader::open(&vol)?)
        } else {
            Box::new(
                std::fs::File::open(&req.source)
                    .map_err(|e| format!("abrir origem `{}`: {e}", req.source))?,
            )
        };
    }
    #[cfg(target_os = "linux")]
    {
        reader = if req.smart {
            mode = 2;
            let fs = req
                .fs
                .clone()
                .ok_or("informe o filesystem (fs) para smart copy no Linux")?;
            Box::new(crate::smart::platform::partclone_spawn(&req.source, &fs)?)
        } else {
            Box::new(
                std::fs::File::open(&req.source)
                    .map_err(|e| format!("abrir origem `{}`: {e}", req.source))?,
            )
        };
    }
    #[cfg(target_os = "macos")]
    {
        reader = Box::new(
            std::fs::File::open(&req.source)
                .map_err(|e| format!("abrir origem `{}`: {e}", req.source))?,
        );
    }

    // ── Pipeline: read → BLAKE3 → zstd → arquivo ─────────────────────────
    let out_f = std::fs::File::create(&req.output)
        .map_err(|e| format!("criar `{}`: {e}", req.output))?;
    let mut w = std::io::BufWriter::with_capacity(IO_BUF, out_f);
    w.write_all(&header_bytes(mode, req.source_size, level))
        .map_err(|e| format!("header: {e}"))?;
    let mut enc = zstd::stream::write::Encoder::new(w, level).map_err(|e| e.to_string())?;

    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; IO_BUF];
    let mut done = 0u64;
    let mut last = Instant::now();
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| format!("ler origem @ offset {done}: {e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        enc.write_all(&buf[..n]).map_err(|e| format!("zstd: {e}"))?;
        done += n as u64;
        if last.elapsed().as_millis() >= 200 {
            on_progress(&CloneProgress::new("compressing", done, req.source_size, started, None));
            last = Instant::now();
        }
    }

    let mut w = enc.finish().map_err(|e| format!("finalizar zstd: {e}"))?;
    let hash = hasher.finalize();
    w.write_all(hash.as_bytes())
        .map_err(|e| format!("footer: {e}"))?;
    w.flush().map_err(|e| e.to_string())?;
    w.get_ref().sync_all().ok();

    let img_size = std::fs::metadata(&req.output).map(|m| m.len()).unwrap_or(0);
    Ok((done, hash.to_hex().to_string(), img_size, "cfimg".to_string()))
}

// ────────────────────────── Restaurar imagem ───────────────────────────

#[tauri::command]
pub async fn restore_image(app: AppHandle, req: RestoreRequest) -> Result<String, String> {
    // Trava de segurança: nunca restaurar sobre o disco do sistema
    let disks = crate::disk::list_disks().await.unwrap_or_default();
    if let Some(dst) = disks.iter().find(|d| d.path == req.target) {
        if dst.is_system {
            return Err("BLOQUEADO: o destino é o disco do sistema em execução".into());
        }
    }

    let image_path = req.image.clone();
    let target_path = req.target.clone();
    let do_verify = req.verify;
    let res = execute_restore(&app, &req).await;

    match &res {
        Ok((written, hash)) => {
            let state = app.state::<crate::db::DbState>();
            crate::db::record_job(
                &state,
                &image_path,
                &target_path,
                *written,
                Some(hash),
                Some(do_verify),
                "ok",
            );
            emit(
                &app,
                &CloneProgress {
                    phase: "done".into(),
                    bytes_done: *written,
                    total_bytes: *written,
                    percent: 100.0,
                    speed_mbps: 0.0,
                    eta_seconds: 0.0,
                    message: Some(format!(
                        "restore OK{} — BLAKE3 {hash}",
                        if do_verify { " (verificado)" } else { "" }
                    )),
                },
            );
            Ok(hash.clone())
        }
        Err(e) => {
            let state = app.state::<crate::db::DbState>();
            crate::db::record_job(&state, &image_path, &target_path, 0, None, Some(false), "error");
            emit(
                &app,
                &CloneProgress {
                    phase: "error".into(),
                    bytes_done: 0,
                    total_bytes: 0,
                    percent: 0.0,
                    speed_mbps: 0.0,
                    eta_seconds: 0.0,
                    message: Some(e.clone()),
                },
            );
            Err(e.clone())
        }
    }
}

/// Roteador de execução: no Linux sem root, delega ao helper via pkexec.
async fn execute_restore(app: &AppHandle, req: &RestoreRequest) -> Result<(u64, String), String> {
    #[cfg(target_os = "linux")]
    if !crate::privilege::is_root() {
        tracing::info!("sem root — delegando restore ao helper via pkexec");
        let v = crate::privilege::run_elevated(
            app,
            serde_json::json!({"job": "restore", "request": req}),
        )
        .await?;
        return Ok((
            v["written"].as_u64().unwrap_or(0),
            v["hash"].as_str().unwrap_or_default().to_string(),
        ));
    }
    let app2 = app.clone();
    let req2 = req.clone();
    tokio::task::spawn_blocking(move || run_restore(&req2, move |p| emit(&app2, p)))
        .await
        .map_err(|e| format!("task: {e}"))?
}

pub fn run_restore<F>(req: &RestoreRequest, on_progress: F) -> Result<(u64, String), String>
where
    F: Fn(&CloneProgress) + Send,
{
    let started = Instant::now();
    let mut f = std::fs::File::open(&req.image)
        .map_err(|e| format!("abrir imagem `{}`: {e}", req.image))?;
    let file_len = f.metadata().map_err(|e| e.to_string())?.len();
    if file_len < (HEADER_SIZE + FOOTER_SIZE) as u64 {
        return Err("arquivo pequeno demais para ser .cfimg".into());
    }

    let mut h = [0u8; HEADER_SIZE];
    f.read_exact(&mut h).map_err(|e| format!("header: {e}"))?;
    if &h[0..8] != CFIMG_MAGIC {
        return Err("não é um arquivo .cfimg (magic inválido)".into());
    }
    let source_size = u64::from_le_bytes(h[16..24].try_into().unwrap());

    // Hash armazenado no footer (lido separadamente; o decoder é limitado
    // exatamente ao frame comprimido para não engolir o footer).
    let mut f2 = std::fs::File::open(&req.image).map_err(|e| e.to_string())?;
    f2.seek(SeekFrom::End(-(FOOTER_SIZE as i64))).map_err(|e| e.to_string())?;
    let mut stored = [0u8; FOOTER_SIZE];
    f2.read_exact(&mut stored).map_err(|e| format!("footer: {e}"))?;

    let comp_len = file_len - HEADER_SIZE as u64 - FOOTER_SIZE as u64;
    let limited = f.take(comp_len);
    let mut dec = zstd::stream::read::Decoder::new(std::io::BufReader::with_capacity(IO_BUF, limited))
        .map_err(|e| format!("zstd decoder: {e}"))?;

    // Mesma heurística do clone: dispositivos de bloco existem; alvos
    // arquivo (testes / discos virtuais) são criados.
    let target_is_device = req.target.starts_with("\\\\.\\") || req.target.starts_with("/dev/");
    let mut dst_opts = std::fs::OpenOptions::new();
    dst_opts.write(true);
    if !target_is_device {
        dst_opts.create(true).truncate(true);
    }
    let mut dst = dst_opts
        .open(&req.target)
        .map_err(|e| format!("abrir destino `{}`: {e} (desmontado? admin/root?)", req.target))?;

    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; IO_BUF];
    let mut done = 0u64;
    let mut last = Instant::now();
    loop {
        let n = dec
            .read(&mut buf)
            .map_err(|e| format!("descompressão @ offset {done}: {e}"))?;
        if n == 0 {
            break;
        }
        dst.write_all(&buf[..n])
            .map_err(|e| format!("escrita no destino @ offset {done}: {e}"))?;
        hasher.update(&buf[..n]);
        done += n as u64;
        if last.elapsed().as_millis() >= 200 {
            on_progress(&CloneProgress::new("restoring", done, source_size, started, None));
            last = Instant::now();
        }
    }
    dst.sync_all().map_err(|e| format!("fsync: {e}"))?;

    if done != source_size {
        return Err(format!(
            "stream restaurado com {done} bytes, mas o header declara {source_size}"
        ));
    }
    if req.verify && hasher.finalize().as_bytes() != &stored {
        return Err("VERIFICAÇÃO FALHOU — hash do restore ≠ hash gravado na imagem. NÃO inicialize por este disco.".into());
    }
    Ok((done, blake3::Hash::from_bytes(stored).to_hex().to_string()))
}
