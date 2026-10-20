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

use serde::Serialize;

/// Intervalo de clusters alocados: [start_cluster, start_cluster + clusters)
#[derive(Debug, Clone, Copy, Serialize)]
pub struct AllocRun {
    pub start_cluster: u64,
    pub clusters: u64,
}

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

// ─────────────────────────────── Linux ─────────────────────────────────
#[cfg(target_os = "linux")]
pub mod platform {
    use std::io::Read;

    /// Stream do `partclone.<fs>` clonando a partição para stdout.
    /// O formato partclone já é sparse-aware por filesystem.
    /// Requer root (via pkexec) e o pacote `partclone` instalado.
    pub struct PartcloneReader {
        child: std::process::Child,
        stdout: std::process::ChildStdout,
    }

    pub fn partclone_spawn(source_part: &str, fs: &str) -> Result<PartcloneReader, String> {
        let tool = format!("partclone.{}", fs.to_lowercase());
        let mut child = std::process::Command::new(&tool)
            .args(["-c", "-s", source_part, "-o", "-"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("falha ao iniciar `{tool}`: {e} (partclone instalado? root?)"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "stdout do partclone indisponível".to_string())?;
        Ok(PartcloneReader { child, stdout })
    }

    impl Read for PartcloneReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.stdout.read(buf)?;
            if n == 0 {
                match self.child.wait() {
                    Ok(st) if st.success() => {}
                    Ok(st) => {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::Other,
                            format!("partclone saiu com {st}"),
                        ))
                    }
                    Err(e) => return Err(e),
                }
            }
            Ok(n)
        }
    }

    impl Drop for PartcloneReader {
        fn drop(&mut self) {
            let _ = self.child.kill();
        }
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
