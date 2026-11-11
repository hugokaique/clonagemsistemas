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
//!   → stdin : {"cancel":true}   (a qualquer momento, para cancelar o job)

#![cfg(target_os = "linux")]

use serde::Deserialize;
use tauri::AppHandle;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::clone::{emit, CloneProgress};

/// Caminhos absolutos: nada de resolver `pkexec` pelo PATH do usuário.
const PKEXEC: &str = "/usr/bin/pkexec";
const INSTALLED_HELPER: &str = "/usr/bin/cloneforge-helper";

/// stdin do helper em execução — canal por onde segue o pedido de cancelamento.
static HELPER_STDIN: tokio::sync::Mutex<Option<tokio::process::ChildStdin>> =
    tokio::sync::Mutex::const_new(None);

/// Caminho do helper. Em build de release só vale o instalado em /usr/bin
/// (o mesmo que a policy do Polkit autoriza); o helper ao lado do executável
/// é aceito apenas em desenvolvimento.
fn helper_path() -> String {
    if cfg!(debug_assertions) {
        let sibling = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("cloneforge-helper")))
            .filter(|p| p.exists());
        if let Some(p) = sibling {
            return p.to_string_lossy().into_owned();
        }
    }
    INSTALLED_HELPER.into()
}

/// Repassa ao helper em execução o pedido de cancelamento da interface.
pub async fn forward_cancel() {
    if let Some(stdin) = HELPER_STDIN.lock().await.as_mut() {
        let _ = stdin.write_all(b"{\"cancel\":true}\n").await;
        let _ = stdin.flush().await;
    }
}

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
    let helper = helper_path();

    let mut child = tokio::process::Command::new(PKEXEC)
        .arg(&helper)
        .arg("run")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("falha ao iniciar pkexec: {e} (Polkit instalado?)"))?;

    let payload = serde_json::to_string(&job).map_err(|e| e.to_string())? + "\n";
    let mut stdin = child.stdin.take().ok_or("stdin do helper indisponível")?;
    let stdout = child.stdout.take().ok_or("stdout do helper indisponível")?;
    stdin
        .write_all(payload.as_bytes())
        .await
        .map_err(|e| format!("escrever job no helper: {e}"))?;
    stdin
        .flush()
        .await
        .map_err(|e| format!("escrever job no helper: {e}"))?;
    // O stdin fica aberto enquanto o job roda: é por ele que um pedido de
    // cancelamento chega ao helper (ver `forward_cancel`).
    *HELPER_STDIN.lock().await = Some(stdin);

    let mut lines = BufReader::new(stdout).lines();
    let mut final_result: Option<Result<serde_json::Value, String>> = None;

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
                final_result = Some(if parsed.ok.unwrap_or(false) {
                    Ok(serde_json::Value::Object(parsed.extra))
                } else {
                    Err(parsed.error.unwrap_or_else(|| "erro desconhecido no helper".into()))
                });
            }
            _ => {}
        }
    }

    HELPER_STDIN.lock().await.take(); // fecha o stdin do helper
    let status = child.wait().await.map_err(|e| format!("wait helper: {e}"))?;
    match final_result {
        // O erro do helper segue como veio (ex.: "BLOQUEADO: ...").
        Some(result) => result,
        // Sem linha de resultado: o helper nem chegou a rodar o job.
        None => Err(match status.code() {
            Some(126) => "autorização de administrador cancelada".to_string(),
            Some(127) => "autorização de administrador negada".to_string(),
            code => format!("o helper terminou sem resultado (código {code:?})"),
        }),
    }
}
