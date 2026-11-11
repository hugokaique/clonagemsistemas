//! Progresso e cancelamento — compartilhados por todos os motores
//! (clonagem, criação e restauração de imagem) e pelo helper.

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Mensagem-sentinela devolvida pelos motores quando o usuário cancela.
pub const CANCELLED: &str = "operação cancelada pelo usuário";

pub fn is_cancelled(err: &str) -> bool {
    err == CANCELLED
}

/// Chamado a cada bloco: aborta o motor assim que o cancelamento é pedido.
pub fn check_cancel(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        Err(CANCELLED.into())
    } else {
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CloneProgress {
    /// "preparing" | "copying" | "flushing" | "verifying" | "compressing" |
    /// "restoring" | "done" | "error" | "cancelled"
    pub phase: String,
    pub bytes_done: u64,
    pub total_bytes: u64,
    pub percent: f64,
    pub speed_mbps: f64,
    /// `None` = desconhecido. (Era `f64::INFINITY`, que o serde_json grava
    /// como `null` e depois não consegue ler de volta — o progresso vindo do
    /// helper era descartado.)
    #[serde(default)]
    pub eta_seconds: Option<f64>,
    #[serde(default)]
    pub message: Option<String>,
}

impl CloneProgress {
    /// Evento sem telemetria de velocidade (mudança de fase, aviso, fim).
    pub fn event(phase: &str, done: u64, total: u64, message: Option<String>) -> Self {
        Self {
            phase: phase.into(),
            bytes_done: done,
            total_bytes: total,
            percent: percent(done, total),
            speed_mbps: 0.0,
            eta_seconds: None,
            message,
        }
    }
}

fn percent(done: u64, total: u64) -> f64 {
    if total > 0 {
        (done as f64 / total as f64 * 100.0).min(100.0)
    } else {
        0.0
    }
}

/// Callback de progresso usado pelos motores.
pub type OnProgress<'a> = &'a dyn Fn(&CloneProgress);

/// Emite um evento avulso (mudança de fase / aviso para o log da UI).
pub fn announce(on_progress: OnProgress, phase: &str, done: u64, total: u64, message: Option<String>) {
    on_progress(&CloneProgress::event(phase, done, total, message));
}

const EMIT_INTERVAL: Duration = Duration::from_millis(200);

/// Telemetria de UMA fase: velocidade e ETA são medidas a partir do início
/// da própria fase (antes a verificação usava o relógio da cópia e mostrava
/// velocidade/ETA errados).
pub struct Tracker<'a> {
    phase: &'static str,
    total: u64,
    started: Instant,
    last_emit: Instant,
    on_progress: OnProgress<'a>,
}

impl<'a> Tracker<'a> {
    pub fn new(phase: &'static str, total: u64, on_progress: OnProgress<'a>) -> Self {
        let now = Instant::now();
        let t = Self { phase, total, started: now, last_emit: now, on_progress };
        // Primeira emissão imediata: a UI troca de fase na hora.
        (t.on_progress)(&CloneProgress::event(phase, 0, total, None));
        t
    }

    /// Emite no máximo a cada 200 ms.
    pub fn tick(&mut self, done: u64) {
        if self.last_emit.elapsed() >= EMIT_INTERVAL {
            self.last_emit = Instant::now();
            (self.on_progress)(&self.snapshot(done));
        }
    }

    fn snapshot(&self, done: u64) -> CloneProgress {
        let elapsed = self.started.elapsed().as_secs_f64();
        let bytes_per_sec = if elapsed > 0.05 { done as f64 / elapsed } else { 0.0 };
        let eta = if bytes_per_sec > 0.0 && self.total >= done {
            Some((self.total - done) as f64 / bytes_per_sec)
        } else {
            None
        };
        CloneProgress {
            phase: self.phase.into(),
            bytes_done: done,
            total_bytes: self.total,
            percent: percent(done, self.total),
            speed_mbps: bytes_per_sec / 1_048_576.0, // MiB/s ≈ MB/s p/ UI
            eta_seconds: eta,
            message: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eta_desconhecido_sobrevive_ao_json() {
        // Regressão: INFINITY virava `null` e a linha de progresso do helper
        // não desserializava mais.
        let p = CloneProgress::event("copying", 0, 100, None);
        let json = serde_json::to_string(&p).unwrap();
        let back: CloneProgress = serde_json::from_str(&json).unwrap();
        assert_eq!(back.eta_seconds, None);
        assert_eq!(back.phase, "copying");
    }

    #[test]
    fn percent_nunca_passa_de_100() {
        assert_eq!(percent(150, 100), 100.0);
        assert_eq!(percent(1, 0), 0.0);
    }

    #[test]
    fn cancelamento() {
        let c = AtomicBool::new(false);
        assert!(check_cancel(&c).is_ok());
        c.store(true, Ordering::Relaxed);
        assert!(is_cancelled(&check_cancel(&c).unwrap_err()));
    }
}
