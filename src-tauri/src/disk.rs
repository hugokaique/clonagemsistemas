//! Enumeração de discos físicos e partições por sistema operacional.
//!
//! Estratégia: delegar a descoberta às ferramentas nativas de cada SO
//! (Get-Disk / lsblk / diskutil) e normalizar para `DiskInfo`.
//!
//! REGRA DE OURO: qualquer disco marcado `is_system = true` JAMAIS pode ser
//! usado como destino de escrita — a UI desabilita e o backend revalida.

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PartitionInfo {
    pub index: u32,
    pub path: Option<String>,
    pub size_bytes: u64,
    pub fs: Option<String>,
    pub label: Option<String>,
    pub mount_point: Option<String>,
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
    pub partitions: Vec<PartitionInfo>,
}

async fn run(cmd: &str, args: &[&str]) -> Result<String, String> {
    let mut command = tokio::process::Command::new(cmd);
    command.args(args);
    // Sem CREATE_NO_WINDOW o powershell abre uma janela de console visível
    // junto com a GUI do app.
    #[cfg(target_os = "windows")]
    command.creation_flags(0x08000000); // CREATE_NO_WINDOW
    let out = command
        .output()
        .await
        .map_err(|e| format!("falha ao executar `{cmd}`: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "`{cmd}` falhou ({:?}): {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    String::from_utf8(out.stdout).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn list_disks() -> Result<Vec<DiskInfo>, String> {
    list_impl().await
}

// ─────────────────────────────── Windows ───────────────────────────────
#[cfg(target_os = "windows")]
async fn list_impl() -> Result<Vec<DiskInfo>, String> {
    // Get-Disk expõe IsSystem/IsBoot — fonte autoritativa para a proteção anti-autodestruição.
    let ps = r#"
[Console]::OutputEncoding = [Text.Encoding]::UTF8
$disks = @(Get-Disk | ForEach-Object {
  [pscustomobject]@{
    Number      = $_.Number
    Model       = $_.FriendlyName
    Size        = $_.Size
    Bus         = [string]$_.BusType
    IsSystem    = [bool]$_.IsSystem
    IsBoot      = [bool]$_.IsBoot
    IsRemovable = ($_.BusType -eq 'USB')
    Partitions  = @(Get-Partition -DiskNumber $_.Number -ErrorAction SilentlyContinue | ForEach-Object {
      [pscustomobject]@{
        Index = $_.PartitionNumber
        Drive = if ($_.DriveLetter) { [string]$_.DriveLetter } else { $null }
        Size  = $_.Size
        Type  = [string]$_.Type
      }
    })
  }
})
ConvertTo-Json -Depth 6 -Compress -InputObject $disks
"#;
    let json = run("powershell", &["-NoProfile", "-NonInteractive", "-Command", ps]).await?;
    let v: serde_json::Value = serde_json::from_str(json.trim()).map_err(|e| e.to_string())?;
    let arr = match v {
        serde_json::Value::Array(a) => a,
        other => vec![other], // ConvertTo-Json devolve objeto único quando há 1 disco
    };

    let mut disks = Vec::new();
    for d in arr {
        // FAIL-CLOSED: entrada malformada NUNCA vira disco. Um parse quebrado
        // que fabricasse PhysicalDrive0 com is_system=false abriria caminho
        // para sobrescrever o disco do sistema — preferimos erro a risco.
        if !d.is_object() {
            return Err(format!("resposta inesperada do Get-Disk (formato): {d}"));
        }
        let number = d["Number"]
            .as_u64()
            .ok_or_else(|| format!("Get-Disk sem Number válido: {d}"))?;
        let partitions = d["Partitions"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|p| PartitionInfo {
                index: p["Index"].as_u64().unwrap_or(0) as u32,
                path: p["Drive"].as_str().map(|l| format!("{l}:")),
                size_bytes: p["Size"].as_u64().unwrap_or(0),
                fs: None,
                label: p["Type"].as_str().map(String::from),
                mount_point: p["Drive"].as_str().map(|l| format!("{l}:\\")),
            })
            .collect();
        disks.push(DiskInfo {
            id: format!("PhysicalDrive{number}"),
            path: format!("\\\\.\\PhysicalDrive{number}"),
            model: d["Model"].as_str().unwrap_or("Desconhecido").trim().to_string(),
            size_bytes: d["Size"].as_u64().unwrap_or(0),
            bus: d["Bus"].as_str().unwrap_or("?").to_string(),
            is_system: d["IsSystem"].as_bool().unwrap_or(false)
                || d["IsBoot"].as_bool().unwrap_or(false),
            is_removable: d["IsRemovable"].as_bool().unwrap_or(false),
            partitions,
        });
    }
    Ok(disks)
}

// ─────────────────────────────── Linux ─────────────────────────────────

/// Pontos de montagem que caracterizam o disco do SO em execução. Inclui o
/// swap ativo e as mídias de sistemas live (o pendrive de boot do técnico).
#[cfg(any(target_os = "linux", test))]
const LINUX_SYSTEM_MOUNTS: &[&str] = &[
    "/",
    "/boot",
    "/boot/efi",
    "/efi",
    "/usr",
    "/var",
    "/home",
    "[SWAP]",
    "/cdrom",
    "/run/live/medium",
    "/lib/live/mount/medium",
    "/run/initramfs/live",
    "/run/archiso/bootmnt",
];

#[cfg(target_os = "linux")]
async fn list_impl() -> Result<Vec<DiskInfo>, String> {
    // MOUNTPOINTS (plural) só existe no util-linux >= 2.37; versões antigas
    // (Ubuntu 20.04, RHEL 8) só conhecem MOUNTPOINT. Tenta as duas.
    const COLS_NEW: &str = "NAME,PATH,SIZE,MODEL,TYPE,RM,MOUNTPOINTS,TRAN,FSTYPE,LABEL";
    const COLS_OLD: &str = "NAME,PATH,SIZE,MODEL,TYPE,RM,MOUNTPOINT,TRAN,FSTYPE,LABEL";
    let json = match run("lsblk", &["--json", "--bytes", "--output", COLS_NEW]).await {
        Ok(j) => j,
        Err(_) => run("lsblk", &["--json", "--bytes", "--output", COLS_OLD]).await?,
    };

    // FAIL-CLOSED: sem saber onde está a raiz não há como proteger o disco
    // do sistema — preferimos erro a devolver uma lista sem a marcação.
    let root = run("findmnt", &["-n", "-o", "SOURCE,FSTYPE", "/"])
        .await
        .map_err(|e| format!("não foi possível identificar o dispositivo da raiz `/`: {e}"))?;
    let mut cols = root.split_whitespace();
    let root_src = cols.next().unwrap_or_default();
    let root_fstype = cols.next().unwrap_or_default();
    if root_src.is_empty() {
        return Err("findmnt não informou o dispositivo da raiz `/`".into());
    }

    let disks = parse_lsblk(&json, root_src, root_fstype)?;
    if !disks.iter().any(|d| d.is_system) {
        tracing::warn!(
            "nenhum disco foi identificado como disco do sistema (raiz em `{root_src}`, {root_fstype})"
        );
    }
    Ok(disks)
}

/// `size`/`rm` vêm como número no lsblk novo e como string no antigo.
#[cfg(any(target_os = "linux", test))]
fn json_u64(v: &serde_json::Value) -> u64 {
    v.as_u64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
        .unwrap_or(0)
}

#[cfg(any(target_os = "linux", test))]
fn json_flag(v: &serde_json::Value) -> bool {
    v.as_bool().unwrap_or(false) || v.as_u64() == Some(1) || v.as_str() == Some("1")
}

/// Pontos de montagem de um nó do lsblk: `mountpoints` (array, >= 2.37) ou
/// `mountpoint` (string, versões antigas).
#[cfg(any(target_os = "linux", test))]
fn lsblk_mounts(node: &serde_json::Value) -> Vec<String> {
    let mut out: Vec<String> = node["mountpoints"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|m| m.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    if let Some(m) = node["mountpoint"].as_str() {
        out.push(m.to_string());
    }
    out
}

/// Primeiro ponto de montagem do nó ou de qualquer descendente (ex.: a
/// partição LUKS mostra o `/` do volume lógico que ela contém).
#[cfg(any(target_os = "linux", test))]
fn lsblk_first_mount(node: &serde_json::Value) -> Option<String> {
    if let Some(m) = lsblk_mounts(node).into_iter().next() {
        return Some(m);
    }
    node["children"]
        .as_array()?
        .iter()
        .find_map(lsblk_first_mount)
}

/// true se o nó, ou QUALQUER descendente (partição → LUKS → LVM → RAID),
/// hospeda o sistema em execução.
#[cfg(any(target_os = "linux", test))]
fn lsblk_hosts_system(node: &serde_json::Value, root_src: &str, root_is_zfs: bool) -> bool {
    if !root_src.is_empty() && node["path"].as_str() == Some(root_src) {
        return true;
    }
    // Raiz em ZFS: o pool não aparece como montagem no lsblk; por segurança,
    // todo disco membro de um pool é tratado como disco do sistema.
    if root_is_zfs && node["fstype"].as_str() == Some("zfs_member") {
        return true;
    }
    if lsblk_mounts(node)
        .iter()
        .any(|m| LINUX_SYSTEM_MOUNTS.contains(&m.as_str()))
    {
        return true;
    }
    node["children"].as_array().is_some_and(|kids| {
        kids.iter()
            .any(|k| lsblk_hosts_system(k, root_src, root_is_zfs))
    })
}

/// Converte a saída de `lsblk --json` em `DiskInfo`. Função pura (sem I/O)
/// para que a detecção do disco do sistema seja testável em qualquer SO.
#[cfg(any(target_os = "linux", test))]
fn parse_lsblk(json: &str, root_src: &str, root_fstype: &str) -> Result<Vec<DiskInfo>, String> {
    // btrfs reporta a origem como `/dev/sda2[/@]` — o sufixo é o subvolume
    let root_src = root_src.split('[').next().unwrap_or_default().trim();
    let root_is_zfs = root_fstype == "zfs";

    let v: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("lsblk: JSON inválido: {e}"))?;
    let devices = v["blockdevices"]
        .as_array()
        .ok_or_else(|| "lsblk: resposta sem `blockdevices`".to_string())?;

    let mut disks = Vec::new();
    for d in devices {
        if d["type"].as_str() != Some("disk") {
            continue;
        }
        let name = d["name"].as_str().unwrap_or_default().to_string();
        let path = d["path"].as_str().unwrap_or_default().to_string();
        // FAIL-CLOSED: entrada malformada nunca vira disco.
        if name.is_empty() || path.is_empty() {
            return Err(format!("lsblk: disco sem NAME/PATH: {d}"));
        }
        let partitions: Vec<PartitionInfo> = d["children"]
            .as_array()
            .map(|kids| {
                kids.iter()
                    .enumerate()
                    .map(|(i, p)| PartitionInfo {
                        index: i as u32 + 1,
                        path: p["path"].as_str().map(String::from),
                        size_bytes: json_u64(&p["size"]),
                        fs: p["fstype"].as_str().map(String::from),
                        label: p["label"].as_str().map(String::from),
                        mount_point: lsblk_first_mount(p),
                    })
                    .collect()
            })
            .unwrap_or_default();
        disks.push(DiskInfo {
            id: name,
            path,
            model: d["model"].as_str().unwrap_or("Desconhecido").trim().to_string(),
            size_bytes: json_u64(&d["size"]),
            bus: d["tran"].as_str().unwrap_or("?").to_string(),
            is_system: lsblk_hosts_system(d, root_src, root_is_zfs),
            is_removable: json_flag(&d["rm"]),
            partitions,
        });
    }

    // FAIL-CLOSED: a raiz está num dispositivo de bloco, mas nenhum disco foi
    // associado a ela — a detecção falhou, e devolver a lista assim liberaria
    // o disco do sistema como destino. (Raiz em overlay/tmpfs/rede, ou em
    // loop/ramdisk de sistema live, legitimamente não tem disco próprio.)
    let root_on_disk = root_src.starts_with("/dev/")
        && !root_src.starts_with("/dev/loop")
        && !root_src.starts_with("/dev/ram");
    if root_on_disk && !disks.iter().any(|d| d.is_system) {
        return Err(format!(
            "não foi possível associar a raiz `{root_src}` a nenhum disco — sem saber qual é o disco do sistema, a lista não é confiável"
        ));
    }
    Ok(disks)
}

// ─────────────────────────────── macOS ─────────────────────────────────

/// Visão mínima de uma entrada de `diskutil list -plist`, suficiente para
/// decidir quais discos hospedam o sistema (sem depender da crate `plist`,
/// para que a regra seja testável em qualquer SO).
#[cfg(any(target_os = "macos", test))]
struct MacDiskEntry {
    /// ex: `disk0` (físico) ou `disk3` (container APFS sintetizado)
    id: String,
    /// pontos de montagem das partições e dos volumes APFS da entrada
    mounts: Vec<String>,
    /// partições físicas que sustentam o container APFS (ex: `disk0s2`)
    physical_stores: Vec<String>,
}

/// `disk0s2` → `disk0` · `disk3s1s1` → `disk3` · `disk5` → `disk5`
#[cfg(any(target_os = "macos", test))]
fn mac_whole_disk(id: &str) -> String {
    match id.strip_prefix("disk") {
        Some(rest) => {
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            format!("disk{digits}")
        }
        None => id.to_string(),
    }
}

/// Conjunto de discos que hospedam o macOS em execução.
///
/// Com APFS, o volume `/` vive num container SINTETIZADO (ex: `disk3`) cujo
/// armazenamento físico é uma partição de outro disco (`disk0s2`). Marcar só
/// o `ParentWholeDisk` de `/` deixaria o SSD interno (`disk0`) desprotegido,
/// então propagamos: container do sistema → disco físico → demais containers
/// que vivem nesse mesmo disco físico (Recovery, iSCPreboot).
#[cfg(any(target_os = "macos", test))]
fn mac_system_disks(
    entries: &[MacDiskEntry],
    boot_parent: &str,
) -> std::collections::HashSet<String> {
    let mut system = std::collections::HashSet::new();
    if !boot_parent.is_empty() {
        system.insert(mac_whole_disk(boot_parent));
    }
    loop {
        let before = system.len();
        for e in entries {
            let hosts_os = e
                .mounts
                .iter()
                .any(|m| m == "/" || m.starts_with("/System/Volumes/"));
            let on_system_disk = e
                .physical_stores
                .iter()
                .any(|s| system.contains(&mac_whole_disk(s)));
            if hosts_os || on_system_disk || system.contains(&e.id) {
                system.insert(e.id.clone());
                for s in &e.physical_stores {
                    system.insert(mac_whole_disk(s));
                }
            }
        }
        if system.len() == before {
            break;
        }
    }
    system
}

/// Extrai `campo` de cada dicionário da lista `chave` (ex.: o `MountPoint`
/// de cada item de `APFSVolumes`).
#[cfg(target_os = "macos")]
fn plist_strings(dict: &plist::Dictionary, chave: &str, campo: &str) -> Vec<String> {
    dict.get(chave)
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| {
                    x.as_dictionary()?
                        .get(campo)?
                        .as_string()
                        .map(String::from)
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(target_os = "macos")]
async fn list_impl() -> Result<Vec<DiskInfo>, String> {
    let xml = run("diskutil", &["list", "-plist"]).await?;
    let root: plist::Value = plist::from_bytes(xml.as_bytes()).map_err(|e| e.to_string())?;

    // FAIL-CLOSED: sem identificar o volume de boot não há como proteger o
    // disco do sistema — preferimos erro a devolver uma lista sem a marcação.
    let boot_xml = run("diskutil", &["info", "-plist", "/"])
        .await
        .map_err(|e| format!("não foi possível identificar o volume de boot: {e}"))?;
    let boot_parent = plist::from_bytes::<plist::Value>(boot_xml.as_bytes())
        .ok()
        .and_then(|v| {
            v.as_dictionary()
                .and_then(|d| d.get("ParentWholeDisk"))
                .and_then(|s| s.as_string().map(String::from))
        })
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            "diskutil não informou o disco do volume de boot (ParentWholeDisk)".to_string()
        })?;

    let list = root
        .as_dictionary()
        .and_then(|d| d.get("AllDisksAndPartitions"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let entries: Vec<MacDiskEntry> = list
        .iter()
        .filter_map(|d| {
            let dict = d.as_dictionary()?;
            let id = dict.get("DeviceIdentifier")?.as_string()?.to_string();
            let mut mounts = plist_strings(dict, "Partitions", "MountPoint");
            mounts.extend(plist_strings(dict, "APFSVolumes", "MountPoint"));
            Some(MacDiskEntry {
                id,
                mounts,
                physical_stores: plist_strings(dict, "APFSPhysicalStores", "DeviceIdentifier"),
            })
        })
        .collect();
    let system = mac_system_disks(&entries, &boot_parent);

    let mut disks = Vec::new();
    for d in list {
        let dict = match d.as_dictionary() {
            Some(x) => x,
            None => continue,
        };
        let dev = dict
            .get("DeviceIdentifier")
            .and_then(|v| v.as_string())
            .unwrap_or_default()
            .to_string();
        if dev.is_empty() {
            continue;
        }
        let parts: Vec<PartitionInfo> = dict
            .get("Partitions")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
            .iter()
            .enumerate()
            .filter_map(|(i, p)| {
                let pd = p.as_dictionary()?;
                Some(PartitionInfo {
                    index: i as u32 + 1,
                    path: pd
                        .get("DeviceIdentifier")
                        .and_then(|v| v.as_string())
                        .map(|s| format!("/dev/{s}")),
                    size_bytes: pd.get("Size").and_then(|v| v.as_unsigned_integer()).unwrap_or(0),
                    fs: pd.get("Content").and_then(|v| v.as_string()).map(String::from),
                    label: pd.get("VolumeName").and_then(|v| v.as_string()).map(String::from),
                    mount_point: pd
                        .get("MountPoint")
                        .and_then(|v| v.as_string())
                        .map(String::from),
                })
            })
            .collect();

        disks.push(DiskInfo {
            is_system: system.contains(&dev),
            id: dev.clone(),
            // rdisk = raw device: bypass do buffer cache, essencial para clonagem rápida
            path: format!("/dev/r{dev}"),
            model: dict
                .get("MediaName")
                .and_then(|v| v.as_string())
                .unwrap_or("Apple Disk")
                .to_string(),
            size_bytes: dict.get("Size").and_then(|v| v.as_unsigned_integer()).unwrap_or(0),
            bus: dict
                .get("BusProtocol")
                .and_then(|v| v.as_string())
                .unwrap_or("?")
                .to_string(),
            is_removable: dict
                .get("Internal")
                .and_then(|v| v.as_boolean())
                .map(|internal| !internal)
                .unwrap_or(false),
            partitions: parts,
        });
    }
    Ok(disks)
}

// ─────────────────────────────── Testes ────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn sistema(disks: &[DiskInfo]) -> Vec<&str> {
        disks
            .iter()
            .filter(|d| d.is_system)
            .map(|d| d.id.as_str())
            .collect()
    }

    // ── Linux ────────────────────────────────────────────────────────────

    #[test]
    fn linux_layout_simples() {
        let json = r#"{"blockdevices":[
          {"name":"sda","path":"/dev/sda","size":500107862016,"model":"Samsung SSD ","type":"disk","rm":false,"mountpoints":[null],"tran":"sata","fstype":null,"label":null,
           "children":[
             {"name":"sda1","path":"/dev/sda1","size":536870912,"type":"part","rm":false,"mountpoints":["/boot/efi"],"fstype":"vfat","label":null},
             {"name":"sda2","path":"/dev/sda2","size":499569991680,"type":"part","rm":false,"mountpoints":["/"],"fstype":"ext4","label":null}]},
          {"name":"sdb","path":"/dev/sdb","size":15376318464,"model":"Cruzer Fit","type":"disk","rm":true,"mountpoints":[null],"tran":"usb","fstype":null,"label":null,
           "children":[
             {"name":"sdb1","path":"/dev/sdb1","size":15375269888,"type":"part","rm":true,"mountpoints":["/media/hugo/PEN"],"fstype":"exfat","label":"PEN"}]},
          {"name":"loop0","path":"/dev/loop0","size":4096,"type":"loop","rm":false,"mountpoints":["/snap/core/1"]}
        ]}"#;
        let disks = parse_lsblk(json, "/dev/sda2", "ext4").unwrap();
        assert_eq!(disks.len(), 2, "loop não é disco");
        assert_eq!(sistema(&disks), ["sda"]);
        assert_eq!(disks[0].model, "Samsung SSD");
        assert_eq!(disks[0].size_bytes, 500107862016);
        assert_eq!(disks[0].partitions.len(), 2);
        assert_eq!(disks[0].partitions[1].mount_point.as_deref(), Some("/"));
        assert!(disks[1].is_removable);
        assert!(!disks[1].is_system, "pendrive de dados montado não é sistema");
    }

    #[test]
    fn linux_raiz_em_luks_lvm_espalhado_em_dois_discos() {
        // `/` é um LV dentro de LUKS; o VG usa também o sdb, que não tem
        // /boot nem nenhuma montagem direta.
        let json = r#"{"blockdevices":[
          {"name":"nvme0n1","path":"/dev/nvme0n1","size":1000204886016,"type":"disk","rm":false,"mountpoints":[null],
           "children":[
             {"name":"nvme0n1p1","path":"/dev/nvme0n1p1","size":1073741824,"type":"part","mountpoints":[null],"fstype":"vfat"},
             {"name":"nvme0n1p2","path":"/dev/nvme0n1p2","size":999129088000,"type":"part","mountpoints":[null],"fstype":"crypto_LUKS",
              "children":[
                {"name":"cryptroot","path":"/dev/mapper/cryptroot","size":999112310784,"type":"crypt","mountpoints":[null],"fstype":"LVM2_member",
                 "children":[
                   {"name":"vg-root","path":"/dev/mapper/vg-root","size":1500000000000,"type":"lvm","mountpoints":["/"],"fstype":"ext4"}]}]}]},
          {"name":"sdb","path":"/dev/sdb","size":1000204886016,"type":"disk","rm":false,"mountpoints":[null],
           "children":[
             {"name":"sdb1","path":"/dev/sdb1","size":1000203837440,"type":"part","mountpoints":[null],"fstype":"LVM2_member",
              "children":[
                {"name":"vg-root","path":"/dev/mapper/vg-root","size":1500000000000,"type":"lvm","mountpoints":["/"],"fstype":"ext4"}]}]},
          {"name":"sdc","path":"/dev/sdc","size":64023257088,"type":"disk","rm":true,"mountpoints":[null]}
        ]}"#;
        let disks = parse_lsblk(json, "/dev/mapper/vg-root", "ext4").unwrap();
        assert_eq!(sistema(&disks), ["nvme0n1", "sdb"]);
        // a partição LUKS exibe a montagem do volume que contém
        assert_eq!(disks[0].partitions[1].mount_point.as_deref(), Some("/"));
    }

    #[test]
    fn linux_raiz_detectada_so_pelo_findmnt() {
        // lsblk sem nenhuma montagem visível + origem btrfs com subvolume
        let json = r#"{"blockdevices":[
          {"name":"sda","path":"/dev/sda","size":256060514304,"type":"disk","rm":false,
           "children":[{"name":"sda2","path":"/dev/sda2","size":255000000000,"type":"part","fstype":"btrfs"}]},
          {"name":"sdab","path":"/dev/sdab","size":256060514304,"type":"disk","rm":false}
        ]}"#;
        let disks = parse_lsblk(json, "/dev/sda2[/@]", "btrfs").unwrap();
        assert_eq!(sistema(&disks), ["sda"]);
    }

    #[test]
    fn linux_lsblk_antigo_com_strings_e_mountpoint_singular() {
        // util-linux < 2.37: tudo string, coluna MOUNTPOINT
        let json = r#"{"blockdevices":[
          {"name":"sda","path":"/dev/sda","size":"240057409536","model":"KINGSTON","type":"disk","rm":"0","mountpoint":null,"tran":"sata",
           "children":[
             {"name":"sda1","path":"/dev/sda1","size":"240055312384","type":"part","rm":"0","mountpoint":"/","fstype":"ext4"}]},
          {"name":"sdb","path":"/dev/sdb","size":"8004829184","model":"USB","type":"disk","rm":"1","mountpoint":null,"tran":"usb"}
        ]}"#;
        let disks = parse_lsblk(json, "/dev/sda1", "ext4").unwrap();
        assert_eq!(sistema(&disks), ["sda"]);
        assert_eq!(disks[0].size_bytes, 240057409536);
        assert_eq!(disks[0].partitions[0].size_bytes, 240055312384);
        assert!(!disks[0].is_removable);
        assert!(disks[1].is_removable);
    }

    #[test]
    fn linux_sistema_live_protege_o_pendrive_de_boot() {
        let json = r#"{"blockdevices":[
          {"name":"sda","path":"/dev/sda","size":500107862016,"type":"disk","rm":false,
           "children":[{"name":"sda1","path":"/dev/sda1","size":500000000000,"type":"part","mountpoints":[null],"fstype":"ntfs"}]},
          {"name":"sdb","path":"/dev/sdb","size":8004829184,"type":"disk","rm":true,
           "children":[{"name":"sdb1","path":"/dev/sdb1","size":8000000000,"type":"part","mountpoints":["/cdrom"],"fstype":"iso9660"}]}
        ]}"#;
        let disks = parse_lsblk(json, "/cow", "overlay").unwrap();
        assert_eq!(sistema(&disks), ["sdb"]);
    }

    #[test]
    fn linux_swap_e_home_em_outro_disco_tambem_sao_sistema() {
        let json = r#"{"blockdevices":[
          {"name":"sda","path":"/dev/sda","size":1,"type":"disk",
           "children":[{"name":"sda1","path":"/dev/sda1","size":1,"type":"part","mountpoints":["/"]}]},
          {"name":"sdb","path":"/dev/sdb","size":1,"type":"disk",
           "children":[{"name":"sdb1","path":"/dev/sdb1","size":1,"type":"part","mountpoints":["[SWAP]"]}]},
          {"name":"sdc","path":"/dev/sdc","size":1,"type":"disk",
           "children":[{"name":"sdc1","path":"/dev/sdc1","size":1,"type":"part","mountpoints":["/home"]}]},
          {"name":"sdd","path":"/dev/sdd","size":1,"type":"disk",
           "children":[{"name":"sdd1","path":"/dev/sdd1","size":1,"type":"part","mountpoints":["/mnt/dados"]}]}
        ]}"#;
        let disks = parse_lsblk(json, "/dev/sda1", "ext4").unwrap();
        assert_eq!(sistema(&disks), ["sda", "sdb", "sdc"]);
    }

    #[test]
    fn linux_raiz_em_zfs_protege_os_membros_do_pool() {
        let json = r#"{"blockdevices":[
          {"name":"sda","path":"/dev/sda","size":1,"type":"disk",
           "children":[{"name":"sda1","path":"/dev/sda1","size":1,"type":"part","fstype":"zfs_member"}]},
          {"name":"sdb","path":"/dev/sdb","size":1,"type":"disk",
           "children":[{"name":"sdb1","path":"/dev/sdb1","size":1,"type":"part","fstype":"ext4"}]}
        ]}"#;
        let disks = parse_lsblk(json, "rpool/ROOT/ubuntu", "zfs").unwrap();
        assert_eq!(sistema(&disks), ["sda"]);
    }

    #[test]
    fn linux_resposta_malformada_e_erro_e_nao_lista_vazia() {
        assert!(parse_lsblk("", "/dev/sda1", "ext4").is_err());
        assert!(parse_lsblk("{}", "/dev/sda1", "ext4").is_err());
        assert!(parse_lsblk(r#"{"blockdevices":[{"type":"disk"}]}"#, "/dev/sda1", "ext4").is_err());
    }

    #[test]
    fn linux_raiz_em_dispositivo_sem_disco_associado_e_erro() {
        // Só um pendrive aparece; a raiz está num dispositivo que o lsblk não ligou a disco algum.
        let json = r#"{"blockdevices":[
            {"name":"sdb","path":"/dev/sdb","size":16000000000,"type":"disk","rm":true,"mountpoints":[null]}
        ]}"#;
        let err = parse_lsblk(json, "/dev/mapper/vg-root", "ext4").unwrap_err();
        assert!(err.contains("/dev/mapper/vg-root"), "{err}");
        // Raiz sem disco próprio (live em RAM, loop, rede): a lista vale.
        for (src, fs) in [("overlay", "overlay"), ("tmpfs", "tmpfs"), ("/dev/loop0", "squashfs"), ("10.0.0.1:/srv/root", "nfs")] {
            let disks = parse_lsblk(json, src, fs).unwrap();
            assert!(sistema(&disks).is_empty(), "{src}");
        }
    }

    // ── macOS ────────────────────────────────────────────────────────────

    fn entrada(id: &str, mounts: &[&str], stores: &[&str]) -> MacDiskEntry {
        MacDiskEntry {
            id: id.into(),
            mounts: mounts.iter().map(|s| s.to_string()).collect(),
            physical_stores: stores.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn mac_disco_inteiro_a_partir_do_identificador() {
        assert_eq!(mac_whole_disk("disk0s2"), "disk0");
        assert_eq!(mac_whole_disk("disk3s1s1"), "disk3");
        assert_eq!(mac_whole_disk("disk12"), "disk12");
        assert_eq!(mac_whole_disk("disk12s3"), "disk12");
    }

    #[test]
    fn mac_apfs_marca_o_ssd_fisico_e_todos_os_containers_dele() {
        // Layout real de um Mac Apple Silicon: `/` em disk3 (sintetizado),
        // cujo armazenamento físico é disk0s2.
        let entradas = [
            entrada("disk0", &[], &[]),
            entrada(
                "disk1",
                &["/System/Volumes/iSCPreboot", "/System/Volumes/xarts"],
                &["disk0s1"],
            ),
            entrada("disk2", &[], &["disk0s3"]),
            entrada(
                "disk3",
                &["/", "/System/Volumes/Preboot", "/System/Volumes/Data"],
                &["disk0s2"],
            ),
            entrada("disk4", &["/Volumes/PENDRIVE"], &[]),
            entrada("disk6", &["/Volumes/Backup"], &["disk5s2"]),
            entrada("disk5", &[], &[]),
        ];
        let sys = mac_system_disks(&entradas, "disk3");
        for id in ["disk0", "disk1", "disk2", "disk3"] {
            assert!(sys.contains(id), "{id} deveria ser disco do sistema");
        }
        for id in ["disk4", "disk5", "disk6"] {
            assert!(!sys.contains(id), "{id} NÃO deveria ser disco do sistema");
        }
    }

    #[test]
    fn mac_sem_parent_ainda_detecta_pela_montagem_da_raiz() {
        let entradas = [
            entrada("disk0", &[], &[]),
            entrada("disk1", &["/"], &["disk0s2"]),
            entrada("disk2", &["/Volumes/USB"], &[]),
        ];
        let sys = mac_system_disks(&entradas, "");
        assert!(sys.contains("disk0") && sys.contains("disk1"));
        assert!(!sys.contains("disk2"));
    }

    #[test]
    fn mac_hfs_antigo_sem_apfs() {
        let entradas = [
            entrada("disk0", &["/"], &[]),
            entrada("disk1", &["/Volumes/USB"], &[]),
        ];
        let sys = mac_system_disks(&entradas, "disk0");
        assert!(sys.contains("disk0"));
        assert!(!sys.contains("disk1"));
    }
}
