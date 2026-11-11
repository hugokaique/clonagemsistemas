//! Preparação do disco de destino antes da gravação raw.
//!
//! Gravar num disco que ainda tem volumes montados não funciona (ou é
//! perigoso) em nenhum SO:
//! - **Windows**: o kernel bloqueia escritas em setores que pertencem a um
//!   filesystem montado — a clonagem falharia com "Acesso negado" logo depois
//!   de sobrescrever a tabela de partições. Solução padrão (Rufus,
//!   Win32DiskImager): travar (`FSCTL_LOCK_VOLUME`) e desmontar
//!   (`FSCTL_DISMOUNT_VOLUME`) cada volume do disco e manter os handles
//!   abertos até o fim.
//! - **Linux**: nada impede a escrita, mas o filesystem montado seria
//!   corrompido por baixo do kernel — desmontamos antes (umount/swapoff).
//! - **macOS**: `/dev/rdiskN` devolve "Resource busy" enquanto houver volume
//!   montado — `diskutil unmountDisk`.
//!
//! Só volumes do disco de DESTINO (já validado como não-sistema e confirmado
//! pelo usuário) são tocados.

use crate::io::PathKind;
use crate::safety::ResolvedTarget;

/// Mantém o destino "reservado" durante a gravação. Soltar o guard libera as
/// travas; `finish` também pede ao SO que releia a tabela de partições.
pub struct TargetGuard {
    notes: Vec<String>,
    #[cfg(windows)]
    win: Option<windows_impl::LockedVolumes>,
}

impl TargetGuard {
    pub fn acquire(target: &ResolvedTarget) -> Result<Self, String> {
        let mut guard = TargetGuard {
            notes: Vec::new(),
            #[cfg(windows)]
            win: None,
        };
        if target.kind != PathKind::Device {
            return Ok(guard);
        }

        #[cfg(windows)]
        {
            if let Some(n) = windows_impl::disk_number(&target.path) {
                let locked = windows_impl::LockedVolumes::acquire(n);
                guard.notes.extend(locked.notes.iter().cloned());
                guard.win = Some(locked);
            }
        }
        #[cfg(target_os = "linux")]
        {
            guard.notes = unmount_linux(&target.mounts)?;
        }
        #[cfg(target_os = "macos")]
        {
            guard.notes = unmount_macos(&target.path)?;
        }
        Ok(guard)
    }

    /// Avisos para o log da UI (ex.: "volume E: travado e desmontado").
    pub fn notes(&self) -> &[String] {
        &self.notes
    }

    /// Chamado após gravação + verificação bem-sucedidas.
    pub fn finish(self, target: &ResolvedTarget) {
        drop(self); // libera as travas antes de pedir a releitura
        if target.kind == PathKind::Device {
            #[cfg(windows)]
            windows_impl::rescan_partitions(&target.path);
        }
    }
}

// ─────────────────────────────── Linux ─────────────────────────────────

/// Comandos para liberar o disco: mais profundos primeiro (montagens
/// aninhadas), swap via `swapoff`.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn linux_unmount_plan(mounts: &[crate::disk::MountInfo]) -> Vec<(String, Vec<String>)> {
    let mut sorted: Vec<&crate::disk::MountInfo> = mounts.iter().collect();
    sorted.sort_by_key(|m| std::cmp::Reverse(m.mount_point.len()));
    sorted
        .into_iter()
        .map(|m| {
            if m.mount_point == "[SWAP]" {
                ("swapoff".to_string(), vec![m.device.clone()])
            } else {
                ("umount".to_string(), vec![m.mount_point.clone()])
            }
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn unmount_linux(mounts: &[crate::disk::MountInfo]) -> Result<Vec<String>, String> {
    let mut notes = Vec::new();
    for (cmd, args) in linux_unmount_plan(mounts) {
        let out = std::process::Command::new(&cmd)
            .args(&args)
            .output()
            .map_err(|e| format!("falha ao executar `{cmd}`: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "não foi possível liberar `{}` no disco de destino (em uso?): {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        notes.push(format!("destino: {cmd} {}", args.join(" ")));
    }
    Ok(notes)
}

// ─────────────────────────────── macOS ─────────────────────────────────

/// `/dev/rdisk4` → `/dev/disk4` (o diskutil trabalha com o nó com cache)
#[cfg(any(target_os = "macos", test))]
pub(crate) fn macos_buffered_node(path: &str) -> String {
    match path.strip_prefix("/dev/rdisk") {
        Some(rest) => format!("/dev/disk{rest}"),
        None => path.to_string(),
    }
}

#[cfg(target_os = "macos")]
fn unmount_macos(target_path: &str) -> Result<Vec<String>, String> {
    let node = macos_buffered_node(target_path);
    let out = std::process::Command::new("diskutil")
        .args(["unmountDisk", "force", &node])
        .output()
        .map_err(|e| format!("falha ao executar diskutil: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "não foi possível desmontar {node}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(vec![format!("destino: volumes de {node} desmontados")])
}

// ─────────────────────────────── Windows ───────────────────────────────

#[cfg(windows)]
mod windows_impl {
    use std::fs::{File, OpenOptions};
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{
        FindFirstVolumeW, FindNextVolumeW, FindVolumeClose, IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
    };
    use windows::Win32::System::Ioctl::{
        DISK_EXTENT, FSCTL_DISMOUNT_VOLUME, FSCTL_LOCK_VOLUME, FSCTL_UNLOCK_VOLUME,
        IOCTL_DISK_UPDATE_PROPERTIES,
    };
    use windows::Win32::System::IO::DeviceIoControl;

    /// `\\.\PhysicalDrive7` → 7
    pub fn disk_number(path: &str) -> Option<u32> {
        path.to_ascii_lowercase()
            .strip_prefix("\\\\.\\physicaldrive")?
            .parse()
            .ok()
    }

    fn ioctl(file: &File, code: u32) -> windows::core::Result<()> {
        let mut returned = 0u32;
        // SAFETY: handle válido enquanto `file` vive; sem buffers de entrada/saída.
        unsafe {
            DeviceIoControl(
                HANDLE(file.as_raw_handle()),
                code,
                None,
                0,
                None,
                0,
                Some(&mut returned),
                None,
            )
        }
    }

    /// Caminhos `\\?\Volume{guid}` (sem barra final) de todos os volumes.
    fn all_volumes() -> Vec<String> {
        let mut out = Vec::new();
        let mut name = [0u16; 512];
        // SAFETY: buffer próprio, tamanho informado pelo slice.
        let Ok(find) = (unsafe { FindFirstVolumeW(&mut name) }) else {
            return out;
        };
        loop {
            let len = name.iter().position(|c| *c == 0).unwrap_or(name.len());
            let s = String::from_utf16_lossy(&name[..len]);
            out.push(s.trim_end_matches('\\').to_string());
            name = [0u16; 512];
            // SAFETY: `find` veio de FindFirstVolumeW e ainda não foi fechado.
            if unsafe { FindNextVolumeW(find, &mut name) }.is_err() {
                break;
            }
        }
        // SAFETY: fecha o handle de busca exatamente uma vez.
        unsafe {
            let _ = FindVolumeClose(find);
        }
        out
    }

    /// O volume tem alguma extensão no disco `disk`?
    fn volume_on_disk(volume: &str, disk: u32) -> bool {
        // Acesso 0: consulta de extensões não exige leitura nem escrita.
        let Ok(f) = OpenOptions::new().access_mode(0).open(volume) else {
            return false;
        };
        // VOLUME_DISK_EXTENTS = { u32 NumberOfDiskExtents; (pad) DISK_EXTENT Extents[] }
        // Usamos um buffer alinhado a 8 bytes com espaço para 32 extensões.
        const MAX_EXTENTS: usize = 32;
        let mut buf = [0u64; 1 + MAX_EXTENTS * 3];
        let mut returned = 0u32;
        // SAFETY: buffer de saída próprio, tamanho em bytes correto.
        let res = unsafe {
            DeviceIoControl(
                HANDLE(f.as_raw_handle()),
                IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
                None,
                0,
                Some(buf.as_mut_ptr() as *mut _),
                std::mem::size_of_val(&buf) as u32,
                Some(&mut returned),
                None,
            )
        };
        // ERROR_MORE_DATA (volume com muitas extensões) ainda preenche o que coube;
        // outros erros (CD-ROM, volume sem disco) → não é do nosso disco.
        if res.is_err() && returned == 0 {
            return false;
        }
        let count = (buf[0] as u32 as usize).min(MAX_EXTENTS);
        let extents = buf[1..].as_ptr() as *const DISK_EXTENT;
        (0..count).any(|i| {
            // SAFETY: `i < count <= MAX_EXTENTS` e o buffer comporta MAX_EXTENTS
            // structs de 24 bytes, alinhadas a 8.
            let e = unsafe { *extents.add(i) };
            e.DiskNumber == disk
        })
    }

    /// Handles abertos = volumes travados. Soltar destrava e fecha.
    pub struct LockedVolumes {
        handles: Vec<File>,
        pub notes: Vec<String>,
    }

    impl LockedVolumes {
        /// Melhor esforço: falha ao travar um volume vira aviso, não erro —
        /// se o Windows ainda bloquear a escrita, o motor reporta o erro real.
        pub fn acquire(disk: u32) -> Self {
            let mut me = LockedVolumes { handles: Vec::new(), notes: Vec::new() };
            for vol in all_volumes() {
                if !volume_on_disk(&vol, disk) {
                    continue;
                }
                let f = match OpenOptions::new().read(true).write(true).open(&vol) {
                    Ok(f) => f,
                    Err(e) => {
                        me.notes.push(format!("destino: não foi possível abrir {vol} para travar: {e}"));
                        continue;
                    }
                };
                let mut locked = ioctl(&f, FSCTL_LOCK_VOLUME).is_ok();
                // Desmonta mesmo sem a trava (handles de outros programas são
                // invalidados) e tenta travar de novo.
                let dismounted = ioctl(&f, FSCTL_DISMOUNT_VOLUME).is_ok();
                for _ in 0..10 {
                    if locked {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(200));
                    locked = ioctl(&f, FSCTL_LOCK_VOLUME).is_ok();
                }
                tracing::info!("volume {vol} do disco {disk}: travado={locked} desmontado={dismounted}");
                me.notes.push(match (locked, dismounted) {
                    (true, _) => format!("destino: volume {vol} travado e desmontado"),
                    (false, true) => format!(
                        "destino: volume {vol} desmontado, mas não foi possível travá-lo (em uso por outro programa)"
                    ),
                    (false, false) => format!(
                        "destino: não foi possível travar/desmontar {vol} — feche os programas que usam o disco"
                    ),
                });
                me.handles.push(f);
            }
            me
        }
    }

    impl Drop for LockedVolumes {
        fn drop(&mut self) {
            for f in &self.handles {
                let _ = ioctl(f, FSCTL_UNLOCK_VOLUME);
            }
        }
    }

    /// Pede ao Windows que releia a tabela de partições recém-gravada.
    pub fn rescan_partitions(disk_path: &str) {
        if let Ok(f) = OpenOptions::new().read(true).write(true).open(disk_path) {
            if let Err(e) = ioctl(&f, IOCTL_DISK_UPDATE_PROPERTIES) {
                tracing::warn!("IOCTL_DISK_UPDATE_PROPERTIES({disk_path}): {e}");
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use windows::Win32::System::Ioctl::{IOCTL_STORAGE_GET_DEVICE_NUMBER, STORAGE_DEVICE_NUMBER};

        #[test]
        fn numero_do_disco_a_partir_do_caminho() {
            assert_eq!(disk_number(r"\\.\PhysicalDrive7"), Some(7));
            assert_eq!(disk_number(r"\\.\physicaldrive12"), Some(12));
            assert_eq!(disk_number(r"\\.\C:"), None);
            assert_eq!(disk_number("arquivo.bin"), None);
        }

        /// Só consulta (acesso 0, sem elevação): confere que o mapeamento
        /// volume → disco usado para decidir QUAIS volumes desmontar bate com
        /// uma segunda API do Windows. Um erro aqui desmontaria o volume errado.
        #[test]
        fn volume_e_atribuido_ao_disco_certo() {
            let file = std::env::temp_dir().join(format!("cloneforge-ext-{}.bin", std::process::id()));
            std::fs::write(&file, b"x").unwrap();
            let guid = crate::safety::file_volume_guid(&file.to_string_lossy());
            let _ = std::fs::remove_file(&file);
            let Some(guid) = guid else {
                eprintln!("volume do diretório temporário sem GUID — teste ignorado");
                return;
            };

            let volumes = all_volumes();
            assert!(!volumes.is_empty());
            assert!(
                volumes.iter().any(|v| v.eq_ignore_ascii_case(&guid)),
                "{guid} deveria estar em {volumes:?}"
            );

            // Número do disco por outra via: IOCTL_STORAGE_GET_DEVICE_NUMBER.
            let Ok(f) = OpenOptions::new().access_mode(0).open(&guid) else {
                eprintln!("não foi possível abrir {guid} — teste ignorado");
                return;
            };
            let mut sdn = STORAGE_DEVICE_NUMBER::default();
            let mut returned = 0u32;
            // SAFETY: struct de saída própria com o tamanho correto.
            let res = unsafe {
                DeviceIoControl(
                    HANDLE(f.as_raw_handle()),
                    IOCTL_STORAGE_GET_DEVICE_NUMBER,
                    None,
                    0,
                    Some(&mut sdn as *mut _ as *mut _),
                    std::mem::size_of::<STORAGE_DEVICE_NUMBER>() as u32,
                    Some(&mut returned),
                    None,
                )
            };
            if res.is_err() {
                eprintln!("volume dinâmico/espelhado — conferência cruzada ignorada");
                return;
            }
            let disk = sdn.DeviceNumber;
            assert!(volume_on_disk(&guid, disk), "{guid} deveria estar no disco {disk}");
            assert!(!volume_on_disk(&guid, disk + 1000));
            // Nenhum volume de OUTRO disco pode ser atribuído a um disco inexistente.
            assert!(volumes.iter().all(|v| !volume_on_disk(v, 9999)));
        }
    }
}

// ───────────────────────── Manter o PC acordado ─────────────────────────

/// Impede a suspensão automática enquanto uma operação longa roda (uma
/// clonagem de 1 TB não pode ser interrompida porque o PC dormiu).
/// Implementado no Windows; nos demais SOs é um no-op.
pub struct KeepAwake;

impl KeepAwake {
    pub fn new() -> Self {
        #[cfg(windows)]
        {
            use windows::Win32::System::Power::{
                SetThreadExecutionState, ES_CONTINUOUS, ES_SYSTEM_REQUIRED, EXECUTION_STATE,
            };
            // SAFETY: chamada sem ponteiros; o estado vale para a thread atual.
            unsafe {
                SetThreadExecutionState(EXECUTION_STATE(ES_CONTINUOUS.0 | ES_SYSTEM_REQUIRED.0));
            }
        }
        KeepAwake
    }
}

impl Default for KeepAwake {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for KeepAwake {
    fn drop(&mut self) {
        #[cfg(windows)]
        {
            use windows::Win32::System::Power::{SetThreadExecutionState, ES_CONTINUOUS};
            // SAFETY: idem; devolve a thread ao estado normal.
            unsafe {
                SetThreadExecutionState(ES_CONTINUOUS);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk::MountInfo;

    #[test]
    fn linux_desmonta_aninhados_primeiro_e_swap_com_swapoff() {
        let mounts = vec![
            MountInfo { device: "/dev/sdb1".into(), mount_point: "/mnt/usb".into() },
            MountInfo { device: "/dev/sdb2".into(), mount_point: "[SWAP]".into() },
            MountInfo { device: "/dev/sdb3".into(), mount_point: "/mnt/usb/dentro".into() },
        ];
        let plan = linux_unmount_plan(&mounts);
        assert_eq!(plan[0], ("umount".to_string(), vec!["/mnt/usb/dentro".to_string()]));
        assert_eq!(plan[1], ("umount".to_string(), vec!["/mnt/usb".to_string()]));
        assert_eq!(plan[2], ("swapoff".to_string(), vec!["/dev/sdb2".to_string()]));
    }

    #[test]
    fn macos_no_com_cache() {
        assert_eq!(macos_buffered_node("/dev/rdisk4"), "/dev/disk4");
        assert_eq!(macos_buffered_node("/dev/disk4"), "/dev/disk4");
    }

    #[test]
    fn destino_arquivo_nao_precisa_de_preparo() {
        let t = ResolvedTarget {
            path: "saida.bin".into(),
            kind: PathKind::File,
            capacity: None,
            sector_size: 1,
            disk_id: None,
            mounts: Vec::new(),
            device_ids: Vec::new(),
        };
        let g = TargetGuard::acquire(&t).unwrap();
        assert!(g.notes().is_empty());
        g.finish(&t);
    }
}
