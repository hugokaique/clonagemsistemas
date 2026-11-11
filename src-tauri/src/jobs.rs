//! Estado global de operações: uma por vez, com cancelamento.
//!
//! O backend é a autoridade: mesmo que a UI seja recarregada ou receba um
//! duplo clique, uma segunda operação destrutiva nunca começa enquanto a
//! primeira roda.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter, Manager};

use cloneforge_core::progress::{is_cancelled, CloneProgress};

/// Evento de progresso para a UI (`clone://progress`).
pub fn emit_progress(app: &AppHandle, p: &CloneProgress) {
    if let Err(e) = app.emit("clone://progress", p) {
        tracing::warn!("falha ao emitir progresso: {e}");
    }
}

#[derive(Default)]
pub struct JobState {
    /// Flag de cancelamento da operação em andamento (None = ocioso).
    /// Cada operação recebe uma flag nova: um cancelamento antigo nunca
    /// atinge a operação seguinte.
    current: Mutex<Option<Arc<AtomicBool>>>,
}

/// Mantém a trava global enquanto a operação roda; soltar libera.
pub struct JobGuard {
    state: Arc<JobState>,
    pub cancel: Arc<AtomicBool>,
}

impl JobState {
    pub fn begin(self: &Arc<Self>) -> Result<JobGuard, String> {
        let mut cur = self.current.lock().unwrap_or_else(|e| e.into_inner());
        if cur.is_some() {
            return Err(
                "já existe uma operação em andamento — aguarde o término ou cancele antes de iniciar outra".into(),
            );
        }
        let cancel = Arc::new(AtomicBool::new(false));
        *cur = Some(cancel.clone());
        Ok(JobGuard { state: self.clone(), cancel })
    }

    /// Pede o cancelamento da operação atual. `false` = nada em andamento.
    pub fn cancel(&self) -> bool {
        match &*self.current.lock().unwrap_or_else(|e| e.into_inner()) {
            Some(flag) => {
                flag.store(true, Ordering::Relaxed);
                true
            }
            None => false,
        }
    }

    pub fn is_busy(&self) -> bool {
        self.current.lock().unwrap_or_else(|e| e.into_inner()).is_some()
    }
}

impl Drop for JobGuard {
    fn drop(&mut self) {
        *self.state.current.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

#[tauri::command]
pub fn cancel_operation(state: tauri::State<Arc<JobState>>) -> bool {
    state.cancel()
}

/// Resultado bem-sucedido de uma operação, pronto para histórico e UI.
pub struct JobReport {
    pub bytes: u64,
    pub hash: String,
    /// `None` = não se aplica (criação de imagem)
    pub verified: Option<bool>,
    pub message: String,
}

/// Fecha uma operação: grava no histórico e emite o evento final
/// (`done` | `cancelled` | `error`). Devolve o hash ou o erro para a UI.
pub fn conclude(
    app: &AppHandle,
    db_id: Option<i64>,
    total: u64,
    result: Result<JobReport, String>,
) -> Result<String, String> {
    let db = app.state::<crate::db::DbState>();
    match result {
        Ok(r) => {
            crate::db::job_finish(&db, db_id, r.bytes, Some(&r.hash), r.verified, "ok", None);
            emit_progress(
                app,
                &CloneProgress {
                    percent: 100.0,
                    ..CloneProgress::event("done", r.bytes, r.bytes, Some(r.message))
                },
            );
            Ok(r.hash)
        }
        Err(e) => {
            let (status, phase) = if is_cancelled(&e) {
                ("cancelled", "cancelled")
            } else {
                tracing::error!("operação falhou: {e}");
                ("error", "error")
            };
            crate::db::job_finish(&db, db_id, 0, None, None, status, Some(&e));
            emit_progress(app, &CloneProgress::event(phase, 0, total, Some(e.clone())));
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uma_operacao_por_vez() {
        let jobs = Arc::new(JobState::default());
        assert!(!jobs.is_busy());
        assert!(!jobs.cancel(), "nada para cancelar");

        let first = jobs.begin().unwrap();
        assert!(jobs.is_busy());
        assert!(jobs.begin().is_err(), "segunda operação simultânea é recusada");

        assert!(jobs.cancel());
        assert!(first.cancel.load(Ordering::Relaxed));
        drop(first);
        assert!(!jobs.is_busy());

        // A próxima operação começa com a flag limpa.
        let second = jobs.begin().unwrap();
        assert!(!second.cancel.load(Ordering::Relaxed));
    }
}
