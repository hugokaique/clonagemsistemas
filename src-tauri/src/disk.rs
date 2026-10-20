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
    let out = tokio::process::Command::new(cmd)
        .args(args)
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
ConvertTo-Json -Depth 6 -Compress -InputObject @(,$disks)
"#;
    let json = run("powershell", &["-NoProfile", "-NonInteractive", "-Command", ps]).await?;
    let v: serde_json::Value = serde_json::from_str(json.trim()).map_err(|e| e.to_string())?;
    let arr = match v {
        serde_json::Value::Array(a) => a,
        other => vec![other], // ConvertTo-Json devolve objeto único quando há 1 disco
    };

    let mut disks = Vec::new();
    for d in arr {
        let number = d["Number"].as_u64().unwrap_or(0);
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
#[cfg(target_os = "linux")]
async fn list_impl() -> Result<Vec<DiskInfo>, String> {
    let json = run(
        "lsblk",
        &[
            "--json", "--bytes", "--output",
            "NAME,PATH,SIZE,MODEL,TYPE,RM,MOUNTPOINTS,TRAN,FSTYPE,LABEL",
        ],
    )
    .await?;
    // Dispositivo que hospeda a raiz "/" — pode ser partição, LV ou mapper
    let root_src = run("findmnt", &["-n", "-o", "SOURCE", "/"])
        .await
        .unwrap_or_default()
        .trim()
        .to_string();

    let v: serde_json::Value = serde_json::from_str(&json).map_err(|e| e.to_string())?;
    let mut disks = Vec::new();
    for d in v["blockdevices"].as_array().cloned().unwrap_or_default() {
        if d["type"].as_str() != Some("disk") {
            continue;
        }
        let name = d["name"].as_str().unwrap_or_default().to_string();
        let path = d["path"].as_str().unwrap_or_default().to_string();
        let mut is_system = root_src.contains(&name);
        let mut parts = Vec::new();

        for p in d["children"].as_array().cloned().unwrap_or_default() {
            let ppath = p["path"].as_str().unwrap_or_default().to_string();
            if ppath == root_src {
                is_system = true;
            }
            let mounts: Vec<String> = p["mountpoints"]
                .as_array()
                .map(|a| a.iter().filter_map(|m| m.as_str().map(String::from)).collect())
                .unwrap_or_default();
            if mounts.iter().any(|m| m == "/" || m == "/boot" || m == "/boot/efi") {
                is_system = true;
            }
            parts.push(PartitionInfo {
                index: parts.len() as u32 + 1,
                path: Some(ppath),
                size_bytes: p["size"].as_u64().unwrap_or(0),
                fs: p["fstype"].as_str().map(String::from),
                label: p["label"].as_str().map(String::from),
                mount_point: mounts.into_iter().next(),
            });
        }
        disks.push(DiskInfo {
            id: name,
            path,
            model: d["model"].as_str().unwrap_or("Desconhecido").trim().to_string(),
            size_bytes: d["size"].as_u64().unwrap_or(0),
            bus: d["tran"].as_str().unwrap_or("?").to_string(),
            is_system,
            is_removable: d["rm"].as_u64() == Some(1) || d["rm"].as_bool().unwrap_or(false),
            partitions: parts,
        });
    }
    Ok(disks)
}

// ─────────────────────────────── macOS ─────────────────────────────────
#[cfg(target_os = "macos")]
async fn list_impl() -> Result<Vec<DiskInfo>, String> {
    let xml = run("diskutil", &["list", "-plist"]).await?;
    let root: plist::Value = plist::from_bytes(xml.as_bytes()).map_err(|e| e.to_string())?;

    // Disco que hospeda o volume de boot — nunca pode ser destino
    let boot_xml = run("diskutil", &["info", "-plist", "/"]).await.unwrap_or_default();
    let boot_disk = plist::from_bytes::<plist::Value>(boot_xml.as_bytes())
        .ok()
        .and_then(|v| {
            v.as_dictionary()
                .and_then(|d| d.get("ParentWholeDisk"))
                .and_then(|s| s.as_string().map(String::from))
        })
        .unwrap_or_default();

    let mut disks = Vec::new();
    let list = root
        .as_dictionary()
        .and_then(|d| d.get("AllDisksAndPartitions"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

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
            is_system: dev == boot_disk || parts.iter().any(|p| p.mount_point.as_deref() == Some("/")),
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
