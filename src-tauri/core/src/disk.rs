//! Enumeração de discos físicos e partições por sistema operacional.
//!
//! Estratégia: delegar a descoberta às ferramentas nativas de cada SO
//! (Get-Disk / lsblk / diskutil) e normalizar para `DiskInfo`.
//!
//! REGRA DE OURO: qualquer disco marcado `is_system = true` JAMAIS pode ser
//! usado como destino de escrita — a UI desabilita e o backend revalida
//! (ver `safety.rs`).
//!
//! Os três parsers são funções puras compiladas em TODAS as plataformas
//! (mesmo o do diskutil), para que os testes unitários de cada um rodem em
//! qualquer CI. Só a execução do comando nativo fica atrás de `cfg`.
//!
//! FAIL-CLOSED: entrada malformada vira erro, nunca um disco "não-sistema".

use serde::Serialize;
use std::time::Duration;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PartitionInfo {
    pub index: u32,
    /// Caminho de I/O do volume/partição: `\\.\C:` ou `\\?\Volume{guid}`
    /// (Windows) | `/dev/sdb1` (Linux) | `/dev/disk4s2` (macOS)
    pub path: Option<String>,
    /// Windows: identidade estável do volume, `\\?\Volume{guid}` (sem barra
    /// final), exista ou não letra de unidade. `None` nos demais SOs.
    pub volume: Option<String>,
    pub size_bytes: u64,
    /// Deslocamento da partição dentro do disco (quando o SO informa)
    pub offset_bytes: Option<u64>,
    pub fs: Option<String>,
    pub label: Option<String>,
    pub mount_point: Option<String>,
}

/// Ponto de montagem ativo em algum volume do disco (em qualquer nível:
/// partição, LUKS, LVM…). `[SWAP]` representa swap ativo no Linux.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MountInfo {
    pub device: String,
    pub mount_point: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskInfo {
    pub id: String,
    /// Caminho de bloco para I/O raw: `\\.\PhysicalDrive1` | `/dev/sdb` | `/dev/rdisk2`
    pub path: String,
    pub model: String,
    pub size_bytes: u64,
    pub bus: String,
    /// true = contém o SO em execução -> nunca pode ser destino
    pub is_system: bool,
    pub is_removable: bool,
    pub is_read_only: bool,
    /// Tamanho do setor lógico (alinhamento exigido em I/O raw)
    pub sector_size: u32,
    pub partitions: Vec<PartitionInfo>,
    pub mounts: Vec<MountInfo>,
}

#[cfg_attr(not(any(windows, target_os = "linux", target_os = "macos")), allow(dead_code))]
async fn run(cmd: &str, args: &[&str]) -> Result<String, String> {
    let mut command = tokio::process::Command::new(cmd);
    command.args(args).kill_on_drop(true);
    // Sem CREATE_NO_WINDOW o powershell abre uma janela de console visível
    // junto com a GUI do app.
    #[cfg(target_os = "windows")]
    command.creation_flags(0x08000000); // CREATE_NO_WINDOW
    // Uma ferramenta travada não pode congelar a validação de segurança.
    let out = tokio::time::timeout(Duration::from_secs(60), command.output())
        .await
        .map_err(|_| format!("`{cmd}` não respondeu em 60 s"))?
        .map_err(|e| format!("falha ao executar `{cmd}`: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "`{cmd}` falhou ({:?}): {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    String::from_utf8(out.stdout).map_err(|e| e.to_string())
}

pub async fn list_disks() -> Result<Vec<DiskInfo>, String> {
    list_impl().await
}

// ─────────────────────────────── Windows ───────────────────────────────

/// Get-Disk expõe IsSystem/IsBoot — fonte autoritativa para a proteção
/// anti-autodestruição. Get-Volume (uma única chamada) dá filesystem e rótulo;
/// o vínculo partição↔volume é o caminho `\\?\Volume{guid}\`.
#[cfg(target_os = "windows")]
const WINDOWS_SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [Text.Encoding]::UTF8
$vols = @{}
Get-Volume -ErrorAction SilentlyContinue | ForEach-Object { if ($_.UniqueId) { $vols[$_.UniqueId] = $_ } }
$disks = @(Get-Disk | ForEach-Object {
  $dn = $_.Number
  [pscustomobject]@{
    Number     = $dn
    Model      = $_.FriendlyName
    Size       = $_.Size
    Bus        = [string]$_.BusType
    IsSystem   = [bool]$_.IsSystem
    IsBoot     = [bool]$_.IsBoot
    IsReadOnly = [bool]$_.IsReadOnly
    SectorSize = $_.LogicalSectorSize
    Partitions = @(Get-Partition -DiskNumber $dn -ErrorAction SilentlyContinue | ForEach-Object {
      $vp = @($_.AccessPaths | Where-Object { $_ -like '\\?\Volume*' }) | Select-Object -First 1
      $v = if ($vp) { $vols[$vp] } else { $null }
      [pscustomobject]@{
        Index  = $_.PartitionNumber
        Drive  = if ($_.DriveLetter) { [string]$_.DriveLetter } else { $null }
        Volume = if ($vp) { [string]$vp } else { $null }
        Size   = $_.Size
        Offset = $_.Offset
        Type   = [string]$_.Type
        Fs     = if ($v -and $v.FileSystem) { [string]$v.FileSystem } else { $null }
        Label  = if ($v -and $v.FileSystemLabel) { [string]$v.FileSystemLabel } else { $null }
      }
    })
  }
})
ConvertTo-Json -Depth 6 -Compress -InputObject $disks
"#;

#[cfg(target_os = "windows")]
async fn list_impl() -> Result<Vec<DiskInfo>, String> {
    let json = run(
        "powershell",
        &["-NoProfile", "-NonInteractive", "-Command", WINDOWS_SCRIPT],
    )
    .await?;
    parse_windows_disks(&json)
}

#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn parse_windows_disks(json: &str) -> Result<Vec<DiskInfo>, String> {
    use serde_json::Value;
    let v: Value = serde_json::from_str(json.trim())
        .map_err(|e| format!("resposta do Get-Disk não é JSON válido: {e}"))?;
    let arr = match v {
        Value::Array(a) => a,
        other => vec![other], // objeto único quando há 1 disco (versões antigas)
    };

    let mut disks = Vec::new();
    for d in arr {
        // FAIL-CLOSED: entrada malformada NUNCA vira disco. Um parse quebrado
        // que fabricasse um disco com is_system=false abriria caminho para
        // sobrescrever o disco do sistema — preferimos erro a risco.
        if !d.is_object() {
            return Err(format!("resposta inesperada do Get-Disk (formato): {d}"));
        }
        let number = d["Number"]
            .as_u64()
            .ok_or_else(|| format!("Get-Disk sem Number válido: {d}"))?;
        let flag = |key: &str| -> Result<bool, String> {
            d[key]
                .as_bool()
                .ok_or_else(|| format!("Get-Disk sem {key} booleano no disco {number}: {d}"))
        };
        // Sem IsSystem/IsBoot confiáveis não há como proteger o disco do SO.
        let is_system = flag("IsSystem")? || flag("IsBoot")?;

        let mut partitions = Vec::new();
        let mut mounts = Vec::new();
        for p in d["Partitions"].as_array().into_iter().flatten() {
            let drive = p["Drive"].as_str().filter(|s| !s.is_empty());
            let volume = p["Volume"]
                .as_str()
                .map(|s| s.trim_end_matches('\\').to_string())
                .filter(|s| !s.is_empty());
            let path = match drive {
                Some(l) => Some(format!("\\\\.\\{l}:")),
                None => volume.clone(),
            };
            let mount_point = drive.map(|l| format!("{l}:\\"));
            if let (Some(dev), Some(mp)) = (&path, &mount_point) {
                mounts.push(MountInfo { device: dev.clone(), mount_point: mp.clone() });
            }
            partitions.push(PartitionInfo {
                index: p["Index"].as_u64().unwrap_or(0) as u32,
                path,
                volume,
                size_bytes: p["Size"].as_u64().unwrap_or(0),
                offset_bytes: p["Offset"].as_u64(),
                fs: p["Fs"].as_str().filter(|s| !s.is_empty()).map(String::from),
                // Sem rótulo de volume, mostra o tipo da partição (System, Recovery…)
                label: p["Label"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .or_else(|| p["Type"].as_str())
                    .map(String::from),
                mount_point,
            });
        }

        let bus = d["Bus"].as_str().unwrap_or("?").to_string();
        disks.push(DiskInfo {
            id: format!("PhysicalDrive{number}"),
            path: format!("\\\\.\\PhysicalDrive{number}"),
            model: d["Model"].as_str().unwrap_or("Desconhecido").trim().to_string(),
            size_bytes: d["Size"].as_u64().unwrap_or(0),
            is_system,
            is_removable: matches!(bus.as_str(), "USB" | "SD" | "MMC"),
            is_read_only: d["IsReadOnly"].as_bool().unwrap_or(false),
            sector_size: d["SectorSize"].as_u64().filter(|s| *s > 0).unwrap_or(512) as u32,
            bus,
            partitions,
            mounts,
        });
    }
    Ok(disks)
}

// ─────────────────────────────── Linux ─────────────────────────────────

#[cfg(target_os = "linux")]
async fn list_impl() -> Result<Vec<DiskInfo>, String> {
    const COLS: &str = "NAME,PATH,SIZE,MODEL,TYPE,RM,RO,MOUNTPOINTS,TRAN,FSTYPE,LABEL,LOG-SEC";
    let json = match run("lsblk", &["--json", "--bytes", "--output", COLS]).await {
        Ok(j) => j,
        // util-linux < 2.37 não conhece a coluna MOUNTPOINTS
        Err(_) => {
            let legacy = COLS.replace("MOUNTPOINTS", "MOUNTPOINT");
            run("lsblk", &["--json", "--bytes", "--output", &legacy]).await?
        }
    };
    // Dispositivo que hospeda a raiz "/" — pode ser partição, LV ou mapper
    let root_src = run("findmnt", &["-n", "-o", "SOURCE", "/"])
        .await
        .unwrap_or_default();
    parse_lsblk(&json, &root_src)
}

/// Pontos de montagem cuja presença marca o disco como "do sistema".
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn is_system_mount(mp: &str) -> bool {
    matches!(mp, "/" | "/boot" | "/boot/efi" | "/efi" | "/usr" | "/var" | "[SWAP]")
}

/// lsblk antigo serializa números como string ("500107862016").
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn json_u64(v: &serde_json::Value) -> Option<u64> {
    v.as_u64().or_else(|| v.as_str()?.trim().parse().ok())
}

/// lsblk antigo serializa booleanos como "0"/"1".
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn json_bool(v: &serde_json::Value) -> bool {
    v.as_bool()
        .or_else(|| v.as_u64().map(|n| n != 0))
        .or_else(|| v.as_str().map(|s| matches!(s.trim(), "1" | "true")))
        .unwrap_or(false)
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn lsblk_mountpoints(node: &serde_json::Value) -> Vec<String> {
    let mut out: Vec<String> = node["mountpoints"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| m.as_str())
        .map(String::from)
        .collect();
    if let Some(m) = node["mountpoint"].as_str() {
        out.push(m.to_string()); // coluna legada MOUNTPOINT
    }
    out
}

/// Percorre TODOS os descendentes (partição → LUKS → LVM → …). A versão
/// anterior olhava só o primeiro nível: com a raiz em LVM/LUKS o disco do
/// sistema não era reconhecido e podia ser escolhido como destino.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn lsblk_walk(
    node: &serde_json::Value,
    root_src: &str,
    mounts: &mut Vec<MountInfo>,
    is_system: &mut bool,
) {
    let path = node["path"].as_str().unwrap_or_default();
    if !root_src.is_empty() && path == root_src {
        *is_system = true;
    }
    for mp in lsblk_mountpoints(node) {
        if is_system_mount(&mp) {
            *is_system = true;
        }
        mounts.push(MountInfo { device: path.to_string(), mount_point: mp });
    }
    for child in node["children"].as_array().into_iter().flatten() {
        lsblk_walk(child, root_src, mounts, is_system);
    }
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn parse_lsblk(json: &str, root_src: &str) -> Result<Vec<DiskInfo>, String> {
    let v: serde_json::Value = serde_json::from_str(json)
        .map_err(|e| format!("resposta do lsblk não é JSON válido: {e}"))?;
    let devices = v["blockdevices"]
        .as_array()
        .ok_or("resposta do lsblk sem `blockdevices`")?;
    // `findmnt` em btrfs devolve "/dev/sda2[/@]"
    let root_src = root_src.trim().split('[').next().unwrap_or_default();

    let mut disks = Vec::new();
    for d in devices {
        if d["type"].as_str() != Some("disk") {
            continue;
        }
        let name = d["name"].as_str().unwrap_or_default().to_string();
        let path = d["path"].as_str().unwrap_or_default().to_string();
        let size_bytes = json_u64(&d["size"]).unwrap_or(0);
        // zram/ramdisk/nbd e áreas especiais de eMMC não são alvos de clonagem;
        // tamanho 0 = leitor de cartão vazio.
        let pseudo = ["zram", "ram", "nbd"].iter().any(|p| name.starts_with(p))
            || (name.starts_with("mmcblk") && (name.contains("boot") || name.contains("rpmb")));
        if pseudo || size_bytes == 0 {
            continue;
        }
        if path.is_empty() {
            return Err(format!("lsblk sem PATH para o disco `{name}`"));
        }

        let mut mounts = Vec::new();
        let mut is_system = false;
        lsblk_walk(d, root_src, &mut mounts, &mut is_system);

        let partitions = d["children"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
            .map(|(i, p)| PartitionInfo {
                index: i as u32 + 1,
                path: p["path"].as_str().map(String::from),
                volume: None,
                size_bytes: json_u64(&p["size"]).unwrap_or(0),
                offset_bytes: None,
                fs: p["fstype"].as_str().map(String::from),
                label: p["label"].as_str().map(String::from),
                mount_point: lsblk_mountpoints(p).into_iter().next(),
            })
            .collect();

        disks.push(DiskInfo {
            id: name,
            path,
            model: d["model"].as_str().unwrap_or("Desconhecido").trim().to_string(),
            size_bytes,
            bus: d["tran"].as_str().unwrap_or("?").to_string(),
            is_system,
            is_removable: json_bool(&d["rm"]),
            is_read_only: json_bool(&d["ro"]),
            sector_size: json_u64(&d["log-sec"]).filter(|s| *s > 0).unwrap_or(512) as u32,
            partitions,
            mounts,
        });
    }
    Ok(disks)
}

// ─────────────────────────────── macOS ─────────────────────────────────

#[cfg(target_os = "macos")]
async fn list_impl() -> Result<Vec<DiskInfo>, String> {
    let xml = run("diskutil", &["list", "-plist"]).await?;
    let list: plist::Value = plist::from_bytes(xml.as_bytes()).map_err(|e| e.to_string())?;

    // Disco (ou container APFS sintetizado) que hospeda o volume de boot.
    let boot_xml = run("diskutil", &["info", "-plist", "/"]).await?;
    let boot_disk = plist::from_bytes::<plist::Value>(boot_xml.as_bytes())
        .ok()
        .and_then(|v| {
            v.as_dictionary()
                .and_then(|d| d.get("ParentWholeDisk"))
                .and_then(|s| s.as_string().map(String::from))
        })
        // FAIL-CLOSED: sem saber qual é o disco de boot não há proteção.
        .ok_or("não foi possível identificar o disco de boot (diskutil info /)")?;

    // MediaName/BusProtocol/Internal/DeviceBlockSize só existem no `info`
    // de cada disco — o `list` não traz esses campos.
    let mut infos = std::collections::HashMap::new();
    for dev in diskutil_physical_ids(&list) {
        if let Ok(x) = run("diskutil", &["info", "-plist", &dev]).await {
            if let Ok(v) = plist::from_bytes::<plist::Value>(x.as_bytes()) {
                infos.insert(dev, v);
            }
        }
    }
    Ok(parse_diskutil(&list, &boot_disk, &infos))
}

/// "disk0s2" → "disk0"; "disk3s1s1" → "disk3"
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn whole_disk_of(dev: &str) -> String {
    let Some(rest) = dev.strip_prefix("disk") else {
        return dev.to_string();
    };
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    format!("disk{digits}")
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn plist_str<'a>(d: &'a plist::Dictionary, key: &str) -> Option<&'a str> {
    d.get(key).and_then(|v| v.as_string())
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn plist_entries(list: &plist::Value) -> Vec<&plist::Dictionary> {
    list.as_dictionary()
        .and_then(|d| d.get("AllDisksAndPartitions"))
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|e| e.as_dictionary()).collect())
        .unwrap_or_default()
}

/// Identificadores dos discos físicos (exclui containers APFS sintetizados).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn diskutil_physical_ids(list: &plist::Value) -> Vec<String> {
    plist_entries(list)
        .into_iter()
        .filter(|d| !d.contains_key("APFSPhysicalStores"))
        .filter_map(|d| plist_str(d, "DeviceIdentifier").map(String::from))
        .collect()
}

/// `list` = `diskutil list -plist`; `boot_disk` = ParentWholeDisk de "/";
/// `infos` = `diskutil info -plist <disco>` por disco físico.
///
/// APFS: o volume de boot vive num container SINTETIZADO (ex.: disk3) cujo
/// armazenamento físico é uma partição de outro disco (ex.: disk0s2). A
/// versão anterior marcava só o container como "sistema" — o disco físico
/// interno ficava liberado como destino. Aqui o vínculo é resolvido via
/// `APFSPhysicalStores`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn parse_diskutil(
    list: &plist::Value,
    boot_disk: &str,
    infos: &std::collections::HashMap<String, plist::Value>,
) -> Vec<DiskInfo> {
    let entries = plist_entries(list);
    let is_sys_mount = |mp: &str| mp == "/" || mp.starts_with("/System/Volumes");

    // 1ª passada: containers APFS → discos físicos que os hospedam.
    let mut system_physical: Vec<String> = Vec::new();
    let mut container_mounts: Vec<(String, MountInfo)> = Vec::new(); // (disco físico, montagem)
    for c in entries.iter().filter(|d| d.contains_key("APFSPhysicalStores")) {
        let container_id = plist_str(c, "DeviceIdentifier").unwrap_or_default();
        let stores: Vec<String> = c
            .get("APFSPhysicalStores")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
            .filter_map(|s| s.as_dictionary())
            .filter_map(|s| plist_str(s, "DeviceIdentifier"))
            .map(whole_disk_of)
            .collect();
        let mut hosts_system = container_id == boot_disk;
        for vol in c
            .get("APFSVolumes")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_dictionary())
        {
            let vol_dev = plist_str(vol, "DeviceIdentifier").unwrap_or_default();
            let mut mps: Vec<String> = plist_str(vol, "MountPoint")
                .map(|m| vec![m.to_string()])
                .unwrap_or_default();
            // A raiz selada aparece como snapshot montado em "/"
            for snap in vol
                .get("MountedSnapshots")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
                .filter_map(|s| s.as_dictionary())
            {
                if let Some(m) = plist_str(snap, "SnapshotMountPoint") {
                    mps.push(m.to_string());
                }
            }
            for mp in mps {
                if is_sys_mount(&mp) {
                    hosts_system = true;
                }
                for store in &stores {
                    container_mounts.push((
                        store.clone(),
                        MountInfo { device: format!("/dev/{vol_dev}"), mount_point: mp.clone() },
                    ));
                }
            }
        }
        if hosts_system {
            system_physical.extend(stores);
        }
    }

    // 2ª passada: discos físicos.
    let mut disks = Vec::new();
    for dict in entries.iter().filter(|d| !d.contains_key("APFSPhysicalStores")) {
        let Some(dev) = plist_str(dict, "DeviceIdentifier").filter(|s| !s.is_empty()) else {
            continue;
        };
        let partitions: Vec<PartitionInfo> = dict
            .get("Partitions")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
            .filter_map(|p| p.as_dictionary())
            .enumerate()
            .map(|(i, pd)| PartitionInfo {
                index: i as u32 + 1,
                path: plist_str(pd, "DeviceIdentifier").map(|s| format!("/dev/{s}")),
                volume: None,
                size_bytes: pd.get("Size").and_then(|v| v.as_unsigned_integer()).unwrap_or(0),
                offset_bytes: None,
                fs: plist_str(pd, "Content").map(String::from),
                label: plist_str(pd, "VolumeName").map(String::from),
                mount_point: plist_str(pd, "MountPoint").map(String::from),
            })
            .collect();

        let mut mounts: Vec<MountInfo> = partitions
            .iter()
            .filter_map(|p| {
                Some(MountInfo { device: p.path.clone()?, mount_point: p.mount_point.clone()? })
            })
            .collect();
        mounts.extend(
            container_mounts
                .iter()
                .filter(|(phys, _)| phys == dev)
                .map(|(_, m)| m.clone()),
        );

        let info = infos.get(dev).and_then(|v| v.as_dictionary());
        let info_bool = |key: &str| info.and_then(|d| d.get(key)).and_then(|v| v.as_boolean());
        let internal = info_bool("Internal");

        disks.push(DiskInfo {
            id: dev.to_string(),
            // rdisk = raw device: bypass do buffer cache, essencial para clonagem rápida
            path: format!("/dev/r{dev}"),
            model: info
                .and_then(|d| plist_str(d, "MediaName"))
                .filter(|s| !s.is_empty())
                .unwrap_or("Disco")
                .to_string(),
            size_bytes: dict.get("Size").and_then(|v| v.as_unsigned_integer()).unwrap_or(0),
            bus: info
                .and_then(|d| plist_str(d, "BusProtocol"))
                .unwrap_or("?")
                .to_string(),
            is_system: dev == boot_disk
                || system_physical.iter().any(|s| s == dev)
                || mounts.iter().any(|m| is_sys_mount(&m.mount_point)),
            is_removable: info_bool("RemovableMediaOrExternalDevice")
                .or(internal.map(|i| !i))
                .unwrap_or(false),
            is_read_only: info_bool("WritableMedia").map(|w| !w).unwrap_or(false),
            sector_size: info
                .and_then(|d| d.get("DeviceBlockSize"))
                .and_then(|v| v.as_unsigned_integer())
                .filter(|s| *s > 0)
                .unwrap_or(512) as u32,
            partitions,
            mounts,
        });
    }
    disks
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Windows ──────────────────────────────────────────────────────────
    const WIN_JSON: &str = r#"[
      {"Number":1,"Model":"KINGSTON SNV2S1000G ","Size":1000204886016,"Bus":"NVMe","IsSystem":true,"IsBoot":true,"IsReadOnly":false,"SectorSize":512,
       "Partitions":[
         {"Index":1,"Drive":null,"Volume":"\\\\?\\Volume{11111111-2222-3333-4444-555555555555}\\","Size":104857600,"Offset":1048576,"Type":"System","Fs":null,"Label":null},
         {"Index":2,"Drive":null,"Volume":null,"Size":16777216,"Offset":105906176,"Type":"Reserved","Fs":null,"Label":null},
         {"Index":3,"Drive":"C","Volume":"\\\\?\\Volume{aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee}\\","Size":999139835904,"Offset":122683392,"Type":"Basic","Fs":"NTFS","Label":"Windows"}]},
      {"Number":0,"Model":"Samsung SSD 990 PRO 2TB","Size":2048408248320,"Bus":"NVMe","IsSystem":false,"IsBoot":false,"IsReadOnly":false,"SectorSize":512,
       "Partitions":[{"Index":1,"Drive":"D","Volume":"\\\\?\\Volume{99999999-0000-0000-0000-100000000000}\\","Size":2048405798912,"Offset":1048576,"Type":"IFS","Fs":"NTFS","Label":null}]},
      {"Number":2,"Model":"SanDisk Cruzer Fit","Size":15376318464,"Bus":"USB","IsSystem":false,"IsBoot":false,"IsReadOnly":false,"SectorSize":512,"Partitions":[]}
    ]"#;

    #[test]
    fn windows_parse_basico() {
        let disks = parse_windows_disks(WIN_JSON).unwrap();
        assert_eq!(disks.len(), 3);

        // O disco do sistema NÃO é necessariamente o PhysicalDrive0.
        let sys = &disks[0];
        assert_eq!(sys.path, r"\\.\PhysicalDrive1");
        assert!(sys.is_system);
        assert_eq!(sys.model, "KINGSTON SNV2S1000G");
        assert_eq!(sys.partitions[0].path.as_deref(), Some(r"\\?\Volume{11111111-2222-3333-4444-555555555555}"));
        assert_eq!(sys.partitions[0].label.as_deref(), Some("System")); // sem rótulo → tipo
        assert_eq!(sys.partitions[1].path, None); // MSR não tem volume
        let c = &sys.partitions[2];
        assert_eq!(c.path.as_deref(), Some(r"\\.\C:"));
        assert_eq!(c.volume.as_deref(), Some(r"\\?\Volume{aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee}"));
        assert_eq!(c.mount_point.as_deref(), Some(r"C:\"));
        assert_eq!(c.fs.as_deref(), Some("NTFS"));
        assert_eq!(c.label.as_deref(), Some("Windows"));
        assert_eq!(c.offset_bytes, Some(122683392));
        assert_eq!(sys.mounts, vec![MountInfo { device: r"\\.\C:".into(), mount_point: r"C:\".into() }]);

        assert!(!disks[1].is_system);
        assert!(!disks[1].is_removable);
        let usb = &disks[2];
        assert!(usb.is_removable && !usb.is_system);
        assert!(usb.partitions.is_empty());
    }

    #[test]
    fn windows_objeto_unico_vira_um_disco() {
        let one = r#"{"Number":0,"Model":"X","Size":10,"Bus":"SATA","IsSystem":true,"IsBoot":true,"Partitions":[]}"#;
        let disks = parse_windows_disks(one).unwrap();
        assert_eq!(disks.len(), 1);
        assert!(disks[0].is_system);
        assert_eq!(disks[0].sector_size, 512);
    }

    #[test]
    fn windows_fail_closed() {
        // Regressão do bug 2.1: array duplo fabricava disco fantasma com is_system=false.
        assert!(parse_windows_disks(r#"[[{"Number":0}]]"#).is_err());
        assert!(parse_windows_disks("").is_err());
        assert!(parse_windows_disks("não é json").is_err());
        assert!(parse_windows_disks(r#"[{"Model":"sem número"}]"#).is_err());
        // Sem IsSystem/IsBoot booleanos o disco não pode ser considerado "seguro".
        assert!(parse_windows_disks(r#"[{"Number":0,"Model":"X","Size":1}]"#).is_err());
        assert!(parse_windows_disks(r#"[{"Number":0,"IsSystem":"False","IsBoot":false}]"#).is_err());
        // Lista vazia é válida (nenhum disco), não erro.
        assert_eq!(parse_windows_disks("[]").unwrap().len(), 0);
    }

    // ── Linux ────────────────────────────────────────────────────────────
    const LSBLK_JSON: &str = r#"{"blockdevices":[
      {"name":"sda","path":"/dev/sda","size":500107862016,"model":"Samsung SSD 860 ","type":"disk","rm":false,"ro":false,"mountpoints":[null],"tran":"sata","fstype":null,"label":null,"log-sec":512,
       "children":[
         {"name":"sda1","path":"/dev/sda1","size":536870912,"type":"part","mountpoints":["/boot/efi"],"fstype":"vfat","label":null},
         {"name":"sda2","path":"/dev/sda2","size":499569991680,"type":"part","mountpoints":[null],"fstype":"ext4","label":"dados"}]},
      {"name":"nvme0n1","path":"/dev/nvme0n1","size":1000204886016,"model":"WD Black","type":"disk","rm":false,"ro":false,"mountpoints":[null],"tran":"nvme","log-sec":4096,
       "children":[
         {"name":"nvme0n1p1","path":"/dev/nvme0n1p1","size":1000203837440,"type":"part","mountpoints":[null],"fstype":"crypto_LUKS",
          "children":[{"name":"cryptroot","path":"/dev/mapper/cryptroot","type":"crypt","mountpoints":[null],"fstype":"LVM2_member",
            "children":[
              {"name":"vg-root","path":"/dev/mapper/vg-root","type":"lvm","mountpoints":["/"],"fstype":"ext4"},
              {"name":"vg-home","path":"/dev/mapper/vg-home","type":"lvm","mountpoints":["/home"],"fstype":"ext4"}]}]}]},
      {"name":"sdb","path":"/dev/sdb","size":15376318464,"model":"Cruzer Fit","type":"disk","rm":true,"ro":false,"mountpoints":[null],"tran":"usb","log-sec":512,
       "children":[{"name":"sdb1","path":"/dev/sdb1","size":15375269888,"type":"part","mountpoints":["/media/hugo/USB"],"fstype":"vfat","label":"USB"}]},
      {"name":"sdc","path":"/dev/sdc","size":0,"model":"Card Reader","type":"disk","rm":true,"ro":false,"mountpoints":[null],"tran":"usb"},
      {"name":"zram0","path":"/dev/zram0","size":4294967296,"model":null,"type":"disk","rm":false,"ro":false,"mountpoints":["[SWAP]"],"tran":null},
      {"name":"sr0","path":"/dev/sr0","size":1073741312,"model":"DVD","type":"rom","rm":true,"ro":false,"mountpoints":[null],"tran":"sata"}
    ]}"#;

    #[test]
    fn linux_parse_e_disco_do_sistema() {
        let disks = parse_lsblk(LSBLK_JSON, "/dev/mapper/vg-root\n").unwrap();
        let ids: Vec<&str> = disks.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(ids, ["sda", "nvme0n1", "sdb"]); // sdc (0 B), zram0 e sr0 ficam de fora

        let sda = &disks[0];
        assert!(sda.is_system, "/boot/efi marca o disco como sistema");
        assert_eq!(sda.model, "Samsung SSD 860");
        assert_eq!(sda.partitions.len(), 2);
        assert_eq!(sda.partitions[1].label.as_deref(), Some("dados"));

        // Regressão: raiz em LVM sobre LUKS (3 níveis abaixo do disco), sem
        // /boot separado — antes este disco NÃO era marcado como sistema.
        let nvme = &disks[1];
        assert!(nvme.is_system, "raiz em LVM/LUKS precisa marcar o disco físico");
        assert_eq!(nvme.sector_size, 4096);
        assert!(nvme.mounts.iter().any(|m| m.mount_point == "/home" && m.device == "/dev/mapper/vg-home"));

        let usb = &disks[2];
        assert!(!usb.is_system && usb.is_removable);
        assert_eq!(usb.mounts, vec![MountInfo { device: "/dev/sdb1".into(), mount_point: "/media/hugo/USB".into() }]);
        assert_eq!(usb.partitions[0].mount_point.as_deref(), Some("/media/hugo/USB"));
    }

    #[test]
    fn linux_raiz_so_pelo_findmnt_e_btrfs() {
        // Sem mountpoints no JSON (ex.: container), a raiz vem só do findmnt;
        // btrfs devolve o subvolume entre colchetes.
        let json = r#"{"blockdevices":[{"name":"vda","path":"/dev/vda","size":"10737418240","type":"disk","rm":"0","ro":"0",
          "children":[{"name":"vda2","path":"/dev/vda2","size":"10000000000","type":"part"}]}]}"#;
        let disks = parse_lsblk(json, "/dev/vda2[/@]").unwrap();
        assert!(disks[0].is_system);
        assert_eq!(disks[0].size_bytes, 10737418240); // número como string (lsblk antigo)
        assert!(!disks[0].is_removable);
        assert!(!parse_lsblk(json, "/dev/outro").unwrap()[0].is_system);
    }

    #[test]
    fn linux_coluna_legada_e_swap() {
        let json = r#"{"blockdevices":[{"name":"sdd","path":"/dev/sdd","size":1000,"type":"disk","rm":"1",
          "children":[{"name":"sdd1","path":"/dev/sdd1","size":500,"type":"part","mountpoint":"[SWAP]"}]}]}"#;
        let disks = parse_lsblk(json, "").unwrap();
        assert!(disks[0].is_system, "swap ativo não pode ser sobrescrito");
        assert!(disks[0].is_removable);
        assert!(parse_lsblk("{}", "").is_err());
        assert!(parse_lsblk("lixo", "").is_err());
    }

    // ── macOS ────────────────────────────────────────────────────────────
    const DISKUTIL_LIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>AllDisksAndPartitions</key>
<array>
  <dict>
    <key>Content</key><string>GUID_partition_scheme</string>
    <key>DeviceIdentifier</key><string>disk0</string>
    <key>Partitions</key>
    <array>
      <dict><key>Content</key><string>Apple_APFS_ISC</string><key>DeviceIdentifier</key><string>disk0s1</string><key>Size</key><integer>524288000</integer></dict>
      <dict><key>Content</key><string>Apple_APFS</string><key>DeviceIdentifier</key><string>disk0s2</string><key>Size</key><integer>494384795648</integer></dict>
      <dict><key>Content</key><string>Apple_APFS_Recovery</string><key>DeviceIdentifier</key><string>disk0s3</string><key>Size</key><integer>5368664064</integer></dict>
    </array>
    <key>Size</key><integer>500277792768</integer>
  </dict>
  <dict>
    <key>APFSPhysicalStores</key>
    <array><dict><key>DeviceIdentifier</key><string>disk0s2</string></dict></array>
    <key>APFSVolumes</key>
    <array>
      <dict>
        <key>DeviceIdentifier</key><string>disk3s1</string>
        <key>MountedSnapshots</key>
        <array><dict><key>SnapshotBSD</key><string>disk3s1s1</string><key>SnapshotMountPoint</key><string>/</string></dict></array>
        <key>VolumeName</key><string>Macintosh HD</string>
      </dict>
      <dict>
        <key>DeviceIdentifier</key><string>disk3s5</string>
        <key>MountPoint</key><string>/System/Volumes/Data</string>
        <key>VolumeName</key><string>Data</string>
      </dict>
    </array>
    <key>DeviceIdentifier</key><string>disk3</string>
    <key>Size</key><integer>494384795648</integer>
  </dict>
  <dict>
    <key>Content</key><string>FDisk_partition_scheme</string>
    <key>DeviceIdentifier</key><string>disk4</string>
    <key>Partitions</key>
    <array>
      <dict><key>Content</key><string>DOS_FAT_32</string><key>DeviceIdentifier</key><string>disk4s1</string>
            <key>MountPoint</key><string>/Volumes/PENDRIVE</string><key>Size</key><integer>15375269888</integer>
            <key>VolumeName</key><string>PENDRIVE</string></dict>
    </array>
    <key>Size</key><integer>15376318464</integer>
  </dict>
</array>
</dict></plist>"#;

    fn plist_of(xml: &str) -> plist::Value {
        plist::from_bytes(xml.as_bytes()).unwrap()
    }

    #[test]
    fn macos_apfs_marca_o_disco_fisico_como_sistema() {
        let list = plist_of(DISKUTIL_LIST);
        assert_eq!(diskutil_physical_ids(&list), ["disk0", "disk4"]);

        let mut infos = std::collections::HashMap::new();
        infos.insert(
            "disk4".to_string(),
            plist_of(
                r#"<plist version="1.0"><dict>
                <key>MediaName</key><string>SanDisk Cruzer</string>
                <key>BusProtocol</key><string>USB</string>
                <key>Internal</key><false/>
                <key>RemovableMediaOrExternalDevice</key><true/>
                <key>DeviceBlockSize</key><integer>512</integer>
                <key>WritableMedia</key><true/></dict></plist>"#,
            ),
        );
        infos.insert(
            "disk0".to_string(),
            plist_of(
                r#"<plist version="1.0"><dict>
                <key>MediaName</key><string>APPLE SSD AP0512Q</string>
                <key>BusProtocol</key><string>Apple Fabric</string>
                <key>Internal</key><true/>
                <key>DeviceBlockSize</key><integer>4096</integer></dict></plist>"#,
            ),
        );

        // "/" pertence ao container sintetizado disk3, hospedado em disk0s2.
        let disks = parse_diskutil(&list, "disk3", &infos);
        let ids: Vec<&str> = disks.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(ids, ["disk0", "disk4"], "container sintetizado não é disco físico");

        let internal = &disks[0];
        assert!(internal.is_system, "disco físico do container de boot precisa ser sistema");
        assert_eq!(internal.path, "/dev/rdisk0");
        assert_eq!(internal.model, "APPLE SSD AP0512Q");
        assert_eq!(internal.sector_size, 4096);
        assert!(!internal.is_removable);
        assert!(internal.mounts.iter().any(|m| m.mount_point == "/"));

        let usb = &disks[1];
        assert!(!usb.is_system);
        assert!(usb.is_removable);
        assert_eq!(usb.bus, "USB");
        assert_eq!(usb.partitions[0].path.as_deref(), Some("/dev/disk4s1"));
        assert_eq!(usb.mounts[0].mount_point, "/Volumes/PENDRIVE");

        // Mesmo sem o ParentWholeDisk bater, os pontos de montagem protegem.
        assert!(parse_diskutil(&list, "", &infos)[0].is_system);
    }

    #[test]
    fn macos_whole_disk() {
        assert_eq!(whole_disk_of("disk0s2"), "disk0");
        assert_eq!(whole_disk_of("disk12s1s1"), "disk12");
        assert_eq!(whole_disk_of("disk3"), "disk3");
    }
}
