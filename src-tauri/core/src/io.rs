//! Primitivas de I/O compartilhadas pelos motores.
//!
//! Regras que valem para dispositivos de bloco brutos:
//! - Windows (`\\.\PhysicalDriveN`, `\\.\X:`) e macOS (`/dev/rdiskN`) exigem
//!   leituras/escritas em múltiplos do setor lógico. Por isso os motores
//!   sempre trabalham com blocos CHEIOS (`read_full`) e completam a cauda
//!   com zeros até o limite do setor.
//! - Ler além do fim de um disco bruto no Windows devolve erro em vez de
//!   EOF; os laços são limitados pelo tamanho conhecido, nunca por EOF.

use std::fs::{File, OpenOptions};
use std::io::{self, Read};

/// 8 MiB — bom equilíbrio entre throughput em NVMe/SSD e memória.
pub const DEFAULT_BLOCK: usize = 8 * 1024 * 1024;
const MIN_BLOCK: usize = 1024 * 1024;
const MAX_BLOCK: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathKind {
    /// Dispositivo de bloco/volume: já existe, nunca é criado nem truncado.
    Device,
    /// Arquivo comum (imagem, massa de teste).
    File,
}

/// Classifica um caminho no SO atual.
pub fn path_kind(path: &str) -> PathKind {
    #[cfg(windows)]
    {
        windows_path_kind(path)
    }
    #[cfg(unix)]
    {
        unix_path_kind(path)
    }
}

/// Windows: tudo sob `\\.\` ou `\\?\` é dispositivo, EXCETO os caminhos de
/// arquivo com prefixo estendido (`\\?\C:\dir\arq`, `\\?\UNC\srv\share\arq`,
/// `\\?\Volume{guid}\arq`). O Win32 aceita `/` no lugar de `\`, então
/// `//./PhysicalDrive0` também é o disco 0.
#[cfg(any(windows, test))]
pub(crate) fn windows_path_kind(path: &str) -> PathKind {
    let p = path.replace('/', "\\").to_ascii_lowercase();
    let Some(rest) = p.strip_prefix("\\\\.\\").or_else(|| p.strip_prefix("\\\\?\\")) else {
        return PathKind::File;
    };
    let b = rest.as_bytes();
    let drive_file = b.len() > 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'\\';
    let unc_file = rest.starts_with("unc\\");
    let volume_file = rest.starts_with("volume{")
        && rest.find('\\').is_some_and(|i| i + 1 < rest.len());
    if drive_file || unc_file || volume_file {
        PathKind::File
    } else {
        PathKind::Device
    }
}

/// Unix: qualquer coisa sob `/dev/` é tratada como dispositivo mesmo que não
/// exista (criar um arquivo comum em `/dev/sdz` encheria o devtmpfs/RAM), e
/// também o que o `stat` disser que é nó de bloco/caractere (symlinks, mknod).
#[cfg(unix)]
fn unix_path_kind(path: &str) -> PathKind {
    use std::os::unix::fs::FileTypeExt;
    if path.starts_with("/dev/") {
        return PathKind::Device;
    }
    match std::fs::metadata(path) {
        Ok(m) if m.file_type().is_block_device() || m.file_type().is_char_device() => {
            PathKind::Device
        }
        _ => PathKind::File,
    }
}

/// Limita o bloco a [1 MiB, 64 MiB] e arredonda para múltiplo de 1 MiB
/// (logo, múltiplo de qualquer setor de 512/4096 bytes).
pub fn normalize_block_size(requested: Option<usize>) -> usize {
    let b = requested.unwrap_or(DEFAULT_BLOCK).clamp(MIN_BLOCK, MAX_BLOCK);
    b / MIN_BLOCK * MIN_BLOCK
}

pub fn align_up(n: u64, align: u64) -> u64 {
    if align <= 1 {
        return n;
    }
    n.div_ceil(align) * align
}

/// Lê até encher `buf` ou chegar ao fim. Retorno < `buf.len()` só no EOF.
pub fn read_full<R: Read + ?Sized>(r: &mut R, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

/// Dica específica do SO para erros de permissão ao abrir dispositivos.
fn permission_hint() -> &'static str {
    if cfg!(target_os = "windows") {
        "execute o CloneForge como Administrador"
    } else if cfg!(target_os = "macos") {
        "no macOS o acesso a disco bruto exige root: execute \
         `sudo /Applications/CloneForge.app/Contents/MacOS/cloneforge` e conceda \
         Acesso Total ao Disco em Ajustes do Sistema → Privacidade e Segurança"
    } else {
        "o acesso a dispositivos de bloco exige root (Polkit/pkexec)"
    }
}

pub fn open_error(what: &str, path: &str, e: &io::Error) -> String {
    match e.kind() {
        io::ErrorKind::PermissionDenied => {
            format!("abrir {what} `{path}`: acesso negado — {}", permission_hint())
        }
        io::ErrorKind::NotFound => format!("abrir {what} `{path}`: não encontrado"),
        _ => format!("abrir {what} `{path}`: {e}"),
    }
}

pub fn open_source(path: &str) -> Result<File, String> {
    File::open(path).map_err(|e| open_error("origem", path, &e))
}

/// Abre o destino para escrita.
/// - Dispositivo: precisa existir; nunca cria nem trunca.
/// - Arquivo: cria se preciso e SÓ trunca depois de confirmar que o que foi
///   aberto é um arquivo regular (um symlink/junction apontando para um
///   dispositivo não passa por aqui).
pub fn open_target(path: &str, kind: PathKind) -> Result<File, String> {
    match kind {
        PathKind::Device => OpenOptions::new()
            .write(true)
            .open(path)
            .map_err(|e| open_error("destino", path, &e)),
        PathKind::File => {
            let f = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .open(path)
                .map_err(|e| open_error("destino", path, &e))?;
            let is_regular = f.metadata().map(|m| m.is_file()).unwrap_or(false);
            if !is_regular {
                return Err(format!(
                    "destino `{path}` não é um arquivo regular — operação recusada por segurança"
                ));
            }
            f.set_len(0)
                .map_err(|e| format!("truncar destino `{path}`: {e}"))?;
            Ok(f)
        }
    }
}

/// Caminho absoluto (sem resolver symlinks; o arquivo pode ainda não existir).
/// Caminhos relativos são resolvidos contra `base` (ou o diretório atual).
pub fn absolute(path: &str, base: Option<&std::path::Path>) -> String {
    let p = std::path::Path::new(path);
    if p.is_absolute() {
        return path.to_string();
    }
    let base = base
        .map(|b| b.to_path_buf())
        .or_else(|| std::env::current_dir().ok());
    match base {
        Some(b) => b.join(p).to_string_lossy().into_owned(),
        None => path.to_string(),
    }
}

/// Os dois caminhos apontam para o MESMO arquivo? Comparar só o texto não
/// basta: `/var/…` e `/private/var/…` (macOS), um symlink, uma junction ou um
/// hard link levam ao mesmo arquivo com grafias diferentes — e gravar a saída
/// "em outro caminho" destruiria a origem.
pub fn same_file(a: &str, b: &str) -> bool {
    let text_eq = |x: &str, y: &str| {
        if cfg!(windows) {
            x.eq_ignore_ascii_case(y)
        } else {
            x == y
        }
    };
    if text_eq(a, b) {
        return true;
    }
    // Só dá para comparar identidades se os dois já existirem.
    let (Ok(ca), Ok(cb)) = (std::fs::canonicalize(a), std::fs::canonicalize(b)) else {
        return false;
    };
    if text_eq(&ca.to_string_lossy(), &cb.to_string_lossy()) {
        return true;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let (Ok(ma), Ok(mb)) = (std::fs::metadata(&ca), std::fs::metadata(&cb)) {
            return ma.dev() == mb.dev() && ma.ino() == mb.ino();
        }
    }
    false
}

/// Linux: descarta o page cache do dispositivo para que a verificação releia
/// a mídia de verdade (sem isso o kernel devolveria os blocos recém-gravados
/// da memória e um erro de gravação passaria despercebido).
#[cfg(target_os = "linux")]
pub fn drop_cache(f: &File) {
    use std::os::unix::io::AsRawFd;
    // SAFETY: fd válido enquanto `f` vive; chamada apenas consultiva.
    unsafe {
        libc::posix_fadvise(f.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED);
    }
}

#[cfg(not(target_os = "linux"))]
pub fn drop_cache(_f: &File) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_classifica_dispositivos_e_arquivos() {
        use PathKind::*;
        for dev in [
            r"\\.\PhysicalDrive0",
            r"\\.\physicaldrive12",
            r"//./PhysicalDrive0",
            r"\\?\PhysicalDrive0",
            r"\\.\C:",
            r"\\.\C:\",
            r"\\?\Volume{35b744df-0d27-4f19-99c7-702d16a7cf0e}",
            r"\\?\Volume{35b744df-0d27-4f19-99c7-702d16a7cf0e}\",
            r"\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy7",
            r"\\.\Harddisk1Partition2",
        ] {
            assert_eq!(windows_path_kind(dev), Device, "{dev}");
        }
        for file in [
            r"C:\backups\win11.cfimg",
            r"backup.cfimg",
            r"testdata/ci_dst.bin",
            r"\\?\C:\backups\win11.cfimg",
            r"\\?\UNC\servidor\share\img.cfimg",
            r"\\servidor\share\img.cfimg",
            r"\\?\Volume{35b744df-0d27-4f19-99c7-702d16a7cf0e}\dir\img.cfimg",
        ] {
            assert_eq!(windows_path_kind(file), File, "{file}");
        }
    }

    #[test]
    fn mesmo_arquivo_por_caminhos_diferentes() {
        let dir = std::env::temp_dir().join(format!("cloneforge-same-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let a = dir.join("a.bin");
        let b = dir.join("b.bin");
        std::fs::write(&a, b"a").unwrap();
        std::fs::write(&b, b"b").unwrap();
        let s = |p: &std::path::Path| p.to_string_lossy().into_owned();

        assert!(same_file(&s(&a), &s(&a)));
        assert!(!same_file(&s(&a), &s(&b)));
        // Mesmo arquivo com outra grafia (`sub/..`): o texto difere, a identidade não.
        assert!(same_file(&s(&a), &s(&dir.join("sub").join("..").join("a.bin"))));
        // Caminho como digitado × caminho canônico (no macOS /var → /private/var).
        let canonical = std::fs::canonicalize(&a).unwrap();
        assert!(same_file(&s(&a), &s(&canonical)));
        // Um dos dois ainda não existe: não há identidade para comparar.
        assert!(!same_file(&s(&a), &s(&dir.join("novo.bin"))));

        #[cfg(unix)]
        {
            // Symlink e hard link apontam para o mesmo arquivo.
            std::os::unix::fs::symlink(&a, dir.join("link.bin")).unwrap();
            assert!(same_file(&s(&a), &s(&dir.join("link.bin"))));
            std::fs::hard_link(&a, dir.join("hard.bin")).unwrap();
            assert!(same_file(&s(&a), &s(&dir.join("hard.bin"))));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bloco_normalizado() {
        assert_eq!(normalize_block_size(None), DEFAULT_BLOCK);
        assert_eq!(normalize_block_size(Some(1)), MIN_BLOCK);
        assert_eq!(normalize_block_size(Some(1_500_001)), MIN_BLOCK);
        assert_eq!(normalize_block_size(Some(usize::MAX)), MAX_BLOCK);
        assert_eq!(normalize_block_size(Some(3 * 1024 * 1024 + 7)), 3 * 1024 * 1024);
    }

    #[test]
    fn alinhamento() {
        assert_eq!(align_up(0, 512), 0);
        assert_eq!(align_up(1, 512), 512);
        assert_eq!(align_up(512, 512), 512);
        assert_eq!(align_up(513, 4096), 4096);
        assert_eq!(align_up(77, 1), 77);
        assert_eq!(align_up(77, 0), 77);
    }

    /// Reader que entrega os dados em pedaços irregulares (como o decoder zstd).
    struct Chunky<'a>(&'a [u8], usize);
    impl Read for Chunky<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let n = self.0.len().min(self.1).min(buf.len());
            buf[..n].copy_from_slice(&self.0[..n]);
            self.0 = &self.0[n..];
            Ok(n)
        }
    }

    #[test]
    fn read_full_junta_pedacos() {
        let data: Vec<u8> = (0..1000u32).map(|i| i as u8).collect();
        let mut r = Chunky(&data, 7);
        let mut buf = [0u8; 512];
        assert_eq!(read_full(&mut r, &mut buf).unwrap(), 512);
        assert_eq!(&buf[..], &data[..512]);
        assert_eq!(read_full(&mut r, &mut buf).unwrap(), 488); // cauda: só no EOF
        assert_eq!(read_full(&mut r, &mut buf).unwrap(), 0);
    }
}
