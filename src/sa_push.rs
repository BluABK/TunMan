//! The optional StreamArchiver hand-off.
//!
//! TunMan is standalone and this file is the only thing that knows another app
//! exists. It is off by default, never runs on a timer, and only ever moves in
//! one direction: an explicit button press upserts the tunnels you choose into
//! StreamArchiver's proxy pool.
//!
//! **It never deletes and never disables.** A proxy in that pool may have been
//! added by hand or be in use by a running capture; TunMan's job is to offer a
//! URL, not to curate someone else's table. Matching is by URL, which is the
//! identity StreamArchiver itself uses.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use rusqlite::Connection;

/// Where StreamArchiver keeps its database, when the setting leaves it blank.
pub fn default_db_path() -> PathBuf {
    directories::ProjectDirs::from("", "", "StreamArchiver")
        .map(|d| d.data_dir().join("streamarchiver.sqlite3"))
        .unwrap_or_default()
}

/// Resolve the configured path, falling back to the default location.
pub fn resolve_db_path(configured: &str) -> PathBuf {
    if configured.trim().is_empty() { default_db_path() } else { PathBuf::from(configured.trim()) }
}

/// What a push did, for the message shown afterwards.
#[derive(Debug, Default, PartialEq)]
pub struct PushResult {
    pub inserted: usize,
    pub updated: usize,
}

impl PushResult {
    pub fn summary(&self) -> String {
        match (self.inserted, self.updated) {
            (0, 0) => "Nothing to push".to_string(),
            (i, 0) => format!("Added {i} proxies to StreamArchiver"),
            (0, u) => format!("Updated {u} proxies in StreamArchiver"),
            (i, u) => format!("Added {i} and updated {u} proxies in StreamArchiver"),
        }
    }
}

/// Upsert `proxies` (label, url) into StreamArchiver's pool.
///
/// The database is normally open in another process, so this waits on the lock
/// rather than failing instantly, and does the whole set in one transaction so
/// a half-applied push is impossible.
pub fn push(db: &Path, proxies: &[(String, String)]) -> Result<PushResult> {
    if !db.exists() {
        bail!("no StreamArchiver database at {}", db.display());
    }
    let conn = Connection::open(db).with_context(|| format!("opening {}", db.display()))?;
    // StreamArchiver is very likely running and holding the write lock.
    conn.busy_timeout(std::time::Duration::from_secs(5))?;

    let has_table: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='proxy'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(0)
        > 0;
    if !has_table {
        bail!("that database has no proxy table — is it a StreamArchiver database?");
    }

    let mut out = PushResult::default();
    let tx = conn.unchecked_transaction()?;
    for (label, url) in proxies {
        // Match on URL: it is the identity StreamArchiver rotates on, and a
        // label is free text that either side may have edited.
        let existing: Option<i64> =
            tx.query_row("SELECT id FROM proxy WHERE url = ?1", [url], |r| r.get(0)).ok();
        match existing {
            Some(id) => {
                // Only the label. Deliberately not `enabled`, `classes` or any
                // health column: those are StreamArchiver's to manage, and a
                // push must not silently re-enable a proxy someone benched.
                tx.execute("UPDATE proxy SET label = ?1 WHERE id = ?2", (label, id))?;
                out.updated += 1;
            }
            None => {
                tx.execute(
                    "INSERT INTO proxy (label, url, enabled, classes) VALUES (?1, ?2, 1, '')",
                    (label, url),
                )?;
                out.inserted += 1;
            }
        }
    }
    tx.commit()?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sa_like_db(path: &Path) -> Connection {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE proxy (
                 id             INTEGER PRIMARY KEY,
                 label          TEXT    NOT NULL DEFAULT '',
                 url            TEXT    NOT NULL,
                 enabled        INTEGER NOT NULL DEFAULT 1,
                 classes        TEXT    NOT NULL DEFAULT '',
                 last_used_at   INTEGER NOT NULL DEFAULT 0,
                 failures       INTEGER NOT NULL DEFAULT 0,
                 cooldown_until INTEGER NOT NULL DEFAULT 0,
                 last_error     TEXT    NOT NULL DEFAULT '',
                 last_ok_at     INTEGER NOT NULL DEFAULT 0,
                 probe_json     TEXT    NOT NULL DEFAULT ''
             );",
        )
        .unwrap();
        conn
    }

    fn tmp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("TunMan-test-{name}.sqlite3"));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn a_new_proxy_is_inserted_and_a_second_push_only_updates() {
        let path = tmp("push");
        let conn = sa_like_db(&path);
        drop(conn);

        let rows = vec![("vps-fi".to_string(), "socks5h://127.0.0.1:1080".to_string())];
        assert_eq!(push(&path, &rows).unwrap(), PushResult { inserted: 1, updated: 0 });
        assert_eq!(push(&path, &rows).unwrap(), PushResult { inserted: 0, updated: 1 });

        let conn = Connection::open(&path).unwrap();
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM proxy", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1, "pushing twice must not duplicate the row");
        let _ = std::fs::remove_file(&path);
    }

    /// A benched or deliberately disabled proxy must stay that way. Re-enabling
    /// one behind the user's back would put traffic back on a tunnel
    /// StreamArchiver had already decided was bad.
    #[test]
    fn a_push_never_re_enables_or_clears_health() {
        let path = tmp("health");
        let conn = sa_like_db(&path);
        conn.execute(
            "INSERT INTO proxy (label, url, enabled, failures, cooldown_until, last_error)
             VALUES ('old', 'socks5h://127.0.0.1:1080', 0, 7, 99999, 'refused')",
            [],
        )
        .unwrap();
        drop(conn);

        push(&path, &[("vps-fi".into(), "socks5h://127.0.0.1:1080".into())]).unwrap();

        let conn = Connection::open(&path).unwrap();
        let (label, enabled, failures, cooldown, err): (String, i64, i64, i64, String) = conn
            .query_row(
                "SELECT label, enabled, failures, cooldown_until, last_error FROM proxy",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!(label, "vps-fi", "the label is the one thing a push updates");
        assert_eq!(enabled, 0, "a disabled proxy stays disabled");
        assert_eq!(failures, 7);
        assert_eq!(cooldown, 99999);
        assert_eq!(err, "refused");
        let _ = std::fs::remove_file(&path);
    }

    /// Untouched rows must survive: the pool may hold proxies that have nothing
    /// to do with TunMan.
    #[test]
    fn proxies_tunman_does_not_manage_are_left_alone() {
        let path = tmp("others");
        let conn = sa_like_db(&path);
        conn.execute(
            "INSERT INTO proxy (label, url) VALUES ('hand-added', 'http://someone:pw@host:8080')",
            [],
        )
        .unwrap();
        drop(conn);

        push(&path, &[("vps-fi".into(), "socks5h://127.0.0.1:1080".into())]).unwrap();

        let conn = Connection::open(&path).unwrap();
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM proxy", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 2);
        let kept: i64 = conn
            .query_row("SELECT COUNT(*) FROM proxy WHERE label = 'hand-added'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(kept, 1);
        let _ = std::fs::remove_file(&path);
    }

    /// Pointing this at the wrong file should say so, not create a proxy table
    /// in some unrelated database.
    #[test]
    fn a_database_without_a_proxy_table_is_refused() {
        let path = tmp("wrong");
        Connection::open(&path).unwrap().execute("CREATE TABLE other (x)", []).unwrap();
        let err = push(&path, &[("a".into(), "b".into())]).unwrap_err().to_string();
        assert!(err.contains("no proxy table"), "{err}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_missing_database_is_refused_by_path() {
        let path = tmp("absent");
        let err = push(&path, &[]).unwrap_err().to_string();
        assert!(err.contains("no StreamArchiver database"), "{err}");
    }

    #[test]
    fn the_summary_reads_naturally_for_each_shape() {
        assert_eq!(PushResult::default().summary(), "Nothing to push");
        assert_eq!(
            PushResult { inserted: 2, updated: 0 }.summary(),
            "Added 2 proxies to StreamArchiver"
        );
        assert_eq!(
            PushResult { inserted: 1, updated: 3 }.summary(),
            "Added 1 and updated 3 proxies in StreamArchiver"
        );
    }
}
