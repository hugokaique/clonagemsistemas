//! Controle do job em andamento: um por vez, e cancelável.
//!
//! - **Um por vez.** Duas operações simultâneas poderiam gravar no mesmo
//!   disco; `begin()` devolve erro se já houver uma rodando.
//! - **Cancelamento.** Os laços de cópia consultam um `CancelToken` a cada
//!   bloco. A GUI liga o token do processo pelo comando `cancel_job`; quando o
//!   trabalho está no helper elevado, o pedido segue pelo stdin dele.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

/// Mensagem de erro devolvida por uma operação cancelada.
pub const CANCELLED: &str = "operação cancelada pelo usuário";

/// Sinal de cancelamento compartilhado entre quem pede e quem copia.
#[derive(Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    fn reset(&self) {
        self.0.store(false, Ordering::SeqCst);
    }

    /// `Err(CANCELLED)` se o cancelamento foi pedido — para usar com `?`
    /// dentro dos laços de cópia.
    pub fn check(&self) -> Result<(), String> {
        if self.is_cancelled() {
            Err(CANCELLED.into())
        } else {
            Ok(())
        }
    }
}

/// Token do processo: a GUI roda um job por vez e o helper, um por processo.
pub fn global() -> CancelToken {
    static TOKEN: OnceLock<CancelToken> = OnceLock::new();
    TOKEN.get_or_init(CancelToken::new).clone()
}

static RUNNING: AtomicBool = AtomicBool::new(false);

/// Posse do "job em andamento". Liberada ao sair de escopo, inclusive em erro.
pub struct JobGuard(());

impl Drop for JobGuard {
    fn drop(&mut self) {
        RUNNING.store(false, Ordering::SeqCst);
    }
}

/// Marca o início de uma operação. Falha se já houver outra em andamento.
pub fn begin() -> Result<JobGuard, String> {
    if RUNNING.swap(true, Ordering::SeqCst) {
        return Err("já existe uma operação em andamento — aguarde o término ou cancele-a".into());
    }
    global().reset();
    Ok(JobGuard(()))
}

pub fn is_running() -> bool {
    RUNNING.load(Ordering::SeqCst)
}

/// Fase final e status de histórico de uma operação que terminou em erro:
/// distingue o cancelamento pedido pelo usuário de uma falha.
pub fn failure_kind(error: &str) -> &'static str {
    if error == CANCELLED || global().is_cancelled() {
        "cancelled"
    } else {
        "error"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_cancelado_interrompe_e_cada_token_e_independente() {
        let a = CancelToken::new();
        let b = a.clone();
        let outro = CancelToken::new();
        assert!(a.check().is_ok());
        b.cancel();
        assert_eq!(a.check().unwrap_err(), CANCELLED);
        assert!(outro.check().is_ok(), "tokens distintos não se afetam");
    }

    #[test]
    fn so_um_job_por_vez_e_a_posse_e_devolvida_no_drop() {
        let primeiro = begin().unwrap();
        assert!(is_running());
        assert!(begin().is_err(), "segundo job simultâneo foi aceito");
        drop(primeiro);
        assert!(!is_running());
        let _de_novo = begin().unwrap();
    }
}
