//! Linux: elevação de privilégios via Polkit (`pkexec`).
//!
//! Modelo (o mesmo do GParted/GNOME Disks): a GUI roda DESPRIVILEGIADA.
//! Operações que tocam dispositivos de bloco são delegadas ao binário
//! `cloneforge-helper`, instalado em /usr/bin e executado via `pkexec`
//! conforme a policy `com.cloneforge.helper.policy`.
//!
//! Protocolo do helper (JSON-lines em stdin/stdout):
//!   → stdin : {"job": "clone"|"create_image"|"restore", "request": {...}}
//!   → stdin : {"cancel": true}                    (opcional, a qualquer momento)
//!   ← stdout: {"type":"progress","payload":{CloneProgress}}  (a cada 200 ms)
//!             {"type":"result","ok":true,"hash":"...",...}
//!             {"type":"result","ok":false,"error":"..."}
//!
//! O stdin fica aberto durante o job: como o helper roda como root, a GUI
//! não tem permissão para matá-lo — o cancelamento vai pelo protocolo.

#![cfg(target_os = "linux")]

use serde::Deserialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

use crate::exec::SharedProgress;
use crate::progress::CloneProgress;

pub fn is_root() -> bool {
    // SAFETY: geteuid é async-signal-safe e não falha
    unsafe { libc::geteuid() == 0 }
}

#[derive(Debug, Deserialize)]
struct HelperLine {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    payload: Option<serde_json::Value>,
    #[serde(default)]
    ok: Option<bool>,
    #[serde(default)]
    error: Option<String>,
    #[serde(flatten)]
    extra: serde_json::Map<String, serde_json::Value>,
}

/// Helper temporário copiado para fora do AppImage (apagado no drop).
struct TempHelper(Option<std::path::PathBuf>);

impl Drop for TempHelper {
    fn drop(&mut self) {
        if let Some(p) = &self.0 {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// Localiza o helper: ao lado do executável (dev/AppImage) ou em /usr/bin.
/// Dentro de um AppImage o binário vive num mount FUSE que o root não
/// enxerga — copiamos para o diretório temporário antes de chamar o pkexec.
fn locate_helper() -> (String, TempHelper) {
    let beside = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("cloneforge-helper")))
        .filter(|p| p.exists());

    if std::env::var_os("APPIMAGE").is_some() {
        if let Some(src) = &beside {
            let dst = std::env::temp_dir().join(format!("cloneforge-helper-{}", std::process::id()));
            if std::fs::copy(src, &dst).is_ok() {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(0o755));
                return (dst.to_string_lossy().into_owned(), TempHelper(Some(dst)));
            }
        }
    }
    let path = beside
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "/usr/bin/cloneforge-helper".into());
    (path, TempHelper(None))
}

/// Executa um job no helper elevado, repassando o progresso ao callback.
/// Retorna o payload do resultado final (campos específicos de cada job).
pub async fn run_elevated(
    job: serde_json::Value,
    cancel: Arc<AtomicBool>,
    on_progress: SharedProgress,
) -> Result<serde_json::Value, String> {
    let (helper, _temp) = locate_helper();

    let mut child = tokio::process::Command::new("pkexec")
        .arg(&helper)
        .arg("run")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("falha ao iniciar pkexec: {e} (Polkit instalado?)"))?;

    let payload = serde_json::to_string(&job).map_err(|e| e.to_string())? + "\n";
    let mut stdin = child.stdin.take().ok_or("stdin do helper indisponível")?;
    stdin
        .write_all(payload.as_bytes())
        .await
        .map_err(|e| format!("escrever job no helper: {e}"))?;
    let _ = stdin.flush().await;

    // stderr drenado em paralelo (diagnóstico quando não há resultado).
    let mut stderr = child.stderr.take().ok_or("stderr do helper indisponível")?;
    let stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf).await;
        String::from_utf8_lossy(&buf).trim().to_string()
    });

    let stdout = child.stdout.take().ok_or("stdout do helper indisponível")?;
    let mut lines = BufReader::new(stdout).lines();
    let mut final_result: Option<Result<serde_json::Value, String>> = None;
    let mut cancel_sent = false;
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(200));

    loop {
        tokio::select! {
            line = lines.next_line() => {
                let Ok(Some(line)) = line else { break };
                let Ok(parsed) = serde_json::from_str::<HelperLine>(&line) else {
                    continue; // linha não-JSON (ruído) — ignora
                };
                match parsed.kind.as_str() {
                    "progress" => {
                        if let Some(v) = parsed.payload {
                            if let Ok(p) = serde_json::from_value::<CloneProgress>(v) {
                                on_progress(&p);
                            }
                        }
                    }
                    "result" => {
                        final_result = Some(if parsed.ok.unwrap_or(false) {
                            Ok(serde_json::Value::Object(parsed.extra))
                        } else {
                            Err(parsed.error.unwrap_or_else(|| "erro desconhecido no helper".into()))
                        });
                    }
                    _ => {}
                }
            }
            _ = tick.tick(), if !cancel_sent => {
                if cancel.load(Ordering::Relaxed) {
                    let _ = stdin.write_all(b"{\"cancel\":true}\n").await;
                    let _ = stdin.flush().await;
                    cancel_sent = true;
                }
            }
        }
    }
    drop(stdin);

    let status = child.wait().await.map_err(|e| format!("wait helper: {e}"))?;
    let stderr_text = stderr_task.await.unwrap_or_default();
    if let Some(result) = final_result {
        return result;
    }
    // Sem linha de resultado: o helper nem chegou a rodar o job.
    Err(match status.code() {
        // Códigos do pkexec
        Some(126) => "autorização negada ou cancelada — nada foi gravado".to_string(),
        Some(127) => format!(
            "não foi possível executar o helper privilegiado `{helper}` (Polkit ausente, helper não instalado ou autenticação falhou)"
        ),
        code => format!("o helper terminou sem resultado (código {code:?}) {stderr_text}"),
    })
}
