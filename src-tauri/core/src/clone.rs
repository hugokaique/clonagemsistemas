//! Motor de clonagem raw em blocos + verificação de integridade.
//!
//! Pipeline single-pass na origem:
//!   read(src) ──> update(BLAKE3) ──> write(dst)
//!
//! - I/O síncrono (`std::fs`) em `spawn_blocking`, blocos cheios de 8 MiB.
//!   A versão anterior usava `tokio::fs::File`, que (a) limita cada operação
//!   a 2 MiB e (b) só reporta o erro de uma escrita na operação SEGUINTE —
//!   o erro do último bloco era engolido pelo `sync_all` e, sem verificação,
//!   um clone truncado era reportado como sucesso.
//! - O laço é limitado pelo tamanho conhecido da origem, nunca por EOF
//!   (discos brutos no Windows devolvem erro ao ler além do fim).
//! - Verificação pós-clone: relê APENAS o destino e compara o hash BLAKE3
//!   com o calculado durante a escrita — a origem nunca é relida.

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::sync::atomic::AtomicBool;

use crate::io::{self, align_up, read_full, PathKind};
use crate::prepare::{KeepAwake, TargetGuard};
pub use crate::progress::CloneProgress;
use crate::progress::{announce, check_cancel, OnProgress, Tracker};
use crate::safety::{self, ResolvedSource, ResolvedTarget};

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CloneRequest {
    /// ex: `\\.\PhysicalDrive1` | `/dev/sdb` | `/dev/rdisk2` | arquivo
    pub source: String,
    pub target: String,
    /// Informativo — o backend resolve o tamanho real pela enumeração.
    #[serde(default)]
    pub source_size: u64,
    #[serde(default)]
    pub block_size: Option<usize>,
    #[serde(default = "default_true")]
    pub verify: bool,
}

/// Operação já validada: só isto chega ao motor.
#[derive(Debug, Clone)]
pub struct ClonePlan {
    pub source: ResolvedSource,
    pub target: ResolvedTarget,
    pub block_size: usize,
    pub verify: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CloneOutcome {
    pub hash: String,
    pub bytes: u64,
    pub verified: bool,
}

/// Revalidações de segurança (nunca confie só na UI). Usada pela GUI e pelo
/// helper privilegiado.
pub async fn plan_clone(req: &CloneRequest) -> Result<ClonePlan, String> {
    let disks = safety::enumerate_for(&[&req.source, &req.target]).await?;
    plan_clone_with(req, &disks)
}

pub fn plan_clone_with(req: &CloneRequest, disks: &[crate::disk::DiskInfo]) -> Result<ClonePlan, String> {
    let source = safety::resolve_source(&req.source, disks)?;
    let target = safety::resolve_target(&req.target, disks)?;
    safety::check_pair(&source, &target)?;
    Ok(ClonePlan {
        source,
        target,
        block_size: io::normalize_block_size(req.block_size),
        verify: req.verify,
    })
}

/// Tamanho a gravar para um pedaço de `n` bytes: em dispositivos a cauda é
/// completada com zeros até o limite do setor (I/O raw exige múltiplos).
fn padded_len(n: usize, kind: PathKind, sector: u32) -> usize {
    match kind {
        PathKind::Device => align_up(n as u64, sector.max(1) as u64) as usize,
        PathKind::File => n,
    }
}

/// O bloco de I/O precisa ser múltiplo do setor do destino (senão a cauda
/// completada estouraria o buffer). Vale para 512/4096; setores exóticos
/// (520, 4160…) são recusados em vez de arriscar gravação desalinhada.
pub(crate) fn check_sector(target: &ResolvedTarget, block: usize) -> Result<(), String> {
    let sector = target.sector_size.max(1) as usize;
    if target.kind == PathKind::Device && block % sector != 0 {
        return Err(format!(
            "tamanho de setor do destino não suportado ({sector} bytes)"
        ));
    }
    Ok(())
}

/// Núcleo da cópia raw. `on_progress` é chamado no máximo a cada 200 ms
/// (além das mudanças de fase), desacoplando o motor do canal de eventos.
pub fn run_clone(
    plan: &ClonePlan,
    cancel: &AtomicBool,
    on_progress: OnProgress,
) -> Result<CloneOutcome, String> {
    let total = plan.source.size;
    let block = plan.block_size;
    let (kind, sector) = (plan.target.kind, plan.target.sector_size);
    check_sector(&plan.target, block)?;
    let _awake = KeepAwake::new();

    announce(on_progress, "preparing", 0, total, None);
    let mut src = io::open_source(&plan.source.path)?;
    if plan.source.kind == PathKind::Device && plan.source.mounted {
        announce(
            on_progress,
            "preparing",
            0,
            total,
            Some("aviso: a origem tem volumes montados — alterações feitas durante a cópia podem deixar o clone inconsistente".into()),
        );
    }
    let guard = TargetGuard::acquire(&plan.target)?;
    for note in guard.notes() {
        announce(on_progress, "preparing", 0, total, Some(note.clone()));
    }
    let mut dst = io::open_target(&plan.target.path, kind)?;

    let mut buf = vec![0u8; block];
    let mut hasher = blake3::Hasher::new();
    let mut done: u64 = 0;

    // ── Fase 1: cópia raw bloco a bloco, hash on-the-fly ─────────────────
    let mut tracker = Tracker::new("copying", total, on_progress);
    while done < total {
        check_cancel(cancel)?;
        let want = (total - done).min(block as u64) as usize;
        let n = read_full(&mut src, &mut buf[..want])
            .map_err(|e| format!("leitura da origem @ offset {done}: {e}"))?;
        if n == 0 {
            return Err(format!(
                "a origem terminou antes do esperado ({done} de {total} bytes lidos)"
            ));
        }
        hasher.update(&buf[..n]);
        let wlen = padded_len(n, kind, sector);
        buf[n..wlen].fill(0);
        dst.write_all(&buf[..wlen])
            .map_err(|e| format!("escrita no destino @ offset {done}: {e}"))?;
        done += n as u64;
        tracker.tick(done);
    }

    // ── Fase 2: flush — garante que os dados saíram do cache p/ o device ─
    announce(on_progress, "flushing", done, total, None);
    dst.sync_all().map_err(|e| format!("fsync do destino: {e}"))?;
    io::drop_cache(&dst);
    drop(dst);

    let hash = hasher.finalize();

    // ── Fase 3: verificação — relê só o destino e compara o BLAKE3 ───────
    if plan.verify {
        verify_target(&plan.target, done, &hash, block, cancel, on_progress)?;
    }
    guard.finish(&plan.target);

    Ok(CloneOutcome { hash: hash.to_hex().to_string(), bytes: done, verified: plan.verify })
}

/// Relê os primeiros `len` bytes do destino e compara com `expected`.
/// Nunca lê além do que foi gravado (o destino pode ser maior).
pub(crate) fn verify_target(
    target: &ResolvedTarget,
    len: u64,
    expected: &blake3::Hash,
    block: usize,
    cancel: &AtomicBool,
    on_progress: OnProgress,
) -> Result<(), String> {
    check_sector(target, block)?;
    let mut rd = std::fs::File::open(&target.path)
        .map_err(|e| format!("reabrir destino p/ verificação: {e}"))?;
    let mut buf = vec![0u8; block];
    let mut hasher = blake3::Hasher::new();
    let mut done: u64 = 0;
    let mut tracker = Tracker::new("verifying", len, on_progress);
    while done < len {
        check_cancel(cancel)?;
        let take = (len - done).min(block as u64) as usize;
        // Em dispositivos a leitura também precisa ser múltipla do setor.
        let want = padded_len(take, target.kind, target.sector_size);
        let n = read_full(&mut rd, &mut buf[..want])
            .map_err(|e| format!("verificação @ offset {done}: {e}"))?;
        if n < take {
            return Err(format!(
                "VERIFICAÇÃO FALHOU — o destino terminou em {} de {len} bytes. NÃO use este clone.",
                done + n as u64
            ));
        }
        hasher.update(&buf[..take]);
        done += take as u64;
        tracker.tick(done);
    }
    let got = hasher.finalize();
    if got != *expected {
        return Err(format!(
            "VERIFICAÇÃO FALHOU — gravado {} ≠ relido {}. NÃO use este clone.",
            expected.to_hex(),
            got.to_hex()
        ));
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::atomic::Ordering;
    use std::sync::Mutex;

    /// Diretório temporário único por teste (apagado no drop).
    pub(crate) struct TempDir(pub std::path::PathBuf);
    impl TempDir {
        pub(crate) fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!(
                "cloneforge-test-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
        pub(crate) fn path(&self, name: &str) -> String {
            self.0.join(name).to_string_lossy().into_owned()
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Bytes pseudo-aleatórios determinísticos (LCG) — incompressíveis.
    pub(crate) fn noise(len: usize, seed: u64) -> Vec<u8> {
        let mut s = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        (0..len)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                (s >> 24) as u8
            })
            .collect()
    }

    fn request(src: &str, dst: &str, verify: bool) -> CloneRequest {
        CloneRequest {
            source: src.into(),
            target: dst.into(),
            source_size: 0,
            block_size: Some(1024 * 1024),
            verify,
        }
    }

    fn no_progress(_: &CloneProgress) {}

    #[test]
    fn clone_arquivo_para_arquivo_bit_identico() {
        let dir = TempDir::new("clone-ok");
        // 2,5 blocos + cauda fora de alinhamento de setor
        let data = noise(2 * 1024 * 1024 + 512 * 1024 + 123, 1);
        std::fs::write(dir.path("src.bin"), &data).unwrap();

        let plan = plan_clone_with(&request(&dir.path("src.bin"), &dir.path("dst.bin"), true), &[]).unwrap();
        assert_eq!(plan.source.size, data.len() as u64);
        let phases = Mutex::new(Vec::<String>::new());
        let out = run_clone(&plan, &AtomicBool::new(false), &|p| {
            let mut ph = phases.lock().unwrap();
            if ph.last() != Some(&p.phase) {
                ph.push(p.phase.clone());
            }
        })
        .unwrap();

        assert_eq!(std::fs::read(dir.path("dst.bin")).unwrap(), data);
        assert_eq!(out.bytes, data.len() as u64);
        assert!(out.verified);
        assert_eq!(out.hash, blake3::hash(&data).to_hex().to_string());
        assert_eq!(*phases.lock().unwrap(), ["preparing", "copying", "flushing", "verifying"]);
    }

    #[test]
    fn clone_recusa_mesmo_arquivo_origem_vazia_e_inexistente() {
        let dir = TempDir::new("clone-inval");
        std::fs::write(dir.path("a.bin"), b"abc").unwrap();
        std::fs::write(dir.path("vazio.bin"), b"").unwrap();
        assert!(plan_clone_with(&request(&dir.path("a.bin"), &dir.path("a.bin"), true), &[]).is_err());
        assert!(plan_clone_with(&request(&dir.path("vazio.bin"), &dir.path("x.bin"), true), &[]).is_err());
        assert!(plan_clone_with(&request(&dir.path("nao-existe.bin"), &dir.path("x.bin"), true), &[]).is_err());
        assert!(!std::path::Path::new(&dir.path("x.bin")).exists(), "nada pode ser criado na validação");
    }

    #[test]
    fn clone_tamanho_informado_pela_ui_e_ignorado() {
        // sourceSize errado (menor) fazia o progresso passar de 100%; agora o
        // backend usa o tamanho real.
        let dir = TempDir::new("clone-size");
        let data = noise(300_000, 2);
        std::fs::write(dir.path("src.bin"), &data).unwrap();
        let mut req = request(&dir.path("src.bin"), &dir.path("dst.bin"), true);
        req.source_size = 10;
        let plan = plan_clone_with(&req, &[]).unwrap();
        let out = run_clone(&plan, &AtomicBool::new(false), &no_progress).unwrap();
        assert_eq!(out.bytes, 300_000);
        assert_eq!(std::fs::read(dir.path("dst.bin")).unwrap(), data);
    }

    #[test]
    fn clone_cancelado() {
        let dir = TempDir::new("clone-cancel");
        std::fs::write(dir.path("src.bin"), noise(4 * 1024 * 1024, 3)).unwrap();
        let plan = plan_clone_with(&request(&dir.path("src.bin"), &dir.path("dst.bin"), true), &[]).unwrap();
        let cancel = AtomicBool::new(false);
        // Cancela assim que a cópia começa a reportar progresso.
        let err = run_clone(&plan, &cancel, &|p| {
            if p.phase == "copying" {
                cancel.store(true, Ordering::Relaxed);
            }
        })
        .unwrap_err();
        assert!(crate::progress::is_cancelled(&err), "{err}");
    }

    #[test]
    fn verificacao_detecta_destino_adulterado_e_truncado() {
        let dir = TempDir::new("verify");
        let data = noise(1024 * 1024 + 77, 4);
        let good = blake3::hash(&data);
        let target = |name: &str| ResolvedTarget {
            path: dir.path(name),
            kind: PathKind::File,
            capacity: None,
            sector_size: 1,
            disk_id: None,
            mounts: Vec::new(),
            device_ids: Vec::new(),
        };
        let run = |name: &str| {
            verify_target(&target(name), data.len() as u64, &good, 1024 * 1024, &AtomicBool::new(false), &no_progress)
        };

        std::fs::write(dir.path("ok.bin"), &data).unwrap();
        assert!(run("ok.bin").is_ok());

        // Destino MAIOR que o gravado (disco maior): só o trecho gravado conta.
        let mut bigger = data.clone();
        bigger.extend_from_slice(&[0xAA; 4096]);
        std::fs::write(dir.path("maior.bin"), &bigger).unwrap();
        assert!(run("maior.bin").is_ok());

        let mut flipped = data.clone();
        flipped[500_000] ^= 1;
        std::fs::write(dir.path("ruim.bin"), &flipped).unwrap();
        assert!(run("ruim.bin").unwrap_err().contains("VERIFICAÇÃO FALHOU"));

        std::fs::write(dir.path("curto.bin"), &data[..data.len() - 1]).unwrap();
        assert!(run("curto.bin").unwrap_err().contains("VERIFICAÇÃO FALHOU"));
    }

    #[test]
    fn cauda_e_completada_ate_o_setor_so_em_dispositivos() {
        assert_eq!(padded_len(1000, PathKind::File, 512), 1000);
        assert_eq!(padded_len(1000, PathKind::Device, 512), 1024);
        assert_eq!(padded_len(1024, PathKind::Device, 512), 1024);
        assert_eq!(padded_len(513, PathKind::Device, 4096), 4096);
        assert_eq!(padded_len(8 * 1024 * 1024, PathKind::Device, 4096), 8 * 1024 * 1024);
    }
}
