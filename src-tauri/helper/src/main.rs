//! cloneforge-helper — motor privilegiado (Linux), executado via pkexec.
//!
//! Protocolo JSON-lines (ver privilege.rs):
//!   stdin  ← {"job": "clone"|"create_image"|"restore", "request": {...}}
//!   stdout → {"type":"progress","payload":{...}} | {"type":"result",...}
//!
//! A GUI nunca roda como root: só este binário mínimo recebe privilégio,
//! reduzindo a superfície exposta a dispositivos de bloco.

use std::io::BufRead;

use cloneforge_lib::clone::{run_clone, CloneProgress, CloneRequest};
use cloneforge_lib::image::{run_create, run_restore, ImageRequest, RestoreRequest};

fn emit_line(v: &serde_json::Value) {
    use std::io::Write;
    println!("{v}");
    let _ = std::io::stdout().flush();
}

fn emit_progress(p: &CloneProgress) {
    emit_line(&serde_json::json!({"type": "progress", "payload": p}));
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) != Some("run") {
        eprintln!("uso: cloneforge-helper run   (job via stdin, JSON-lines)");
        std::process::exit(2);
    }

    let line = std::io::stdin()
        .lock()
        .lines()
        .next()
        .ok_or_else(|| anyhow::anyhow!("stdin vazio — esperado um job JSON"))??;
    let job: serde_json::Value = serde_json::from_str(&line)?;
    let kind = job["job"].as_str().unwrap_or_default().to_string();
    let request = job["request"].clone();

    let result: Result<serde_json::Value, String> = match kind.as_str() {
        "clone" => match serde_json::from_value::<CloneRequest>(request) {
            Ok(req) => run_clone(&req, emit_progress)
                .await
                .map(|hash| serde_json::json!({"hash": hash})),
            Err(e) => Err(format!("job clone inválido: {e}")),
        },
        "create_image" => match serde_json::from_value::<ImageRequest>(request) {
            Ok(req) => match tokio::task::spawn_blocking(move || run_create(&req, emit_progress)).await {
                Ok(r) => r.map(|(logical, hash, image_size, format)| {
                    serde_json::json!({
                        "hash": hash,
                        "logical": logical,
                        "image_size": image_size,
                        "format": format,
                    })
                }),
                Err(e) => Err(format!("task: {e}")),
            },
            Err(e) => Err(format!("job create_image inválido: {e}")),
        },
        "restore" => match serde_json::from_value::<RestoreRequest>(request) {
            Ok(req) => match tokio::task::spawn_blocking(move || run_restore(&req, emit_progress)).await {
                Ok(r) => r.map(|(written, hash)| serde_json::json!({"hash": hash, "written": written})),
                Err(e) => Err(format!("task: {e}")),
            },
            Err(e) => Err(format!("job restore inválido: {e}")),
        },
        other => Err(format!("job desconhecido: `{other}`")),
    };

    match result {
        Ok(mut extra) => {
            extra["type"] = serde_json::json!("result");
            extra["ok"] = serde_json::json!(true);
            emit_line(&extra);
        }
        Err(e) => {
            emit_line(&serde_json::json!({"type": "result", "ok": false, "error": e}));
        }
    }
    Ok(())
}
