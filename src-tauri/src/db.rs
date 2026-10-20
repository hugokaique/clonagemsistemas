//! SQLite local (modo WAL) — histórico de clonagens, catálogo de imagens
//! (.iso/.wim/.img + hashes) e log de auditoria.
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
            format      TEXT,              -- iso | wim | img | cfimg(zstd)
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
    Ok(conn)
}

/// Grava o resultado de um job de clonagem + entrada de auditoria.
pub fn record_job(
    state: &DbState,
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
    let now = chrono::Utc::now().to_rfc3339();
    if let Err(e) = conn.execute(
        "INSERT INTO clone_jobs
           (source, target, started_at, finished_at, bytes_written, hash_hex, verify_ok, status)
         VALUES (?1, ?2, ?3, datetime('now'), ?4, ?5, ?6, ?7)",
        params![
            source,
            target,
            now,
            bytes as i64,
            hash,
            verify_ok.map(|b| b as i64),
            status
        ],
    ) {
        tracing::error!("falha ao gravar clone_job: {e}");
    }
    let _ = conn.execute(
        "INSERT INTO audit_log (event, detail) VALUES ('clone_job', ?1)",
        params![format!("{source} → {target} | {status} | hash={}", hash.unwrap_or("-"))],
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
    let _ = conn.execute(
        "INSERT INTO audit_log (event, detail) VALUES ('image_created', ?1)",
        params![format!("{path} | {format} | {size_bytes} B | {hash}")],
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
