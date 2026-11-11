//! Lado privilegiado do protocolo do helper (`cloneforge-helper run`).
//!
//! O helper roda como root via pkexec, e a GUI não é a única que pode
//! chamá-lo: qualquer processo do usuário consegue executar
//! `pkexec cloneforge-helper run` e mandar um job arbitrário pelo stdin (a
//! autorização do Polkit fica em cache por alguns minutos). Por isso nada do
//! que chega no job é confiável, e as travas da GUI não contam — elas rodam
//! do lado sem privilégio.
//!
//! Regras quando o processo é privilegiado (euid 0):
//!
//! 1. **Dispositivos.** Origem e destino têm de constar na enumeração feita
//!    AQUI, como root, e passam a ser abertos pelo caminho da enumeração — não
//!    pelo texto recebido. Isso aplica a trava do disco do sistema e fecha a
//!    troca de symlink entre a checagem e o uso.
//! 2. **Arquivos** (imagem de entrada, imagem de saída). São abertos com a
//!    identidade de quem chamou o pkexec (`PKEXEC_UID`), nunca como root: o
//!    helper não lê nem escreve arquivo algum que o próprio usuário não
//!    pudesse ler ou escrever.
//!
//! Sem privilégio (CI, testes file→file) o processo só alcança o que o
//! próprio usuário já alcança, então os jobs rodam sem essas travas.

use serde_json::{json, Value};

use crate::clone::{run_clone, CloneProgress, CloneRequest};
use crate::disk::DiskInfo;
use crate::guard;
use crate::image::{self, ImageRequest, RestoreRequest};

pub type ProgressFn = fn(&CloneProgress);

#[cfg(unix)]
pub(crate) fn is_privileged() -> bool {
    // SAFETY: geteuid não falha nem tem pré-condições.
    unsafe { libc::geteuid() == 0 }
}

#[cfg(not(unix))]
pub(crate) fn is_privileged() -> bool {
    // Fora do Unix o helper é só ferramenta de teste: ninguém o eleva.
    false
}

/// true quando a GUI NÃO executa a operação neste processo: no Linux, sem
/// root, o trabalho vai para o helper via pkexec.
pub(crate) fn delegates_to_helper() -> bool {
    cfg!(target_os = "linux") && !is_privileged()
}

async fn enumerate() -> Result<Vec<DiskInfo>, String> {
    crate::disk::list_disks()
        .await
        .map_err(|e| format!("BLOQUEADO: falha ao enumerar os discos para validar o job: {e}"))
}

/// Executa um job recebido pelo helper. Devolve os campos do resultado
/// (sem `type`/`ok`, que são do protocolo).
pub async fn run_job(job: &Value, on_progress: ProgressFn) -> Result<Value, String> {
    let kind = job["job"].as_str().unwrap_or_default();
    let request = job["request"].clone();
    let privileged = is_privileged();

    match kind {
        "clone" => {
            let mut req: CloneRequest =
                serde_json::from_value(request).map_err(|e| format!("job clone inválido: {e}"))?;
            if privileged {
                let disks = enumerate().await?;
                authorize_clone(&disks, &mut req)?;
                release(&disks, &req.target).await?;
            }
            let hash = run_clone(&req, on_progress).await?;
            Ok(json!({ "hash": hash }))
        }
        "create_image" => {
            let mut req: ImageRequest = serde_json::from_value(request)
                .map_err(|e| format!("job create_image inválido: {e}"))?;
            if !privileged {
                let (logical, hash, image_size, format) =
                    tokio::task::spawn_blocking(move || image::run_create(&req, on_progress))
                        .await
                        .map_err(|e| format!("task: {e}"))??;
                return Ok(json!({
                    "hash": hash,
                    "logical": logical,
                    "image_size": image_size,
                    "format": format,
                }));
            }

            authorize_create(&enumerate().await?, &mut req)?;
            // A imagem é escrita num arquivo vizinho e só toma o lugar da
            // saída quando está completa. Abrir, promover e descartar são
            // feitos com a identidade do usuário — nunca como root.
            let output = req.output.clone();
            let partial = image::partial_path(&output);
            let out = as_caller(|| {
                guard::check_image_output(&output)?;
                open_user_output(&partial)
            })??;
            let cancel = crate::job::global();
            let result = tokio::task::spawn_blocking(move || {
                image::run_create_with(&req, move || Ok(out), &cancel, on_progress)
            })
            .await
            .map_err(|e| format!("task: {e}"))
            .and_then(|r| r);
            let (logical, hash, image_size, format) = match result {
                Ok(r) => {
                    as_caller(|| image::commit_output(&partial, &output))??;
                    r
                }
                Err(e) => {
                    let _ = as_caller(|| image::discard_output(&partial));
                    return Err(e);
                }
            };
            Ok(json!({
                "hash": hash,
                "logical": logical,
                "image_size": image_size,
                "format": format,
            }))
        }
        "restore" => {
            let mut req: RestoreRequest = serde_json::from_value(request)
                .map_err(|e| format!("job restore inválido: {e}"))?;
            let pre_opened = if privileged {
                let disks = enumerate().await?;
                let mut img = as_caller(|| {
                    std::fs::File::open(&req.image)
                        .map_err(|e| format!("abrir imagem `{}`: {e}", req.image))
                })??;
                let (header, _, _) = image::read_header(&mut img)?;
                authorize_restore(&disks, &mut req, header.source_size)?;
                release(&disks, &req.target).await?;
                Some(img)
            } else {
                None
            };
            let out = tokio::task::spawn_blocking(move || match pre_opened {
                Some(img) => image::run_restore_from(img, &req, on_progress),
                None => image::run_restore(&req, on_progress),
            })
            .await
            .map_err(|e| format!("task: {e}"))??;
            Ok(json!({ "hash": out.hash, "written": out.written, "verified": out.verified }))
        }
        other => Err(format!("job desconhecido: `{other}`")),
    }
}

/// Desmonta os volumes do destino já autorizado, antes da escrita.
async fn release(disks: &[DiskInfo], target: &str) -> Result<(), String> {
    match guard::find_disk(disks, target) {
        Some(dst) => crate::unmount::release_target(dst).await,
        None => Err(format!("BLOQUEADO: o destino `{target}` saiu da enumeração")),
    }
}

// ───────────────────── Autorização (sem I/O, testável) ─────────────────

/// Clonagem: os dois lados têm de ser discos inteiros da enumeração, e o
/// destino não pode ser o disco do sistema. Reescreve o pedido com os
/// caminhos e o tamanho da enumeração.
fn authorize_clone(disks: &[DiskInfo], req: &mut CloneRequest) -> Result<(), String> {
    let (src, dst) = guard::check_clone(disks, &req.source, &req.target, req.source_size)?;
    req.source = src.path.clone();
    req.target = dst.path.clone();
    req.source_size = src.size_bytes;
    Ok(())
}

/// Criação de imagem: a origem tem de ser um disco ou partição da
/// enumeração (o helper não lê arquivos arbitrários como root). A saída é
/// tratada à parte, em `open_user_output`.
fn authorize_create(disks: &[DiskInfo], req: &mut ImageRequest) -> Result<(), String> {
    if req.source_size == 0 {
        return Err("tamanho da origem inválido".into());
    }
    req.source = guard::check_image_source(disks, &req.source, req.smart, cfg!(windows))?;
    Ok(())
}

/// Valida e abre a saída de uma imagem. Chamada já com a identidade do
/// usuário: nem a checagem do caminho nem a abertura enxergam nada que ele
/// próprio não enxergue.
fn open_user_output(path: &str) -> Result<std::fs::File, String> {
    guard::check_image_output(path)?;
    image::open_output(path)
}

/// Restore: o destino tem de ser um disco inteiro da enumeração, fora do
/// sistema e com pelo menos o tamanho declarado no header da imagem.
fn authorize_restore(
    disks: &[DiskInfo],
    req: &mut RestoreRequest,
    required_bytes: u64,
) -> Result<(), String> {
    let dst = guard::check_write_target(disks, &req.target, required_bytes)?;
    req.target = dst.path.clone();
    Ok(())
}

// ───────────────────── Identidade de quem chamou ───────────────────────

/// UID de quem invocou o helper. O pkexec zera o ambiente e define
/// `PKEXEC_UID` ele mesmo, então o valor não é controlado por quem chama.
/// `None` = root chamou diretamente: não há identidade menor para assumir.
fn caller_uid() -> Result<Option<u32>, String> {
    for var in ["PKEXEC_UID", "SUDO_UID"] {
        // Presente mas ilegível é erro, nunca "sem identidade".
        let Some(raw) = std::env::var_os(var) else {
            continue;
        };
        let uid: u32 = raw
            .to_str()
            .and_then(|v| v.trim().parse().ok())
            .ok_or_else(|| format!("BLOQUEADO: {var} inválido"))?;
        return Ok((uid != 0).then_some(uid));
    }
    Ok(None)
}

/// Executa `f` (abrir um arquivo do usuário) com a identidade de quem chamou.
fn as_caller<T>(f: impl FnOnce() -> T) -> Result<T, String> {
    as_user(caller_uid()?, f)
}

#[cfg(not(unix))]
fn as_user<T>(_uid: Option<u32>, f: impl FnOnce() -> T) -> Result<T, String> {
    Ok(f())
}

/// Troca uid/gid efetivos e grupos suplementares para os de `uid`, roda `f`
/// e retoma os anteriores. FAIL-CLOSED: se a troca falhar, `f` não roda.
///
/// A troca vale para o processo inteiro; o helper a faz antes de iniciar
/// qualquer I/O do job.
#[cfg(unix)]
#[allow(clippy::unnecessary_cast)] // os tipos da libc variam entre Linux e macOS
fn as_user<T>(uid: Option<u32>, f: impl FnOnce() -> T) -> Result<T, String> {
    let Some(uid) = uid else {
        return Ok(f());
    };
    let os_err =
        |what: &str| format!("BLOQUEADO: {what}: {}", std::io::Error::last_os_error());

    // SAFETY: chamadas diretas da libc sobre buffers locais, com os tamanhos
    // passados corretamente; `pwd.pw_name` aponta para `buf`, vivo até o fim.
    unsafe {
        let mut pwd: libc::passwd = std::mem::zeroed();
        let mut buf: Vec<libc::c_char> = vec![0; 16 * 1024];
        let mut found: *mut libc::passwd = std::ptr::null_mut();
        let rc = libc::getpwuid_r(uid, &mut pwd, buf.as_mut_ptr(), buf.len(), &mut found);
        if rc != 0 || found.is_null() {
            return Err(format!("BLOQUEADO: usuário de uid {uid} não encontrado"));
        }
        let gid = pwd.pw_gid;

        let count = libc::getgroups(0, std::ptr::null_mut());
        if count < 0 {
            return Err(os_err("getgroups"));
        }
        let mut saved_groups: Vec<libc::gid_t> = vec![0; count as usize];
        if count > 0 {
            let got = libc::getgroups(count, saved_groups.as_mut_ptr());
            if got < 0 {
                return Err(os_err("getgroups"));
            }
            saved_groups.truncate(got as usize);
        }
        let saved_egid = libc::getegid();
        let saved_euid = libc::geteuid();

        if libc::initgroups(pwd.pw_name, gid as _) != 0 {
            return Err(os_err("initgroups"));
        }
        if libc::setegid(gid) != 0 {
            let e = os_err("setegid");
            libc::setgroups(saved_groups.len() as _, saved_groups.as_ptr());
            return Err(e);
        }
        if libc::seteuid(uid) != 0 {
            let e = os_err("seteuid");
            libc::setegid(saved_egid);
            libc::setgroups(saved_groups.len() as _, saved_groups.as_ptr());
            return Err(e);
        }

        let out = f();

        let restored = libc::seteuid(saved_euid) == 0
            && libc::setegid(saved_egid) == 0
            && libc::setgroups(saved_groups.len() as _, saved_groups.as_ptr()) == 0;
        if !restored {
            return Err(os_err("não foi possível retomar o privilégio após abrir o arquivo"));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk::PartitionInfo;

    fn part(index: u32, path: &str) -> PartitionInfo {
        PartitionInfo {
            index,
            path: Some(path.into()),
            size_bytes: 1 << 30,
            fs: Some("ext4".into()),
            label: None,
            mount_point: None,
        }
    }

    fn disk(id: &str, path: &str, size_gib: u64, is_system: bool, parts: &[&str]) -> DiskInfo {
        DiskInfo {
            id: id.into(),
            path: path.into(),
            model: "Teste".into(),
            size_bytes: size_gib << 30,
            bus: "?".into(),
            is_system,
            is_removable: false,
            partitions: parts
                .iter()
                .enumerate()
                .map(|(i, p)| part(i as u32 + 1, p))
                .collect(),
        }
    }

    // Caminhos no estilo Windows: a comparação é só léxica, então o teste
    // não depende de quais dispositivos existem na máquina que o roda.
    fn disks() -> Vec<DiskInfo> {
        vec![
            disk("d0", "\\\\.\\PhysicalDrive0", 500, true, &["\\\\.\\Harddisk0Partition1"]),
            disk("d1", "\\\\.\\PhysicalDrive1", 1000, false, &["\\\\.\\Harddisk1Partition1"]),
            disk("d2", "\\\\.\\PhysicalDrive2", 16, false, &[]),
        ]
    }

    fn clone_req(source: &str, target: &str) -> CloneRequest {
        CloneRequest {
            source: source.into(),
            target: target.into(),
            source_size: 1,
            block_size: None,
            verify: true,
        }
    }

    fn image_req(source: &str, output: &str) -> ImageRequest {
        ImageRequest {
            source: source.into(),
            source_size: 1 << 30,
            output: output.into(),
            level: None,
            smart: false,
            fs: None,
            live: None,
        }
    }

    fn restore_req(target: &str) -> RestoreRequest {
        RestoreRequest { image: "img.cfimg".into(), target: target.into(), verify: true }
    }

    #[test]
    fn clone_troca_apelidos_pelos_caminhos_da_enumeracao() {
        let mut req = clone_req("//./physicaldrive2", "\\\\?\\PHYSICALDRIVE1\\");
        authorize_clone(&disks(), &mut req).unwrap();
        assert_eq!(req.source, "\\\\.\\PhysicalDrive2");
        assert_eq!(req.target, "\\\\.\\PhysicalDrive1");
        assert_eq!(req.source_size, 16 << 30, "tamanho vem da enumeração");
    }

    #[test]
    fn clone_recusa_sistema_arquivos_e_particoes() {
        let d = disks();
        for (origem, destino) in [
            ("\\\\.\\PhysicalDrive2", "\\\\.\\PhysicalDrive0"), // destino = sistema
            ("/etc/shadow", "\\\\.\\PhysicalDrive1"),           // ler arquivo como root
            ("\\\\.\\PhysicalDrive2", "/etc/passwd"),           // escrever arquivo como root
            ("\\\\.\\PhysicalDrive2", "\\\\.\\Harddisk1Partition1"), // partição como destino
            ("\\\\.\\PhysicalDrive1", "\\\\.\\PhysicalDrive2"), // destino menor
        ] {
            let mut req = clone_req(origem, destino);
            assert!(authorize_clone(&d, &mut req).is_err(), "{origem} → {destino}");
        }
        assert!(authorize_clone(&[], &mut clone_req("a", "b")).is_err());
    }

    #[test]
    fn imagem_so_le_disco_ou_particao_enumerados() {
        let d = disks();
        let saida = std::env::temp_dir().join("cloneforge-elevated-nao-existe.cfimg");
        let saida = saida.to_string_lossy();

        let mut disco = image_req("\\\\?\\physicaldrive0", &saida); // ler o sistema é permitido
        authorize_create(&d, &mut disco).unwrap();
        assert_eq!(disco.source, "\\\\.\\PhysicalDrive0");

        let mut particao = image_req("\\\\.\\harddisk1partition1", &saida);
        authorize_create(&d, &mut particao).unwrap();
        assert_eq!(particao.source, "\\\\.\\Harddisk1Partition1");

        for origem in ["/etc/shadow", "\\\\.\\PhysicalDrive9", "\\\\.\\C:", ""] {
            let err = authorize_create(&d, &mut image_req(origem, &saida)).unwrap_err();
            assert!(err.starts_with("BLOQUEADO"), "{origem}: {err}");
        }
    }

    #[test]
    fn imagem_recusa_saida_em_dispositivo() {
        for saida in ["\\\\.\\PhysicalDrive1", "/dev/sdb", ""] {
            assert!(open_user_output(saida).is_err(), "aceitou saída `{saida}`");
        }
    }

    #[test]
    fn restore_so_grava_em_disco_inteiro_fora_do_sistema() {
        let d = disks();

        let mut ok = restore_req("//./physicaldrive1");
        authorize_restore(&d, &mut ok, 900 << 30).unwrap();
        assert_eq!(ok.target, "\\\\.\\PhysicalDrive1");

        let sistema = authorize_restore(&d, &mut restore_req("\\\\.\\PhysicalDrive0"), 0);
        assert_eq!(sistema.unwrap_err(), guard::BLOCKED_SYSTEM);
        for alvo in ["\\\\.\\Harddisk1Partition1", "/etc/sudoers.d/x", "/home/u/disco.img"] {
            let err = authorize_restore(&d, &mut restore_req(alvo), 0).unwrap_err();
            assert!(err.starts_with("BLOQUEADO"), "{alvo}: {err}");
        }
        // imagem maior que o destino
        assert!(authorize_restore(&d, &mut restore_req("\\\\.\\PhysicalDrive2"), 17 << 30).is_err());
    }

    #[test]
    fn sem_identidade_menor_roda_direto() {
        assert_eq!(as_user(None, || 7).unwrap(), 7);
    }

    #[cfg(unix)]
    #[test]
    fn troca_de_identidade_que_falha_nao_executa_a_acao() {
        // Sem root não dá para trocar de identidade: tem de falhar FECHADO.
        if is_privileged() {
            return;
        }
        let uid = unsafe { libc::getuid() };
        let mut rodou = false;
        let res = as_user(Some(uid), || rodou = true);
        assert!(res.unwrap_err().starts_with("BLOQUEADO"));
        assert!(!rodou, "a ação rodou mesmo sem a troca de identidade");
    }

    /// Fluxo completo sem privilégio (o mesmo do E2E do CI): clone, imagem e
    /// restore file→file pelo protocolo do helper.
    #[tokio::test]
    async fn jobs_sem_privilegio_rodam_arquivo_a_arquivo() {
        if is_privileged() {
            return;
        }
        fn nada(_: &CloneProgress) {}

        let dir = std::env::temp_dir().join(format!("cloneforge-elevated-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = |nome: &str| dir.join(nome).to_string_lossy().into_owned();

        let dados: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(p("src.bin"), &dados).unwrap();
        let size = dados.len();

        let r = run_job(
            &json!({"job": "clone", "request": {"source": p("src.bin"), "target": p("dst.bin"),
                    "sourceSize": size, "blockSize": null, "verify": true}}),
            nada,
        )
        .await
        .unwrap();
        assert_eq!(r["hash"], blake3::hash(&dados).to_hex().to_string());
        assert_eq!(std::fs::read(p("dst.bin")).unwrap(), dados);

        let r = run_job(
            &json!({"job": "create_image", "request": {"source": p("src.bin"), "sourceSize": size,
                    "output": p("img.cfimg"), "level": 3, "smart": false, "fs": null, "live": null}}),
            nada,
        )
        .await
        .unwrap();
        assert_eq!(r["logical"], size);
        assert_eq!(r["format"], "cfimg");

        let r = run_job(
            &json!({"job": "restore", "request": {"image": p("img.cfimg"), "target": p("rst.bin"), "verify": true}}),
            nada,
        )
        .await
        .unwrap();
        assert_eq!(r["written"], size);
        assert_eq!(r["verified"], true);
        assert_eq!(std::fs::read(p("rst.bin")).unwrap(), dados);
        assert!(!dir.join("img.cfimg.partial").exists());

        assert!(run_job(&json!({"job": "formatar", "request": {}}), nada).await.is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
