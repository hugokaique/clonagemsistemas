//! cloneforge-helper — motor privilegiado (Linux), executado via pkexec.
//!
//! Protocolo JSON-lines (ver privilege.rs):
//!   stdin  ← {"job": "clone"|"create_image"|"restore", "request": {...}}
//!   stdout → {"type":"progress","payload":{...}} | {"type":"result",...}
//!   stdin  ← {"cancel": true}   (opcional, a qualquer momento)
//!
//! Código de saída: 0 se o job concluiu, 1 se falhou ou foi cancelado.
//!
//! A GUI nunca roda como root: só este binário mínimo recebe privilégio,
//! reduzindo a superfície exposta a dispositivos de bloco.
//!
//! O job recebido NÃO é confiável (qualquer processo do usuário pode chamar
//! `pkexec cloneforge-helper run`); as regras estão em `cloneforge_lib::elevated`.

use std::io::BufRead;

use cloneforge_lib::clone::CloneProgress;

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

    // Linhas seguintes do stdin: pedidos de cancelamento vindos da GUI. O
    // fim do stdin (EOF) NÃO cancela — é o que acontece em `echo job | helper`.
    std::thread::spawn(|| {
        for line in std::io::stdin().lock().lines().map_while(Result::ok) {
            let cancel = serde_json::from_str::<serde_json::Value>(&line)
                .map(|v| v["cancel"] == true)
                .unwrap_or(false);
            if cancel {
                cloneforge_lib::job::global().cancel();
            }
        }
    });

    // Toda a validação (enumeração, trava do disco do sistema, abertura de
    // arquivos com a identidade do usuário) acontece dentro de `run_job`.
    let result = cloneforge_lib::elevated::run_job(&job, emit_progress).await;

    match result {
        Ok(mut extra) => {
            extra["type"] = serde_json::json!("result");
            extra["ok"] = serde_json::json!(true);
            emit_line(&extra);
        }
        Err(e) => {
            emit_line(&serde_json::json!({"type": "result", "ok": false, "error": e}));
            // Código de saída ≠ 0: quem chama por script não precisa ler o JSON.
            std::process::exit(1);
        }
    }
    Ok(())
}
