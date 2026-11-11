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
//!
//! Os valores interpolados nos scripts PowerShell são validados por formato
//! estrito antes (letra de unidade / GUID): o app roda elevado e um caminho
//! com aspas na requisição viraria execução de comando como Administrador.

/// Raiz de volume aceita pelo `Win32_ShadowCopy.Create`: `C:\` ou
/// `\\?\Volume{guid}\`. Devolve `None` para qualquer outra coisa.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn volume_root(volume_path: &str) -> Option<String> {
    let p = volume_path.trim_end_matches('\\');
    let b = p.as_bytes();
    // \\.\C:
    if b.len() == 6 && p.starts_with("\\\\.\\") && b[4].is_ascii_alphabetic() && b[5] == b':' {
        return Some(format!("{}:\\", (b[4] as char).to_ascii_uppercase()));
    }
    // \\?\Volume{xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx}
    let guid = p
        .strip_prefix("\\\\?\\Volume")
        .or_else(|| p.strip_prefix("\\\\?\\volume"))?;
    is_braced_guid(guid).then(|| format!("\\\\?\\Volume{guid}\\"))
}

/// `{xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx}` — só hexadecimais e hifens.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn is_braced_guid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 38
        && b[0] == b'{'
        && b[37] == b'}'
        && b[1..37].iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

#[cfg(windows)]
pub struct ShadowCopy {
    pub id: String,
    /// ex: `\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy7`
    pub device_object: String,
}

#[cfg(windows)]
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

/// Cria um shadow copy `ClientAccessible` do volume (`\\.\C:` ou
/// `\\?\Volume{guid}`). Requer Administrador (garantido pelo manifesto UAC).
#[cfg(windows)]
pub fn create(volume_path: &str) -> Result<ShadowCopy, String> {
    let root = volume_root(volume_path).ok_or_else(|| {
        format!("Live Copy (VSS) exige um volume com letra ou GUID — `{volume_path}` não é suportado")
    })?;
    let ps = format!(
        r#"
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [Text.Encoding]::UTF8
$r = Invoke-CimMethod -ClassName Win32_ShadowCopy -MethodName Create `
     -Arguments @{{ Volume = '{root}'; Context = 'ClientAccessible' }}
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
    let (id, dev) = (id.trim(), dev.trim());
    if !is_braced_guid(id) || !dev.starts_with("\\\\?\\GLOBALROOT\\Device\\") {
        return Err(format!("saída VSS inesperada: {line}"));
    }
    Ok(ShadowCopy { id: id.to_string(), device_object: dev.to_string() })
}

/// Apaga um shadow copy pelo ID (GUID com chaves, ex: `{XXXXXXXX-...}`).
#[cfg(windows)]
pub fn delete(id: &str) -> Result<(), String> {
    if !is_braced_guid(id) {
        return Err(format!("ID de shadow copy inválido: {id}"));
    }
    let ps = format!(
        "Get-CimInstance Win32_ShadowCopy -Filter \"ID='{id}'\" | Remove-CimInstance"
    );
    run_ps(&ps).map(|_| ())
}

/// Guard RAII — apaga o snapshot ao sair de escopo, mesmo em caso de erro.
#[cfg(windows)]
pub struct ShadowGuard(pub ShadowCopy);

#[cfg(windows)]
impl Drop for ShadowGuard {
    fn drop(&mut self) {
        if let Err(e) = delete(&self.0.id) {
            tracing::warn!("falha ao apagar shadow copy {}: {e}", self.0.id);
        } else {
            tracing::info!("shadow copy {} apagado", self.0.id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raiz_de_volume_valida() {
        assert_eq!(volume_root(r"\\.\C:").as_deref(), Some(r"C:\"));
        assert_eq!(volume_root(r"\\.\d:").as_deref(), Some(r"D:\"));
        assert_eq!(
            volume_root(r"\\?\Volume{35b744df-0d27-4f19-99c7-702d16a7cf0e}").as_deref(),
            Some(r"\\?\Volume{35b744df-0d27-4f19-99c7-702d16a7cf0e}\")
        );
        assert_eq!(
            volume_root(r"\\?\Volume{35b744df-0d27-4f19-99c7-702d16a7cf0e}\").as_deref(),
            Some(r"\\?\Volume{35b744df-0d27-4f19-99c7-702d16a7cf0e}\")
        );
    }

    #[test]
    fn injecao_de_powershell_e_recusada() {
        for bad in [
            r"\\.\C:'; Remove-Item C:\ -Recurse; '",
            r"\\.\C:\Windows",
            r"\\.\PhysicalDrive0",
            r"C:",
            r"\\?\Volume{35b744df-0d27-4f19-99c7-702d16a7cf0e}'; calc; '",
            r"\\?\Volume{zzzzzzzz-0d27-4f19-99c7-702d16a7cf0e}",
            r"\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy7",
            "",
        ] {
            assert_eq!(volume_root(bad), None, "{bad}");
        }
        assert!(is_braced_guid("{35B744DF-0D27-4F19-99C7-702D16A7CF0E}"));
        assert!(!is_braced_guid("{35B744DF-0D27-4F19-99C7-702D16A7CF0E}' -or '1'='1"));
        assert!(!is_braced_guid("35B744DF-0D27-4F19-99C7-702D16A7CF0E"));
    }
}
