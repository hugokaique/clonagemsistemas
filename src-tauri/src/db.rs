//! SQLite local (modo WAL) — histórico de clonagens, catálogo de imagens
//! (.cfimg/.dmg + hashes) e log de auditoria.
//!
//! Um job é gravado como `running` ao começar e fechado ao terminar (`ok`,
//! `error` ou `cancelled`). Linhas que ficaram em `running` porque o app foi
//! encerrado no meio da operação viram `interrupted` na abertura seguinte.
//!
//! `rusqlite` com feature `bundled`: o SQLite é compilado junto ao binário,
//! sem depender de DLL/.so do sistema — mesmo comportamento nos 3 SOs.

use rusqlite::{params, Connection};
use serde::Serialize;
use std::sync::Mutex;
use tauri::State;

/// Conexão compartilhada (rusqlite::Connection não é Sync — Mutex resolve)
pub struct DbState(pub Mutex<Connection>);

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntry {
    pub id: i64,
    pub source: String,
    pub target: String,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub bytes_written: u64,
    pub hash_algo: String,
    pub hash_hex: Option<String>,
    pub verify_ok: Option<bool>,
    pub status: String,
}

pub fn init(path: &std::path::Path) -> anyhow::Result<Connection> {
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS clone_jobs (
            id            INTEGER PRIMARY KEY AUTOINCREMENT,
            source        TEXT NOT NULL,
            target        TEXT NOT NULL,
            started_at    TEXT NOT NULL,
            finished_at   TEXT,
            bytes_written INTEGER NOT NULL DEFAULT 0,
            hash_algo     TEXT NOT NULL DEFAULT 'BLAKE3',
            hash_hex      TEXT,
            verify_ok     INTEGER,
            status        TEXT NOT NULL DEFAULT 'running'
        );
        CREATE TABLE IF NOT EXISTS images (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            path        TEXT NOT NULL UNIQUE,
            format      TEXT,              -- cfimg (zstd) | dmg
            size_bytes  INTEGER,
            hash_algo   TEXT DEFAULT 'BLAKE3',
            hash_hex    TEXT,
            created_at  TEXT NOT NULL DEFAULT (datetime('now'))
        );
        CREATE TABLE IF NOT EXISTS audit_log (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            event      TEXT NOT NULL,
            detail     TEXT,
            created_at TEXT NOT NULL DEFAULT (datetime('now'))
        );
        ",
    )?;

    // Versão do esquema: ponto de apoio para migrações futuras.
    let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if version < SCHEMA_VERSION {
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    }

    // Jobs que estavam rodando quando o processo morreu nunca terminaram.
    let stale = conn.execute(
        "UPDATE clone_jobs SET status = 'interrupted', finished_at = ?1 WHERE status = 'running'",
        params![now()],
    )?;
    if stale > 0 {
        tracing::warn!("{stale} job(s) interrompido(s) por encerramento do app");
    }
    Ok(conn)
}

const SCHEMA_VERSION: i64 = 1;

/// Instante atual em RFC 3339 (UTC) — o mesmo formato para início e fim.
fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn audit(conn: &Connection, event: &str, detail: &str) {
    if let Err(e) = conn.execute(
        "INSERT INTO audit_log (event, detail) VALUES (?1, ?2)",
        params![event, detail],
    ) {
        tracing::warn!("falha ao gravar auditoria ({event}): {e}");
    }
}

/// Registra o início de um job (status `running`) e devolve o id da linha,
/// ou `None` se o banco não aceitou a gravação.
pub fn begin_job(state: &DbState, source: &str, target: &str) -> Option<i64> {
    let Ok(conn) = state.0.lock() else {
        tracing::error!("Mutex do SQLite envenenado — início do job não registrado");
        return None;
    };
    match conn.execute(
        "INSERT INTO clone_jobs (source, target, started_at, status) VALUES (?1, ?2, ?3, 'running')",
        params![source, target, now()],
    ) {
        Ok(_) => Some(conn.last_insert_rowid()),
        Err(e) => {
            tracing::error!("falha ao registrar início do job: {e}");
            None
        }
    }
}

/// Fecha um job (resultado + entrada de auditoria). `id` é o devolvido por
/// `begin_job`; sem ele (ou se a linha sumiu) o job é inserido já concluído.
#[allow(clippy::too_many_arguments)]
pub fn finish_job(
    state: &DbState,
    id: Option<i64>,
    source: &str,
    target: &str,
    bytes: u64,
    hash: Option<&str>,
    verify_ok: Option<bool>,
    status: &str,
) {
    let Ok(conn) = state.0.lock() else {
        tracing::error!("Mutex do SQLite envenenado — auditoria perdida");
        return;
    };
    let verify = verify_ok.map(|b| b as i64);
    let updated = match id {
        Some(id) => conn
            .execute(
                "UPDATE clone_jobs
                    SET finished_at = ?1, bytes_written = ?2, hash_hex = ?3, verify_ok = ?4, status = ?5
                  WHERE id = ?6",
                params![now(), bytes as i64, hash, verify, status, id],
            )
            .unwrap_or_else(|e| {
                tracing::error!("falha ao fechar clone_job {id}: {e}");
                0
            }),
        None => 0,
    };
    if updated == 0 {
        let now = now();
        if let Err(e) = conn.execute(
            "INSERT INTO clone_jobs
               (source, target, started_at, finished_at, bytes_written, hash_hex, verify_ok, status)
             VALUES (?1, ?2, ?3, ?3, ?4, ?5, ?6, ?7)",
            params![source, target, now, bytes as i64, hash, verify, status],
        ) {
            tracing::error!("falha ao gravar clone_job: {e}");
        }
    }
    audit(
        &conn,
        "clone_job",
        &format!("{source} → {target} | {status} | hash={}", hash.unwrap_or("-")),
    );
}

#[tauri::command]
pub fn list_history(state: State<DbState>) -> Result<Vec<HistoryEntry>, String> {
    let conn = state.0.lock().map_err(|e| e.to_string())?;
    let mut stmt = conn
        .prepare(
            "SELECT id, source, target, started_at, finished_at,
                    bytes_written, hash_algo, hash_hex, verify_ok, status
             FROM clone_jobs ORDER BY id DESC LIMIT 200",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| {
            Ok(HistoryEntry {
                id: r.get(0)?,
                source: r.get(1)?,
                target: r.get(2)?,
                started_at: r.get(3)?,
                finished_at: r.get(4)?,
                bytes_written: r.get::<_, i64>(5)? as u64,
                hash_algo: r.get(6)?,
                hash_hex: r.get(7)?,
                verify_ok: r.get::<_, Option<i64>>(8)?.map(|v| v != 0),
                status: r.get(9)?,
            })
        })
        .map_err(|e| e.to_string())?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

// ───────────────────── Catálogo de imagens (.cfimg / .dmg) ─────────────────────

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageEntry {
    pub id: i64,
    pub path: String,
    pub format: Option<String>,
    pub size_bytes: u64,
    pub hash_hex: Option<String>,
    pub created_at: String,
}

/// Registra/atualiza uma imagem no catálogo + trilha de auditoria.
pub fn record_image(state: &DbState, path: &str, format: &str, size_bytes: u64, hash: &str) {
    let Ok(conn) = state.0.lock() else {
        tracing::error!("Mutex do SQLite envenenado — catálogo não atualizado");
        return;
    };
    if let Err(e) = conn.execute(
        "INSERT INTO images (path, format, size_bytes, hash_hex)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(path) DO UPDATE SET
           size_bytes = excluded.size_bytes,
           hash_hex   = excluded.hash_hex,
           created_at = datetime('now')",
        params![path, format, size_bytes as i64, hash],
    ) {
        tracing::error!("falha ao catalogar imagem: {e}");
    }
    audit(
        &conn,
        "image_created",
        &format!("{path} | {format} | {size_bytes} B | {hash}"),
    );
}

#[tauri::command]
pub fn list_images(state: State<DbState>) -> Result<Vec<ImageEntry>, String> {
    let conn = state.0.lock().map_err(|e| e.to_string())?;
    let mut stmt = conn
        .prepare(
            "SELECT id, path, format, size_bytes, hash_hex, created_at
             FROM images ORDER BY id DESC LIMIT 100",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| {
            Ok(ImageEntry {
                id: r.get(0)?,
                path: r.get(1)?,
                format: r.get(2)?,
                size_bytes: r.get::<_, i64>(3)? as u64,
                hash_hex: r.get(4)?,
                created_at: r.get(5)?,
            })
        })
        .map_err(|e| e.to_string())?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn banco(nome: &str) -> (std::path::PathBuf, DbState) {
        let path = std::env::temp_dir().join(format!("cloneforge-db-{}-{nome}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let state = DbState(Mutex::new(init(&path).unwrap()));
        (path, state)
    }

    fn linhas(state: &DbState) -> Vec<(String, Option<String>, i64)> {
        let conn = state.0.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT status, finished_at, bytes_written FROM clone_jobs ORDER BY id")
            .unwrap();
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap();
        rows.map(|r| r.unwrap()).collect()
    }

    #[test]
    fn job_comeca_running_e_e_fechado_na_mesma_linha() {
        let (path, state) = banco("ciclo");
        let id = begin_job(&state, "/dev/a", "/dev/b");
        assert!(id.is_some());
        assert_eq!(linhas(&state), vec![("running".to_string(), None, 0)]);

        finish_job(&state, id, "/dev/a", "/dev/b", 4096, Some("abc"), Some(true), "ok");
        let l = linhas(&state);
        assert_eq!(l.len(), 1, "o fechamento criou outra linha");
        assert_eq!(l[0].0, "ok");
        assert_eq!(l[0].2, 4096);
        assert!(l[0].1.as_deref().is_some_and(|d| d.contains('T')), "fim em RFC 3339");

        // sem id (início não registrado): insere já concluído
        finish_job(&state, None, "/dev/a", "/dev/b", 0, None, None, "error");
        assert_eq!(linhas(&state).len(), 2);

        let conn = state.0.lock().unwrap();
        let auditoria: i64 = conn
            .query_row("SELECT COUNT(*) FROM audit_log WHERE event = 'clone_job'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(auditoria, 2);
        let versao: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
        assert_eq!(versao, SCHEMA_VERSION);
        drop(conn);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn job_que_ficou_running_vira_interrupted_na_reabertura() {
        let (path, state) = banco("interrompido");
        begin_job(&state, "/dev/a", "/dev/b").unwrap();
        drop(state); // app encerrado no meio da operação

        let reaberto = DbState(Mutex::new(init(&path).unwrap()));
        let l = linhas(&reaberto);
        assert_eq!(l[0].0, "interrupted");
        assert!(l[0].1.is_some());
        drop(reaberto);
        let _ = std::fs::remove_file(path);
    }
}
