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
//! O restore escolhe o caminho pelo MODO gravado no header:
//! - raw (0) e smart-bitmap (1): o stream é a origem byte a byte (no smart, os
//!   gaps livres já vêm como zeros), então é gravado direto no destino;
//! - partclone (2): o stream está no formato do partclone e só pode ser
//!   entregue ao `partclone.restore` — gravá-lo cru destruiria o destino sem
//!   produzir um filesystem válido.
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

pub const MODE_RAW: u32 = 0;
pub const MODE_SMART_BITMAP: u32 = 1;
pub const MODE_PARTCLONE: u32 = 2;

/// Campos do header, já validados.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageHeader {
    pub mode: u32,
    pub source_size: u64,
    pub block_size: u32,
    pub level: i32,
}

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

/// Valida o header. Qualquer campo que este código não saiba interpretar
/// (versão, modo, algoritmo de hash) recusa a imagem: é melhor não restaurar
/// do que gravar no destino um stream mal interpretado.
fn parse_header(h: &[u8; HEADER_SIZE]) -> Result<ImageHeader, String> {
    if &h[0..8] != CFIMG_MAGIC {
        return Err("não é um arquivo .cfimg (magic inválido)".into());
    }
    let u32_at = |i: usize| u32::from_le_bytes(h[i..i + 4].try_into().unwrap());
    let version = u32_at(8);
    if version != 1 {
        return Err(format!("versão {version} do formato .cfimg não é suportada"));
    }
    let mode = u32_at(12);
    if mode > MODE_PARTCLONE {
        return Err(format!("modo {mode} desconhecido no header da imagem"));
    }
    let algo = u32_at(32);
    if algo != 1 {
        return Err(format!("algoritmo de hash {algo} desconhecido no header da imagem"));
    }
    Ok(ImageHeader {
        mode,
        source_size: u64::from_le_bytes(h[16..24].try_into().unwrap()),
        block_size: u32_at(24),
        level: i32::from_le_bytes(h[28..32].try_into().unwrap()),
    })
}

/// Lê e valida header e footer de uma imagem já aberta. Devolve o header, o
/// hash gravado e o tamanho do trecho comprimido; deixa o cursor no início
/// desse trecho.
pub fn read_header(f: &mut std::fs::File) -> Result<(ImageHeader, [u8; FOOTER_SIZE], u64), String> {
    let file_len = f.metadata().map_err(|e| e.to_string())?.len();
    if file_len < (HEADER_SIZE + FOOTER_SIZE) as u64 {
        return Err("arquivo pequeno demais para ser .cfimg".into());
    }
    f.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
    let mut h = [0u8; HEADER_SIZE];
    f.read_exact(&mut h).map_err(|e| format!("header: {e}"))?;
    let header = parse_header(&h)?;

    f.seek(SeekFrom::End(-(FOOTER_SIZE as i64)))
        .map_err(|e| e.to_string())?;
    let mut stored = [0u8; FOOTER_SIZE];
    f.read_exact(&mut stored).map_err(|e| format!("footer: {e}"))?;

    f.seek(SeekFrom::Start(HEADER_SIZE as u64))
        .map_err(|e| e.to_string())?;
    Ok((header, stored, file_len - (HEADER_SIZE + FOOTER_SIZE) as u64))
}

#[tauri::command]
pub fn read_image_header(path: String) -> Result<ImageHeaderInfo, String> {
    let mut f = std::fs::File::open(&path).map_err(|e| format!("abrir {path}: {e}"))?;
    let (h, stored, _) = read_header(&mut f)?;
    Ok(ImageHeaderInfo {
        valid: true,
        mode: h.mode,
        source_size: h.source_size,
        block_size: h.block_size,
        level: h.level,
        hash_hex: blake3::Hash::from_bytes(stored).to_hex().to_string(),
    })
}

// ─────────────────────────── Criar imagem ──────────────────────────────

/// Informa se já existe algo no caminho de saída (a interface usa para
/// avisar que uma imagem anterior será substituída).
#[tauri::command]
pub fn output_exists(path: String) -> bool {
    let path = path.trim();
    !path.is_empty() && std::path::Path::new(path).exists()
}

#[tauri::command]
pub async fn create_image(app: AppHandle, mut req: ImageRequest) -> Result<String, String> {
    let _job = crate::job::begin()?;
    if req.source_size == 0 {
        return Err("tamanho da origem inválido".into());
    }
    // macOS smart: o hdiutil acrescenta `.dmg` ao nome quando falta. Fixa o
    // nome aqui para o hash e o catálogo apontarem para o arquivo real.
    #[cfg(target_os = "macos")]
    if req.smart && !req.output.to_lowercase().ends_with(".dmg") {
        req.output.push_str(".dmg");
    }
    // A saída é texto livre na interface: nunca pode ser um dispositivo.
    crate::guard::check_image_output(&req.output)?;
    // A origem tem de ser um disco, partição ou volume conhecido.
    let disks = crate::disk::list_disks().await.map_err(|e| {
        format!("BLOQUEADO: falha ao enumerar os discos para validar a origem: {e}")
    })?;
    req.source = crate::guard::check_image_source(&disks, &req.source, req.smart, cfg!(windows))?;

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
            crate::clone::emit_failure(&app, total, e, "a imagem parcial foi descartada");
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
        // Sem hash não há como afirmar que a imagem está completa.
        let hash = match v["hash"].as_str() {
            Some(h) if !h.is_empty() => h.to_string(),
            _ => return Err("o helper terminou sem informar o hash da imagem".into()),
        };
        return Ok((
            v["logical"].as_u64().unwrap_or(0),
            hash,
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

/// Abre (criando, se preciso) um arquivo de imagem para escrita, SEM truncar.
/// A checagem "é arquivo comum" é feita no handle já aberto, então não há
/// janela entre validar o caminho e usá-lo. Arquivos novos nascem 0600: uma
/// imagem de disco contém tudo o que havia no disco.
pub fn open_output(path: &str) -> Result<std::fs::File, String> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let f = opts.open(path).map_err(|e| format!("criar `{path}`: {e}"))?;
    let meta = f.metadata().map_err(|e| format!("criar `{path}`: {e}"))?;
    if !meta.is_file() {
        return Err(crate::guard::output_refused(path));
    }
    Ok(f)
}

/// A imagem é gravada primeiro neste arquivo vizinho e só toma o lugar da
/// saída quando está completa. Assim uma falha (ou um cancelamento) nunca
/// destrói o backup anterior nem deixa um `.cfimg` truncado com cara de bom.
pub fn partial_path(output: &str) -> String {
    format!("{output}.partial")
}

/// Promove o arquivo parcial à saída definitiva (rename atômico).
pub fn commit_output(partial: &str, output: &str) -> Result<(), String> {
    std::fs::rename(partial, output).map_err(|e| {
        let _ = std::fs::remove_file(partial);
        format!("finalizar `{output}`: {e}")
    })
}

/// Descarta o arquivo parcial de uma criação que não terminou.
pub fn discard_output(partial: &str) {
    let _ = std::fs::remove_file(partial);
}

/// Retorna (bytes lógicos lidos, hash BLAKE3, tamanho do arquivo, formato).
pub fn run_create<F>(req: &ImageRequest, on_progress: F) -> Result<(u64, String, u64, String), String>
where
    F: Fn(&CloneProgress) + Send,
{
    // macOS smart: delega ao hdiutil (.dmg UDZO) — único formato que o asr
    // restaura corretamente em containers APFS (volumes selados).
    #[cfg(target_os = "macos")]
    if req.smart {
        crate::smart::platform::create_dmg(&req.source, &req.output)?;
        let hash = crate::smart::platform::hash_file(&req.output)?;
        let size = std::fs::metadata(&req.output).map(|m| m.len()).unwrap_or(0);
        return Ok((size, hash, size, "dmg".to_string()));
    }
    let partial = partial_path(&req.output);
    let cancel = crate::job::global();
    match run_create_with(req, || open_output(&partial), &cancel, on_progress) {
        Ok(result) => {
            commit_output(&partial, &req.output)?;
            Ok(result)
        }
        Err(e) => {
            discard_output(&partial);
            Err(e)
        }
    }
}

/// Núcleo da criação: escreve a imagem no arquivo devolvido por `open_out`
/// (que quem chama decide como abrir — o helper elevado o abre antes, com a
/// identidade do usuário). Não renomeia nem apaga nada: promover ou descartar
/// o arquivo é de quem chamou. `open_out` só é chamado, e o arquivo só é
/// truncado, depois que a origem abriu.
pub fn run_create_with<F, O>(
    req: &ImageRequest,
    open_out: O,
    cancel: &crate::job::CancelToken,
    on_progress: F,
) -> Result<(u64, String, u64, String), String>
where
    F: Fn(&CloneProgress) + Send,
    O: FnOnce() -> Result<std::fs::File, String>,
{
    let level = req.level.unwrap_or(3).clamp(1, 19);
    let started = Instant::now();

    #[cfg(target_os = "macos")]
    if req.smart {
        return Err("smart copy no macOS gera .dmg via hdiutil — use `run_create`".into());
    }

    #[allow(unused_mut)]
    let mut mode = MODE_RAW;

    // Windows: com live=true, congela o volume via VSS e lê do shadow copy.
    // Declarado ANTES do leitor de propósito: variáveis são destruídas na
    // ordem inversa, então o leitor fecha primeiro e só depois o snapshot é
    // apagado (o contrário deixava shadow copy órfão).
    #[cfg(windows)]
    let mut _shadow_guard: Option<crate::vss::ShadowGuard> = None;

    #[allow(unused_mut)]
    let mut reader: Box<dyn Read + Send>;

    #[cfg(windows)]
    {
        reader = if req.smart {
            mode = MODE_SMART_BITMAP;
            let mut vol = req.source.clone();
            if req.live.unwrap_or(false) {
                // Win32_ShadowCopy.Create espera "C:\"; nossa fonte é "\\.\C:".
                // A conversão valida a forma exata: o valor entra num script.
                let root = crate::guard::windows_volume_root(&vol)?;
                let sc = crate::vss::create(&root)?;
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
            mode = MODE_PARTCLONE;
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
    let out_f = open_out()?;
    out_f
        .set_len(0)
        .map_err(|e| format!("truncar `{}`: {e}", req.output))?;
    let mut w = std::io::BufWriter::with_capacity(IO_BUF, out_f);
    w.write_all(&header_bytes(mode, req.source_size, level))
        .map_err(|e| format!("header: {e}"))?;
    let mut enc = zstd::stream::write::Encoder::new(w, level).map_err(|e| e.to_string())?;
    // Checksum do frame: o decoder acusa corrupção mesmo sem conferir o hash.
    enc.include_checksum(true).map_err(|e| format!("zstd: {e}"))?;
    // Compressão em várias threads (a leitura e o BLAKE3 seguem nesta).
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(8) as u32;
    if workers > 1 {
        enc.multithread(workers).map_err(|e| format!("zstd: {e}"))?;
    }

    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; IO_BUF];
    let mut done = 0u64;
    let mut last = Instant::now();
    loop {
        cancel.check()?;
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
    // Erro de fsync é erro de gravação: a imagem pode não estar no disco.
    w.get_ref()
        .sync_all()
        .map_err(|e| format!("fsync da imagem: {e}"))?;

    let img_size = w.get_ref().metadata().map(|m| m.len()).unwrap_or(0);
    Ok((done, hash.to_hex().to_string(), img_size, "cfimg".to_string()))
}

// ────────────────────────── Restaurar imagem ───────────────────────────

/// Resultado de um restore.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreOutcome {
    /// Bytes do stream entregues ao destino.
    pub written: u64,
    /// BLAKE3 gravado na imagem (e conferido contra o stream restaurado).
    pub hash: String,
    /// true se o destino foi relido do disco e bateu com o hash.
    pub verified: bool,
}

#[tauri::command]
pub async fn restore_image(app: AppHandle, req: RestoreRequest) -> Result<String, String> {
    let _job = crate::job::begin()?;
    // Trava de segurança (fail-closed): nunca restaurar sobre o disco do
    // sistema, e só sobre um disco inteiro presente na enumeração.
    let disks = crate::disk::list_disks().await.map_err(|e| {
        format!("BLOQUEADO: falha ao enumerar os discos para validar o destino: {e}")
    })?;
    // Tamanho exigido = o declarado no header, quando a GUI consegue lê-lo
    // (no Linux a imagem pode ser legível só pelo helper elevado).
    let header = read_image_header(req.image.clone()).ok();
    let required = header.as_ref().map(|h| h.source_size).unwrap_or(0);
    let dst = crate::guard::check_write_target(&disks, &req.target, required)?;
    // Quando o trabalho vai para o helper elevado, é ele quem desmonta.
    if !crate::elevated::delegates_to_helper() {
        crate::unmount::release_target(dst).await?;
    }
    // Imagens smart são de UMA partição, e o destino é sempre um disco inteiro.
    let partition_note = match header.as_ref().map(|h| h.mode) {
        Some(m) if m != MODE_RAW => {
            " — imagem de partição: gravada a partir do início do disco, sem tabela de partição (não inicializável)"
        }
        _ => "",
    };

    let row = crate::db::begin_job(&app.state::<crate::db::DbState>(), &req.image, &req.target);
    let res = execute_restore(&app, &req).await;
    let state = app.state::<crate::db::DbState>();

    match &res {
        Ok(out) => {
            crate::db::finish_job(
                &state,
                row,
                &req.image,
                &req.target,
                out.written,
                Some(&out.hash),
                out.verified.then_some(true),
                "ok",
            );
            let how = if out.verified {
                " (verificado por releitura do destino)"
            } else {
                " (sem releitura do destino)"
            };
            emit(
                &app,
                &CloneProgress {
                    phase: "done".into(),
                    bytes_done: out.written,
                    total_bytes: out.written,
                    percent: 100.0,
                    speed_mbps: 0.0,
                    eta_seconds: 0.0,
                    message: Some(format!("restore OK{how} — BLAKE3 {}{partition_note}", out.hash)),
                },
            );
            Ok(out.hash.clone())
        }
        Err(e) => {
            crate::db::finish_job(
                &state,
                row,
                &req.image,
                &req.target,
                0,
                None,
                None,
                crate::job::failure_kind(e),
            );
            crate::clone::emit_failure(&app, 0, e, "o destino ficou incompleto");
            Err(e.clone())
        }
    }
}

/// Roteador de execução: no Linux sem root, delega ao helper via pkexec.
async fn execute_restore(app: &AppHandle, req: &RestoreRequest) -> Result<RestoreOutcome, String> {
    #[cfg(target_os = "linux")]
    if !crate::privilege::is_root() {
        tracing::info!("sem root — delegando restore ao helper via pkexec");
        let v = crate::privilege::run_elevated(
            app,
            serde_json::json!({"job": "restore", "request": req}),
        )
        .await?;
        // Sem hash não há como afirmar que o restore terminou.
        let hash = match v["hash"].as_str() {
            Some(h) if !h.is_empty() => h.to_string(),
            _ => return Err("o helper terminou sem informar o hash do restore".into()),
        };
        return Ok(RestoreOutcome {
            written: v["written"].as_u64().unwrap_or(0),
            hash,
            verified: v["verified"].as_bool().unwrap_or(false),
        });
    }
    let app2 = app.clone();
    let req2 = req.clone();
    tokio::task::spawn_blocking(move || run_restore(&req2, move |p| emit(&app2, p)))
        .await
        .map_err(|e| format!("task: {e}"))?
}

/// Destino do stream restaurado, conforme o modo da imagem.
enum Sink {
    /// Dispositivo ou arquivo: o stream é gravado byte a byte. Guarda o
    /// caminho para a releitura de verificação.
    Raw(std::fs::File, String),
    /// `partclone.restore` recebendo o stream pelo stdin.
    #[cfg(any(target_os = "linux", all(test, unix)))]
    Pipe(crate::smart::pipe::ChildSink),
}

impl Sink {
    fn write_all(&mut self, buf: &[u8], offset: u64) -> Result<(), String> {
        match self {
            Sink::Raw(f, _) => f
                .write_all(buf)
                .map_err(|e| format!("escrita no destino @ offset {offset}: {e}")),
            #[cfg(any(target_os = "linux", all(test, unix)))]
            Sink::Pipe(p) => p.write_all(buf),
        }
    }

    /// Conclui a escrita. Devolve o caminho a reler na verificação, quando o
    /// destino contém o stream byte a byte.
    fn finish(self) -> Result<Option<String>, String> {
        match self {
            Sink::Raw(f, path) => {
                f.sync_all().map_err(|e| format!("fsync: {e}"))?;
                Ok(Some(path))
            }
            #[cfg(any(target_os = "linux", all(test, unix)))]
            Sink::Pipe(p) => p.finish().map(|()| None),
        }
    }
}

#[cfg(target_os = "linux")]
fn partclone_sink(target: &str) -> Result<Sink, String> {
    crate::smart::platform::partclone_restore_spawn(target).map(Sink::Pipe)
}

#[cfg(not(target_os = "linux"))]
fn partclone_sink(target: &str) -> Result<Sink, String> {
    Err(format!(
        "esta imagem foi criada com partclone (smart copy do Linux) e só pode ser restaurada no Linux — nada foi gravado em `{target}`"
    ))
}

/// Abre o destino de acordo com o modo. Chamado só depois de o header ter
/// sido validado; um modo não tratado nunca cai na escrita crua.
fn open_sink(header: &ImageHeader, target: &str) -> Result<Sink, String> {
    match header.mode {
        MODE_RAW | MODE_SMART_BITMAP => {
            // Mesma heurística do clone: dispositivos de bloco existem; alvos
            // arquivo (testes / discos virtuais) são criados.
            let target_is_device = target.starts_with("\\\\.\\") || target.starts_with("/dev/");
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true);
            if !target_is_device {
                opts.create(true).truncate(true);
            }
            opts.open(target)
                .map(|f| Sink::Raw(f, target.to_string()))
                .map_err(|e| format!("abrir destino `{target}`: {e} (desmontado? admin/root?)"))
        }
        MODE_PARTCLONE => partclone_sink(target),
        other => Err(format!("modo {other} desconhecido no header da imagem")),
    }
}

pub fn run_restore<F>(req: &RestoreRequest, on_progress: F) -> Result<RestoreOutcome, String>
where
    F: Fn(&CloneProgress) + Send,
{
    let image = std::fs::File::open(&req.image)
        .map_err(|e| format!("abrir imagem `{}`: {e}", req.image))?;
    run_restore_from(image, req, on_progress)
}

/// Igual a `run_restore`, com a imagem já aberta por quem chama (o helper
/// elevado a abre com a identidade do usuário, não como root).
pub fn run_restore_from<F>(
    image: std::fs::File,
    req: &RestoreRequest,
    on_progress: F,
) -> Result<RestoreOutcome, String>
where
    F: Fn(&CloneProgress) + Send,
{
    let cancel = crate::job::global();
    restore_stream(image, req.verify, &cancel, |h| open_sink(h, &req.target), on_progress)
}

const VERIFY_FAILED_STREAM: &str = "VERIFICAÇÃO FALHOU — hash do restore ≠ hash gravado na imagem. NÃO inicialize por este disco.";
const VERIFY_FAILED_TARGET: &str = "VERIFICAÇÃO FALHOU — o conteúdo relido do destino difere do que a imagem contém. NÃO inicialize por este disco.";

fn restore_stream<F, S>(
    mut f: std::fs::File,
    verify: bool,
    cancel: &crate::job::CancelToken,
    open_sink: S,
    on_progress: F,
) -> Result<RestoreOutcome, String>
where
    F: Fn(&CloneProgress) + Send,
    S: FnOnce(&ImageHeader) -> Result<Sink, String>,
{
    let started = Instant::now();
    // Header e footer validados ANTES de o destino ser aberto. O decoder é
    // limitado exatamente ao trecho comprimido para não engolir o footer.
    let (header, stored, comp_len) = read_header(&mut f)?;
    let source_size = header.source_size;
    let mut dec =
        zstd::stream::read::Decoder::new(std::io::BufReader::with_capacity(IO_BUF, f.take(comp_len)))
            .map_err(|e| format!("zstd decoder: {e}"))?;

    let mut sink = open_sink(&header)?;

    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; IO_BUF];
    let mut done = 0u64;
    let mut last = Instant::now();
    loop {
        cancel.check()?;
        let n = dec
            .read(&mut buf)
            .map_err(|e| format!("descompressão @ offset {done}: {e}"))?;
        if n == 0 {
            break;
        }
        sink.write_all(&buf[..n], done)?;
        hasher.update(&buf[..n]);
        done += n as u64;
        if last.elapsed().as_millis() >= 200 {
            on_progress(&CloneProgress::new("restoring", done, source_size, started, None));
            last = Instant::now();
        }
    }
    let reread = sink.finish()?;

    // raw: o stream É a origem, então o tamanho tem de bater com o header.
    // smart-bitmap / partclone: o stream é menor que a partição de origem
    // (volume NTFS < partição; formato partclone), sem tamanho a comparar.
    if header.mode == MODE_RAW && done != source_size {
        return Err(format!(
            "stream restaurado com {done} bytes, mas o header declara {source_size}"
        ));
    }
    // O hash do stream sai de graça da passada única, então é conferido
    // sempre: imagem corrompida nunca termina como sucesso.
    if hasher.finalize().as_bytes() != &stored {
        return Err(VERIFY_FAILED_STREAM.into());
    }

    // "Verificar" = reler o que ficou no destino. Só é possível quando o
    // destino guarda o stream byte a byte (não é o caso do partclone, que
    // confere a própria escrita).
    let mut verified = false;
    if verify {
        if let Some(path) = reread {
            verify_target(&path, done, &stored, cancel, &on_progress)?;
            verified = true;
        }
    }
    Ok(RestoreOutcome {
        written: done,
        hash: blake3::Hash::from_bytes(stored).to_hex().to_string(),
        verified,
    })
}

/// Relê `len` bytes do destino e confere com o hash gravado na imagem.
fn verify_target<F>(
    path: &str,
    len: u64,
    expected: &[u8; FOOTER_SIZE],
    cancel: &crate::job::CancelToken,
    on_progress: &F,
) -> Result<(), String>
where
    F: Fn(&CloneProgress) + Send,
{
    let mut rd = std::fs::File::open(path)
        .map_err(|e| format!("reabrir destino p/ verificação: {e}"))?;
    crate::clone::drop_page_cache(&rd);

    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; IO_BUF];
    let mut done = 0u64;
    let started = Instant::now();
    let mut last = Instant::now();
    while done < len {
        cancel.check()?;
        // Lê blocos inteiros (dispositivos raw exigem múltiplos do setor) e
        // usa só o que foi gravado: o destino pode ser maior que a imagem.
        let n = rd
            .read(&mut buf)
            .map_err(|e| format!("verificação @ offset {done}: {e}"))?;
        if n == 0 {
            return Err(format!(
                "VERIFICAÇÃO FALHOU — o destino terminou em {done} bytes, antes dos {len} gravados"
            ));
        }
        let take = (n as u64).min(len - done) as usize;
        hasher.update(&buf[..take]);
        done += take as u64;
        if last.elapsed().as_millis() >= 200 {
            on_progress(&CloneProgress::new("verifying", done, len, started, None));
            last = Instant::now();
        }
    }
    if hasher.finalize().as_bytes() != expected {
        return Err(VERIFY_FAILED_TARGET.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nada(_: &CloneProgress) {}

    /// Diretório temporário exclusivo do teste.
    fn tmp(nome: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("cloneforge-image-{}-{nome}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn s(p: &std::path::Path) -> String {
        p.to_string_lossy().into_owned()
    }

    /// Monta um .cfimg válido com o modo e o tamanho de origem pedidos.
    fn montar_imagem(path: &std::path::Path, mode: u32, source_size: u64, stream: &[u8]) {
        let mut bytes = header_bytes(mode, source_size, 3).to_vec();
        bytes.extend(zstd::stream::encode_all(stream, 3).unwrap());
        bytes.extend(blake3::hash(stream).as_bytes());
        std::fs::write(path, bytes).unwrap();
    }

    fn restore(image: &std::path::Path, target: &std::path::Path, verify: bool) -> Result<RestoreOutcome, String> {
        let req = RestoreRequest { image: s(image), target: s(target), verify };
        run_restore(&req, nada)
    }

    #[test]
    fn header_valido_e_campos_desconhecidos_recusados() {
        let ok = parse_header(&header_bytes(MODE_PARTCLONE, 123, 7)).unwrap();
        assert_eq!(ok, ImageHeader { mode: MODE_PARTCLONE, source_size: 123, block_size: IO_BUF as u32, level: 7 });

        let mut magic = header_bytes(MODE_RAW, 1, 3);
        magic[0] = b'X';
        assert!(parse_header(&magic).is_err());

        let mut versao = header_bytes(MODE_RAW, 1, 3);
        versao[8] = 2;
        assert!(parse_header(&versao).unwrap_err().contains("versão 2"));

        assert!(parse_header(&header_bytes(3, 1, 3)).unwrap_err().contains("modo 3"));

        let mut algo = header_bytes(MODE_RAW, 1, 3);
        algo[32] = 9;
        assert!(parse_header(&algo).is_err());
    }

    #[test]
    fn raw_ida_e_volta_e_tamanho_divergente_falha() {
        let dir = tmp("raw");
        let dados: Vec<u8> = (0..2_000_000u32).map(|i| (i % 253) as u8).collect();
        std::fs::write(dir.join("src.bin"), &dados).unwrap();

        let req = ImageRequest {
            source: s(&dir.join("src.bin")),
            source_size: dados.len() as u64,
            output: s(&dir.join("img.cfimg")),
            level: None,
            smart: false,
            fs: None,
            live: None,
        };
        let (logical, hash, img_size, format) = run_create(&req, nada).unwrap();
        assert_eq!(logical, dados.len() as u64);
        assert_eq!(hash, blake3::hash(&dados).to_hex().to_string());
        assert_eq!(img_size, std::fs::metadata(dir.join("img.cfimg")).unwrap().len());
        assert_eq!(format, "cfimg");
        assert_eq!(read_image_header(req.output.clone()).unwrap().mode, MODE_RAW);

        let out = restore(&dir.join("img.cfimg"), &dir.join("out.bin"), true).unwrap();
        assert_eq!(out.written, dados.len() as u64);
        assert!(out.verified, "verify=true tem de reler o destino");
        assert!(!dir.join("img.cfimg.partial").exists(), "sobrou arquivo parcial");
        assert_eq!(std::fs::read(dir.join("out.bin")).unwrap(), dados);

        // raw com header declarando outro tamanho: stream truncado/alterado
        montar_imagem(&dir.join("curta.cfimg"), MODE_RAW, 999, b"abc");
        let err = restore(&dir.join("curta.cfimg"), &dir.join("out2.bin"), false).unwrap_err();
        assert!(err.contains("header declara 999"), "{err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn criar_imagem_regrava_saida_existente_por_inteiro() {
        let dir = tmp("trunca");
        std::fs::write(dir.join("src.bin"), b"conteudo novo").unwrap();
        std::fs::write(dir.join("img.cfimg"), vec![0xAAu8; 500_000]).unwrap(); // imagem antiga, maior
        let req = ImageRequest {
            source: s(&dir.join("src.bin")),
            source_size: 13,
            output: s(&dir.join("img.cfimg")),
            level: None,
            smart: false,
            fs: None,
            live: None,
        };
        run_create(&req, nada).unwrap();
        restore(&dir.join("img.cfimg"), &dir.join("out.bin"), true).unwrap();
        assert_eq!(std::fs::read(dir.join("out.bin")).unwrap(), b"conteudo novo");

        // origem que não abre: a imagem existente não é tocada
        let antes = std::fs::read(dir.join("img.cfimg")).unwrap();
        let ruim = ImageRequest { source: s(&dir.join("nao-existe.bin")), ..req.clone() };
        assert!(run_create(&ruim, nada).is_err());
        assert_eq!(std::fs::read(dir.join("img.cfimg")).unwrap(), antes);
        assert!(!dir.join("img.cfimg.partial").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn criacao_cancelada_preserva_a_imagem_anterior() {
        let dir = tmp("cancela-criar");
        std::fs::write(dir.join("src.bin"), vec![3u8; 100_000]).unwrap();
        std::fs::write(dir.join("img.cfimg"), b"backup anterior").unwrap();
        let req = ImageRequest {
            source: s(&dir.join("src.bin")),
            source_size: 100_000,
            output: s(&dir.join("img.cfimg")),
            level: None,
            smart: false,
            fs: None,
            live: None,
        };
        let partial = partial_path(&req.output);
        let cancel = crate::job::CancelToken::new();
        cancel.cancel();

        // mesmo roteiro do `run_create`, com um token próprio já cancelado
        let err = run_create_with(&req, || open_output(&partial), &cancel, nada).unwrap_err();
        assert_eq!(err, crate::job::CANCELLED);
        discard_output(&partial);

        assert_eq!(std::fs::read(dir.join("img.cfimg")).unwrap(), b"backup anterior");
        assert!(!std::path::Path::new(&partial).exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn restore_cancelado_devolve_cancelamento() {
        let dir = tmp("cancela-restore");
        montar_imagem(&dir.join("img.cfimg"), MODE_RAW, 5, b"12345");
        let cancel = crate::job::CancelToken::new();
        cancel.cancel();
        let img = std::fs::File::open(dir.join("img.cfimg")).unwrap();
        let alvo = s(&dir.join("out.bin"));
        let header_lido = std::cell::Cell::new(false);
        let err = restore_stream(
            img,
            true,
            &cancel,
            |h| {
                header_lido.set(true);
                open_sink(h, &alvo)
            },
            nada,
        )
        .unwrap_err();
        assert_eq!(err, crate::job::CANCELLED);
        assert!(header_lido.get());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn releitura_do_destino_detecta_conteudo_diferente() {
        let dir = tmp("releitura");
        let dados = vec![9u8; 300_000];
        let hash = *blake3::hash(&dados).as_bytes();
        let cancel = crate::job::CancelToken::new();

        // destino maior que a imagem: só os bytes gravados contam
        let mut maior = dados.clone();
        maior.extend_from_slice(b"resto do disco");
        std::fs::write(dir.join("ok.bin"), &maior).unwrap();
        verify_target(&s(&dir.join("ok.bin")), dados.len() as u64, &hash, &cancel, &nada).unwrap();

        // um byte trocado depois da gravação
        let mut ruim = dados.clone();
        ruim[150_000] ^= 1;
        std::fs::write(dir.join("ruim.bin"), &ruim).unwrap();
        let err = verify_target(&s(&dir.join("ruim.bin")), dados.len() as u64, &hash, &cancel, &nada).unwrap_err();
        assert_eq!(err, VERIFY_FAILED_TARGET);

        // destino mais curto do que o que foi gravado
        std::fs::write(dir.join("curto.bin"), &dados[..1000]).unwrap();
        let err = verify_target(&s(&dir.join("curto.bin")), dados.len() as u64, &hash, &cancel, &nada).unwrap_err();
        assert!(err.contains("terminou em 1000 bytes"), "{err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn imagem_corrompida_falha_mesmo_sem_verificar() {
        let dir = tmp("corrompida");
        montar_imagem(&dir.join("img.cfimg"), MODE_RAW, 5, b"12345");
        let mut bytes = std::fs::read(dir.join("img.cfimg")).unwrap();
        let fim = bytes.len() - 1;
        bytes[fim] ^= 0xFF; // footer (hash) adulterado
        std::fs::write(dir.join("img.cfimg"), bytes).unwrap();
        let err = restore(&dir.join("img.cfimg"), &dir.join("out.bin"), false).unwrap_err();
        assert_eq!(err, VERIFY_FAILED_STREAM);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn imagem_nova_nasce_legivel_so_pelo_dono() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp("modo");
        std::fs::write(dir.join("src.bin"), b"abc").unwrap();
        let req = ImageRequest {
            source: s(&dir.join("src.bin")),
            source_size: 3,
            output: s(&dir.join("img.cfimg")),
            level: None,
            smart: false,
            fs: None,
            live: None,
        };
        run_create(&req, nada).unwrap();
        let modo = std::fs::metadata(dir.join("img.cfimg")).unwrap().permissions().mode() & 0o777;
        assert_eq!(modo & 0o077, 0, "imagem acessível a outros usuários: {modo:o}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn smart_bitmap_aceita_stream_menor_que_a_particao_e_sempre_confere_o_hash() {
        let dir = tmp("smart");
        let volume = vec![7u8; 100_000];
        // header declara a PARTIÇÃO (maior que o volume NTFS dentro dela)
        montar_imagem(&dir.join("vol.cfimg"), MODE_SMART_BITMAP, 104_096, &volume);
        let out = restore(&dir.join("vol.cfimg"), &dir.join("out.bin"), false).unwrap();
        assert_eq!(out.written, 100_000);
        assert!(!out.verified, "verify=false não relê o destino");
        assert_eq!(std::fs::read(dir.join("out.bin")).unwrap(), volume);

        // footer adulterado: falha mesmo com verify = false
        let mut bytes = std::fs::read(dir.join("vol.cfimg")).unwrap();
        let fim = bytes.len() - 1;
        bytes[fim] ^= 0xFF;
        std::fs::write(dir.join("ruim.cfimg"), bytes).unwrap();
        let err = restore(&dir.join("ruim.cfimg"), &dir.join("out2.bin"), false).unwrap_err();
        assert!(err.contains("VERIFICAÇÃO FALHOU"), "{err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// O bug original: imagem partclone gravada crua no destino.
    #[cfg(not(target_os = "linux"))]
    #[test]
    fn partclone_fora_do_linux_e_recusado_sem_tocar_no_destino() {
        let dir = tmp("pc-recusa");
        montar_imagem(&dir.join("pc.cfimg"), MODE_PARTCLONE, 1 << 30, b"partclone-image stream");
        std::fs::write(dir.join("disco.bin"), b"dados que nao podem sumir").unwrap();

        let err = restore(&dir.join("pc.cfimg"), &dir.join("disco.bin"), true).unwrap_err();
        assert!(err.contains("partclone"), "{err}");
        assert_eq!(std::fs::read(dir.join("disco.bin")).unwrap(), b"dados que nao podem sumir");
        assert!(restore(&dir.join("pc.cfimg"), &dir.join("novo.bin"), true).is_err());
        assert!(!dir.join("novo.bin").exists(), "o destino foi criado");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn partclone_entrega_o_stream_ao_programa_de_restore() {
        let dir = tmp("pc-pipe");
        let stream: Vec<u8> = (0..1_500_000u32).map(|i| (i % 241) as u8).collect();
        montar_imagem(&dir.join("pc.cfimg"), MODE_PARTCLONE, 1 << 30, &stream);
        let recebido = s(&dir.join("recebido.bin"));

        // `sh` faz o papel do partclone.restore: grava o que chega no stdin.
        let img = std::fs::File::open(dir.join("pc.cfimg")).unwrap();
        let out = restore_stream(
            img,
            true,
            &crate::job::CancelToken::new(),
            |h| {
                assert_eq!(h.mode, MODE_PARTCLONE);
                crate::smart::pipe::spawn_sink("sh", &["-c", "cat > \"$0\"", &recebido]).map(Sink::Pipe)
            },
            nada,
        )
        .unwrap();
        assert_eq!(out.written, stream.len() as u64);
        assert!(!out.verified, "partclone não permite releitura byte a byte");
        assert_eq!(std::fs::read(&recebido).unwrap(), stream);

        // programa de restore falha: o erro dele chega a quem chamou
        let img = std::fs::File::open(dir.join("pc.cfimg")).unwrap();
        let err = restore_stream(
            img,
            false,
            &crate::job::CancelToken::new(),
            |_| {
                crate::smart::pipe::spawn_sink("sh", &["-c", "cat >/dev/null; echo destino pequeno >&2; exit 1"])
                    .map(Sink::Pipe)
            },
            nada,
        )
        .unwrap_err();
        assert!(err.contains("destino pequeno"), "{err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn imagem_invalida_nao_abre_o_destino() {
        let dir = tmp("invalida");
        std::fs::write(dir.join("lixo.cfimg"), vec![0u8; 4096]).unwrap();
        montar_imagem(&dir.join("modo9.cfimg"), 9, 10, b"0123456789");
        for img in ["lixo.cfimg", "modo9.cfimg"] {
            assert!(restore(&dir.join(img), &dir.join("alvo.bin"), true).is_err(), "{img}");
            assert!(!dir.join("alvo.bin").exists(), "{img}: o destino foi criado");
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
