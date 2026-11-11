//! Smart Copy — copia somente os blocos realmente alocados do volume/partição.
//!
//! Estratégia por plataforma:
//! - **Windows** (NTFS/ReFS): bitmap de alocação via `FSCTL_GET_VOLUME_BITMAP`.
//!   Intervalos livres são sintetizados como zeros pelo `SparseVolumeReader` —
//!   o zstd comprime zeros a vários GB/s, logo a imagem .cfimg fica pequena e
//!   o restore é byte-idêntico ao modo raw (sem formato especial).
//! - **Linux**: stream do `partclone.<fs>` (ext4/xfs/btrfs/ntfs/fat32/exfat),
//!   que já entende o bitmap de cada filesystem. A imagem guarda o stream
//!   partclone comprimido; o restore exige `partclone.restore`.
//! - **macOS**: imagem comprimida nativa via `hdiutil` (UDZO) — o formato .dmg
//!   entende APFS (incl. volumes selados); restore via `asr` (Apple Software
//!   Restore), o caminho suportado pela Apple inclusive no Apple Silicon.

#[cfg(windows)]
use serde::Serialize;

/// Intervalo de clusters alocados: [start_cluster, start_cluster + clusters)
#[cfg(windows)]
#[derive(Debug, Clone, Copy, Serialize)]
pub struct AllocRun {
    pub start_cluster: u64,
    pub clusters: u64,
}

#[cfg(windows)]
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AllocMap {
    pub cluster_size: u64,
    pub total_clusters: u64,
    pub allocated_clusters: u64,
    pub runs: Vec<AllocRun>,
}

// ─────────────────────────────── Windows ───────────────────────────────
#[cfg(windows)]
pub mod platform {
    use super::{AllocMap, AllocRun};
    use std::io::{Read, Seek, SeekFrom};
    use windows::core::HSTRING;
    use windows::Win32::Foundation::{CloseHandle, ERROR_MORE_DATA, HANDLE};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, GetDiskFreeSpaceW, FILE_FLAGS_AND_ATTRIBUTES, FILE_GENERIC_READ,
        FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows::Win32::System::Ioctl::{
        FSCTL_GET_VOLUME_BITMAP, STARTING_LCN_INPUT_BUFFER,
    };
    use windows::Win32::System::IO::DeviceIoControl;

    struct HandleGuard(HANDLE);
    impl Drop for HandleGuard {
        fn drop(&mut self) {
            unsafe { let _ = CloseHandle(self.0); }
        }
    }

    /// Layout do VOLUME_BITMAP_BUFFER: LARGE_INTEGER StartingLcn @0,
    /// LARGE_INTEGER BitmapSize @8, BYTE Buffer[] @16 (alinhamento 8).
    const VBB_HEADER: usize = 16;

    /// Consulta o bitmap de alocação de um volume NTFS/ReFS (ex: `\\.\C:`).
    /// Requer Administrador. FAT32/exFAT não suportam este FSCTL — use raw.
    pub fn volume_bitmap(volume_path: &str) -> Result<AllocMap, String> {
        unsafe {
            let path = HSTRING::from(volume_path);
            let handle = CreateFileW(
                &path,
                FILE_GENERIC_READ.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            )
            .map_err(|e| format!("CreateFileW({volume_path}): {e} (rode como Administrador)"))?;
            if handle.is_invalid() {
                return Err("CreateFileW retornou handle inválido".into());
            }
            let _guard = HandleGuard(handle);

            // Tamanho do cluster do volume ("C:\" a partir de "\\.\C:")
            let root = {
                let v = volume_path.trim_start_matches("\\\\.\\");
                HSTRING::from(format!("{v}\\"))
            };
            let mut spc = 0u32;
            let mut bps = 0u32;
            let mut free_c = 0u32;
            let mut total_c = 0u32;
            GetDiskFreeSpaceW(&root, Some(&mut spc), Some(&mut bps), Some(&mut free_c), Some(&mut total_c))
                .map_err(|e| format!("GetDiskFreeSpaceW({volume_path}): {e}"))?;
            let cluster_size = spc as u64 * bps as u64;

            // FSCTL_GET_VOLUME_BITMAP com buffer crescente até caber o bitmap
            // inteiro (bitmaps de volumes grandes chegam a dezenas de MB).
            let input = STARTING_LCN_INPUT_BUFFER { StartingLcn: 0 };
            let mut cap: usize = 32 * 1024 * 1024;
            let (total_clusters, bitmap) = loop {
                let mut out = vec![0u8; cap];
                let mut returned = 0u32;
                let res = DeviceIoControl(
                    handle,
                    FSCTL_GET_VOLUME_BITMAP,
                    Some(&input as *const _ as *const _),
                    std::mem::size_of_val(&input) as u32,
                    Some(out.as_mut_ptr() as *mut _),
                    out.len() as u32,
                    Some(&mut returned),
                    None,
                );
                // O cabeçalho é preenchido mesmo em ERROR_MORE_DATA:
                // BitmapSize (i64 @8) = total de clusters do volume.
                let clusters = i64::from_le_bytes(out[8..16].try_into().unwrap()) as u64;
                match res {
                    Ok(()) => {
                        let nbytes = (clusters as usize).div_ceil(8);
                        break (clusters, out[VBB_HEADER..VBB_HEADER + nbytes].to_vec());
                    }
                    Err(e) if e.code() == ERROR_MORE_DATA.to_hresult() => {
                        let need = VBB_HEADER + (clusters as usize).div_ceil(8) + 4096;
                        cap = if need > cap { need } else { cap.saturating_mul(2) };
                        continue;
                    }
                    Err(e) => {
                        return Err(format!(
                            "FSCTL_GET_VOLUME_BITMAP({volume_path}): {e} (o volume é NTFS/ReFS?)"
                        ))
                    }
                }
            };

            // Converte bits → runs coalescidos (1 = alocado)
            let mut runs = Vec::with_capacity(1024);
            let mut allocated = 0u64;
            let mut cur: Option<u64> = None;
            for i in 0..total_clusters {
                let bit = (bitmap[(i / 8) as usize] >> (i % 8)) & 1;
                if bit == 1 {
                    allocated += 1;
                    if cur.is_none() {
                        cur = Some(i);
                    }
                } else if let Some(s) = cur.take() {
                    runs.push(AllocRun { start_cluster: s, clusters: i - s });
                }
            }
            if let Some(s) = cur {
                runs.push(AllocRun { start_cluster: s, clusters: total_clusters - s });
            }

            Ok(AllocMap { cluster_size, total_clusters, allocated_clusters: allocated, runs })
        }
    }

    /// Reader que lê apenas os clusters alocados do volume real e sintetiza
    /// zeros nos intervalos livres — o pipeline de compressão/hash nem percebe.
    pub struct SparseVolumeReader {
        inner: std::fs::File,
        inner_pos: u64,
        map: AllocMap,
        run_idx: usize,
        pos: u64,
        total_bytes: u64,
    }

    impl SparseVolumeReader {
        pub fn open(volume_path: &str) -> Result<Self, String> {
            let map = volume_bitmap(volume_path)?;
            let inner = std::fs::File::open(volume_path)
                .map_err(|e| format!("abrir volume {volume_path}: {e}"))?;
            let total_bytes = map.total_clusters * map.cluster_size;
            tracing::info!(
                "smart copy: {}/{} clusters alocados ({:.1}% do volume)",
                map.allocated_clusters,
                map.total_clusters,
                map.allocated_clusters as f64 / map.total_clusters.max(1) as f64 * 100.0
            );
            Ok(Self { inner, inner_pos: 0, map, run_idx: 0, pos: 0, total_bytes })
        }
    }

    impl Read for SparseVolumeReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.pos >= self.total_bytes {
                return Ok(0);
            }
            let cs = self.map.cluster_size;
            let limit = (self.total_bytes - self.pos).min(buf.len() as u64) as usize;

            // pos é monotônica — cursor linear sobre runs ordenados
            while self.run_idx < self.map.runs.len() {
                let r = &self.map.runs[self.run_idx];
                if (r.start_cluster + r.clusters) * cs <= self.pos {
                    self.run_idx += 1;
                } else {
                    break;
                }
            }

            if self.run_idx >= self.map.runs.len() {
                buf[..limit].fill(0); // cauda inteira livre
                self.pos += limit as u64;
                return Ok(limit);
            }

            let run = self.map.runs[self.run_idx];
            let run_start = run.start_cluster * cs;
            let run_end = run_start + run.clusters * cs;

            if self.pos < run_start {
                // gap livre antes do próximo run → zeros sintetizados
                let n = ((run_start - self.pos).min(limit as u64)) as usize;
                buf[..n].fill(0);
                self.pos += n as u64;
                return Ok(n);
            }

            // dentro de run alocado → leitura real do volume
            if self.inner_pos != self.pos {
                self.inner.seek(SeekFrom::Start(self.pos))?;
                self.inner_pos = self.pos;
            }
            let n_max = ((run_end - self.pos).min(limit as u64)) as usize;
            let n = self.inner.read(&mut buf[..n_max])?;
            self.pos += n as u64;
            self.inner_pos = self.pos;
            Ok(n)
        }
    }
}

// ───────────────────── Processos-filho como stream ─────────────────────

/// Binário do partclone para um filesystem reportado pelo `lsblk`.
///
/// Lista fechada de propósito: o valor chega do cliente e vira o nome de um
/// programa executado como root pelo helper.
#[cfg(any(target_os = "linux", test))]
pub fn partclone_tool(fs: &str) -> Result<String, String> {
    const SUPPORTED: &[&str] = &[
        "ext2", "ext3", "ext4", "xfs", "btrfs", "ntfs", "vfat", "fat12", "fat16", "fat32",
        "exfat", "f2fs", "hfsplus", "reiserfs", "jfs", "nilfs2", "minix",
    ];
    let fs = fs.trim().to_lowercase();
    if SUPPORTED.contains(&fs.as_str()) {
        Ok(format!("partclone.{fs}"))
    } else {
        Err(format!(
            "filesystem `{fs}` não é suportado pelo smart copy — use a cópia raw"
        ))
    }
}

/// Programas externos usados como origem (stdout) ou destino (stdin) de um
/// stream. Independente de plataforma para poder ser testado fora do Linux.
#[cfg(any(target_os = "linux", all(test, unix)))]
pub mod pipe {
    use std::io::{Read, Write};
    use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
    use std::thread::JoinHandle;

    const TAIL_BYTES: usize = 2048;

    /// Drena o stderr do filho numa thread, guardando só o final. Sem isso o
    /// pipe enche (o partclone escreve progresso continuamente) e o filho trava.
    fn drain_tail(mut stderr: impl Read + Send + 'static) -> JoinHandle<String> {
        std::thread::spawn(move || {
            let mut tail: Vec<u8> = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                match stderr.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        tail.extend_from_slice(&buf[..n]);
                        if tail.len() > TAIL_BYTES {
                            let cut = tail.len() - TAIL_BYTES;
                            tail.drain(..cut);
                        }
                    }
                }
            }
            // O progresso do partclone usa `\r`; fica só com as últimas linhas.
            let text = String::from_utf8_lossy(&tail).replace('\r', "\n");
            let lines: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
            lines[lines.len().saturating_sub(3)..].join(" | ")
        })
    }

    struct Proc {
        name: String,
        child: Child,
        tail: Option<JoinHandle<String>>,
    }

    impl Proc {
        /// Espera o filho terminar; saída ≠ 0 vira erro com o final do stderr.
        fn wait_ok(&mut self) -> Result<(), String> {
            let st = self
                .child
                .wait()
                .map_err(|e| format!("aguardar `{}`: {e}", self.name))?;
            let tail = self
                .tail
                .take()
                .and_then(|h| h.join().ok())
                .unwrap_or_default();
            if st.success() {
                Ok(())
            } else if tail.is_empty() {
                Err(format!("`{}` saiu com {st}", self.name))
            } else {
                Err(format!("`{}` saiu com {st}: {tail}", self.name))
            }
        }
    }

    impl Drop for Proc {
        fn drop(&mut self) {
            // Sem efeito se o filho já terminou; evita órfão em caso de erro.
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    fn spawn(program: &str, args: &[&str], stdin: Stdio, stdout: Stdio) -> Result<Proc, String> {
        let mut child = Command::new(program)
            .args(args)
            .stdin(stdin)
            .stdout(stdout)
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("falha ao iniciar `{program}`: {e}"))?;
        let tail = child.stderr.take().map(drain_tail);
        Ok(Proc { name: program.to_string(), child, tail })
    }

    /// Filho cujo stdout é lido como stream. O EOF só é entregue como sucesso
    /// se o filho saiu com código 0.
    pub struct ChildSource {
        proc_: Proc,
        stdout: ChildStdout,
        finished: bool,
    }

    pub fn spawn_source(program: &str, args: &[&str]) -> Result<ChildSource, String> {
        let mut proc_ = spawn(program, args, Stdio::null(), Stdio::piped())?;
        let stdout = proc_
            .child
            .stdout
            .take()
            .ok_or_else(|| format!("stdout de `{program}` indisponível"))?;
        Ok(ChildSource { proc_, stdout, finished: false })
    }

    impl Read for ChildSource {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.stdout.read(buf)?;
            if n == 0 && !self.finished {
                self.finished = true;
                self.proc_.wait_ok().map_err(std::io::Error::other)?;
            }
            Ok(n)
        }
    }

    /// Filho que recebe o stream pelo stdin.
    pub struct ChildSink {
        proc_: Proc,
        stdin: Option<ChildStdin>,
    }

    pub fn spawn_sink(program: &str, args: &[&str]) -> Result<ChildSink, String> {
        let mut proc_ = spawn(program, args, Stdio::piped(), Stdio::null())?;
        let stdin = proc_
            .child
            .stdin
            .take()
            .ok_or_else(|| format!("stdin de `{program}` indisponível"))?;
        Ok(ChildSink { proc_, stdin: Some(stdin) })
    }

    impl ChildSink {
        pub fn write_all(&mut self, buf: &[u8]) -> Result<(), String> {
            let Some(stdin) = self.stdin.as_mut() else {
                return Err(format!("stdin de `{}` já fechado", self.proc_.name));
            };
            if let Err(e) = stdin.write_all(buf) {
                // Quase sempre EPIPE: o filho morreu, e o motivo está no stderr dele.
                self.stdin = None;
                return Err(match self.proc_.wait_ok() {
                    Err(why) => why,
                    Ok(()) => format!("escrever em `{}`: {e}", self.proc_.name),
                });
            }
            Ok(())
        }

        /// Fecha o stdin (EOF) e exige que o filho termine com sucesso.
        pub fn finish(mut self) -> Result<(), String> {
            self.stdin = None;
            self.proc_.wait_ok()
        }
    }
}

// ─────────────────────────────── Linux ─────────────────────────────────
#[cfg(target_os = "linux")]
pub mod platform {
    use super::pipe::{spawn_sink, spawn_source, ChildSink, ChildSource};

    /// Stream do `partclone.<fs>` clonando a partição para stdout.
    /// O formato partclone já é sparse-aware por filesystem.
    /// Requer root (via pkexec) e o pacote `partclone` instalado.
    pub fn partclone_spawn(source_part: &str, fs: &str) -> Result<ChildSource, String> {
        let tool = super::partclone_tool(fs)?;
        spawn_source(&tool, &["-c", "-s", source_part, "-o", "-"])
            .map_err(|e| format!("{e} (partclone instalado? root?)"))
    }

    /// `partclone.restore` lendo o stream partclone do stdin e recriando o
    /// filesystem em `target_dev`. É o único caminho válido para imagens de
    /// modo 2: o stream NÃO é uma cópia byte a byte da partição.
    pub fn partclone_restore_spawn(target_dev: &str) -> Result<ChildSink, String> {
        spawn_sink("partclone.restore", &["-s", "-", "-o", target_dev])
            .map_err(|e| format!("{e} (partclone instalado? root?)"))
    }
}

// ─────────────────────────────── macOS ─────────────────────────────────
#[cfg(target_os = "macos")]
pub mod platform {
    /// Cria imagem .dmg comprimida (UDZO) a partir de dispositivo/partição
    /// (ex: `/dev/disk4s2`). UDZO (zlib) é o formato que o `asr` restaura.
    pub fn create_dmg(source_dev: &str, output: &str) -> Result<(), String> {
        let st = std::process::Command::new("hdiutil")
            .args(["create", "-srcdevice", source_dev, "-format", "UDZO", "-o", output])
            .status()
            .map_err(|e| format!("falha ao iniciar hdiutil: {e}"))?;
        if st.success() {
            Ok(())
        } else {
            Err(format!("hdiutil saiu com {st} (Full Disk Access concedido?)"))
        }
    }

    /// Restaura uma imagem .dmg em um disco via asr — o único caminho que
    /// recria corretamente containers APFS e volumes selados (Apple Silicon).
    ///
    /// Ainda NÃO está ligada ao restore da interface (teste de aceitação 6.3).
    #[allow(dead_code)]
    pub fn restore_dmg(image: &str, target_dev: &str) -> Result<(), String> {
        let st = std::process::Command::new("asr")
            .args(["restore", "--source", image, "--target", target_dev, "--erase", "--noprompt"])
            .status()
            .map_err(|e| format!("falha ao iniciar asr: {e}"))?;
        if st.success() {
            Ok(())
        } else {
            Err(format!("asr saiu com {st}"))
        }
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

    #[test]
    fn partclone_so_aceita_filesystems_da_lista() {
        assert_eq!(partclone_tool("ext4").unwrap(), "partclone.ext4");
        assert_eq!(partclone_tool(" VFAT ").unwrap(), "partclone.vfat");
        for ruim in ["", "../../tmp/x", "ext4 -o /dev/sda", "crypto_LUKS", "LVM2_member", "swap", "restore"] {
            assert!(partclone_tool(ruim).is_err(), "aceitou `{ruim}`");
        }
    }

    #[cfg(unix)]
    #[test]
    fn origem_filho_entrega_o_stdout_e_falha_se_o_filho_falhar() {
        use std::io::Read;

        let mut ok = pipe::spawn_source("sh", &["-c", "printf abc"]).unwrap();
        let mut lido = String::new();
        ok.read_to_string(&mut lido).unwrap();
        assert_eq!(lido, "abc");

        let mut ruim =
            pipe::spawn_source("sh", &["-c", "printf abc; echo disco quebrado >&2; exit 3"]).unwrap();
        let err = ruim.read_to_string(&mut String::new()).unwrap_err().to_string();
        assert!(err.contains("disco quebrado"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn stderr_volumoso_nao_trava_o_filho() {
        use std::io::Read;

        // 1 MiB no stderr: bem acima do buffer do pipe (64 KiB).
        let script = "i=0; while [ $i -lt 16384 ]; do echo 0123456789012345678901234567890123456789012345678901234567890 >&2; i=$((i+1)); done; printf fim";
        let mut src = pipe::spawn_source("sh", &["-c", script]).unwrap();
        let mut lido = String::new();
        src.read_to_string(&mut lido).unwrap();
        assert_eq!(lido, "fim");
    }

    #[cfg(unix)]
    #[test]
    fn destino_filho_que_falha_devolve_o_stderr() {
        let mut sink = pipe::spawn_sink("sh", &["-c", "cat >/dev/null; echo sem espaco >&2; exit 5"]).unwrap();
        sink.write_all(b"dados").unwrap();
        let err = sink.finish().unwrap_err();
        assert!(err.contains("sem espaco"), "{err}");
    }
}
