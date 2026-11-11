//! Desmonta os volumes do disco de destino antes da escrita raw.
//!
//! - macOS: com volumes montados, abrir `/dev/rdiskN` para escrita falha
//!   ("Resource busy").
//! - Linux: a escrita no disco FUNCIONA com partições montadas — e corrompe
//!   o filesystem em uso. Por isso a desmontagem é obrigatória: se falhar, a
//!   operação é abortada antes de qualquer byte ser gravado.
//! - Windows: travar/desmontar os volumes (`FSCTL_LOCK_VOLUME` +
//!   `FSCTL_DISMOUNT_VOLUME`) ainda NÃO está implementado.

use crate::disk::DiskInfo;

/// Partições montadas do disco: (dispositivo, ponto de montagem).
#[cfg(any(target_os = "linux", test))]
fn mounted_partitions(disk: &DiskInfo) -> Vec<(String, String)> {
    disk.partitions
        .iter()
        .filter_map(|p| {
            let mount = p.mount_point.as_deref()?.trim();
            if mount.is_empty() || mount == "[SWAP]" {
                return None;
            }
            Some((p.path.clone()?, mount.to_string()))
        })
        .collect()
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn run(cmd: &str, args: &[&str]) -> Result<(), String> {
    let out = tokio::process::Command::new(cmd)
        .args(args)
        .output()
        .await
        .map_err(|e| format!("falha ao executar `{cmd}`: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

#[cfg(target_os = "macos")]
pub async fn release_target(dst: &DiskInfo) -> Result<(), String> {
    let dev = crate::guard::normalize_device_path(&dst.path); // /dev/rdiskN → /dev/diskN
    run("diskutil", &["unmountDisk", &dev]).await.map_err(|e| {
        format!("não foi possível desmontar os volumes de `{dev}`: {e} — feche os programas que usam o disco e tente de novo")
    })
}

#[cfg(target_os = "linux")]
pub async fn release_target(dst: &DiskInfo) -> Result<(), String> {
    for (dev, mount) in mounted_partitions(dst) {
        run("umount", &[dev.as_str()]).await.map_err(|e| {
            format!("não foi possível desmontar `{dev}` ({mount}): {e} — feche os programas que usam o disco e tente de novo")
        })?;
    }
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub async fn release_target(_dst: &DiskInfo) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk::PartitionInfo;

    fn part(path: Option<&str>, mount: Option<&str>) -> PartitionInfo {
        PartitionInfo {
            index: 1,
            path: path.map(String::from),
            size_bytes: 1 << 30,
            fs: None,
            label: None,
            mount_point: mount.map(String::from),
        }
    }

    #[test]
    fn so_lista_particoes_realmente_montadas() {
        let disk = DiskInfo {
            id: "sdb".into(),
            path: "/dev/sdb".into(),
            model: "Teste".into(),
            size_bytes: 8 << 30,
            bus: "usb".into(),
            is_system: false,
            is_removable: true,
            partitions: vec![
                part(Some("/dev/sdb1"), Some("/media/u/PENDRIVE")),
                part(Some("/dev/sdb2"), None),
                part(Some("/dev/sdb3"), Some("[SWAP]")),
                part(Some("/dev/sdb4"), Some("")),
                part(None, Some("/mnt/x")),
            ],
        };
        assert_eq!(
            mounted_partitions(&disk),
            vec![("/dev/sdb1".to_string(), "/media/u/PENDRIVE".to_string())]
        );
    }
}
