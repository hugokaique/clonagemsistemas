//! Linux: elevação de privilégios via Polkit (`pkexec`).
//!
//! Modelo (o mesmo do GParted/GNOME Disks): a GUI roda DESPRIVILEGIADA.
//! Operações que tocam dispositivos de bloco são delegadas ao binário
//! `cloneforge-helper`, instalado em /usr/bin e executado via `pkexec`
//! conforme a policy `com.cloneforge.helper.policy`.
//!
//! Protocolo do helper (JSON-lines em stdin/stdout):
//!   → stdin : {"job": "clone"|"create_image"|"restore", "request": {...}}
//!   ← stdout: {"type":"progress","payload":{CloneProgress}}  (a cada 200 ms)
//!             {"type":"result","ok":true,"hash":"...","logical":N,...}
//!             {"type":"result","ok":false,"error":"..."}

#![cfg(target_os = "linux")]

use serde::Deserialize;
use tauri::AppHandle;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::clone::{emit, CloneProgress};

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

/// Executa um job no helper elevado, reemitindo o progresso para a UI.
/// Retorna o payload do resultado final (campos específicos de cada job).
pub async fn run_elevated(app: &AppHandle, job: serde_json::Value) -> Result<serde_json::Value, String> {
    // O helper fica ao lado do executável principal (dev) ou em /usr/bin (instalado)
    let helper = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("cloneforge-helper")))
        .filter(|p| p.exists())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "/usr/bin/cloneforge-helper".into());

    let mut child = tokio::process::Command::new("pkexec")
        .arg(&helper)
        .arg("run")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("falha ao iniciar pkexec: {e} (Polkit instalado?)"))?;

    let payload = serde_json::to_string(&job).map_err(|e| e.to_string())? + "\n";
    let mut stdin = child.stdin.take().ok_or("stdin do helper indisponível")?;
    stdin
        .write_all(payload.as_bytes())
        .await
        .map_err(|e| format!("escrever job no helper: {e}"))?;
    drop(stdin); // fecha o stdin: sinaliza "um único job"

    let stdout = child.stdout.take().ok_or("stdout do helper indisponível")?;
    let mut lines = BufReader::new(stdout).lines();
    let mut final_result: Result<serde_json::Value, String> =
        Err("helper terminou sem resultado".into());

    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(parsed) = serde_json::from_str::<HelperLine>(&line) else {
            continue; // linha não-JSON (ruído) — ignora
        };
        match parsed.kind.as_str() {
            "progress" => {
                if let Some(v) = parsed.payload {
                    if let Ok(p) = serde_json::from_value::<CloneProgress>(v) {
                        emit(app, &p);
                    }
                }
            }
            "result" => {
                final_result = if parsed.ok.unwrap_or(false) {
                    Ok(serde_json::Value::Object(parsed.extra))
                } else {
                    Err(parsed.error.unwrap_or_else(|| "erro desconhecido no helper".into()))
                };
            }
            _ => {}
        }
    }

    let status = child.wait().await.map_err(|e| format!("wait helper: {e}"))?;
    if !status.success() && final_result.is_err() {
        return Err(format!(
            "helper saiu com {:?} — {}",
            status.code(),
            final_result.err().unwrap_or_default()
        ));
    }
    final_result
}
