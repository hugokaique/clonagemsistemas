//! Validação de origem e destino — a última linha de defesa antes de gravar.
//!
//! Princípios (todos FAIL-CLOSED):
//! 1. Destino que é dispositivo só é aceito se for um disco FÍSICO presente
//!    na enumeração, e nunca o disco do sistema. É uma lista de permissão:
//!    qualquer outro caminho de dispositivo (volume, partição, alias, symlink
//!    para algo desconhecido) é recusado — antes a checagem era uma comparação
//!    exata de strings, contornável com `\\.\physicaldrive0`, `//./…` etc.
//! 2. Se a enumeração falhar, a operação é abortada (antes o erro era
//!    engolido e a proteção do disco do sistema simplesmente não rodava).
//! 3. Tamanhos vêm da enumeração feita pelo backend, não do que a UI informa.
//!
//! Este módulo é usado pela GUI e também pelo helper privilegiado (Linux):
//! quem tem o privilégio revalida.

use crate::disk::{DiskInfo, MountInfo, PartitionInfo};
use crate::io::{self, PathKind};

pub const BLOCKED_SYSTEM: &str = "BLOQUEADO: o destino é o disco do sistema em execução";

/// Origem da imagem/clonagem gravada no header `.cfimg`.
pub const KIND_UNKNOWN: u32 = 0;
pub const KIND_DISK: u32 = 1;
pub const KIND_PARTITION: u32 = 2;

#[derive(Debug, Clone)]
pub struct ResolvedTarget {
    /// Caminho canônico a abrir (`\\.\PhysicalDrive2`, `/dev/sdb`, `/dev/rdisk4` ou arquivo)
    pub path: String,
    pub kind: PathKind,
    /// Capacidade em bytes (somente dispositivos)
    pub capacity: Option<u64>,
    pub sector_size: u32,
    pub disk_id: Option<String>,
    /// Volumes montados no disco de destino — liberados antes da gravação
    pub mounts: Vec<MountInfo>,
    /// Identidades dos volumes/partições do disco de destino: GUIDs de volume
    /// (`\\?\Volume{guid}`) no Windows, nós de dispositivo no Unix. Servem
    /// para detectar um arquivo de origem guardado no próprio destino mesmo
    /// atrás de junction, pasta de montagem ou symlink.
    pub device_ids: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ResolvedSource {
    pub path: String,
    pub kind: PathKind,
    pub size: u64,
    pub disk_id: Option<String>,
    /// `KIND_DISK` | `KIND_PARTITION` | `KIND_UNKNOWN` (arquivo)
    pub source_kind: u32,
    /// Descrição legível gravada no header da imagem
    pub label: String,
    pub fs: Option<String>,
    pub mounted: bool,
}

/// Enumera os discos se (e só se) algum dos caminhos for um dispositivo.
/// Falha de enumeração ABORTA a operação.
pub async fn enumerate_for(paths: &[&str]) -> Result<Vec<DiskInfo>, String> {
    if paths.iter().all(|p| io::path_kind(p) == PathKind::File) {
        return Ok(Vec::new());
    }
    crate::disk::list_disks().await.map_err(|e| {
        format!("não foi possível enumerar os discos para validar a operação — abortado por segurança: {e}")
    })
}

pub fn resolve_target(target: &str, disks: &[DiskInfo]) -> Result<ResolvedTarget, String> {
    if target.trim().is_empty() {
        return Err("destino não informado".into());
    }
    #[cfg(windows)]
    {
        resolve_target_windows(target, disks)
    }
    #[cfg(unix)]
    {
        // Resolve symlinks (/dev/disk/by-id/…) antes de classificar/comparar.
        let canonical = std::fs::canonicalize(target)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| target.to_string());
        match io::path_kind(&canonical) {
            PathKind::File => Ok(file_target(&canonical)),
            PathKind::Device => resolve_unix_device_target(&canonical, disks),
        }
    }
}

pub fn resolve_source(source: &str, disks: &[DiskInfo]) -> Result<ResolvedSource, String> {
    if source.trim().is_empty() {
        return Err("origem não informada".into());
    }
    #[cfg(windows)]
    {
        resolve_source_windows(source, disks)
    }
    #[cfg(unix)]
    {
        let canonical = std::fs::canonicalize(source)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| source.to_string());
        match io::path_kind(&canonical) {
            PathKind::File => file_source(&canonical),
            PathKind::Device => resolve_unix_device_source(&canonical, disks),
        }
    }
}

/// Checagens cruzadas origem × destino (mesmo disco, capacidade, origem
/// armazenada no próprio destino).
pub fn check_pair(source: &ResolvedSource, target: &ResolvedTarget) -> Result<(), String> {
    let same = if cfg!(windows) {
        source.path.eq_ignore_ascii_case(&target.path)
    } else {
        source.path == target.path
    };
    if same {
        return Err("origem e destino são o mesmo dispositivo/arquivo".into());
    }
    if let (Some(a), Some(b)) = (&source.disk_id, &target.disk_id) {
        if a == b {
            return Err("origem e destino estão no mesmo disco físico".into());
        }
    }
    check_file_not_on_target(&source.path, source.kind, target)?;
    if source.size == 0 {
        return Err("tamanho da origem inválido (0 bytes)".into());
    }
    if let Some(cap) = target.capacity {
        if source.size > cap {
            return Err(format!(
                "destino ({}) menor que a origem ({})",
                human(cap),
                human(source.size)
            ));
        }
    }
    Ok(())
}

/// Um arquivo de origem (imagem) guardado no disco que vai ser sobrescrito
/// seria destruído no meio da operação.
pub fn check_file_not_on_target(
    file: &str,
    file_kind: PathKind,
    target: &ResolvedTarget,
) -> Result<(), String> {
    if file_kind == PathKind::File
        && target.kind == PathKind::Device
        && (file_on_mounts(file, &target.mounts, cfg!(windows))
            || file_on_device_ids(file, &target.device_ids))
    {
        return Err(format!(
            "`{file}` está armazenado no próprio disco de destino — ele seria apagado durante a operação"
        ));
    }
    Ok(())
}

pub(crate) fn file_on_mounts(file: &str, mounts: &[MountInfo], case_insensitive: bool) -> bool {
    let norm = |s: &str| {
        let s = s.replace('\\', "/");
        let s = s.strip_prefix("//?/").unwrap_or(&s).to_string();
        if case_insensitive {
            s.to_lowercase()
        } else {
            s
        }
    };
    let f = norm(file);
    mounts.iter().any(|m| {
        if m.mount_point.starts_with('[') {
            return false; // [SWAP]
        }
        let mp = norm(&m.mount_point);
        let mp = mp.trim_end_matches('/');
        f == mp || f.starts_with(&format!("{mp}/"))
    })
}

/// `\\?\Volume{guid}` do volume onde o arquivo REALMENTE está (junctions,
/// pastas de montagem e symlinks já resolvidos pelo sistema).
#[cfg(windows)]
pub(crate) fn file_volume_guid(file: &str) -> Option<String> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{GetFinalPathNameByHandleW, VOLUME_NAME_GUID};

    let f = std::fs::File::open(file).ok()?;
    let mut buf = vec![0u16; 1024];
    // SAFETY: handle válido enquanto `f` vive; buffer próprio com o tamanho do slice.
    let len = unsafe {
        GetFinalPathNameByHandleW(HANDLE(f.as_raw_handle()), &mut buf, VOLUME_NAME_GUID)
    } as usize;
    if len == 0 || len >= buf.len() {
        return None;
    }
    let path = String::from_utf16_lossy(&buf[..len]);
    let end = path.find('}')?;
    Some(path[..=end].to_string())
}

#[cfg(windows)]
fn file_on_device_ids(file: &str, ids: &[String]) -> bool {
    match file_volume_guid(file) {
        Some(guid) => ids.iter().any(|id| id.eq_ignore_ascii_case(&guid)),
        None => false,
    }
}

/// Unix: o `st_dev` do arquivo é o número do dispositivo de bloco que hospeda
/// o filesystem — basta comparar com o `st_rdev` de cada nó do destino.
#[cfg(unix)]
fn file_on_device_ids(file: &str, ids: &[String]) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(meta) = std::fs::metadata(file) else {
        return false;
    };
    let dev = meta.dev();
    ids.iter()
        .any(|id| std::fs::metadata(id).map(|m| m.rdev() == dev).unwrap_or(false))
}

pub fn human(bytes: u64) -> String {
    const GIB: f64 = 1_073_741_824.0;
    const MIB: f64 = 1_048_576.0;
    if bytes as f64 >= GIB {
        format!("{:.1} GiB", bytes as f64 / GIB)
    } else {
        format!("{:.1} MiB", bytes as f64 / MIB)
    }
}

// ───────────────────────────── alvos: comum ─────────────────────────────

fn file_target(path: &str) -> ResolvedTarget {
    ResolvedTarget {
        path: io::absolute(path, None),
        kind: PathKind::File,
        capacity: None,
        sector_size: 1,
        disk_id: None,
        mounts: Vec::new(),
        device_ids: Vec::new(),
    }
}

/// Tudo que identifica um volume/partição deste disco (ver `device_ids`).
fn device_ids_of(disk: &DiskInfo) -> Vec<String> {
    let mut ids = vec![disk.path.clone()];
    for p in &disk.partitions {
        ids.extend(p.volume.clone());
        ids.extend(p.path.clone());
    }
    ids.extend(disk.mounts.iter().map(|m| m.device.clone()));
    ids.sort();
    ids.dedup();
    ids
}

fn disk_target(disk: &DiskInfo) -> Result<ResolvedTarget, String> {
    if disk.is_system {
        return Err(BLOCKED_SYSTEM.into());
    }
    if disk.is_read_only {
        return Err(format!("o disco de destino `{}` está somente-leitura", disk.path));
    }
    if disk.size_bytes == 0 {
        return Err(format!("o disco de destino `{}` informa tamanho 0", disk.path));
    }
    Ok(ResolvedTarget {
        path: disk.path.clone(),
        kind: PathKind::Device,
        capacity: Some(disk.size_bytes),
        sector_size: disk.sector_size.max(1),
        disk_id: Some(disk.id.clone()),
        mounts: disk.mounts.clone(),
        device_ids: device_ids_of(disk),
    })
}

fn not_enumerated(what: &str, path: &str) -> String {
    format!("{what} `{path}` não está entre os discos enumerados — clique em Reescanear e tente de novo")
}

fn file_source(path: &str) -> Result<ResolvedSource, String> {
    let m = std::fs::metadata(path).map_err(|e| io::open_error("origem", path, &e))?;
    if !m.is_file() {
        return Err(format!("origem `{path}` não é um arquivo regular"));
    }
    if m.len() == 0 {
        return Err(format!("origem `{path}` está vazia (0 bytes)"));
    }
    let abs = io::absolute(path, None);
    let name = std::path::Path::new(&abs)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| abs.clone());
    Ok(ResolvedSource {
        path: abs,
        kind: PathKind::File,
        size: m.len(),
        disk_id: None,
        source_kind: KIND_UNKNOWN,
        label: name,
        fs: None,
        mounted: false,
    })
}

fn disk_source(disk: &DiskInfo) -> Result<ResolvedSource, String> {
    if disk.size_bytes == 0 {
        return Err(format!("o disco de origem `{}` informa tamanho 0", disk.path));
    }
    Ok(ResolvedSource {
        path: disk.path.clone(),
        kind: PathKind::Device,
        size: disk.size_bytes,
        disk_id: Some(disk.id.clone()),
        source_kind: KIND_DISK,
        label: format!("{} ({})", disk.model, disk.id),
        fs: None,
        mounted: !disk.mounts.is_empty(),
    })
}

fn partition_source(disk: &DiskInfo, part: &PartitionInfo, path: &str) -> Result<ResolvedSource, String> {
    if part.size_bytes == 0 {
        return Err(format!("a partição de origem `{path}` informa tamanho 0"));
    }
    let name = part
        .mount_point
        .clone()
        .or_else(|| part.label.clone())
        .unwrap_or_else(|| format!("partição {}", part.index));
    Ok(ResolvedSource {
        path: path.to_string(),
        kind: PathKind::Device,
        size: part.size_bytes,
        disk_id: Some(disk.id.clone()),
        source_kind: KIND_PARTITION,
        label: format!("{name} — {} ({})", disk.model, disk.id),
        fs: part.fs.clone(),
        mounted: part.mount_point.is_some(),
    })
}

// ─────────────────────────────── Windows ───────────────────────────────

#[cfg(any(windows, test))]
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum WinDevice {
    PhysicalDrive(u32),
    /// Caminho normalizado do volume: `\\.\C:` ou `\\?\Volume{guid}`
    Volume(String),
    Other,
}

/// `None` = caminho de arquivo. Só `PhysicalDriveN` exato (sem sufixos,
/// espaços ou pontos finais, que o Win32 descartaria ao abrir) é reconhecido
/// como disco; qualquer variação cai em `Other` e é recusada como destino.
#[cfg(any(windows, test))]
pub(crate) fn classify_windows_device(path: &str) -> Option<WinDevice> {
    if io::windows_path_kind(path) != PathKind::Device {
        return None;
    }
    let p = path.replace('/', "\\");
    let rest = &p[4..]; // após `\\.\` ou `\\?\` (ASCII)
    let lower = rest.to_ascii_lowercase();

    if let Some(num) = lower.strip_prefix("physicaldrive") {
        let valid = !num.is_empty() && num.len() <= 6 && num.bytes().all(|b| b.is_ascii_digit());
        return Some(match num.parse::<u32>() {
            Ok(n) if valid => WinDevice::PhysicalDrive(n),
            _ => WinDevice::Other,
        });
    }
    let b = lower.as_bytes();
    if b.len() == 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        let letter = (b[0] as char).to_ascii_uppercase();
        return Some(WinDevice::Volume(format!("\\\\.\\{letter}:")));
    }
    let vol = rest.trim_end_matches('\\');
    if vol.to_ascii_lowercase().starts_with("volume{") && vol.ends_with('}') {
        return Some(WinDevice::Volume(format!("\\\\?\\{vol}")));
    }
    Some(WinDevice::Other)
}

#[cfg(any(windows, test))]
pub(crate) fn resolve_target_windows(target: &str, disks: &[DiskInfo]) -> Result<ResolvedTarget, String> {
    match classify_windows_device(target) {
        None => Ok(file_target(target)),
        Some(WinDevice::PhysicalDrive(n)) => {
            let id = format!("PhysicalDrive{n}");
            let disk = disks
                .iter()
                .find(|d| d.id == id)
                .ok_or_else(|| not_enumerated("destino", target))?;
            disk_target(disk)
        }
        Some(_) => Err(format!(
            "destino `{target}` não suportado: escolha um disco físico da lista ou um arquivo"
        )),
    }
}

#[cfg(any(windows, test))]
pub(crate) fn resolve_source_windows(source: &str, disks: &[DiskInfo]) -> Result<ResolvedSource, String> {
    match classify_windows_device(source) {
        None => file_source(source),
        Some(WinDevice::PhysicalDrive(n)) => {
            let id = format!("PhysicalDrive{n}");
            let disk = disks
                .iter()
                .find(|d| d.id == id)
                .ok_or_else(|| not_enumerated("origem", source))?;
            disk_source(disk)
        }
        Some(WinDevice::Volume(vol)) => {
            for d in disks {
                for p in &d.partitions {
                    if p.path.as_deref().is_some_and(|pp| pp.eq_ignore_ascii_case(&vol)) {
                        return partition_source(d, p, &vol);
                    }
                }
            }
            Err(not_enumerated("origem", source))
        }
        Some(WinDevice::Other) => Err(format!("origem `{source}` não suportada")),
    }
}

// ──────────────────────────────── Unix ─────────────────────────────────

/// macOS: `/dev/diskN…` (com cache) → `/dev/rdiskN…` (raw), a forma enumerada.
/// No Linux é inofensivo: `/dev/disk/by-id/…` não tem dígito após "disk".
#[cfg(any(unix, test))]
pub(crate) fn mac_raw(path: &str) -> String {
    match path.strip_prefix("/dev/disk") {
        Some(rest) if rest.starts_with(|c: char| c.is_ascii_digit()) => format!("/dev/rdisk{rest}"),
        _ => path.to_string(),
    }
}

#[cfg(any(unix, test))]
fn mac_buffered(path: &str) -> String {
    match path.strip_prefix("/dev/rdisk") {
        Some(rest) if rest.starts_with(|c: char| c.is_ascii_digit()) => format!("/dev/disk{rest}"),
        _ => path.to_string(),
    }
}

#[cfg(any(unix, test))]
fn find_unix_disk<'a>(canonical: &str, disks: &'a [DiskInfo]) -> Option<&'a DiskInfo> {
    let raw = mac_raw(canonical);
    disks.iter().find(|d| d.path == canonical || d.path == raw)
}

#[cfg(any(unix, test))]
fn find_unix_partition<'a>(
    canonical: &str,
    disks: &'a [DiskInfo],
) -> Option<(&'a DiskInfo, &'a PartitionInfo)> {
    let buffered = mac_buffered(canonical);
    disks.iter().find_map(|d| {
        d.partitions
            .iter()
            .find(|p| p.path.as_deref() == Some(canonical) || p.path.as_deref() == Some(buffered.as_str()))
            .map(|p| (d, p))
    })
}

#[cfg(any(unix, test))]
pub(crate) fn resolve_unix_device_target(canonical: &str, disks: &[DiskInfo]) -> Result<ResolvedTarget, String> {
    if let Some(disk) = find_unix_disk(canonical, disks) {
        return disk_target(disk);
    }
    if let Some((disk, _)) = find_unix_partition(canonical, disks) {
        if disk.is_system {
            return Err(BLOCKED_SYSTEM.into());
        }
        return Err(format!(
            "destino `{canonical}` é uma partição — escolha o disco inteiro ({})",
            disk.path
        ));
    }
    Err(not_enumerated("destino", canonical))
}

#[cfg(any(unix, test))]
pub(crate) fn resolve_unix_device_source(canonical: &str, disks: &[DiskInfo]) -> Result<ResolvedSource, String> {
    if let Some(disk) = find_unix_disk(canonical, disks) {
        return disk_source(disk);
    }
    if let Some((disk, part)) = find_unix_partition(canonical, disks) {
        return partition_source(disk, part, canonical);
    }
    Err(not_enumerated("origem", canonical))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn disk(id: &str, path: &str, size: u64, system: bool) -> DiskInfo {
        DiskInfo {
            id: id.into(),
            path: path.into(),
            model: format!("Modelo {id}"),
            size_bytes: size,
            bus: "USB".into(),
            is_system: system,
            is_removable: !system,
            is_read_only: false,
            sector_size: 512,
            partitions: Vec::new(),
            mounts: Vec::new(),
        }
    }

    fn part(index: u32, path: &str, size: u64, mount: Option<&str>) -> PartitionInfo {
        PartitionInfo {
            index,
            path: Some(path.into()),
            volume: None,
            size_bytes: size,
            offset_bytes: None,
            fs: Some("NTFS".into()),
            label: None,
            mount_point: mount.map(String::from),
        }
    }

    fn windows_disks() -> Vec<DiskInfo> {
        let mut sys = disk("PhysicalDrive1", r"\\.\PhysicalDrive1", 1_000_000_000_000, true);
        sys.partitions.push(part(3, r"\\.\C:", 999_000_000_000, Some(r"C:\")));
        sys.mounts.push(MountInfo { device: r"\\.\C:".into(), mount_point: r"C:\".into() });
        let mut data = disk("PhysicalDrive0", r"\\.\PhysicalDrive0", 2_000_000_000_000, false);
        data.partitions.push(part(1, r"\\.\D:", 1_999_000_000_000, Some(r"D:\")));
        data.partitions.push(part(2, r"\\?\Volume{aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee}", 500_000_000, None));
        data.mounts.push(MountInfo { device: r"\\.\D:".into(), mount_point: r"D:\".into() });
        let usb = disk("PhysicalDrive2", r"\\.\PhysicalDrive2", 16_000_000_000, false);
        vec![sys, data, usb]
    }

    #[test]
    fn windows_classifica_dispositivo() {
        use WinDevice::*;
        assert_eq!(classify_windows_device(r"\\.\PhysicalDrive0"), Some(PhysicalDrive(0)));
        assert_eq!(classify_windows_device(r"\\.\PHYSICALDRIVE12"), Some(PhysicalDrive(12)));
        assert_eq!(classify_windows_device(r"//./physicaldrive3"), Some(PhysicalDrive(3)));
        assert_eq!(classify_windows_device(r"\\?\PhysicalDrive07"), Some(PhysicalDrive(7)));
        assert_eq!(classify_windows_device(r"\\.\c:"), Some(Volume(r"\\.\C:".into())));
        assert_eq!(
            classify_windows_device(r"\\?\Volume{AAAAAAAA-bbbb-cccc-dddd-eeeeeeeeeeee}\"),
            Some(Volume(r"\\?\Volume{AAAAAAAA-bbbb-cccc-dddd-eeeeeeeeeeee}".into()))
        );
        // Variações que o Win32 normalizaria para o disco — recusadas (Other).
        for alias in [
            r"\\.\PhysicalDrive0 ",
            r"\\.\PhysicalDrive0.",
            r"\\.\PhysicalDrive0\",
            r"\\.\PhysicalDrive",
            r"\\.\PhysicalDrive-1",
            r"\\.\PhysicalDrive99999999999",
            r"\\.\Harddisk0Partition1",
            r"\\?\GLOBALROOT\Device\Harddisk0\DR0",
        ] {
            assert_eq!(classify_windows_device(alias), Some(Other), "{alias}");
        }
        assert_eq!(classify_windows_device(r"D:\img.cfimg"), None);
        assert_eq!(classify_windows_device(r"\\?\D:\img.cfimg"), None);
    }

    #[test]
    fn windows_destino_disco_do_sistema_bloqueado_em_qualquer_grafia() {
        let disks = windows_disks();
        for alias in [
            r"\\.\PhysicalDrive1",
            r"\\.\physicaldrive1",
            r"\\.\PHYSICALDRIVE1",
            r"//./PhysicalDrive1",
            r"\\?\PhysicalDrive1",
            r"\\.\PhysicalDrive01",
        ] {
            assert_eq!(resolve_target_windows(alias, &disks).unwrap_err(), BLOCKED_SYSTEM, "{alias}");
        }
    }

    #[test]
    fn windows_destino_lista_de_permissao() {
        let disks = windows_disks();
        let t = resolve_target_windows(r"\\.\physicaldrive2", &disks).unwrap();
        assert_eq!(t.path, r"\\.\PhysicalDrive2"); // caminho canônico da enumeração
        assert_eq!(t.kind, PathKind::Device);
        assert_eq!(t.capacity, Some(16_000_000_000));
        assert_eq!(t.disk_id.as_deref(), Some("PhysicalDrive2"));

        // Volumes, partições e aliases nunca são destino — inclusive o C:.
        for bad in [
            r"\\.\C:",
            r"\\.\D:",
            r"\\?\Volume{aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee}",
            r"\\.\Harddisk1Partition3",
            r"\\.\PhysicalDrive1 ",
            r"\\?\GLOBALROOT\Device\Harddisk1\DR1",
        ] {
            assert!(resolve_target_windows(bad, &disks).is_err(), "{bad}");
        }
        // Disco que não está na enumeração.
        assert!(resolve_target_windows(r"\\.\PhysicalDrive9", &disks).is_err());
        // FAIL-CLOSED: enumeração vazia não libera nada.
        assert!(resolve_target_windows(r"\\.\PhysicalDrive1", &[]).is_err());
        // Somente-leitura.
        let mut ro = windows_disks();
        ro[2].is_read_only = true;
        assert!(resolve_target_windows(r"\\.\PhysicalDrive2", &ro).is_err());
        // Arquivo comum é aceito como destino.
        let f = resolve_target_windows(r"X:\saida\clone.img", &disks).unwrap();
        assert_eq!(f.kind, PathKind::File);
        assert_eq!(f.capacity, None);
    }

    #[test]
    fn windows_origem() {
        let disks = windows_disks();
        let d = resolve_source_windows(r"\\.\PHYSICALDRIVE1", &disks).unwrap();
        assert_eq!((d.size, d.source_kind), (1_000_000_000_000, KIND_DISK)); // sistema pode ser ORIGEM
        let v = resolve_source_windows(r"\\.\c:", &disks).unwrap();
        assert_eq!((v.size, v.source_kind), (999_000_000_000, KIND_PARTITION));
        assert_eq!(v.path, r"\\.\C:");
        assert!(v.mounted);
        assert_eq!(v.fs.as_deref(), Some("NTFS"));
        let g = resolve_source_windows(r"\\?\volume{AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE}\", &disks).unwrap();
        assert_eq!(g.size, 500_000_000);
        assert!(!g.mounted);
        assert!(resolve_source_windows(r"\\.\Z:", &disks).is_err());
        assert!(resolve_source_windows(r"\\.\PhysicalDrive7", &disks).is_err());
        assert!(resolve_source_windows(r"\\?\GLOBALROOT\Device\X", &disks).is_err());
    }

    fn unix_disks() -> Vec<DiskInfo> {
        let mut sys = disk("sda", "/dev/sda", 500_000_000_000, true);
        sys.partitions.push(part(1, "/dev/sda1", 499_000_000_000, Some("/")));
        let mut usb = disk("sdb", "/dev/sdb", 16_000_000_000, false);
        usb.partitions.push(part(1, "/dev/sdb1", 15_000_000_000, Some("/media/usb")));
        usb.mounts.push(MountInfo { device: "/dev/sdb1".into(), mount_point: "/media/usb".into() });
        let mac = disk("disk4", "/dev/rdisk4", 32_000_000_000, false);
        vec![sys, usb, mac]
    }

    #[test]
    fn unix_destino() {
        let disks = unix_disks();
        assert_eq!(resolve_unix_device_target("/dev/sda", &disks).unwrap_err(), BLOCKED_SYSTEM);
        // Partição do disco do sistema também é bloqueada (antes passava).
        assert_eq!(resolve_unix_device_target("/dev/sda1", &disks).unwrap_err(), BLOCKED_SYSTEM);
        let t = resolve_unix_device_target("/dev/sdb", &disks).unwrap();
        assert_eq!(t.capacity, Some(16_000_000_000));
        assert_eq!(t.mounts.len(), 1);
        assert!(resolve_unix_device_target("/dev/sdb1", &disks).is_err()); // partição: não suportado
        assert!(resolve_unix_device_target("/dev/sdz", &disks).is_err()); // não enumerado
        assert!(resolve_unix_device_target("/dev/dm-0", &disks).is_err());
        assert!(resolve_unix_device_target("/dev/sda", &[]).is_err()); // fail-closed
        // macOS: /dev/disk4 e /dev/rdisk4 são o mesmo disco.
        assert_eq!(resolve_unix_device_target("/dev/disk4", &disks).unwrap().path, "/dev/rdisk4");
        assert_eq!(resolve_unix_device_target("/dev/rdisk4", &disks).unwrap().path, "/dev/rdisk4");
    }

    #[test]
    fn unix_origem_e_alias_mac() {
        let disks = unix_disks();
        let p = resolve_unix_device_source("/dev/sdb1", &disks).unwrap();
        assert_eq!((p.size, p.source_kind), (15_000_000_000, KIND_PARTITION));
        assert_eq!(resolve_unix_device_source("/dev/sda", &disks).unwrap().source_kind, KIND_DISK);
        assert!(resolve_unix_device_source("/dev/nada", &disks).is_err());
        assert_eq!(mac_raw("/dev/disk4s2"), "/dev/rdisk4s2");
        assert_eq!(mac_raw("/dev/disk/by-id/usb-X"), "/dev/disk/by-id/usb-X");
        assert_eq!(mac_raw("/dev/sdb"), "/dev/sdb");
        assert_eq!(mac_buffered("/dev/rdisk4s2"), "/dev/disk4s2");
    }

    #[test]
    fn pares_invalidos() {
        let disks = windows_disks();
        let usb = resolve_target_windows(r"\\.\PhysicalDrive2", &disks).unwrap();
        let data = resolve_target_windows(r"\\.\PhysicalDrive0", &disks).unwrap();

        // Origem maior que o destino.
        let big = resolve_source_windows(r"\\.\PhysicalDrive1", &disks).unwrap();
        assert!(check_pair(&big, &usb).unwrap_err().contains("menor que a origem"));

        // Mesmo disco (grafias diferentes) e partição do próprio disco de destino.
        let same = resolve_source_windows(r"\\.\physicaldrive0", &disks).unwrap();
        assert!(check_pair(&same, &data).is_err());
        let part_of_target = resolve_source_windows(r"\\.\D:", &disks).unwrap();
        assert!(check_pair(&part_of_target, &data).unwrap_err().contains("mesmo disco"));

        // Disco pequeno → disco grande: ok.
        let small = resolve_source_windows(r"\\.\PhysicalDrive2", &disks).unwrap();
        assert!(check_pair(&small, &data).is_ok());
    }

    #[test]
    fn origem_armazenada_no_destino() {
        let mounts = vec![
            MountInfo { device: r"\\.\D:".into(), mount_point: r"D:\".into() },
            MountInfo { device: "/dev/sdb1".into(), mount_point: "/media/usb".into() },
            MountInfo { device: "/dev/sdb2".into(), mount_point: "[SWAP]".into() },
        ];
        assert!(file_on_mounts(r"D:\backups\win.cfimg", &mounts, true));
        assert!(file_on_mounts(r"d:\backups\win.cfimg", &mounts, true));
        assert!(file_on_mounts(r"\\?\D:\backups\win.cfimg", &mounts, true));
        assert!(!file_on_mounts(r"E:\backups\win.cfimg", &mounts, true));
        assert!(file_on_mounts("/media/usb/img.cfimg", &mounts, false));
        assert!(!file_on_mounts("/media/usb2/img.cfimg", &mounts, false));
        assert!(!file_on_mounts("/home/hugo/img.cfimg", &mounts, false));
    }

    /// A checagem por identidade do volume não depende do caminho digitado:
    /// pega o arquivo mesmo atrás de junction/pasta de montagem/symlink.
    #[test]
    fn origem_armazenada_no_destino_pela_identidade_do_volume() {
        let file = std::env::temp_dir().join(format!("cloneforge-vol-{}.bin", std::process::id()));
        std::fs::write(&file, b"x").unwrap();
        let file_str = file.to_string_lossy().into_owned();

        // Identidade real do volume/dispositivo que hospeda o arquivo temporário.
        #[cfg(windows)]
        let real_id = {
            let guid = file_volume_guid(&file_str).expect("GUID do volume do arquivo");
            assert!(guid.starts_with(r"\\?\Volume{") && guid.ends_with('}'), "{guid}");
            // a comparação ignora maiúsculas/minúsculas
            Some(guid.to_uppercase())
        };
        #[cfg(unix)]
        let real_id = {
            use std::os::unix::fs::MetadataExt;
            let dev = std::fs::metadata(&file).unwrap().dev();
            // procura em /dev o nó de bloco com esse número (tmpfs/overlay não têm)
            std::fs::read_dir("/dev").ok().and_then(|entries| {
                entries
                    .filter_map(|e| e.ok())
                    .map(|e| e.path())
                    .find(|p| std::fs::metadata(p).map(|m| m.rdev() == dev && dev != 0).unwrap_or(false))
                    .map(|p| p.to_string_lossy().into_owned())
            })
        };

        let mut target = ResolvedTarget {
            path: "alvo".into(),
            kind: PathKind::Device,
            capacity: Some(1),
            sector_size: 512,
            disk_id: None,
            mounts: Vec::new(),
            device_ids: vec!["/dev/null".into(), r"\\?\Volume{00000000-0000-0000-0000-000000000000}".into()],
        };
        // Destino que NÃO hospeda o arquivo: liberado.
        assert!(check_file_not_on_target(&file_str, PathKind::File, &target).is_ok());
        if let Some(id) = real_id {
            // Destino que hospeda o arquivo: bloqueado, sem nenhum ponto de montagem informado.
            target.device_ids.push(id);
            let err = check_file_not_on_target(&file_str, PathKind::File, &target).unwrap_err();
            assert!(err.contains("próprio disco de destino"), "{err}");
        } else {
            eprintln!("identidade do volume indisponível neste ambiente — parte positiva ignorada");
        }
        let _ = std::fs::remove_file(&file);
    }

    #[test]
    fn identidades_do_disco_de_destino() {
        let mut d = disk("PhysicalDrive2", r"\\.\PhysicalDrive2", 100, false);
        let mut p = part(1, r"\\.\E:", 90, Some(r"E:\"));
        p.volume = Some(r"\\?\Volume{aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee}".into());
        d.partitions.push(p);
        d.mounts.push(MountInfo { device: r"\\.\E:".into(), mount_point: r"E:\".into() });
        let t = disk_target(&d).unwrap();
        assert!(t.device_ids.contains(&r"\\?\Volume{aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee}".to_string()));
        assert!(t.device_ids.contains(&r"\\.\E:".to_string()));
        assert!(t.device_ids.contains(&r"\\.\PhysicalDrive2".to_string()));
        assert_eq!(t.device_ids.len(), 3, "sem duplicatas: {:?}", t.device_ids);
    }
}
