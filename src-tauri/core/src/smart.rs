//! Smart Copy — copia somente os blocos realmente alocados do volume/partição.
//!
//! Estratégia por plataforma:
//! - **Windows** (NTFS/ReFS): bitmap de alocação via `FSCTL_GET_VOLUME_BITMAP`.
//!   Intervalos livres são sintetizados como zeros pelo `SparseReader` —
//!   o zstd comprime zeros a vários GB/s, logo a imagem .cfimg fica pequena e
//!   o restore é byte-idêntico ao modo raw (sem formato especial).
//! - **Linux**: stream do `partclone.<fs>` (ext4/xfs/btrfs/ntfs/vfat/exfat),
//!   que já entende o bitmap de cada filesystem. A imagem guarda o stream
//!   partclone comprimido; o restore passa pelo `partclone.restore`.
//! - **macOS**: imagem comprimida nativa via `hdiutil` (UDZO) — o formato .dmg
//!   entende APFS (incl. volumes selados); restore via `asr` (Apple Software
//!   Restore), o caminho suportado pela Apple inclusive no Apple Silicon.

use std::io::{self, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Intervalo de bytes alocados: `[start, end)`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRun {
    pub start: u64,
    pub end: u64,
}

#[derive(Debug, Clone)]
pub struct AllocMap {
    pub cluster_size: u64,
    pub total_clusters: u64,
    pub allocated_clusters: u64,
    pub runs: Vec<ByteRun>,
}

/// Converte o bitmap do volume (1 bit por cluster, LSB primeiro; 1 = alocado)
/// em intervalos de bytes coalescidos.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn bitmap_to_map(bitmap: &[u8], total_clusters: u64, cluster_size: u64) -> AllocMap {
    // Nunca indexa além do bitmap recebido (um cabeçalho corrompido não pode
    // derrubar o app — o release usa panic=abort).
    let total_clusters = total_clusters.min(bitmap.len() as u64 * 8);
    let mut runs = Vec::with_capacity(1024);
    let mut allocated = 0u64;
    let mut open: Option<u64> = None; // cluster inicial do intervalo aberto
    let close = |open: &mut Option<u64>, end: u64, runs: &mut Vec<ByteRun>| {
        if let Some(s) = open.take() {
            runs.push(ByteRun { start: s * cluster_size, end: end * cluster_size });
        }
    };

    let mut i = 0u64;
    while i < total_clusters {
        let byte = bitmap[(i / 8) as usize];
        // Atalho: byte inteiro alocado/livre (volumes grandes têm milhões de bits)
        if i % 8 == 0 && i + 8 <= total_clusters {
            if byte == 0xFF {
                allocated += 8;
                open.get_or_insert(i);
                i += 8;
                continue;
            }
            if byte == 0x00 {
                close(&mut open, i, &mut runs);
                i += 8;
                continue;
            }
        }
        if (byte >> (i % 8)) & 1 == 1 {
            allocated += 1;
            open.get_or_insert(i);
        } else {
            close(&mut open, i, &mut runs);
        }
        i += 1;
    }
    close(&mut open, total_clusters, &mut runs);

    AllocMap { cluster_size, total_clusters, allocated_clusters: allocated, runs }
}

/// Reader que lê apenas os intervalos alocados do volume real e sintetiza
/// zeros nos intervalos livres — o pipeline de compressão/hash nem percebe.
///
/// O stream cobre o DISPOSITIVO inteiro do volume (`device_len`), não só a
/// área de clusters: no NTFS a área de clusters é menor que a partição (o
/// último setor guarda o boot sector de backup). Antes o stream terminava
/// antes do tamanho declarado no header e todo restore de imagem smart
/// falhava com "stream restaurado com X bytes, mas o header declara Y".
#[cfg_attr(not(windows), allow(dead_code))]
pub struct SparseReader<R> {
    inner: R,
    /// Posição real do cursor de `inner` (None = desconhecida → força seek)
    inner_pos: Option<u64>,
    runs: Vec<ByteRun>,
    run_idx: usize,
    pos: u64,
    total_bytes: u64,
    /// Fim da área de clusters: dali em diante (cauda) erro de leitura vira zeros
    soft_from: u64,
}

#[cfg_attr(not(windows), allow(dead_code))]
impl<R: Read + Seek> SparseReader<R> {
    pub fn new(inner: R, map: &AllocMap, device_len: Option<u64>) -> Self {
        let fs_bytes = map.total_clusters * map.cluster_size;
        let total_bytes = device_len.filter(|l| *l >= fs_bytes).unwrap_or(fs_bytes);
        let mut runs = map.runs.clone();
        if total_bytes > fs_bytes {
            // Cauda fora da área de clusters: sempre lida de verdade.
            runs.push(ByteRun { start: fs_bytes, end: total_bytes });
        }
        Self { inner, inner_pos: None, runs, run_idx: 0, pos: 0, total_bytes, soft_from: fs_bytes }
    }

    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    fn read_inner(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.inner_pos != Some(self.pos) {
            self.inner_pos = None;
            self.inner.seek(SeekFrom::Start(self.pos))?;
        }
        match self.inner.read(buf) {
            Ok(n) => {
                self.inner_pos = Some(self.pos + n as u64);
                Ok(n)
            }
            Err(e) => {
                self.inner_pos = None;
                Err(e)
            }
        }
    }
}

impl<R: Read + Seek> Read for SparseReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.total_bytes || buf.is_empty() {
            return Ok(0);
        }
        let limit = (self.total_bytes - self.pos).min(buf.len() as u64) as usize;

        // pos é monotônica — cursor linear sobre intervalos ordenados
        while self.run_idx < self.runs.len() && self.runs[self.run_idx].end <= self.pos {
            self.run_idx += 1;
        }
        let Some(run) = self.runs.get(self.run_idx).copied() else {
            buf[..limit].fill(0); // cauda inteira livre
            self.pos += limit as u64;
            return Ok(limit);
        };

        if self.pos < run.start {
            // intervalo livre antes do próximo alocado → zeros sintetizados
            let n = (run.start - self.pos).min(limit as u64) as usize;
            buf[..n].fill(0);
            self.pos += n as u64;
            return Ok(n);
        }

        // dentro de intervalo alocado → leitura real do volume
        let n_max = (run.end - self.pos).min(limit as u64) as usize;
        match self.read_inner(&mut buf[..n_max]) {
            Ok(n) if n > 0 => {
                self.pos += n as u64;
                Ok(n)
            }
            // Cauda além do filesystem: alguns drivers não deixam ler ali.
            // Não há dado de arquivo nessa região — zeros, sem falhar a imagem.
            _ if self.pos >= self.soft_from => {
                buf[..n_max].fill(0);
                self.pos += n_max as u64;
                Ok(n_max)
            }
            Ok(_) => Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("o volume terminou em {} bytes, dentro de uma área alocada", self.pos),
            )),
            Err(e) => Err(e),
        }
    }
}

// ───────────────────── utilitários de processo filho ────────────────────

/// Últimos bytes do stderr de um processo filho, drenados em thread própria.
/// Drenar é obrigatório: um pipe de stderr cheio (64 KiB) trava o filho —
/// o partclone escreve progresso ali o tempo todo.
#[cfg_attr(windows, allow(dead_code))]
pub(crate) struct StderrTail {
    buf: Arc<Mutex<Vec<u8>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[cfg_attr(windows, allow(dead_code))]
impl StderrTail {
    const KEEP: usize = 4096;

    pub(crate) fn spawn(mut reader: impl Read + Send + 'static) -> Self {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let shared = buf.clone();
        let thread = std::thread::spawn(move || {
            let mut chunk = [0u8; 4096];
            while let Ok(n) = reader.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                let mut b = shared.lock().unwrap_or_else(|e| e.into_inner());
                b.extend_from_slice(&chunk[..n]);
                if b.len() > Self::KEEP {
                    let cut = b.len() - Self::KEEP;
                    b.drain(..cut);
                }
            }
        });
        Self { buf, thread: Some(thread) }
    }

    /// Chamar só depois que o filho terminou (o pipe fecha e a thread acaba).
    pub(crate) fn finish(&mut self) -> String {
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        let b = self.buf.lock().unwrap_or_else(|e| e.into_inner());
        // partclone usa \r para redesenhar o progresso
        String::from_utf8_lossy(&b).replace('\r', "\n").trim().to_string()
    }
}

/// Executa um comando externo até o fim, matando-o se o usuário cancelar.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn run_cancellable(
    mut cmd: std::process::Command,
    what: &str,
    cancel: &AtomicBool,
) -> Result<(), String> {
    use std::process::Stdio;
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("falha ao iniciar {what}: {e}"))?;
    let mut tail = child.stderr.take().map(StderrTail::spawn);
    loop {
        match child.try_wait() {
            Ok(Some(st)) if st.success() => return Ok(()),
            Ok(Some(st)) => {
                let detail = tail.as_mut().map(|t| t.finish()).unwrap_or_default();
                return Err(format!("{what} saiu com {st}: {detail}"));
            }
            Ok(None) => {}
            Err(e) => return Err(format!("{what}: {e}")),
        }
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(crate::progress::CANCELLED.into());
        }
        std::thread::sleep(std::time::Duration::from_millis(150));
    }
}

/// Binário do partclone para um filesystem — LISTA DE PERMISSÃO. O valor de
/// `fs` vem da requisição e antes era concatenado direto no nome do
/// executável (rodado como root): `fs = "../../tmp/x"` executava qualquer coisa.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn partclone_tool(fs: &str) -> Option<&'static str> {
    Some(match fs.trim().to_ascii_lowercase().as_str() {
        "ext2" => "partclone.ext2",
        "ext3" => "partclone.ext3",
        "ext4" => "partclone.ext4",
        "xfs" => "partclone.xfs",
        "btrfs" => "partclone.btrfs",
        "ntfs" => "partclone.ntfs",
        "vfat" | "fat" | "fat12" | "fat16" | "fat32" | "msdos" => "partclone.vfat",
        "exfat" => "partclone.exfat",
        "f2fs" => "partclone.f2fs",
        _ => return None,
    })
}

// ─────────────────────────────── Windows ───────────────────────────────
#[cfg(windows)]
pub mod platform {
    use super::{bitmap_to_map, AllocMap, SparseReader};
    use std::fs::File;
    use std::os::windows::io::AsRawHandle;
    use windows::core::HSTRING;
    use windows::Win32::Foundation::{ERROR_MORE_DATA, HANDLE};
    use windows::Win32::Storage::FileSystem::{
        GetDiskFreeSpaceW, GetVolumeInformationByHandleW, GetVolumeInformationW,
    };
    use windows::Win32::System::Ioctl::{
        FSCTL_ALLOW_EXTENDED_DASD_IO, FSCTL_GET_VOLUME_BITMAP, GET_LENGTH_INFORMATION,
        IOCTL_DISK_GET_LENGTH_INFO, STARTING_LCN_INPUT_BUFFER,
    };
    use windows::Win32::System::IO::DeviceIoControl;

    /// Layout do VOLUME_BITMAP_BUFFER: LARGE_INTEGER StartingLcn @0,
    /// LARGE_INTEGER BitmapSize @8, BYTE Buffer[] @16 (alinhamento 8).
    const VBB_HEADER: usize = 16;

    /// "C:\" a partir de "\\.\C:"; para `\\?\Volume{guid}` e dispositivos de
    /// shadow copy basta acrescentar a barra final.
    fn root_of(volume_path: &str) -> HSTRING {
        let v = volume_path.trim_start_matches("\\\\.\\");
        HSTRING::from(format!("{v}\\"))
    }

    fn filesystem_name(file: &File, volume_path: &str) -> Option<String> {
        let mut name = [0u16; 64];
        // SAFETY: buffers próprios; handle válido enquanto `file` vive.
        let ok = unsafe {
            GetVolumeInformationByHandleW(
                HANDLE(file.as_raw_handle()),
                None,
                None,
                None,
                None,
                Some(&mut name),
            )
            .is_ok()
                || GetVolumeInformationW(&root_of(volume_path), None, None, None, None, Some(&mut name))
                    .is_ok()
        };
        if !ok {
            return None;
        }
        let len = name.iter().position(|c| *c == 0).unwrap_or(name.len());
        Some(String::from_utf16_lossy(&name[..len]))
    }

    /// Consulta o bitmap de alocação de um volume NTFS/ReFS (ex: `\\.\C:`).
    /// Requer Administrador.
    fn volume_bitmap(file: &File, volume_path: &str) -> Result<AllocMap, String> {
        let handle = HANDLE(file.as_raw_handle());

        // FAT/exFAT numeram os clusters a partir da área de dados, não do
        // início do volume: o bitmap ficaria deslocado e a imagem corrompida
        // sem nenhum aviso. Smart copy só para NTFS/ReFS.
        match filesystem_name(file, volume_path) {
            Some(fs) if fs.eq_ignore_ascii_case("NTFS") || fs.eq_ignore_ascii_case("ReFS") => {}
            Some(fs) => {
                return Err(format!(
                    "Smart Copy no Windows só suporta NTFS/ReFS — o volume {volume_path} é {fs}. Desmarque Smart Copy para cópia raw."
                ))
            }
            None => {
                return Err(format!(
                    "não foi possível identificar o filesystem de {volume_path} — desmarque Smart Copy para cópia raw"
                ))
            }
        }

        let mut spc = 0u32;
        let mut bps = 0u32;
        let mut free_c = 0u32;
        let mut total_c = 0u32;
        // SAFETY: ponteiros para variáveis locais válidas.
        unsafe {
            GetDiskFreeSpaceW(
                &root_of(volume_path),
                Some(&mut spc),
                Some(&mut bps),
                Some(&mut free_c),
                Some(&mut total_c),
            )
        }
        .map_err(|e| format!("GetDiskFreeSpaceW({volume_path}): {e}"))?;
        let cluster_size = spc as u64 * bps as u64;
        if cluster_size == 0 {
            return Err(format!("tamanho de cluster inválido em {volume_path}"));
        }

        // FSCTL_GET_VOLUME_BITMAP com buffer crescente até caber o bitmap
        // inteiro (bitmaps de volumes grandes chegam a dezenas de MB).
        let input = STARTING_LCN_INPUT_BUFFER { StartingLcn: 0 };
        let mut cap: usize = 32 * 1024 * 1024;
        loop {
            let mut out = vec![0u8; cap];
            let mut returned = 0u32;
            // SAFETY: buffers próprios com os tamanhos informados.
            let res = unsafe {
                DeviceIoControl(
                    handle,
                    FSCTL_GET_VOLUME_BITMAP,
                    Some(&input as *const _ as *const _),
                    std::mem::size_of_val(&input) as u32,
                    Some(out.as_mut_ptr() as *mut _),
                    out.len() as u32,
                    Some(&mut returned),
                    None,
                )
            };
            // O cabeçalho é preenchido mesmo em ERROR_MORE_DATA:
            // BitmapSize (i64 @8) = total de clusters do volume.
            let clusters = i64::from_le_bytes(out[8..16].try_into().unwrap_or([0; 8]));
            if clusters <= 0 {
                return Err(format!("FSCTL_GET_VOLUME_BITMAP({volume_path}): bitmap vazio"));
            }
            let clusters = clusters as u64;
            let nbytes = clusters.div_ceil(8) as usize;
            match res {
                Ok(()) => {
                    let bitmap = out
                        .get(VBB_HEADER..VBB_HEADER + nbytes)
                        .ok_or_else(|| format!("FSCTL_GET_VOLUME_BITMAP({volume_path}): resposta truncada"))?;
                    return Ok(bitmap_to_map(bitmap, clusters, cluster_size));
                }
                Err(e) if e.code() == ERROR_MORE_DATA.to_hresult() && cap < (1 << 31) => {
                    cap = (VBB_HEADER + nbytes + 4096).max(cap.saturating_mul(2)).min(1 << 31);
                }
                Err(e) => {
                    return Err(format!("FSCTL_GET_VOLUME_BITMAP({volume_path}): {e}"));
                }
            }
        }
    }

    /// Tamanho do dispositivo do volume (a partição inteira).
    fn device_len(file: &File) -> Option<u64> {
        let mut info = GET_LENGTH_INFORMATION::default();
        let mut returned = 0u32;
        // SAFETY: struct de saída própria com o tamanho correto.
        unsafe {
            DeviceIoControl(
                HANDLE(file.as_raw_handle()),
                IOCTL_DISK_GET_LENGTH_INFO,
                None,
                0,
                Some(&mut info as *mut _ as *mut _),
                std::mem::size_of::<GET_LENGTH_INFORMATION>() as u32,
                Some(&mut returned),
                None,
            )
        }
        .ok()?;
        u64::try_from(info.Length).ok()
    }

    pub struct SmartVolume {
        pub reader: SparseReader<File>,
        /// Resumo para o log da UI/auditoria
        pub summary: String,
    }

    /// Abre um volume para Smart Copy (`\\.\C:`, `\\?\Volume{guid}` ou o
    /// dispositivo de um shadow copy VSS).
    pub fn open_volume(volume_path: &str) -> Result<SmartVolume, String> {
        let file = File::open(volume_path)
            .map_err(|e| crate::io::open_error("volume", volume_path, &e))?;
        let map = volume_bitmap(&file, volume_path)?;
        let len = device_len(&file);
        // Permite ler até o fim da partição (além do que o NTFS considera
        // "o volume"). Melhor esforço: sem isso a cauda é lida como zeros.
        // SAFETY: handle válido; sem buffers.
        unsafe {
            let _ = DeviceIoControl(
                HANDLE(file.as_raw_handle()),
                FSCTL_ALLOW_EXTENDED_DASD_IO,
                None,
                0,
                None,
                0,
                None,
                None,
            );
        }
        let summary = format!(
            "smart copy: {}/{} clusters alocados ({:.1}% do volume, cluster de {} bytes)",
            map.allocated_clusters,
            map.total_clusters,
            map.allocated_clusters as f64 / map.total_clusters.max(1) as f64 * 100.0,
            map.cluster_size
        );
        tracing::info!("{summary}");
        Ok(SmartVolume { reader: SparseReader::new(file, &map, len), summary })
    }
}

// ─────────────────────────────── Linux ─────────────────────────────────
#[cfg(target_os = "linux")]
pub mod platform {
    use super::{partclone_tool, StderrTail};
    use std::io::{Read, Write};
    use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

    /// Stream do `partclone.<fs>` clonando a partição para stdout.
    /// O formato partclone já é sparse-aware por filesystem.
    /// Requer root (via pkexec) e o pacote `partclone` instalado.
    pub struct PartcloneReader {
        child: Child,
        stdout: ChildStdout,
        tail: StderrTail,
        reaped: bool,
    }

    pub fn partclone_spawn(source_part: &str, fs: &str) -> Result<PartcloneReader, String> {
        let tool = partclone_tool(fs).ok_or_else(|| {
            format!("Smart Copy não suporta o filesystem `{fs}` — desmarque Smart Copy para cópia raw")
        })?;
        let mut child = Command::new(tool)
            .args(["-c", "-s", source_part, "-o", "-"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("falha ao iniciar `{tool}`: {e} (pacote partclone instalado?)"))?;
        let stdout = child.stdout.take().ok_or("stdout do partclone indisponível")?;
        let tail = StderrTail::spawn(child.stderr.take().ok_or("stderr do partclone indisponível")?);
        Ok(PartcloneReader { child, stdout, tail, reaped: false })
    }

    impl Read for PartcloneReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.stdout.read(buf)?;
            if n == 0 && !self.reaped {
                self.reaped = true;
                let st = self.child.wait()?;
                if !st.success() {
                    return Err(std::io::Error::other(format!(
                        "partclone saiu com {st}: {}",
                        self.tail.finish()
                    )));
                }
            }
            Ok(n)
        }
    }

    impl Drop for PartcloneReader {
        fn drop(&mut self) {
            if !self.reaped {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }
    }

    /// `partclone.restore` recebendo o stream pelo stdin e gravando no destino.
    pub struct PartcloneWriter {
        child: Child,
        stdin: Option<ChildStdin>,
        tail: StderrTail,
        reaped: bool,
    }

    pub fn partclone_restore_spawn(target: &str, target_is_file: bool) -> Result<PartcloneWriter, String> {
        // -o exige que o destino exista (dispositivo); -O cria/sobrescreve arquivo.
        let out_flag = if target_is_file { "-O" } else { "-o" };
        let mut child = Command::new("partclone.restore")
            .args(["-s", "-", out_flag, target])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("falha ao iniciar `partclone.restore`: {e} (pacote partclone instalado?)"))?;
        let stdin = child.stdin.take();
        let tail = StderrTail::spawn(child.stderr.take().ok_or("stderr do partclone indisponível")?);
        Ok(PartcloneWriter { child, stdin, tail, reaped: false })
    }

    impl PartcloneWriter {
        pub fn write_all(&mut self, data: &[u8]) -> Result<(), String> {
            let stdin = self.stdin.as_mut().ok_or("stdin do partclone já fechado")?;
            if let Err(e) = stdin.write_all(data) {
                // Pipe quebrado = o partclone morreu; o motivo está no stderr.
                self.stdin = None;
                self.reaped = true;
                let st = self.child.wait().map_err(|w| w.to_string())?;
                return Err(format!("partclone.restore falhou ({st}; {e}): {}", self.tail.finish()));
            }
            Ok(())
        }

        /// Fecha o stdin (EOF) e espera o partclone terminar de gravar.
        pub fn finish(mut self) -> Result<(), String> {
            self.stdin = None;
            self.reaped = true;
            let st = self.child.wait().map_err(|e| format!("partclone.restore: {e}"))?;
            if st.success() {
                Ok(())
            } else {
                Err(format!("partclone.restore saiu com {st}: {}", self.tail.finish()))
            }
        }
    }

    impl Drop for PartcloneWriter {
        fn drop(&mut self) {
            if !self.reaped {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }
    }
}

// ─────────────────────────────── macOS ─────────────────────────────────

/// Argumentos do hdiutil para criar .dmg comprimido (UDZO) a partir de um
/// dispositivo/partição (ex: `/dev/disk4s2`). UDZO (zlib) é o formato que o
/// `asr` restaura. `-ov` sobrescreve o arquivo de saída.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn hdiutil_create_args(source_dev: &str, output: &str) -> Vec<String> {
    ["create", "-srcdevice", source_dev, "-format", "UDZO", "-ov", "-o", output]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// Argumentos do asr para restaurar um .dmg num disco — o único caminho que
/// recria corretamente containers APFS e volumes selados (Apple Silicon).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn asr_restore_args(image: &str, target_dev: &str) -> Vec<String> {
    ["restore", "--source", image, "--target", target_dev, "--erase", "--noprompt"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

#[cfg(target_os = "macos")]
pub mod platform {
    use super::{asr_restore_args, hdiutil_create_args, run_cancellable};
    use std::sync::atomic::AtomicBool;

    pub fn create_dmg(source_dev: &str, output: &str, cancel: &AtomicBool) -> Result<(), String> {
        let mut cmd = std::process::Command::new("hdiutil");
        cmd.args(hdiutil_create_args(source_dev, output));
        run_cancellable(cmd, "hdiutil", cancel)
            .map_err(|e| format!("{e} (Acesso Total ao Disco concedido?)"))
    }

    pub fn restore_dmg(image: &str, target_dev: &str, cancel: &AtomicBool) -> Result<(), String> {
        let mut cmd = std::process::Command::new("asr");
        cmd.args(asr_restore_args(image, target_dev));
        run_cancellable(cmd, "asr", cancel)
    }

    /// BLAKE3 de um arquivo (para catalogar o .dmg criado)
    pub fn hash_file(path: &str) -> Result<String, String> {
        let mut f = std::fs::File::open(path).map_err(|e| e.to_string())?;
        let mut h = blake3::Hasher::new();
        std::io::copy(&mut f, &mut h).map_err(|e| e.to_string())?;
        Ok(h.finalize().to_hex().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn bitmap_vira_intervalos() {
        // clusters: 0-1 alocados, 2-7 livres | 8-15 alocados | 16-17 alocados, resto livre
        let bitmap = [0b0000_0011u8, 0xFF, 0b0000_0011, 0x00];
        let map = bitmap_to_map(&bitmap, 30, 4096);
        assert_eq!(map.allocated_clusters, 12);
        assert_eq!(
            map.runs,
            vec![
                ByteRun { start: 0, end: 2 * 4096 },
                ByteRun { start: 8 * 4096, end: 18 * 4096 }, // byte cheio emenda com o seguinte
            ]
        );
        // Tudo alocado, total não múltiplo de 8
        let all = bitmap_to_map(&[0xFF, 0xFF], 13, 512);
        assert_eq!(all.runs, vec![ByteRun { start: 0, end: 13 * 512 }]);
        assert_eq!(all.allocated_clusters, 13);
        // Bitmap mais curto que o declarado não estoura
        let short = bitmap_to_map(&[0xFF], 1_000_000, 512);
        assert_eq!(short.total_clusters, 8);
        // Vazio
        assert!(bitmap_to_map(&[0, 0], 16, 512).runs.is_empty());
    }

    /// "Volume" sintético: cada byte vale (offset % 251) + 1 (nunca zero).
    fn volume(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8 + 1).collect()
    }

    #[test]
    fn sparse_reader_zera_livres_e_le_a_cauda() {
        const CS: u64 = 512;
        // 16 clusters de 512 B + cauda de 300 B fora da área de clusters
        let vol = volume(16 * 512 + 300);
        // clusters alocados: 1, 2, 8..11
        let bitmap = [0b0000_0110u8, 0b0000_1111];
        let map = bitmap_to_map(&bitmap, 16, CS);
        let mut r = SparseReader::new(Cursor::new(vol.clone()), &map, Some(vol.len() as u64));
        assert_eq!(r.total_bytes(), vol.len() as u64);

        let mut out = Vec::new();
        // buffer pequeno e desalinhado força leituras parciais em todos os caminhos
        let mut buf = [0u8; 700];
        loop {
            let n = r.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        assert_eq!(out.len(), vol.len(), "o stream cobre o dispositivo inteiro");

        let alloc = |off: usize| {
            let c = off / 512;
            c == 1 || c == 2 || (8..12).contains(&c) || off >= 16 * 512
        };
        for (i, b) in out.iter().enumerate() {
            if alloc(i) {
                assert_eq!(*b, vol[i], "byte alocado {i}");
            } else {
                assert_eq!(*b, 0, "byte livre {i} deveria ser zero");
            }
        }
    }

    #[test]
    fn sparse_reader_sem_tamanho_do_dispositivo_usa_area_de_clusters() {
        let vol = volume(8 * 512);
        let map = bitmap_to_map(&[0xFF], 8, 512);
        let mut r = SparseReader::new(Cursor::new(vol.clone()), &map, None);
        let mut out = Vec::new();
        r.read_to_end(&mut out).unwrap();
        assert_eq!(out, vol);
    }

    #[test]
    fn sparse_reader_volume_truncado_em_area_alocada_e_erro() {
        // O bitmap diz que há 8 clusters alocados, mas o "volume" só tem 4.
        let map = bitmap_to_map(&[0xFF], 8, 512);
        let mut r = SparseReader::new(Cursor::new(volume(4 * 512)), &map, None);
        let mut out = Vec::new();
        let err = r.read_to_end(&mut out).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn sparse_reader_cauda_ilegivel_vira_zeros() {
        // Dispositivo declara 300 B além dos clusters, mas o reader não entrega.
        let map = bitmap_to_map(&[0xFF], 8, 512);
        let vol = volume(8 * 512);
        let mut r = SparseReader::new(Cursor::new(vol.clone()), &map, Some(8 * 512 + 300));
        let mut out = Vec::new();
        r.read_to_end(&mut out).unwrap();
        assert_eq!(out.len(), 8 * 512 + 300);
        assert_eq!(&out[..8 * 512], &vol[..]);
        assert!(out[8 * 512..].iter().all(|b| *b == 0));
    }

    #[test]
    fn partclone_lista_de_permissao() {
        assert_eq!(partclone_tool("ext4"), Some("partclone.ext4"));
        assert_eq!(partclone_tool(" NTFS "), Some("partclone.ntfs"));
        assert_eq!(partclone_tool("vfat"), Some("partclone.vfat"));
        // Injeção de caminho / filesystems sem suporte
        for bad in ["../../tmp/x", "ext4; rm -rf /", "", "swap", "crypto_LUKS", "LVM2_member", "restore"] {
            assert_eq!(partclone_tool(bad), None, "{bad}");
        }
    }

    #[test]
    fn argumentos_macos() {
        assert_eq!(
            hdiutil_create_args("/dev/disk4s2", "/tmp/a.dmg"),
            ["create", "-srcdevice", "/dev/disk4s2", "-format", "UDZO", "-ov", "-o", "/tmp/a.dmg"]
        );
        assert_eq!(
            asr_restore_args("/tmp/a.dmg", "/dev/disk4"),
            ["restore", "--source", "/tmp/a.dmg", "--target", "/dev/disk4", "--erase", "--noprompt"]
        );
    }

    fn shell(script: &str) -> std::process::Command {
        if cfg!(windows) {
            let mut c = std::process::Command::new("cmd");
            c.args(["/C", script]);
            c
        } else {
            let mut c = std::process::Command::new("sh");
            c.args(["-c", script]);
            c
        }
    }

    #[test]
    fn comando_externo_sucesso_falha_e_cancelamento() {
        let never = AtomicBool::new(false);
        assert!(run_cancellable(shell("exit 0"), "teste", &never).is_ok());

        let err = run_cancellable(shell("echo motivo-do-erro 1>&2 & exit 3"), "teste", &never).unwrap_err();
        assert!(err.contains("teste saiu com"), "{err}");
        assert!(err.contains("motivo-do-erro"), "{err}");

        assert!(run_cancellable(std::process::Command::new("binario-que-nao-existe-xyz"), "teste", &never)
            .unwrap_err()
            .contains("falha ao iniciar"));

        // Processo longo é morto quando o cancelamento chega.
        let cancel = AtomicBool::new(true);
        let long = if cfg!(windows) {
            let mut c = std::process::Command::new("ping");
            c.args(["-n", "30", "127.0.0.1"]);
            c
        } else {
            let mut c = std::process::Command::new("sleep");
            c.arg("30");
            c
        };
        let t0 = std::time::Instant::now();
        let err = run_cancellable(long, "teste", &cancel).unwrap_err();
        assert!(crate::progress::is_cancelled(&err), "{err}");
        assert!(t0.elapsed().as_secs() < 10);
    }
}
