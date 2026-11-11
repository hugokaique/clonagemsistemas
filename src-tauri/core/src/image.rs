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
//!  16..24  tamanho do stream descomprimido (u64) — gravado ao FINAL com o
//!          valor real (imagens da v0.1.0 em modo smart traziam o tamanho da
//!          partição, maior que o stream)
//!  24..28  tamanho do bloco de I/O (u32)
//!  28..32  nível zstd (i32)
//!  32..36  algoritmo de hash (u32): 1 = BLAKE3
//!  36..40  tipo da origem (u32): 0 = desconhecido, 1 = disco, 2 = partição
//!  40..48  data de criação (u64, segundos Unix; 0 = desconhecida)
//!  48..64  reservado (zeros)
//!  64..128 descrição da origem (UTF-8, preenchida com NUL)
//!
//! Os campos a partir do byte 36 ocupam a área que era reservada (zeros) na
//! v0.1.0 — imagens antigas continuam legíveis.
//!
//! Modos raw e smart-bitmap guardam os bytes do dispositivo (gaps livres =
//! zeros): o restore é um único laço decompress→write. O modo partclone
//! guarda o stream do partclone e o restore passa pelo `partclone.restore`.
//!
//! Live copy (Windows): com `live = true`, o volume de origem é congelado via
//! snapshot VSS (Win32_ShadowCopy) e a leitura acontece do dispositivo de
//! shadow copy — permite imagem consistente do volume do sistema em execução.
//!
//! A pipeline roda em `spawn_blocking`: compressão zstd é CPU-bound e as
//! chamadas Win32 (DeviceIoControl) são síncronas por natureza.

use serde::{Deserialize, Serialize};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::AtomicBool;

use crate::clone::{check_sector, verify_target};
use crate::io::{self, align_up, read_full, PathKind};
use crate::prepare::{KeepAwake, TargetGuard};
use crate::progress::{announce, check_cancel, OnProgress, Tracker};
use crate::safety::{self, ResolvedSource, ResolvedTarget};

pub const CFIMG_MAGIC: &[u8; 8] = b"CFIMG001";
pub const HEADER_SIZE: usize = 128;
pub const FOOTER_SIZE: usize = 32; // BLAKE3

pub const MODE_RAW: u32 = 0;
pub const MODE_SMART_BITMAP: u32 = 1;
pub const MODE_PARTCLONE: u32 = 2;

const LABEL_OFFSET: usize = 64;

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageHeader {
    pub mode: u32,
    /// Bytes do stream descomprimido
    pub source_size: u64,
    pub block_size: u32,
    pub level: i32,
    pub source_kind: u32,
    pub created_unix: u64,
    pub label: String,
}

impl ImageHeader {
    pub fn to_bytes(&self) -> [u8; HEADER_SIZE] {
        let mut h = [0u8; HEADER_SIZE];
        h[0..8].copy_from_slice(CFIMG_MAGIC);
        h[8..12].copy_from_slice(&1u32.to_le_bytes());
        h[12..16].copy_from_slice(&self.mode.to_le_bytes());
        h[16..24].copy_from_slice(&self.source_size.to_le_bytes());
        h[24..28].copy_from_slice(&self.block_size.to_le_bytes());
        h[28..32].copy_from_slice(&self.level.to_le_bytes());
        h[32..36].copy_from_slice(&1u32.to_le_bytes()); // 1 = BLAKE3
        h[36..40].copy_from_slice(&self.source_kind.to_le_bytes());
        h[40..48].copy_from_slice(&self.created_unix.to_le_bytes());
        // Rótulo: no máximo 64 bytes, cortado em fronteira de caractere
        let mut end = self.label.len().min(HEADER_SIZE - LABEL_OFFSET);
        while !self.label.is_char_boundary(end) {
            end -= 1;
        }
        h[LABEL_OFFSET..LABEL_OFFSET + end].copy_from_slice(&self.label.as_bytes()[..end]);
        h
    }

    pub fn parse(h: &[u8; HEADER_SIZE]) -> Result<Self, String> {
        if &h[0..8] != CFIMG_MAGIC {
            return Err("não é um arquivo .cfimg (magic inválido)".into());
        }
        let u32_at = |i: usize| u32::from_le_bytes([h[i], h[i + 1], h[i + 2], h[i + 3]]);
        let u64_at = |i: usize| {
            let mut b = [0u8; 8];
            b.copy_from_slice(&h[i..i + 8]);
            u64::from_le_bytes(b)
        };
        let version = u32_at(8);
        if version != 1 {
            return Err(format!(
                "imagem .cfimg versão {version} não suportada — atualize o CloneForge"
            ));
        }
        if u32_at(32) != 1 {
            return Err(format!("algoritmo de hash {} não suportado", u32_at(32)));
        }
        let mode = u32_at(12);
        if mode > MODE_PARTCLONE {
            return Err(format!("modo de imagem desconhecido: {mode}"));
        }
        let label_bytes = &h[LABEL_OFFSET..];
        let label_end = label_bytes.iter().position(|b| *b == 0).unwrap_or(label_bytes.len());
        Ok(Self {
            mode,
            source_size: u64_at(16),
            block_size: u32_at(24),
            level: u32_at(28) as i32,
            source_kind: u32_at(36),
            created_unix: u64_at(40),
            label: String::from_utf8_lossy(&label_bytes[..label_end]).into_owned(),
        })
    }
}

/// Imagem aberta: arquivo posicionado logo após o header.
struct OpenImage {
    file: std::fs::File,
    header: ImageHeader,
    stored_hash: [u8; FOOTER_SIZE],
    compressed_len: u64,
    file_len: u64,
}

fn open_image(path: &str) -> Result<OpenImage, String> {
    let mut file = std::fs::File::open(path).map_err(|e| io::open_error("imagem", path, &e))?;
    let file_len = file.metadata().map_err(|e| e.to_string())?.len();
    if file_len < (HEADER_SIZE + FOOTER_SIZE) as u64 {
        return Err("arquivo pequeno demais para ser .cfimg".into());
    }
    let mut h = [0u8; HEADER_SIZE];
    file.read_exact(&mut h).map_err(|e| format!("header: {e}"))?;
    let header = ImageHeader::parse(&h)?;

    // Hash armazenado no footer (o decoder é limitado exatamente ao trecho
    // comprimido para não engolir o footer).
    let mut stored_hash = [0u8; FOOTER_SIZE];
    file.seek(SeekFrom::End(-(FOOTER_SIZE as i64))).map_err(|e| e.to_string())?;
    file.read_exact(&mut stored_hash).map_err(|e| format!("footer: {e}"))?;
    file.seek(SeekFrom::Start(HEADER_SIZE as u64)).map_err(|e| e.to_string())?;

    Ok(OpenImage {
        file,
        header,
        stored_hash,
        compressed_len: file_len - (HEADER_SIZE + FOOTER_SIZE) as u64,
        file_len,
    })
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
    pub source_kind: u32,
    pub created_unix: u64,
    pub label: String,
    pub file_size: u64,
}

pub fn read_image_header(path: &str) -> Result<ImageHeaderInfo, String> {
    let img = open_image(path)?;
    Ok(ImageHeaderInfo {
        valid: true,
        mode: img.header.mode,
        source_size: img.header.source_size,
        block_size: img.header.block_size,
        level: img.header.level,
        hash_hex: blake3::Hash::from_bytes(img.stored_hash).to_hex().to_string(),
        source_kind: img.header.source_kind,
        created_unix: img.header.created_unix,
        label: img.header.label,
        file_size: img.file_len,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageRequest {
    /// disco (`\\.\PhysicalDrive1`), volume (`\\.\C:`) ou partição (`/dev/sdb1`, `/dev/disk4s2`)
    pub source: String,
    /// Informativo — o backend resolve o tamanho real pela enumeração.
    #[serde(default)]
    pub source_size: u64,
    /// caminho de saída do .cfimg (ou .dmg no macOS smart)
    pub output: String,
    /// nível zstd 1..19 (default 3 — sweet spot ratio/velocidade)
    #[serde(default)]
    pub level: Option<i32>,
    /// true = copiar só blocos alocados
    #[serde(default)]
    pub smart: bool,
    /// filesystem p/ partclone no Linux (ext4, xfs, btrfs, ntfs, vfat, exfat)
    #[serde(default)]
    pub fs: Option<String>,
    /// Windows: criar snapshot VSS antes de ler (origem em uso / volume do sistema)
    #[serde(default)]
    pub live: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreRequest {
    pub image: String,
    pub target: String,
    #[serde(default = "default_true")]
    pub verify: bool,
}

/// Criação de imagem já validada.
#[derive(Debug, Clone)]
pub struct ImagePlan {
    pub source: ResolvedSource,
    /// Caminho absoluto do arquivo de saída
    pub output: String,
    pub level: i32,
    pub smart: bool,
    pub fs: Option<String>,
    pub live: bool,
}

/// Restauração já validada. `header = None` → imagem `.dmg` (macOS/asr).
#[derive(Debug, Clone)]
pub struct RestorePlan {
    pub image: String,
    pub header: Option<ImageHeader>,
    pub target: ResolvedTarget,
    pub verify: bool,
}

/// Resultado da criação (campos em snake_case: protocolo do helper).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageOutcome {
    pub logical: u64,
    pub hash: String,
    pub image_size: u64,
    pub format: String,
    pub output: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestoreOutcome {
    pub hash: String,
    pub written: u64,
    pub verified: bool,
}

// ─────────────────────────── Criar imagem ──────────────────────────────

/// Valida a requisição. Caminho de saída relativo é resolvido contra
/// `base_dir` (GUI: pasta Documentos; helper/CLI: diretório atual).
pub async fn plan_create(req: &ImageRequest, base_dir: Option<&Path>) -> Result<ImagePlan, String> {
    let disks = safety::enumerate_for(&[&req.source]).await?;
    plan_create_with(req, &disks, base_dir)
}

pub fn plan_create_with(
    req: &ImageRequest,
    disks: &[crate::disk::DiskInfo],
    base_dir: Option<&Path>,
) -> Result<ImagePlan, String> {
    if req.output.trim().is_empty() {
        return Err("informe o caminho de saída da imagem".into());
    }
    if io::path_kind(req.output.trim()) == PathKind::Device {
        return Err(format!(
            "a saída `{}` é um dispositivo — a imagem precisa ser gravada em um arquivo",
            req.output
        ));
    }
    let source = safety::resolve_source(&req.source, disks)?;
    let mut output = io::absolute(req.output.trim(), base_dir);

    let mut fs = req.fs.clone().or_else(|| source.fs.clone());
    if req.smart {
        if source.kind != PathKind::Device || source.source_kind != safety::KIND_PARTITION {
            return Err(
                "Smart Copy opera sobre uma partição/volume — para um disco inteiro ou arquivo desmarque Smart Copy".into(),
            );
        }
        if cfg!(target_os = "linux") {
            let name = fs.clone().unwrap_or_default();
            if crate::smart::partclone_tool(&name).is_none() {
                return Err(format!(
                    "Smart Copy não suporta o filesystem `{name}` desta partição — desmarque Smart Copy para cópia raw"
                ));
            }
        }
        // O hdiutil acrescenta ".dmg" por conta própria; fixamos o nome final aqui.
        if cfg!(target_os = "macos") && !output.to_ascii_lowercase().ends_with(".dmg") {
            output.push_str(".dmg");
        }
    } else {
        fs = None;
    }

    let out_path = Path::new(&output);
    if out_path.is_dir() {
        return Err(format!("a saída `{output}` é uma pasta — informe o nome do arquivo"));
    }
    match out_path.parent() {
        Some(dir) if dir.as_os_str().is_empty() || dir.is_dir() => {}
        _ => return Err(format!("a pasta de destino de `{output}` não existe")),
    }
    // Por identidade, não por texto: no macOS a mesma pasta temporária é
    // `/var/…` e `/private/var/…`; um symlink/junction faria o mesmo. A saída
    // é renomeada por cima do destino ao final — se fosse a origem, ela se perderia.
    if io::same_file(&output, &source.path) {
        return Err("a saída não pode ser o próprio arquivo de origem".into());
    }

    Ok(ImagePlan {
        source,
        output,
        level: req.level.unwrap_or(3).clamp(1, 19),
        smart: req.smart,
        fs,
        live: req.smart && req.live.unwrap_or(false),
    })
}

/// Origem aberta para leitura. A ordem dos campos importa: o reader (que
/// segura o handle do shadow copy) é destruído ANTES do snapshot VSS.
struct ImageSource {
    reader: Box<dyn Read + Send>,
    mode: u32,
    /// Tamanho exato esperado do stream (None = desconhecido, ex.: partclone)
    expected: Option<u64>,
    #[cfg(windows)]
    _shadow: Option<crate::vss::ShadowGuard>,
}

fn open_image_source(plan: &ImagePlan, on_progress: OnProgress) -> Result<ImageSource, String> {
    let total = plan.source.size;
    let note = |msg: String| announce(on_progress, "preparing", 0, total, Some(msg));

    if plan.smart {
        #[cfg(windows)]
        {
            let mut volume = plan.source.path.clone();
            let mut shadow = None;
            if plan.live {
                note("criando snapshot VSS do volume…".into());
                let sc = crate::vss::create(&volume).map_err(|e| {
                    format!("{e} — se este volume não aceita snapshots VSS (ex.: mídia removível), desmarque Live Copy")
                })?;
                tracing::info!("VSS snapshot {} → {}", sc.id, sc.device_object);
                note(format!("VSS snapshot {} criado", sc.id));
                volume = sc.device_object.clone();
                shadow = Some(crate::vss::ShadowGuard(sc));
            } else if plan.source.mounted {
                note("aviso: volume em uso sem Live Copy — arquivos alterados durante a cópia podem deixar a imagem inconsistente".into());
            }
            // Se a abertura falhar, `shadow` é destruído aqui e o snapshot apagado.
            let sv = crate::smart::platform::open_volume(&volume)?;
            note(sv.summary.clone());
            let len = sv.reader.total_bytes();
            return Ok(ImageSource {
                reader: Box::new(sv.reader),
                mode: MODE_SMART_BITMAP,
                expected: Some(len),
                _shadow: shadow,
            });
        }
        #[cfg(target_os = "linux")]
        {
            let fs = plan.fs.clone().unwrap_or_default();
            let reader = crate::smart::platform::partclone_spawn(&plan.source.path, &fs)?;
            note(format!("smart copy via partclone ({fs})"));
            return Ok(ImageSource {
                reader: Box::new(reader),
                mode: MODE_PARTCLONE,
                expected: None,
            });
        }
        #[cfg(not(any(windows, target_os = "linux")))]
        {
            let _ = &note;
            return Err("Smart Copy em .cfimg não é suportado neste sistema".into());
        }
    }

    if plan.source.kind == PathKind::Device && plan.source.mounted {
        note("aviso: a origem tem volumes montados — alterações feitas durante a cópia podem deixar a imagem inconsistente".into());
    }
    let file = io::open_source(&plan.source.path)?;
    // `take` garante que nenhuma leitura cruza o fim conhecido do dispositivo.
    Ok(ImageSource {
        reader: Box::new(file.take(total)),
        mode: MODE_RAW,
        expected: Some(total),
        #[cfg(windows)]
        _shadow: None,
    })
}

/// Threads de compressão: deixa um núcleo para leitura+hash, no máximo 8.
fn zstd_workers() -> u32 {
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    cores.saturating_sub(1).min(8) as u32
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Pipeline read → BLAKE3 → zstd → arquivo. Devolve (bytes lógicos, hash,
/// tamanho do arquivo). O header é regravado ao final com o tamanho REAL.
#[allow(clippy::too_many_arguments)]
pub(crate) fn write_image(
    path: &str,
    mut header: ImageHeader,
    reader: &mut dyn Read,
    expected: Option<u64>,
    progress_total: u64,
    cancel: &AtomicBool,
    on_progress: OnProgress,
) -> Result<(u64, blake3::Hash, u64), String> {
    let out_f = std::fs::File::create(path).map_err(|e| io::open_error("saída", path, &e))?;
    let mut w = BufWriter::with_capacity(1024 * 1024, out_f);
    w.write_all(&header.to_bytes()).map_err(|e| format!("header: {e}"))?;

    let mut enc = zstd::stream::write::Encoder::new(w, header.level).map_err(|e| e.to_string())?;
    // Checksum do frame: o próprio zstd detecta corrupção na descompressão.
    enc.include_checksum(true).map_err(|e| e.to_string())?;
    let workers = zstd_workers();
    if workers > 0 {
        enc.multithread(workers).map_err(|e| e.to_string())?;
    }

    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; header.block_size as usize];
    let mut done = 0u64;
    let mut tracker = Tracker::new("compressing", progress_total, on_progress);
    loop {
        check_cancel(cancel)?;
        let n = read_full(reader, &mut buf).map_err(|e| format!("ler origem @ offset {done}: {e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        enc.write_all(&buf[..n]).map_err(|e| format!("gravar imagem (zstd): {e}"))?;
        done += n as u64;
        tracker.tick(done);
        if n < buf.len() {
            break; // read_full só devolve bloco parcial no fim do stream
        }
    }
    if let Some(exp) = expected {
        if done != exp {
            return Err(format!(
                "a origem terminou antes do esperado ({done} de {exp} bytes lidos)"
            ));
        }
    }
    if done == 0 {
        return Err("a origem não entregou nenhum dado".into());
    }

    announce(on_progress, "flushing", done, done, None);
    let mut w = enc.finish().map_err(|e| format!("finalizar zstd: {e}"))?;
    let hash = hasher.finalize();
    w.write_all(hash.as_bytes()).map_err(|e| format!("footer: {e}"))?;
    let mut f = w.into_inner().map_err(|e| format!("gravar imagem: {}", e.error()))?;

    // Header definitivo: tamanho real do stream.
    header.source_size = done;
    f.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
    f.write_all(&header.to_bytes()).map_err(|e| format!("header final: {e}"))?;
    f.sync_all().map_err(|e| format!("fsync da imagem: {e}"))?;
    let size = f.metadata().map_err(|e| e.to_string())?.len();
    Ok((done, hash, size))
}

/// Cria a imagem. Grava em `<saída>.part` e só renomeia para o nome final
/// em caso de sucesso: uma falha/cancelamento não deixa imagem truncada nem
/// destrói uma imagem boa que já existisse no mesmo caminho.
pub fn run_create(
    plan: &ImagePlan,
    cancel: &AtomicBool,
    on_progress: OnProgress,
) -> Result<ImageOutcome, String> {
    let _awake = KeepAwake::new();
    announce(on_progress, "preparing", 0, plan.source.size, None);

    // macOS smart: delega ao hdiutil (.dmg UDZO) — único formato que o asr
    // restaura corretamente em containers APFS (volumes selados).
    #[cfg(target_os = "macos")]
    if plan.smart {
        announce(
            on_progress,
            "compressing",
            0,
            plan.source.size,
            Some("criando .dmg via hdiutil (sem telemetria de progresso)".into()),
        );
        if let Err(e) = crate::smart::platform::create_dmg(&plan.source.path, &plan.output, cancel) {
            let _ = std::fs::remove_file(&plan.output);
            return Err(e);
        }
        let hash = crate::smart::platform::hash_file(&plan.output)?;
        let size = std::fs::metadata(&plan.output).map(|m| m.len()).unwrap_or(0);
        return Ok(ImageOutcome {
            logical: size,
            hash,
            image_size: size,
            format: "dmg".into(),
            output: plan.output.clone(),
        });
    }

    let mut source = open_image_source(plan, on_progress)?;
    let header = ImageHeader {
        mode: source.mode,
        source_size: source.expected.unwrap_or(plan.source.size),
        block_size: io::DEFAULT_BLOCK as u32,
        level: plan.level,
        source_kind: plan.source.source_kind,
        created_unix: unix_now(),
        label: plan.source.label.clone(),
    };

    let part = format!("{}.part", plan.output);
    let progress_total = source.expected.unwrap_or(plan.source.size);
    let written = write_image(
        &part,
        header,
        source.reader.as_mut(),
        source.expected,
        progress_total,
        cancel,
        on_progress,
    );
    drop(source); // fecha a origem e apaga o snapshot VSS antes de finalizar

    match written {
        Ok((logical, hash, image_size)) => {
            std::fs::rename(&part, &plan.output).map_err(|e| {
                let _ = std::fs::remove_file(&part);
                format!("finalizar `{}`: {e}", plan.output)
            })?;
            Ok(ImageOutcome {
                logical,
                hash: hash.to_hex().to_string(),
                image_size,
                format: "cfimg".into(),
                output: plan.output.clone(),
            })
        }
        Err(e) => {
            let _ = std::fs::remove_file(&part);
            Err(e)
        }
    }
}

// ────────────────────────── Restaurar imagem ───────────────────────────

pub async fn plan_restore(req: &RestoreRequest) -> Result<RestorePlan, String> {
    let disks = safety::enumerate_for(&[&req.target]).await?;
    plan_restore_with(req, &disks)
}

pub fn plan_restore_with(
    req: &RestoreRequest,
    disks: &[crate::disk::DiskInfo],
) -> Result<RestorePlan, String> {
    if req.image.trim().is_empty() {
        return Err("selecione a imagem a restaurar".into());
    }
    let image = io::absolute(req.image.trim(), None);
    if io::path_kind(&image) == PathKind::Device {
        return Err(format!("`{image}` é um dispositivo, não um arquivo de imagem"));
    }

    // .dmg (macOS smart) não tem header .cfimg — vai direto para o asr.
    let is_dmg = image.to_ascii_lowercase().ends_with(".dmg");
    let header = if is_dmg {
        if !cfg!(target_os = "macos") {
            return Err("imagens .dmg só podem ser restauradas no macOS".into());
        }
        if !Path::new(&image).is_file() {
            return Err(format!("imagem `{image}` não encontrada"));
        }
        None
    } else {
        Some(open_image(&image)?.header)
    };

    let target = safety::resolve_target(&req.target, disks)?;
    // Trava de segurança: a imagem não pode estar no disco que será apagado.
    safety::check_file_not_on_target(&image, PathKind::File, &target)?;
    if target.kind == PathKind::File && io::same_file(&image, &target.path) {
        return Err("imagem e destino são o mesmo arquivo".into());
    }

    if let Some(h) = &header {
        if h.mode == MODE_PARTCLONE && !cfg!(target_os = "linux") {
            return Err("esta imagem foi criada com partclone (Smart Copy no Linux) e só pode ser restaurada no Linux".into());
        }
        // ANTES de gravar qualquer coisa: o destino comporta a imagem?
        if h.mode != MODE_PARTCLONE {
            check_capacity(h, &target)?;
        }
    }

    Ok(RestorePlan { image, header, target, verify: req.verify })
}

fn check_capacity(header: &ImageHeader, target: &ResolvedTarget) -> Result<(), String> {
    if let Some(cap) = target.capacity {
        if header.source_size > cap {
            return Err(format!(
                "destino ({}) menor que o conteúdo da imagem ({})",
                safety::human(cap),
                safety::human(header.source_size)
            ));
        }
    }
    Ok(())
}

const CORRUPT_HINT: &str = "O destino recebeu dados incompletos/inválidos: NÃO inicialize por este disco.";

pub fn run_restore(
    plan: &RestorePlan,
    cancel: &AtomicBool,
    on_progress: OnProgress,
) -> Result<RestoreOutcome, String> {
    let _awake = KeepAwake::new();
    if plan.header.is_none() {
        return restore_dmg(plan, cancel, on_progress);
    }

    // Reabre a imagem aqui (quem executa pode ser o helper) e revalida.
    let img = open_image(&plan.image)?;
    let header = img.header.clone();
    let block = io::DEFAULT_BLOCK;
    check_sector(&plan.target, block)?;
    if header.mode != MODE_PARTCLONE {
        check_capacity(&header, &plan.target)?;
    }

    announce(on_progress, "preparing", 0, header.source_size, None);
    let guard = TargetGuard::acquire(&plan.target)?;
    for note in guard.notes() {
        announce(on_progress, "preparing", 0, header.source_size, Some(note.clone()));
    }

    let limited = img.file.take(img.compressed_len);
    let mut dec = zstd::stream::read::Decoder::with_buffer(BufReader::with_capacity(1024 * 1024, limited))
        .map_err(|e| format!("zstd decoder: {e}"))?;

    let (done, stream_hash) = match header.mode {
        MODE_PARTCLONE => restore_partclone(&mut dec, &header, &plan.target, block, cancel, on_progress)?,
        _ => restore_blocks(&mut dec, &header, &plan.target, block, cancel, on_progress)?,
    };

    // A integridade da imagem é SEMPRE conferida (o hash já foi calculado de
    // graça durante a escrita). Antes, sem "verificar", uma imagem corrompida
    // era restaurada e reportada como sucesso.
    if stream_hash.as_bytes() != &img.stored_hash {
        return Err(format!(
            "IMAGEM CORROMPIDA — o conteúdo não confere com o hash gravado na imagem. {CORRUPT_HINT}"
        ));
    }
    let size_ok = match header.mode {
        MODE_RAW => done == header.source_size,
        // imagens smart da v0.1.0 declaravam o tamanho da partição (maior)
        MODE_SMART_BITMAP => done <= header.source_size,
        _ => true,
    };
    if !size_ok {
        return Err(format!(
            "stream restaurado com {done} bytes, mas o header declara {}. {CORRUPT_HINT}",
            header.source_size
        ));
    }

    // "Verificar" = reler o DESTINO e comparar com o hash da imagem — pega
    // erro de gravação na mídia, que a conferência acima não enxerga.
    let mut verified = false;
    if plan.verify && header.mode != MODE_PARTCLONE {
        verify_target(&plan.target, done, &stream_hash, block, cancel, on_progress)?;
        verified = true;
    }
    guard.finish(&plan.target);

    Ok(RestoreOutcome { hash: stream_hash.to_hex().to_string(), written: done, verified })
}

/// Laço decompress → write em blocos CHEIOS. O decoder zstd entrega pedaços
/// de tamanho arbitrário; gravá-los direto num disco bruto falha no Windows
/// e no macOS (escrita precisa ser múltipla do setor).
fn restore_blocks(
    dec: &mut dyn Read,
    header: &ImageHeader,
    target: &ResolvedTarget,
    block: usize,
    cancel: &AtomicBool,
    on_progress: OnProgress,
) -> Result<(u64, blake3::Hash), String> {
    let mut dst = io::open_target(&target.path, target.kind)?;
    let sector = target.sector_size.max(1) as u64;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; block];
    let mut done = 0u64;
    let mut tracker = Tracker::new("restoring", header.source_size, on_progress);
    loop {
        check_cancel(cancel)?;
        let n = read_full(dec, &mut buf)
            .map_err(|e| format!("descompressão @ offset {done}: {e} — imagem corrompida. {CORRUPT_HINT}"))?;
        if n == 0 {
            break;
        }
        if done + n as u64 > header.source_size {
            return Err(format!(
                "a imagem contém mais dados que os {} bytes declarados no header. {CORRUPT_HINT}",
                header.source_size
            ));
        }
        hasher.update(&buf[..n]);
        let wlen = match target.kind {
            PathKind::Device => align_up(n as u64, sector) as usize,
            PathKind::File => n,
        };
        buf[n..wlen].fill(0);
        dst.write_all(&buf[..wlen])
            .map_err(|e| format!("escrita no destino @ offset {done}: {e}"))?;
        done += n as u64;
        tracker.tick(done);
        if n < buf.len() {
            break;
        }
    }
    announce(on_progress, "flushing", done, header.source_size, None);
    dst.sync_all().map_err(|e| format!("fsync do destino: {e}"))?;
    io::drop_cache(&dst);
    Ok((done, hasher.finalize()))
}

/// Imagem partclone: o stream descomprimido vai para o `partclone.restore`,
/// que entende o formato e grava só os blocos usados. (Antes o stream era
/// gravado cru no disco — o resultado era lixo.)
#[cfg(target_os = "linux")]
fn restore_partclone(
    dec: &mut dyn Read,
    header: &ImageHeader,
    target: &ResolvedTarget,
    block: usize,
    cancel: &AtomicBool,
    on_progress: OnProgress,
) -> Result<(u64, blake3::Hash), String> {
    let mut pc = crate::smart::platform::partclone_restore_spawn(
        &target.path,
        target.kind == PathKind::File,
    )?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; block];
    let mut done = 0u64;
    let mut tracker = Tracker::new("restoring", header.source_size, on_progress);
    loop {
        check_cancel(cancel)?; // o Drop do PartcloneWriter mata o processo
        let n = read_full(dec, &mut buf)
            .map_err(|e| format!("descompressão @ offset {done}: {e} — imagem corrompida. {CORRUPT_HINT}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        pc.write_all(&buf[..n])?;
        done += n as u64;
        tracker.tick(done);
        if n < buf.len() {
            break;
        }
    }
    announce(on_progress, "flushing", done, header.source_size, None);
    pc.finish()?;
    Ok((done, hasher.finalize()))
}

#[cfg(not(target_os = "linux"))]
fn restore_partclone(
    _dec: &mut dyn Read,
    _header: &ImageHeader,
    _target: &ResolvedTarget,
    _block: usize,
    _cancel: &AtomicBool,
    _on_progress: OnProgress,
) -> Result<(u64, blake3::Hash), String> {
    Err("imagens partclone só podem ser restauradas no Linux".into())
}

/// macOS: restaura um `.dmg` via `asr` (que já verifica os dados gravados).
#[cfg(target_os = "macos")]
fn restore_dmg(
    plan: &RestorePlan,
    cancel: &AtomicBool,
    on_progress: OnProgress,
) -> Result<RestoreOutcome, String> {
    announce(
        on_progress,
        "restoring",
        0,
        0,
        Some("restaurando via asr (sem telemetria de progresso)".into()),
    );
    let node = crate::prepare::macos_buffered_node(&plan.target.path);
    crate::smart::platform::restore_dmg(&plan.image, &node, cancel)?;
    let hash = crate::smart::platform::hash_file(&plan.image)?;
    let written = std::fs::metadata(&plan.image).map(|m| m.len()).unwrap_or(0);
    Ok(RestoreOutcome { hash, written, verified: true })
}

#[cfg(not(target_os = "macos"))]
fn restore_dmg(
    _plan: &RestorePlan,
    _cancel: &AtomicBool,
    _on_progress: OnProgress,
) -> Result<RestoreOutcome, String> {
    Err("imagens .dmg só podem ser restauradas no macOS".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clone::tests::{noise, TempDir};
    use crate::progress::CloneProgress;
    use crate::smart::{bitmap_to_map, SparseReader};
    use std::sync::atomic::Ordering;

    fn no_progress(_: &CloneProgress) {}
    fn never() -> AtomicBool {
        AtomicBool::new(false)
    }

    /// Massa mista: aleatório + zeros + repetitivo, tamanho fora de alinhamento.
    fn sample() -> Vec<u8> {
        let mut d = noise(700_000, 7);
        d.extend(std::iter::repeat(0u8).take(900_000));
        d.extend(b"CloneForge ".iter().cycle().take(123_457));
        d
    }

    fn create(dir: &TempDir, data: &[u8], out: &str) -> ImageOutcome {
        std::fs::write(dir.path("src.bin"), data).unwrap();
        let req = ImageRequest {
            source: dir.path("src.bin"),
            source_size: 0,
            output: dir.path(out),
            level: Some(3),
            smart: false,
            fs: None,
            live: None,
        };
        let plan = plan_create_with(&req, &[], None).unwrap();
        run_create(&plan, &never(), &no_progress).unwrap()
    }

    fn restore(dir: &TempDir, image: &str, out: &str, verify: bool) -> Result<RestoreOutcome, String> {
        let req = RestoreRequest { image: dir.path(image), target: dir.path(out), verify };
        let plan = plan_restore_with(&req, &[])?;
        run_restore(&plan, &never(), &no_progress)
    }

    #[test]
    fn header_ida_e_volta_e_compatibilidade() {
        let h = ImageHeader {
            mode: MODE_SMART_BITMAP,
            source_size: 0x1122_3344_5566,
            block_size: 8 * 1024 * 1024,
            level: 7,
            source_kind: safety::KIND_PARTITION,
            created_unix: 1_790_000_000,
            label: "C:\\ — KINGSTON SNV2S1000G (PhysicalDrive1)".into(),
        };
        assert_eq!(ImageHeader::parse(&h.to_bytes()).unwrap(), h);

        // Rótulo longo com acentos é cortado em fronteira de caractere (≤ 64 bytes).
        let long = ImageHeader { label: "ção".repeat(40), ..h.clone() };
        let parsed = ImageHeader::parse(&long.to_bytes()).unwrap();
        assert!(parsed.label.len() <= 64 && !parsed.label.is_empty());
        assert!(long.label.starts_with(&parsed.label));

        // Header no layout da v0.1.0: área 36..128 zerada.
        let mut old = [0u8; HEADER_SIZE];
        old[0..8].copy_from_slice(CFIMG_MAGIC);
        old[8..12].copy_from_slice(&1u32.to_le_bytes());
        old[16..24].copy_from_slice(&12345u64.to_le_bytes());
        old[24..28].copy_from_slice(&(8u32 * 1024 * 1024).to_le_bytes());
        old[28..32].copy_from_slice(&3i32.to_le_bytes());
        old[32..36].copy_from_slice(&1u32.to_le_bytes());
        let p = ImageHeader::parse(&old).unwrap();
        assert_eq!((p.mode, p.source_size, p.level), (MODE_RAW, 12345, 3));
        assert_eq!((p.source_kind, p.created_unix, p.label.as_str()), (safety::KIND_UNKNOWN, 0, ""));

        // Rejeições
        let mut bad = old;
        bad[0] = b'X';
        assert!(ImageHeader::parse(&bad).is_err());
        let mut v2 = old;
        v2[8..12].copy_from_slice(&2u32.to_le_bytes());
        assert!(ImageHeader::parse(&v2).unwrap_err().contains("versão 2"));
        let mut mode9 = old;
        mode9[12..16].copy_from_slice(&9u32.to_le_bytes());
        assert!(ImageHeader::parse(&mode9).is_err());
    }

    #[test]
    fn imagem_ida_e_volta_bit_identica() {
        let dir = TempDir::new("img-roundtrip");
        let data = sample();
        let out = create(&dir, &data, "a.cfimg");
        assert_eq!(out.logical, data.len() as u64);
        assert_eq!(out.hash, blake3::hash(&data).to_hex().to_string());
        assert_eq!(out.format, "cfimg");
        assert!(out.image_size < data.len() as u64, "zeros/repetição precisam comprimir");
        assert!(!Path::new(&dir.path("a.cfimg.part")).exists(), ".part não pode sobrar");

        let info = read_image_header(&dir.path("a.cfimg")).unwrap();
        assert_eq!(info.source_size, data.len() as u64);
        assert_eq!(info.hash_hex, out.hash);
        assert_eq!(info.mode, MODE_RAW);
        assert_eq!(info.label, "src.bin");
        assert!(info.created_unix > 1_700_000_000);

        let r = restore(&dir, "a.cfimg", "volta.bin", true).unwrap();
        assert_eq!(r.written, data.len() as u64);
        assert!(r.verified);
        assert_eq!(r.hash, out.hash);
        assert_eq!(std::fs::read(dir.path("volta.bin")).unwrap(), data);
    }

    #[test]
    fn imagem_corrompida_e_detectada_mesmo_sem_verificar() {
        // Regressão: com verify=false a imagem corrompida era restaurada e
        // reportada como sucesso.
        let dir = TempDir::new("img-corrupt");
        let data = sample();
        create(&dir, &data, "a.cfimg");
        let good = std::fs::read(dir.path("a.cfimg")).unwrap();

        // 1 bit invertido em vários pontos do trecho comprimido
        for pos in [HEADER_SIZE + 40, good.len() / 2, good.len() - FOOTER_SIZE - 9] {
            let mut bad = good.clone();
            bad[pos] ^= 0x01;
            std::fs::write(dir.path("bad.cfimg"), &bad).unwrap();
            for verify in [false, true] {
                let err = restore(&dir, "bad.cfimg", "bad.bin", verify).unwrap_err();
                assert!(err.contains("corrompida") || err.contains("CORROMPIDA"), "pos {pos}: {err}");
            }
        }
        // Footer (hash) adulterado
        let mut bad = good.clone();
        let last = bad.len() - 1;
        bad[last] ^= 0xFF;
        std::fs::write(dir.path("bad.cfimg"), &bad).unwrap();
        assert!(restore(&dir, "bad.cfimg", "bad.bin", false).unwrap_err().contains("CORROMPIDA"));
        // Truncada
        std::fs::write(dir.path("bad.cfimg"), &good[..good.len() / 2]).unwrap();
        assert!(restore(&dir, "bad.cfimg", "bad.bin", false).is_err());
        std::fs::write(dir.path("bad.cfimg"), &good[..100]).unwrap();
        assert!(restore(&dir, "bad.cfimg", "bad.bin", false).unwrap_err().contains("pequeno demais"));
        assert!(read_image_header(&dir.path("bad.cfimg")).is_err());
    }

    /// Imagem como a v0.1.0 gravava: sem checksum de frame, single-thread,
    /// header com área reservada zerada e tamanho informado pelo chamador.
    fn legacy_image(path: &str, mode: u32, declared: u64, stream: &[u8]) {
        let mut h = [0u8; HEADER_SIZE];
        h[0..8].copy_from_slice(CFIMG_MAGIC);
        h[8..12].copy_from_slice(&1u32.to_le_bytes());
        h[12..16].copy_from_slice(&mode.to_le_bytes());
        h[16..24].copy_from_slice(&declared.to_le_bytes());
        h[24..28].copy_from_slice(&(8u32 * 1024 * 1024).to_le_bytes());
        h[28..32].copy_from_slice(&3i32.to_le_bytes());
        h[32..36].copy_from_slice(&1u32.to_le_bytes());
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(&h).unwrap();
        let mut enc = zstd::stream::write::Encoder::new(f, 3).unwrap();
        enc.write_all(stream).unwrap();
        let mut f = enc.finish().unwrap();
        f.write_all(blake3::hash(stream).as_bytes()).unwrap();
    }

    #[test]
    fn imagens_da_v010_continuam_restauraveis() {
        let dir = TempDir::new("img-legacy");
        let data = sample();
        legacy_image(&dir.path("raw.cfimg"), MODE_RAW, data.len() as u64, &data);
        assert!(restore(&dir, "raw.cfimg", "raw.bin", true).unwrap().verified);
        assert_eq!(std::fs::read(dir.path("raw.bin")).unwrap(), data);

        // Smart da v0.1.0: header declara a PARTIÇÃO (maior que o stream).
        // Era exatamente o caso que sempre falhava no restore.
        legacy_image(&dir.path("smart.cfimg"), MODE_SMART_BITMAP, data.len() as u64 + 4096, &data);
        let r = restore(&dir, "smart.cfimg", "smart.bin", true).unwrap();
        assert_eq!(r.written, data.len() as u64);
        assert_eq!(std::fs::read(dir.path("smart.bin")).unwrap(), data);

        // Em modo raw o tamanho precisa bater exatamente.
        legacy_image(&dir.path("raw-curto.cfimg"), MODE_RAW, data.len() as u64 + 4096, &data);
        assert!(restore(&dir, "raw-curto.cfimg", "x.bin", false).unwrap_err().contains("header declara"));
        // Stream maior que o declarado é recusado.
        legacy_image(&dir.path("raw-longo.cfimg"), MODE_RAW, 1000, &data);
        assert!(restore(&dir, "raw-longo.cfimg", "y.bin", false).unwrap_err().contains("mais dados"));
    }

    #[test]
    fn smart_copy_grava_o_tamanho_real_no_header() {
        // Volume sintético de 64 clusters de 4 KiB + cauda de 512 B (como o
        // setor de boot de backup do NTFS), com metade dos clusters livres.
        const CS: usize = 4096;
        let dir = TempDir::new("img-smart");
        let vol: Vec<u8> = noise(64 * CS + 512, 9).iter().map(|b| b | 1).collect(); // nunca zero
        std::fs::write(dir.path("vol.bin"), &vol).unwrap();
        let mut bitmap = [0u8; 8];
        bitmap[0] = 0xFF; // clusters 0..8
        bitmap[3] = 0x0F; // clusters 24..28
        bitmap[7] = 0x80; // cluster 63
        let map = bitmap_to_map(&bitmap, 64, CS as u64);

        let mut expected = vec![0u8; vol.len()];
        for r in &map.runs {
            expected[r.start as usize..r.end as usize].copy_from_slice(&vol[r.start as usize..r.end as usize]);
        }
        expected[64 * CS..].copy_from_slice(&vol[64 * CS..]); // a cauda é sempre lida

        let file = std::fs::File::open(dir.path("vol.bin")).unwrap();
        let mut reader = SparseReader::new(file, &map, Some(vol.len() as u64));
        let total = reader.total_bytes();
        let header = ImageHeader {
            mode: MODE_SMART_BITMAP,
            source_size: 999_999_999, // provisório errado: precisa ser corrigido ao final
            block_size: 1024 * 1024,
            level: 3,
            source_kind: safety::KIND_PARTITION,
            created_unix: 1,
            label: "teste".into(),
        };
        let (done, hash, _) = write_image(
            &dir.path("smart.cfimg"),
            header,
            &mut reader,
            Some(total),
            total,
            &never(),
            &no_progress,
        )
        .unwrap();
        assert_eq!(done, vol.len() as u64);
        assert_eq!(hash, blake3::hash(&expected));
        assert_eq!(read_image_header(&dir.path("smart.cfimg")).unwrap().source_size, vol.len() as u64);

        let r = restore(&dir, "smart.cfimg", "volta.bin", true).unwrap();
        assert_eq!(r.written, vol.len() as u64);
        assert_eq!(std::fs::read(dir.path("volta.bin")).unwrap(), expected);
    }

    #[test]
    fn falha_ou_cancelamento_nao_deixa_lixo_nem_destroi_imagem_existente() {
        let dir = TempDir::new("img-cancel");
        let data = sample();
        std::fs::write(dir.path("src.bin"), &data).unwrap();
        std::fs::write(dir.path("boa.cfimg"), b"imagem anterior valiosa").unwrap();
        let req = ImageRequest {
            source: dir.path("src.bin"),
            source_size: 0,
            output: dir.path("boa.cfimg"),
            level: None,
            smart: false,
            fs: None,
            live: None,
        };
        let plan = plan_create_with(&req, &[], None).unwrap();
        let cancel = AtomicBool::new(false);
        let err = run_create(&plan, &cancel, &|p| {
            if p.phase == "compressing" {
                cancel.store(true, Ordering::Relaxed);
            }
        })
        .unwrap_err();
        assert!(crate::progress::is_cancelled(&err));
        assert!(!Path::new(&dir.path("boa.cfimg.part")).exists());
        assert_eq!(std::fs::read(dir.path("boa.cfimg")).unwrap(), b"imagem anterior valiosa");

        // Origem mais curta que o esperado → erro e nada de .part
        let mut short: &[u8] = &data[..1000];
        let header = ImageHeader {
            mode: MODE_RAW,
            source_size: 5000,
            block_size: 1024 * 1024,
            level: 3,
            source_kind: 0,
            created_unix: 0,
            label: String::new(),
        };
        let e = write_image(&dir.path("x.part"), header, &mut short, Some(5000), 5000, &never(), &no_progress)
            .unwrap_err();
        assert!(e.contains("terminou antes"), "{e}");
    }

    #[test]
    fn validacao_da_criacao() {
        let dir = TempDir::new("img-plan");
        std::fs::write(dir.path("src.bin"), b"dados").unwrap();
        let req = |output: &str, smart: bool| ImageRequest {
            source: dir.path("src.bin"),
            source_size: 0,
            output: output.into(),
            level: Some(99),
            smart,
            fs: None,
            live: Some(true),
        };
        // Relativo → resolvido contra a pasta base; nível limitado a 19; live só com smart
        let p = plan_create_with(&req("backup.cfimg", false), &[], Some(&dir.0)).unwrap();
        assert_eq!(p.output, dir.path("backup.cfimg"));
        assert_eq!(p.level, 19);
        assert!(!p.live);
        // Smart em arquivo, saída vazia, pasta inexistente, saída = origem, saída = pasta
        assert!(plan_create_with(&req("backup.cfimg", true), &[], Some(&dir.0)).is_err());
        assert!(plan_create_with(&req("  ", false), &[], Some(&dir.0)).is_err());
        assert!(plan_create_with(&req(&dir.path("nao-existe/x.cfimg"), false), &[], None).is_err());
        assert!(plan_create_with(&req(&dir.path("src.bin"), false), &[], None).is_err());
        assert!(plan_create_with(&req(&dir.0.to_string_lossy(), false), &[], None).is_err());
        // Saída em dispositivo
        let dev = if cfg!(windows) { r"\\.\PhysicalDrive9" } else { "/dev/sdz9" };
        assert!(plan_create_with(&req(dev, false), &[], None).unwrap_err().contains("dispositivo"));
    }

    #[test]
    fn restore_recusa_destino_menor_que_a_imagem_antes_de_gravar() {
        let dir = TempDir::new("img-cap");
        let data = sample(); // ~1,7 MB
        create(&dir, &data, "a.cfimg");
        let (id, path) = if cfg!(windows) {
            ("PhysicalDrive7", r"\\.\PhysicalDrive7")
        } else {
            ("sdzz", "/dev/sdzz")
        };
        let small = crate::safety::tests::disk(id, path, 1024 * 1024, false);
        let req = RestoreRequest { image: dir.path("a.cfimg"), target: path.into(), verify: true };
        let err = plan_restore_with(&req, &[small]).unwrap_err();
        assert!(err.contains("menor que o conteúdo da imagem"), "{err}");

        // Disco do sistema e disco não enumerado continuam bloqueados no restore.
        let sys = crate::safety::tests::disk(id, path, 1 << 40, true);
        assert_eq!(plan_restore_with(&req, &[sys]).unwrap_err(), safety::BLOCKED_SYSTEM);
        assert!(plan_restore_with(&req, &[]).is_err());

        // Com capacidade suficiente a validação passa (nada é gravado aqui).
        let big = crate::safety::tests::disk(id, path, 1 << 40, false);
        let plan = plan_restore_with(&req, &[big]).unwrap();
        assert_eq!(plan.target.kind, PathKind::Device);
        assert_eq!(plan.header.unwrap().source_size, data.len() as u64);
    }
}
