//! SQLite local (modo WAL) — histórico de operações, catálogo de imagens
//! e log de auditoria.
//!
//! `rusqlite` com feature `bundled`: o SQLite é compilado junto ao binário,
//! sem depender de DLL/.so do sistema — mesmo comportamento nos 3 SOs.
//!
//! Ciclo de vida de um job: `job_start` grava a linha com status `running`
//! no INÍCIO (com a hora real de início) e `job_finish` a atualiza ao final.
//! Se o app morrer no meio, a linha fica `running` e é marcada como
//! `interrupted` na próxima abertura — antes o job simplesmente não aparecia
//! e `started_at` guardava a hora do FIM.
//!
//! Datas: sempre RFC 3339 UTC (`2026-10-19T12:00:00Z`). Linhas antigas usam o
//! formato do `datetime('now')` (`2026-10-19 12:00:00`); a UI lê os dois.

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use tauri::State;

/// Conexão compartilhada (rusqlite::Connection não é Sync — Mutex resolve)
pub struct DbState(pub Mutex<Connection>);

impl DbState {
    fn conn(&self) -> MutexGuard<'_, Connection> {
        // Mutex envenenado não pode custar a auditoria: recupera o guard.
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntry {
    pub id: i64,
    /// "clone" | "image" | "restore"
    pub kind: String,
    pub source: String,
    pub target: String,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub bytes_written: u64,
    pub hash_algo: String,
    pub hash_hex: Option<String>,
    pub verify_ok: Option<bool>,
    /// "running" | "ok" | "error" | "cancelled" | "interrupted"
    pub status: String,
    pub message: Option<String>,
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

const SCHEMA: &str = "
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
";

fn has_column(conn: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let names = stmt.query_map([], |r| r.get::<_, String>(1))?;
    for name in names {
        if name? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

pub fn init(path: &Path) -> anyhow::Result<Connection> {
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.execute_batch(SCHEMA)?;

    // Migração 1: tipo da operação e mensagem de erro no histórico.
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version < 1 {
        if !has_column(&conn, "clone_jobs", "kind")? {
            conn.execute_batch("ALTER TABLE clone_jobs ADD COLUMN kind TEXT NOT NULL DEFAULT 'clone'")?;
        }
        if !has_column(&conn, "clone_jobs", "message")? {
            conn.execute_batch("ALTER TABLE clone_jobs ADD COLUMN message TEXT")?;
        }
        conn.pragma_update(None, "user_version", 1)?;
    }

    // Jobs que ficaram "running" = o app foi encerrado no meio da operação.
    let stale = conn.execute(
        "UPDATE clone_jobs
            SET status = 'interrupted', finished_at = ?1,
                message = 'o aplicativo foi encerrado durante a operação'
          WHERE status = 'running'",
        params![now()],
    )?;
    if stale > 0 {
        tracing::warn!("{stale} operação(ões) interrompida(s) na execução anterior");
    }
    Ok(conn)
}

/// Abre o banco; se o arquivo estiver corrompido, guarda-o de lado e começa
/// um novo — um histórico danificado não pode impedir o app de abrir. Em
/// último caso usa um banco em memória (histórico só desta sessão).
pub fn open_or_recover(path: &Path) -> Connection {
    match init(path) {
        Ok(c) => return c,
        Err(e) => tracing::error!("falha ao abrir {}: {e}", path.display()),
    }
    let aside = path.with_extension(format!("corrompido-{}.db", chrono::Utc::now().timestamp()));
    if std::fs::rename(path, &aside).is_ok() {
        tracing::warn!("banco movido para {}", aside.display());
        if let Ok(c) = init(path) {
            return c;
        }
    }
    tracing::error!("usando banco em memória — o histórico desta sessão não será salvo");
    init(Path::new(":memory:")).expect("SQLite em memória indisponível")
}

/// Registra o INÍCIO de uma operação. Devolve o id para `job_finish`.
pub fn job_start(state: &DbState, kind: &str, source: &str, target: &str) -> Option<i64> {
    let conn = state.conn();
    match conn.execute(
        "INSERT INTO clone_jobs (kind, source, target, started_at, status)
         VALUES (?1, ?2, ?3, ?4, 'running')",
        params![kind, source, target, now()],
    ) {
        Ok(_) => Some(conn.last_insert_rowid()),
        Err(e) => {
            tracing::error!("falha ao registrar início do job: {e}");
            None
        }
    }
}

/// Grava o resultado de uma operação + entrada de auditoria.
#[allow(clippy::too_many_arguments)]
pub fn job_finish(
    state: &DbState,
    id: Option<i64>,
    bytes: u64,
    hash: Option<&str>,
    verify_ok: Option<bool>,
    status: &str,
    message: Option<&str>,
) {
    let conn = state.conn();
    if let Some(id) = id {
        if let Err(e) = conn.execute(
            "UPDATE clone_jobs
                SET finished_at = ?1, bytes_written = ?2, hash_hex = ?3,
                    verify_ok = ?4, status = ?5, message = ?6
              WHERE id = ?7",
            params![
                now(),
                bytes as i64,
                hash,
                verify_ok.map(|b| b as i64),
                status,
                message,
                id
            ],
        ) {
            tracing::error!("falha ao gravar resultado do job {id}: {e}");
        }
    }
    let detail: Option<String> = id.and_then(|id| {
        conn.query_row(
            "SELECT kind || ': ' || source || ' → ' || target FROM clone_jobs WHERE id = ?1",
            params![id],
            |r| r.get::<_, String>(0),
        )
        .optional()
        .ok()
        .flatten()
    });
    let _ = conn.execute(
        "INSERT INTO audit_log (event, detail, created_at) VALUES ('job', ?1, ?2)",
        params![
            format!(
                "{} | {status} | hash={}{}",
                detail.unwrap_or_else(|| "?".into()),
                hash.unwrap_or("-"),
                message.map(|m| format!(" | {m}")).unwrap_or_default()
            ),
            now()
        ],
    );
}

#[tauri::command]
pub fn list_history(state: State<DbState>) -> Result<Vec<HistoryEntry>, String> {
    let conn = state.conn();
    let mut stmt = conn
        .prepare(
            "SELECT id, kind, source, target, started_at, finished_at,
                    bytes_written, hash_algo, hash_hex, verify_ok, status, message
             FROM clone_jobs ORDER BY id DESC LIMIT 200",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| {
            Ok(HistoryEntry {
                id: r.get(0)?,
                kind: r.get(1)?,
                source: r.get(2)?,
                target: r.get(3)?,
                started_at: r.get(4)?,
                finished_at: r.get(5)?,
                bytes_written: r.get::<_, i64>(6)? as u64,
                hash_algo: r.get(7)?,
                hash_hex: r.get(8)?,
                verify_ok: r.get::<_, Option<i64>>(9)?.map(|v| v != 0),
                status: r.get(10)?,
                message: r.get(11)?,
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
    /// false = o arquivo foi movido/apagado depois de catalogado
    pub exists: bool,
}

/// Registra/atualiza uma imagem no catálogo + trilha de auditoria.
pub fn record_image(state: &DbState, path: &str, format: &str, size_bytes: u64, hash: &str) {
    let conn = state.conn();
    if let Err(e) = conn.execute(
        "INSERT INTO images (path, format, size_bytes, hash_hex, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(path) DO UPDATE SET
           format     = excluded.format,
           size_bytes = excluded.size_bytes,
           hash_hex   = excluded.hash_hex,
           created_at = excluded.created_at",
        params![path, format, size_bytes as i64, hash, now()],
    ) {
        tracing::error!("falha ao catalogar imagem: {e}");
    }
    let _ = conn.execute(
        "INSERT INTO audit_log (event, detail, created_at) VALUES ('image_created', ?1, ?2)",
        params![format!("{path} | {format} | {size_bytes} B | {hash}"), now()],
    );
}

#[tauri::command]
pub fn list_images(state: State<DbState>) -> Result<Vec<ImageEntry>, String> {
    let conn = state.conn();
    let mut stmt = conn
        .prepare(
            "SELECT id, path, format, size_bytes, hash_hex, created_at
             FROM images ORDER BY created_at DESC, id DESC LIMIT 100",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| {
            let path: String = r.get(1)?;
            Ok(ImageEntry {
                id: r.get(0)?,
                exists: Path::new(&path).is_file(),
                path,
                format: r.get(2)?,
                size_bytes: r.get::<_, Option<i64>>(3)?.unwrap_or(0) as u64,
                hash_hex: r.get(4)?,
                created_at: r.get(5)?,
            })
        })
        .map_err(|e| e.to_string())?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

/// Remove uma entrada do catálogo (o arquivo em disco NÃO é apagado).
#[tauri::command]
pub fn forget_image(state: State<DbState>, id: i64) -> Result<(), String> {
    state
        .conn()
        .execute("DELETE FROM images WHERE id = ?1", params![id])
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> DbState {
        DbState(Mutex::new(init(Path::new(":memory:")).unwrap()))
    }

    /// (kind, status, finished_at, verify_ok, bytes_written, message)
    type Row = (String, String, Option<String>, Option<i64>, i64, Option<String>);

    fn history(state: &DbState) -> Vec<Row> {
        let conn = state.conn();
        let mut stmt = conn
            .prepare("SELECT kind, status, finished_at, verify_ok, bytes_written, message FROM clone_jobs ORDER BY id")
            .unwrap();
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))
            .unwrap();
        rows.map(|r| r.unwrap()).collect()
    }

    #[test]
    fn ciclo_de_vida_do_job() {
        let db = mem();
        let id = job_start(&db, "clone", "/dev/sdb", "/dev/sdc");
        assert!(id.is_some());
        let h = history(&db);
        assert_eq!((h[0].0.as_str(), h[0].1.as_str()), ("clone", "running"));
        assert_eq!(h[0].2, None, "sem data de fim enquanto roda");

        job_finish(&db, id, 4096, Some("abc"), Some(true), "ok", None);
        let h = history(&db);
        assert_eq!(h[0].1, "ok");
        assert!(h[0].2.as_deref().unwrap().ends_with('Z'), "RFC 3339 UTC");
        assert_eq!((h[0].3, h[0].4), (Some(1), 4096));

        let id2 = job_start(&db, "image", "C:", "a.cfimg");
        job_finish(&db, id2, 0, None, None, "error", Some("falhou"));
        let h = history(&db);
        assert_eq!((h[1].0.as_str(), h[1].1.as_str(), h[1].3), ("image", "error", None));
        assert_eq!(h[1].5.as_deref(), Some("falhou"));

        let audit: i64 = db.conn().query_row("SELECT COUNT(*) FROM audit_log", [], |r| r.get(0)).unwrap();
        assert_eq!(audit, 2);
    }

    #[test]
    fn migra_banco_da_v010_e_marca_jobs_interrompidos() {
        let dir = std::env::temp_dir().join(format!("cloneforge-db-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cloneforge.db");

        // Banco no esquema antigo (sem kind/message), com um job concluído e
        // um que ficou "running" quando o app foi morto.
        {
            let old = Connection::open(&path).unwrap();
            old.execute_batch(SCHEMA).unwrap();
            old.execute_batch(
                "INSERT INTO clone_jobs (source, target, started_at, finished_at, bytes_written, hash_hex, verify_ok, status)
                   VALUES ('a', 'b', '2026-10-19T12:00:00+00:00', '2026-10-19 12:00:00', 10, 'h', 1, 'ok');
                 INSERT INTO clone_jobs (source, target, started_at) VALUES ('c', 'd', '2026-10-20T08:00:00+00:00');
                 INSERT INTO images (path, format, size_bytes, hash_hex) VALUES ('/nao/existe.cfimg', 'cfimg', 5, 'h');",
            )
            .unwrap();
        }

        let db = DbState(Mutex::new(init(&path).unwrap()));
        let h = history(&db);
        assert_eq!((h[0].0.as_str(), h[0].1.as_str()), ("clone", "ok"), "linha antiga ganha kind padrão");
        assert_eq!(h[1].1, "interrupted", "job órfão vira interrompido");
        assert!(h[1].2.is_some());

        // Reabrir é idempotente (não tenta recriar colunas).
        drop(db);
        let db = DbState(Mutex::new(init(&path).unwrap()));
        assert_eq!(history(&db).len(), 2);

        // Catálogo: UPSERT atualiza inclusive o formato e não duplica.
        record_image(&db, "/x/a.cfimg", "cfimg", 100, "h1");
        record_image(&db, "/x/a.cfimg", "dmg", 200, "h2");
        let (n, fmt, size): (i64, String, i64) = db
            .conn()
            .query_row(
                "SELECT COUNT(*), MAX(format), MAX(size_bytes) FROM images WHERE path = '/x/a.cfimg'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!((n, fmt.as_str(), size), (1, "dmg", 200));

        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn banco_corrompido_nao_impede_a_abertura() {
        let dir = std::env::temp_dir().join(format!("cloneforge-dbbad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cloneforge.db");
        std::fs::write(&path, b"isto nao e um banco sqlite, de jeito nenhum............").unwrap();

        let db = DbState(Mutex::new(open_or_recover(&path)));
        assert!(job_start(&db, "clone", "a", "b").is_some());
        drop(db);
        let aside = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().contains("corrompido"));
        assert!(aside, "o arquivo danificado é preservado de lado");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
