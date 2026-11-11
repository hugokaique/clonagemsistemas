//! Travas de segurança do destino de escrita.
//!
//! PRINCÍPIO: fail-closed. O destino só é aceito se for EXATAMENTE um dos
//! discos inteiros devolvidos pela enumeração (`disk::list_disks`). Qualquer
//! outra coisa — enumeração vazia, caminho desconhecido, apelido do mesmo
//! disco (`\\.\physicaldrive0`, `\\?\PhysicalDrive0`, `/dev/disk0` vs
//! `/dev/rdisk0`, `/dev/disk/by-id/...`), partição ou volume (`\\.\C:`,
//! `/dev/sda1`) — é recusada antes de qualquer byte ser escrito.
//!
//! Estas funções não fazem I/O de disco: recebem a lista já enumerada, o que
//! as torna testáveis em qualquer SO.

use crate::disk::DiskInfo;

pub const BLOCKED_SYSTEM: &str = "BLOQUEADO: o destino é o disco do sistema em execução";

/// Forma canônica (apenas léxica) de um caminho de dispositivo, para que
/// apelidos do mesmo disco comparem como iguais.
pub fn normalize_device_path(path: &str) -> String {
    let p = path.trim();

    // Windows: `\\.\X`, `\\?\X` e `//./X` são o mesmo namespace de
    // dispositivos, e nomes de dispositivo não diferenciam maiúsculas.
    let win = p.replace('/', "\\");
    if let Some(rest) = win
        .strip_prefix("\\\\.\\")
        .or_else(|| win.strip_prefix("\\\\?\\"))
    {
        return format!("\\\\.\\{}", rest.trim_end_matches('\\').to_lowercase());
    }

    // macOS: /dev/rdiskN (raw) e /dev/diskN (bufferizado) são o mesmo disco.
    if let Some(rest) = p.strip_prefix("/dev/rdisk") {
        if rest.starts_with(|c: char| c.is_ascii_digit()) {
            return format!("/dev/disk{rest}");
        }
    }

    p.to_string()
}

/// Resolve symlinks (`/dev/disk/by-id/...` → `/dev/sda`). Se o caminho não
/// existir, devolve o texto original — que então não baterá com disco algum.
fn resolve_links(path: &str) -> String {
    #[cfg(unix)]
    if let Ok(real) = std::fs::canonicalize(path) {
        return real.to_string_lossy().into_owned();
    }
    path.to_string()
}

/// Localiza, na enumeração, o disco inteiro ao qual `path` se refere.
pub fn find_disk<'a>(disks: &'a [DiskInfo], path: &str) -> Option<&'a DiskInfo> {
    let wanted = normalize_device_path(&resolve_links(path));
    disks
        .iter()
        .find(|d| normalize_device_path(&d.path) == wanted)
}

fn gib(bytes: u64) -> f64 {
    bytes as f64 / 2f64.powi(30)
}

/// Valida um destino de escrita. `required_bytes` = espaço mínimo exigido
/// (0 quando o tamanho da origem não é conhecido).
pub fn check_write_target<'a>(
    disks: &'a [DiskInfo],
    target: &str,
    required_bytes: u64,
) -> Result<&'a DiskInfo, String> {
    if disks.is_empty() {
        return Err(
            "BLOQUEADO: nenhum disco foi enumerado — sem a lista não é possível validar o destino"
                .into(),
        );
    }
    let dst = find_disk(disks, target).ok_or_else(|| {
        format!("BLOQUEADO: o destino `{target}` não é um disco inteiro reconhecido pela enumeração")
    })?;
    if dst.is_system {
        return Err(BLOCKED_SYSTEM.into());
    }
    if dst.size_bytes == 0 {
        return Err(format!(
            "BLOQUEADO: o tamanho do destino `{target}` é desconhecido"
        ));
    }
    if dst.size_bytes < required_bytes {
        return Err(format!(
            "destino ({:.1} GiB) menor que a origem ({:.1} GiB)",
            gib(dst.size_bytes),
            gib(required_bytes)
        ));
    }
    Ok(dst)
}

/// Valida uma clonagem disco→disco: destino permitido, origem conhecida,
/// origem ≠ destino (mesmo sob apelidos diferentes) e destino grande o bastante.
///
/// Devolve `(origem, destino)` como constam na enumeração, para que quem
/// chama possa abrir os dispositivos pelo caminho canônico.
pub fn check_clone<'a>(
    disks: &'a [DiskInfo],
    source: &str,
    target: &str,
    source_size: u64,
) -> Result<(&'a DiskInfo, &'a DiskInfo), String> {
    let dst = check_write_target(disks, target, source_size)?;
    let src = find_disk(disks, source).ok_or_else(|| {
        format!("BLOQUEADO: a origem `{source}` não é um disco reconhecido pela enumeração")
    })?;
    if src.id == dst.id {
        return Err("origem e destino são o mesmo dispositivo".into());
    }
    // Não confia só no tamanho informado pelo cliente: usa o da enumeração.
    if dst.size_bytes < src.size_bytes {
        return Err(format!(
            "destino ({:.1} GiB) menor que a origem ({:.1} GiB)",
            gib(dst.size_bytes),
            gib(src.size_bytes)
        ));
    }
    Ok((src, dst))
}

/// Caminho canônico (o da enumeração) do disco inteiro ou da partição a que
/// `path` se refere — as únicas origens válidas para uma imagem quando quem
/// lê é o helper elevado. Qualquer outro caminho (`/etc/shadow`, um arquivo
/// do usuário, um dispositivo fora da lista) devolve `None`.
pub fn find_source_device(disks: &[DiskInfo], path: &str) -> Option<String> {
    let wanted = normalize_device_path(&resolve_links(path));
    for d in disks {
        if normalize_device_path(&d.path) == wanted {
            return Some(d.path.clone());
        }
        for part in d.partitions.iter().filter_map(|p| p.path.as_ref()) {
            if normalize_device_path(part) == wanted {
                return Some(part.clone());
            }
        }
    }
    None
}

/// Windows: converte a origem de volume `\\.\X:` na raiz `X:\`. Aceita só
/// essa forma exata — o resultado é interpolado no script do VSS, que roda
/// como Administrador.
pub fn windows_volume_root(source: &str) -> Result<String, String> {
    let norm = normalize_device_path(source);
    let letter = norm.strip_prefix("\\\\.\\").unwrap_or_default().as_bytes();
    if letter.len() == 2 && letter[0].is_ascii_alphabetic() && letter[1] == b':' {
        Ok(format!("{}:\\", (letter[0] as char).to_ascii_uppercase()))
    } else {
        Err(format!(
            "BLOQUEADO: `{source}` não é um volume no formato `\\\\.\\X:`"
        ))
    }
}

/// Valida a origem de uma imagem e devolve o caminho a abrir.
///
/// - Smart Copy no Windows lê um VOLUME (`\\.\C:`), que tem de ser a letra de
///   uma partição enumerada.
/// - Nos demais casos a origem é um disco inteiro ou uma partição da
///   enumeração, aberta pelo caminho canônico.
pub fn check_image_source(
    disks: &[DiskInfo],
    source: &str,
    smart: bool,
    windows_host: bool,
) -> Result<String, String> {
    let unknown = || {
        format!("BLOQUEADO: a origem `{source}` não é um disco, partição ou volume reconhecido pela enumeração")
    };
    if windows_host && smart {
        let root = windows_volume_root(source)?;
        let known = disks
            .iter()
            .flat_map(|d| d.partitions.iter())
            .filter_map(|p| p.mount_point.as_deref())
            .any(|m| m.eq_ignore_ascii_case(&root));
        if !known {
            return Err(unknown());
        }
        return Ok(format!("\\\\.\\{}", &root[..2]));
    }
    find_source_device(disks, source).ok_or_else(unknown)
}

pub(crate) fn output_refused(output: &str) -> String {
    format!(
        "BLOQUEADO: a saída `{output}` é um dispositivo ou diretório, não um arquivo — a imagem sobrescreveria o disco"
    )
}

/// Valida o caminho de saída de uma imagem: tem de ser um arquivo comum,
/// nunca um dispositivo (o campo é texto livre na interface).
pub fn check_image_output(output: &str) -> Result<(), String> {
    let out = output.trim();
    if out.is_empty() {
        return Err("caminho de saída da imagem vazio".into());
    }
    let norm = normalize_device_path(&resolve_links(out));

    if let Some(rest) = norm.strip_prefix("\\\\.\\") {
        // Nesse namespace só `\\?\C:\pasta\arquivo` e `\\?\UNC\servidor\...`
        // são arquivos; o resto (`PhysicalDrive0`, `C:`, `Volume{...}`) é dispositivo.
        let b = rest.as_bytes();
        let drive_file =
            b.len() > 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'\\';
        let unc_file = rest.starts_with("unc\\");
        if !(drive_file || unc_file) {
            return Err(output_refused(output));
        }
    }
    if norm.starts_with("/dev/") {
        return Err(output_refused(output));
    }
    // Existe e não é arquivo comum (dispositivo de bloco, diretório, symlink
    // para dispositivo — `metadata` segue symlinks).
    if let Ok(meta) = std::fs::metadata(out) {
        if !meta.is_file() {
            return Err(output_refused(output));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disk(id: &str, path: &str, size_gib: u64, is_system: bool) -> DiskInfo {
        DiskInfo {
            id: id.into(),
            path: path.into(),
            model: "Teste".into(),
            size_bytes: size_gib << 30,
            bus: "?".into(),
            is_system,
            is_removable: false,
            partitions: Vec::new(),
        }
    }

    fn windows() -> Vec<DiskInfo> {
        vec![
            disk("PhysicalDrive0", "\\\\.\\PhysicalDrive0", 1000, true),
            disk("PhysicalDrive1", "\\\\.\\PhysicalDrive1", 500, false),
            disk("PhysicalDrive2", "\\\\.\\PhysicalDrive2", 16, false),
        ]
    }

    fn macos() -> Vec<DiskInfo> {
        vec![
            disk("disk0", "/dev/rdisk0", 250, true),
            disk("disk4", "/dev/rdisk4", 64, false),
        ]
    }

    #[test]
    fn raiz_de_volume_so_aceita_a_forma_exata() {
        assert_eq!(windows_volume_root("\\\\.\\C:").unwrap(), "C:\\");
        assert_eq!(windows_volume_root("//./d:").unwrap(), "D:\\");
        assert_eq!(windows_volume_root("\\\\?\\e:\\").unwrap(), "E:\\");
        for ruim in [
            "\\\\.\\C:'; Remove-Item -Recurse C:\\ #",
            "\\\\.\\C:\\Windows",
            "\\\\.\\PhysicalDrive0",
            "C:",
            "C:\\",
            "\\\\.\\CC:",
            "\\\\.\\1:",
            "",
        ] {
            assert!(windows_volume_root(ruim).is_err(), "aceitou `{ruim}`");
        }
    }

    #[test]
    fn origem_de_imagem_tem_de_ser_conhecida() {
        let mut d = windows();
        d[1].partitions.push(crate::disk::PartitionInfo {
            index: 1,
            path: Some("E:".into()),
            size_bytes: 1 << 30,
            fs: Some("NTFS".into()),
            label: None,
            mount_point: Some("E:\\".into()),
        });

        // smart no Windows: volume com letra de uma partição enumerada
        assert_eq!(check_image_source(&d, "\\\\.\\e:", true, true).unwrap(), "\\\\.\\E:");
        assert!(check_image_source(&d, "\\\\.\\Z:", true, true).is_err(), "letra desconhecida");
        assert!(check_image_source(&d, "\\\\.\\PhysicalDrive1", true, true).is_err());
        // raw: disco inteiro da enumeração, pelo caminho canônico
        assert_eq!(
            check_image_source(&d, "\\\\?\\physicaldrive1", false, true).unwrap(),
            "\\\\.\\PhysicalDrive1"
        );
        for ruim in ["C:\\Users\\x\\segredo.txt", "/etc/shadow", "\\\\.\\PhysicalDrive9", ""] {
            assert!(check_image_source(&d, ruim, false, true).is_err(), "aceitou `{ruim}`");
            assert!(check_image_source(&d, ruim, false, false).is_err(), "aceitou `{ruim}`");
        }
    }

    #[test]
    fn lista_vazia_bloqueia() {
        let err = check_write_target(&[], "\\\\.\\PhysicalDrive1", 0).unwrap_err();
        assert!(err.starts_with("BLOQUEADO"), "{err}");
    }

    #[test]
    fn destino_fora_da_lista_bloqueia() {
        let d = windows();
        for alvo in [
            "\\\\.\\PhysicalDrive9",
            "\\\\.\\C:",
            "\\\\.\\Harddisk0Partition1",
            "/dev/disk/by-id/nao-existe",
            "C:\\qualquer.bin",
            "",
        ] {
            let err = check_write_target(&d, alvo, 0).unwrap_err();
            assert!(err.starts_with("BLOQUEADO"), "{alvo}: {err}");
        }
    }

    #[test]
    fn disco_do_sistema_bloqueia_sob_qualquer_apelido() {
        let d = windows();
        for alvo in [
            "\\\\.\\PhysicalDrive0",
            "\\\\.\\physicaldrive0",
            "\\\\.\\PHYSICALDRIVE0",
            "\\\\?\\PhysicalDrive0",
            "//./PhysicalDrive0",
            "\\\\.\\PhysicalDrive0\\",
            "  \\\\.\\PhysicalDrive0  ",
        ] {
            assert_eq!(
                check_write_target(&d, alvo, 0).unwrap_err(),
                BLOCKED_SYSTEM,
                "{alvo}"
            );
        }
    }

    #[test]
    fn macos_disk_e_rdisk_sao_o_mesmo_disco() {
        let d = macos();
        assert_eq!(
            check_write_target(&d, "/dev/disk0", 0).unwrap_err(),
            BLOCKED_SYSTEM
        );
        assert_eq!(
            check_write_target(&d, "/dev/rdisk0", 0).unwrap_err(),
            BLOCKED_SYSTEM
        );
        // partição do disco do sistema não é disco inteiro → fora da lista
        assert!(check_write_target(&d, "/dev/disk0s2", 0).is_err());
        assert!(check_write_target(&d, "/dev/disk4", 0).is_ok());
    }

    #[test]
    fn destino_menor_que_a_origem_e_recusado() {
        let d = windows();
        assert!(check_write_target(&d, "\\\\.\\PhysicalDrive2", 32 << 30).is_err());
        assert!(check_write_target(&d, "\\\\.\\PhysicalDrive2", 16 << 30).is_ok());
    }

    #[test]
    fn destino_de_tamanho_desconhecido_e_recusado() {
        let d = vec![disk("PhysicalDrive1", "\\\\.\\PhysicalDrive1", 0, false)];
        assert!(check_write_target(&d, "\\\\.\\PhysicalDrive1", 0).is_err());
    }

    #[test]
    fn clone_valido_passa() {
        let d = windows();
        assert!(check_clone(&d, "\\\\.\\PhysicalDrive2", "\\\\.\\PhysicalDrive1", 16 << 30).is_ok());
        // clonar o disco do sistema (como ORIGEM) para outro disco é permitido
        let d = vec![
            disk("PhysicalDrive0", "\\\\.\\PhysicalDrive0", 250, true),
            disk("PhysicalDrive1", "\\\\.\\PhysicalDrive1", 500, false),
        ];
        assert!(check_clone(&d, "\\\\.\\PhysicalDrive0", "\\\\.\\PhysicalDrive1", 250 << 30).is_ok());
    }

    #[test]
    fn clone_para_o_mesmo_disco_sob_apelidos_e_recusado() {
        let d = windows();
        let err = check_clone(&d, "\\\\.\\PhysicalDrive1", "\\\\?\\physicaldrive1", 1).unwrap_err();
        assert_eq!(err, "origem e destino são o mesmo dispositivo");
        let d = macos();
        assert!(check_clone(&d, "/dev/disk4", "/dev/rdisk4", 1).is_err());
    }

    #[test]
    fn clone_usa_o_tamanho_real_da_origem() {
        // cliente declara 1 byte, mas a origem enumerada tem 500 GiB e o destino 16
        let d = windows();
        assert!(check_clone(&d, "\\\\.\\PhysicalDrive1", "\\\\.\\PhysicalDrive2", 1).is_err());
    }

    #[test]
    fn clone_com_origem_desconhecida_e_recusado() {
        let d = windows();
        assert!(check_clone(&d, "\\\\.\\PhysicalDrive7", "\\\\.\\PhysicalDrive1", 1).is_err());
    }

    #[test]
    fn saida_de_imagem_nunca_e_dispositivo() {
        for saida in [
            "\\\\.\\PhysicalDrive1",
            "\\\\?\\PhysicalDrive1",
            "\\\\.\\C:",
            "\\\\.\\c:\\",
            "\\\\?\\Volume{01234567-89ab-cdef-0123-456789abcdef}",
            "/dev/sda",
            "/dev/nvme0n1p2",
            "/dev/rdisk4",
            "",
            "   ",
        ] {
            assert!(check_image_output(saida).is_err(), "{saida}");
        }
        let dir = std::env::temp_dir();
        assert!(check_image_output(&dir.to_string_lossy()).is_err());
    }

    #[test]
    fn saida_de_imagem_em_arquivo_e_aceita() {
        let novo = std::env::temp_dir().join("cloneforge-guard-nao-existe.cfimg");
        assert!(check_image_output(&novo.to_string_lossy()).is_ok());
        assert!(check_image_output("\\\\?\\C:\\backups\\win11.cfimg").is_ok());
        assert!(check_image_output("\\\\?\\UNC\\servidor\\share\\win11.cfimg").is_ok());
    }
}
