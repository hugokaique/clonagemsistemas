//! cloneforge-helper — motor privilegiado (Linux), executado via pkexec.
//!
//! Protocolo JSON-lines (ver privilege.rs):
//!   stdin  ← {"job": "clone"|"create_image"|"restore", "request": {...}}
//!   stdin  ← {"cancel": true}                       (opcional, durante o job)
//!   stdout → {"type":"progress","payload":{...}} | {"type":"result",...}
//!
//! A GUI nunca roda como root: só este binário mínimo recebe privilégio,
//! reduzindo a superfície exposta a dispositivos de bloco. Por ser quem tem
//! o privilégio, o helper REFAZ todas as validações de segurança (disco do
//! sistema, capacidade, origem × destino) antes de tocar em qualquer disco.
//!
//! Código de saída: 0 = sucesso · 1 = job falhou · 2 = uso incorreto.

use std::io::BufRead;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use cloneforge_core::clone::{plan_clone, run_clone, CloneRequest};
use cloneforge_core::image::{
    plan_create, plan_restore, run_create, run_restore, ImageRequest, RestoreRequest,
};
use cloneforge_core::progress::CloneProgress;

fn emit_line(v: &serde_json::Value) {
    use std::io::Write;
    println!("{v}");
    let _ = std::io::stdout().flush();
}

fn emit_progress(p: &CloneProgress) {
    emit_line(&serde_json::json!({"type": "progress", "payload": p}));
}

/// O arquivo de imagem criado como root passa a pertencer a quem pediu a
/// elevação (o pkexec exporta o uid em PKEXEC_UID) — senão o usuário não
/// conseguiria mover/apagar o próprio backup.
#[cfg(unix)]
fn give_to_invoker(path: &str) {
    let uid = std::env::var("PKEXEC_UID").ok().and_then(|s| s.parse::<u32>().ok());
    if let Some(uid) = uid {
        if let Err(e) = std::os::unix::fs::chown(path, Some(uid), None) {
            eprintln!("aviso: não foi possível ajustar o dono de {path}: {e}");
        }
    }
}

#[cfg(not(unix))]
fn give_to_invoker(_path: &str) {}

fn to_json<T: serde::Serialize>(v: T) -> Result<serde_json::Value, String> {
    serde_json::to_value(v).map_err(|e| e.to_string())
}

async fn blocking<T, F>(f: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, String> + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| format!("task: {e}"))?
}

async fn run_job(job: serde_json::Value, cancel: Arc<AtomicBool>) -> Result<serde_json::Value, String> {
    let kind = job["job"].as_str().unwrap_or_default().to_string();
    let request = job["request"].clone();

    match kind.as_str() {
        "clone" => {
            let req: CloneRequest =
                serde_json::from_value(request).map_err(|e| format!("job clone inválido: {e}"))?;
            let plan = plan_clone(&req).await?;
            to_json(blocking(move || run_clone(&plan, &cancel, &emit_progress)).await?)
        }
        "create_image" => {
            let req: ImageRequest = serde_json::from_value(request)
                .map_err(|e| format!("job create_image inválido: {e}"))?;
            let plan = plan_create(&req, None).await?;
            let out = blocking(move || run_create(&plan, &cancel, &emit_progress)).await?;
            give_to_invoker(&out.output);
            to_json(out)
        }
        "restore" => {
            let req: RestoreRequest =
                serde_json::from_value(request).map_err(|e| format!("job restore inválido: {e}"))?;
            let plan = plan_restore(&req).await?;
            to_json(blocking(move || run_restore(&plan, &cancel, &emit_progress)).await?)
        }
        other => Err(format!("job desconhecido: `{other}`")),
    }
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) != Some("run") {
        eprintln!("uso: cloneforge-helper run   (job via stdin, JSON-lines)");
        std::process::exit(2);
    }

    let mut first = String::new();
    let job = match std::io::stdin().lock().read_line(&mut first) {
        Ok(n) if n > 0 => serde_json::from_str::<serde_json::Value>(&first)
            .map_err(|e| format!("job não é JSON válido: {e}")),
        Ok(_) => Err("stdin vazio — esperado um job JSON".to_string()),
        Err(e) => Err(format!("ler stdin: {e}")),
    };

    // Depois do job, o stdin só carrega pedidos de cancelamento. EOF não
    // cancela: uso headless (`echo '{…}' | cloneforge-helper run`) fecha o
    // stdin logo após a linha do job.
    let cancel = Arc::new(AtomicBool::new(false));
    let watcher_flag = cancel.clone();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            let wants_cancel = serde_json::from_str::<serde_json::Value>(&line)
                .ok()
                .and_then(|v| v["cancel"].as_bool())
                .unwrap_or(false);
            if wants_cancel {
                watcher_flag.store(true, Ordering::Relaxed);
            }
        }
    });

    let result = match job {
        Ok(job) => run_job(job, cancel).await,
        Err(e) => Err(e),
    };

    let code = match result {
        Ok(mut extra) => {
            extra["type"] = serde_json::json!("result");
            extra["ok"] = serde_json::json!(true);
            emit_line(&extra);
            0
        }
        Err(e) => {
            emit_line(&serde_json::json!({"type": "result", "ok": false, "error": e}));
            1
        }
    };
    // `exit` encerra também a thread que ainda está bloqueada lendo o stdin.
    std::process::exit(code);
}
