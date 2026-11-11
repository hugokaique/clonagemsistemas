//! Windows: snapshots VSS (Volume Shadow Copy) para cópia a quente.
//!
//! Implementação via CIM (`Win32_ShadowCopy`) — a mesma API que o wbadmin usa
//! por baixo. Escolha deliberada sobre o COM `IVssBackupComponents` direto:
//! menos código unsafe, mesma semântica, e o provider padrão do Windows
//! (VSS_SYSTEM_PROVIDER) produz snapshots `ClientAccessible` que expõem um
//! dispositivo legível `\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopyN`.
//!
//! O snapshot é apagado via RAII (`ShadowGuard`) ao fim da operação — mesmo
//! em caso de erro, nunca deixamos shadows órfãos acumulando no sistema.

#![cfg(target_os = "windows")]

pub struct ShadowCopy {
    pub id: String,
    /// ex: `\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy7`
    pub device_object: String,
}

fn run_ps(script: &str) -> Result<String, String> {
    use std::os::windows::process::CommandExt;
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .creation_flags(0x08000000) // CREATE_NO_WINDOW — não abrir console
        .output()
        .map_err(|e| format!("falha ao iniciar powershell: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "powershell VSS falhou: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Exatamente `X:\`. O valor é interpolado num script PowerShell executado
/// como Administrador, então nada além disso pode passar.
fn is_volume_root(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'\\'
}

/// GUID de shadow copy como o WMI devolve: `{XXXXXXXX-XXXX-...}`.
fn is_shadow_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_hexdigit() || matches!(c, '-' | '{' | '}'))
}

/// Cria um shadow copy `ClientAccessible` do volume (ex: `C:\`).
/// Requer Administrador (garantido pelo manifesto UAC do app).
pub fn create(volume_root: &str) -> Result<ShadowCopy, String> {
    if !is_volume_root(volume_root) {
        return Err(format!(
            "BLOQUEADO: `{volume_root}` não é a raiz de um volume (esperado `X:\\`)"
        ));
    }
    let ps = format!(
        r#"
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [Text.Encoding]::UTF8
$r = Invoke-CimMethod -ClassName Win32_ShadowCopy -MethodName Create `
     -Arguments @{{ Volume = '{volume_root}'; Context = 'ClientAccessible' }}
if ($r.ReturnValue -ne 0) {{ throw "VSS Create falhou (ReturnValue=$($r.ReturnValue))" }}
$s = Get-CimInstance Win32_ShadowCopy -Filter "ID='$($r.ShadowID)'"
if (-not $s) {{ throw "shadow copy criado mas não localizado: $($r.ShadowID)" }}
Write-Output "$($r.ShadowID)|$($s.DeviceObject)"
"#
    );
    let stdout = run_ps(&ps)?;
    let line = stdout
        .lines()
        .rev()
        .find(|l| l.contains('|'))
        .ok_or_else(|| format!("saída inesperada do VSS: {stdout}"))?;
    let (id, dev) = line
        .split_once('|')
        .ok_or_else(|| format!("saída VSS malformada: {line}"))?;
    let id = id.trim();
    if !is_shadow_id(id) {
        return Err(format!("saída VSS malformada (ID inesperado): {line}"));
    }
    Ok(ShadowCopy {
        id: id.to_string(),
        device_object: dev.trim().to_string(),
    })
}

/// Apaga um shadow copy pelo ID (GUID com chaves, ex: `{XXXXXXXX-...}`).
pub fn delete(id: &str) -> Result<(), String> {
    if !is_shadow_id(id) {
        return Err(format!("ID de shadow copy inválido: `{id}`"));
    }
    let ps = format!(
        "Get-CimInstance Win32_ShadowCopy -Filter \"ID='{id}'\" | Remove-CimInstance"
    );
    run_ps(&ps).map(|_| ())
}

/// Guard RAII — apaga o snapshot ao sair de escopo, mesmo em caso de erro.
pub struct ShadowGuard(pub ShadowCopy);

impl Drop for ShadowGuard {
    fn drop(&mut self) {
        if let Err(e) = delete(&self.0.id) {
            tracing::warn!("falha ao apagar shadow copy {}: {e}", self.0.id);
        } else {
            tracing::info!("shadow copy {} apagado", self.0.id);
        }
    }
}
