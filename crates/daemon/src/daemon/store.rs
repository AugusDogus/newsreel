use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use newsreel_core::{DownloadId, Error, PieceMap, Release, ResultId, Selection, TitleLink};
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use super::admission::Cap;
use super::book::{Home, Identity, Intent, JobRecord, Linked, Play, PlayName};
use super::db::{Db, corrupt, sql_error};
use super::search_port::SavedLink;

#[derive(Clone, PartialEq, Debug)]
pub struct Tombstone {
    pub id: DownloadId,
    pub name: Option<String>,
    pub path: Option<PathBuf>,
    pub removed_at: i64,
}

pub const TOMBSTONE_SECS: i64 = 86_400;
pub const RESULTS_SECS: i64 = 7 * 86_400;
pub const LAYOUT_SECS: i64 = RESULTS_SECS;
pub const LAYOUT_BYTES: i64 = 64 << 20;

/// The deletion of a play's folder in the plays folder, on record until it is done.
#[derive(Clone, PartialEq, Debug)]
pub struct Recorded {
    pub key: i64,
    pub job: DownloadId,
    pub play: PlayName,
    /// The folder newsreel made for the play, the only one this deletes.
    pub folder: Identity,
    /// The most it takes up on disk.
    pub bytes: u64,
}

#[derive(Clone, PartialEq, Debug)]
pub struct StoredJob {
    pub record: JobRecord,
    pub layout: Vec<u8>,
}

pub struct Store {
    conn: Connection,
    path: PathBuf,
}

impl Store {
    pub fn open(db: &Db) -> Result<Self, Error> {
        Ok(Self { conn: db.open()?, path: db.path().to_path_buf() })
    }

    fn fail(&self, e: rusqlite::Error) -> Error {
        sql_error(&self.path, e)
    }

    fn write<T>(&mut self, f: impl FnOnce(&Transaction) -> rusqlite::Result<T>) -> Result<T, Error> {
        let Self { conn, path } = self;
        let tx = conn.transaction().map_err(|e| sql_error(path, e))?;
        let value = f(&tx).map_err(|e| sql_error(path, e))?;
        tx.commit().map_err(|e| sql_error(path, e))?;
        Ok(value)
    }

    pub fn jobs(&self) -> Result<Vec<StoredJob>, Error> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT j.id, j.name, j.dir, j.selection, j.max_size, j.intent, j.finished_at, j.error, j.added_at, \
                 j.verified, j.piece_map, j.files_verified, m.bytes, j.max_size_from_setup, \
                 t.title, t.span, t.release, t.played, p.watched_at, p.by_hand, p.device, p.inode FROM jobs j \
                 LEFT JOIN layout m ON m.id = j.id \
                 LEFT JOIN job_titles t ON t.job = j.id \
                 LEFT JOIN plays p ON p.job = j.id \
                 ORDER BY j.added_at",
            )
            .map_err(|e| self.fail(e))?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Vec<u8>>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, Option<i64>>(6)?,
                    r.get::<_, Option<String>>(7)?,
                    r.get::<_, i64>(8)?,
                    r.get::<_, i64>(9)?,
                    r.get::<_, Option<String>>(10)?,
                    r.get::<_, Option<String>>(11)?,
                    r.get::<_, Option<Vec<u8>>>(12)?,
                    r.get::<_, bool>(13)?,
                    (
                        r.get::<_, Option<String>>(14)?,
                        r.get::<_, Option<String>>(15)?,
                        r.get::<_, Option<String>>(16)?,
                        r.get::<_, Option<bool>>(17)?,
                    ),
                    (
                        r.get::<_, Option<i64>>(18)?,
                        r.get::<_, Option<bool>>(19)?,
                        r.get::<_, Option<i64>>(20)?,
                        r.get::<_, Option<i64>>(21)?,
                    ),
                ))
            })
            .map_err(|e| self.fail(e))?;
        let mut jobs = Vec::new();
        for row in rows {
            let (
                id,
                name,
                dir,
                selection,
                max_size,
                intent,
                finished_at,
                error,
                added_at,
                verified,
                piece_map,
                files_verified,
                bytes,
                from_setup,
                title,
                play,
            ) = row.map_err(|e| self.fail(e))?;
            let bad = |what: &str| corrupt(&self.path, &format!("job {id} has an unreadable {what}"));
            let intent = match (intent.as_str(), finished_at, error) {
                ("running", None, None) => Intent::Running,
                ("paused", None, None) => Intent::Paused,
                ("done", Some(finished_at), None) => Intent::Done { finished_at },
                ("failed", None, Some(error)) => {
                    Intent::Failed { error: serde_json::from_str(&error).map_err(|_| bad("error"))? }
                }
                _ => return Err(bad("intent")),
            };
            let record = JobRecord {
                id: id.parse().map_err(|_| bad("id"))?,
                name,
                dir: path_from(dir),
                selection: serde_json::from_str::<Selection>(&selection).map_err(|_| bad("selection"))?,
                cap: Cap { bytes: max_size as u64, from_setup },
                added_at,
                intent,
                verified: verified as u64,
                piece_map: piece_map.map(|m| m.parse()).transpose().map_err(|_| bad("piece map"))?,
                files_verified: files_verified
                    .map(|f| serde_json::from_str(&f))
                    .transpose()
                    .map_err(|_| bad("file progress"))?,
                title: linked(title).map_err(|_| bad("title link"))?,
                home: match play {
                    (Some(watched_at), Some(by_hand), Some(device), Some(inode)) => {
                        Home::Plays(Play { watched_at: at(watched_at), by_hand, folder: identity(device, inode) })
                    }
                    _ => Home::Folder,
                },
            };
            let layout = bytes.ok_or_else(|| bad("manifest, which is missing"))?;
            jobs.push(StoredJob { record, layout });
        }
        Ok(jobs)
    }

    pub fn insert(&mut self, record: &JobRecord, layout: &[u8], now: i64) -> Result<(), Error> {
        let (intent, finished_at, error) = intent_columns(&record.intent);
        self.write(|tx| {
            tx.execute(
                "INSERT INTO jobs (id, name, dir, selection, max_size, intent, finished_at, error, added_at, verified, \
                 piece_map, files_verified, max_size_from_setup) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                params![
                    record.id.to_hex(),
                    record.name,
                    path_bytes(&record.dir),
                    json(&record.selection),
                    record.cap.bytes as i64,
                    intent,
                    finished_at,
                    error,
                    record.added_at,
                    record.verified as i64,
                    record.piece_map.map(|m| m.to_string()),
                    record.files_verified.as_ref().map(json),
                    record.cap.from_setup,
                ],
            )?;
            upsert_layout(tx, record.id, layout, now)?;
            if let Some(linked) = &record.title {
                insert_link(tx, record.id, &linked.link, now)?;
            }
            if let Home::Plays(play) = record.home {
                tx.execute(
                    "INSERT INTO plays (job, watched_at, by_hand, device, inode) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        record.id.to_hex(),
                        unix(play.watched_at),
                        play.by_hand,
                        play.folder.device as i64,
                        play.folder.inode as i64
                    ],
                )?;
            }
            tx.execute("DELETE FROM tombstones WHERE id = ?1", [record.id.to_hex()])?;
            Ok(())
        })
    }

    pub fn set_intent(&mut self, id: DownloadId, intent: &Intent) -> Result<(), Error> {
        let (intent, finished_at, error) = intent_columns(intent);
        self.write(|tx| {
            tx.execute(
                "UPDATE jobs SET intent = ?2, finished_at = ?3, error = ?4 WHERE id = ?1",
                params![id.to_hex(), intent, finished_at, error],
            )
            .map(drop)
        })
    }

    pub fn record_check(
        &mut self,
        id: DownloadId,
        intent: Option<&Intent>,
        verified: u64,
        piece_map: Option<PieceMap>,
        files: &[u64],
    ) -> Result<(), Error> {
        self.write(|tx| {
            if let Some(intent) = intent {
                let (intent, finished_at, error) = intent_columns(intent);
                tx.execute(
                    "UPDATE jobs SET intent = ?2, finished_at = ?3, error = ?4 WHERE id = ?1",
                    params![id.to_hex(), intent, finished_at, error],
                )?;
            }
            tx.execute(
                "UPDATE jobs SET verified = ?2, piece_map = ?3, files_verified = ?4 WHERE id = ?1",
                params![id.to_hex(), verified as i64, piece_map.map(|m| m.to_string()), json(&files)],
            )?;
            tx.execute("DELETE FROM resume WHERE job = ?1", [id.to_hex()]).map(drop)
        })
    }

    pub fn set_selection(&mut self, id: DownloadId, selection: &Selection) -> Result<(), Error> {
        self.write(|tx| {
            tx.execute("UPDATE jobs SET selection = ?2 WHERE id = ?1", params![id.to_hex(), json(selection)]).map(drop)
        })
    }

    pub fn set_cap(&mut self, id: DownloadId, cap: Cap) -> Result<(), Error> {
        self.write(|tx| {
            tx.execute(
                "UPDATE jobs SET max_size = ?2, max_size_from_setup = ?3 WHERE id = ?1",
                params![id.to_hex(), cap.bytes as i64, cap.from_setup],
            )
            .map(drop)
        })
    }

    pub fn cache_layout(&mut self, id: DownloadId, bytes: &[u8], now: i64) -> Result<(), Error> {
        self.write(|tx| upsert_layout(tx, id, bytes, now))
    }

    /// Records that the NZB behind search result `result` is download `download`.
    pub fn save_became(&mut self, result: ResultId, download: DownloadId, now: i64) -> Result<(), Error> {
        self.write(|tx| {
            tx.execute(
                "INSERT INTO became (result, download, saved_at) VALUES (?1, ?2, ?3) \
                 ON CONFLICT (result) DO UPDATE SET download = excluded.download, saved_at = excluded.saved_at",
                params![result.to_hex(), download.to_hex(), now],
            )?;
            tx.execute("DELETE FROM became WHERE saved_at < ?1", [now - LAYOUT_SECS]).map(drop)
        })
    }

    /// The downloads that search results whose ID starts with `prefix` became.
    pub fn became(&self, prefix: &str) -> Result<Vec<DownloadId>, Error> {
        let mut stmt = self
            .conn
            .prepare("SELECT download FROM became WHERE substr(result, 1, length(?1)) = ?1")
            .map_err(|e| self.fail(e))?;
        let rows = stmt.query_map([prefix], |r| r.get::<_, String>(0)).map_err(|e| self.fail(e))?;
        let mut ids = Vec::new();
        for row in rows {
            let id = row.map_err(|e| self.fail(e))?;
            ids.push(id.parse().map_err(|_| corrupt(&self.path, "a search result names an unreadable download id"))?);
        }
        Ok(ids)
    }

    pub fn layout(&self, id: DownloadId) -> Result<Option<Vec<u8>>, Error> {
        self.conn
            .query_row("SELECT bytes FROM layout WHERE id = ?1", [id.to_hex()], |r| r.get(0))
            .optional()
            .map_err(|e| self.fail(e))
    }

    pub fn resume(&self, id: DownloadId) -> Result<Option<Vec<u8>>, Error> {
        self.conn
            .query_row("SELECT bytes FROM resume WHERE job = ?1", [id.to_hex()], |r| r.get(0))
            .optional()
            .map_err(|e| self.fail(e))
    }

    pub fn save_resume(
        &mut self,
        id: DownloadId,
        bytes: &[u8],
        verified: u64,
        piece_map: Option<PieceMap>,
        files: Option<&[u64]>,
        now: i64,
    ) -> Result<(), Error> {
        self.write(|tx| save_resume(tx, id, bytes, verified, piece_map, files, now))
    }

    pub fn finish(&mut self, id: DownloadId, finished_at: i64, bytes: &[u8], verified: u64) -> Result<(), Error> {
        self.write(|tx| {
            save_resume(tx, id, bytes, verified, None, None, finished_at)?;
            tx.execute(
                "UPDATE jobs SET intent = 'done', finished_at = ?2, error = NULL WHERE id = ?1",
                params![id.to_hex(), finished_at],
            )
            .map(drop)
        })
    }

    /// Removes a download, and puts on record that its play's folder, the one with that identity
    /// and taking up at most that many bytes, is to be deleted, both or neither. Answers the
    /// deletion's key.
    pub fn remove(
        &mut self,
        tombstone: &Tombstone,
        play: Option<(&PlayName, Identity, u64)>,
    ) -> Result<Option<i64>, Error> {
        self.write(|tx| {
            let id = tombstone.id.to_hex();
            tx.execute("DELETE FROM jobs WHERE id = ?1", [&id])?;
            let key = match play {
                Some((play, folder, bytes)) => {
                    tx.execute(
                        "INSERT INTO deletions (job, play, device, inode, bytes) VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![id, play.as_str(), folder.device as i64, folder.inode as i64, bytes as i64],
                    )?;
                    Some(tx.last_insert_rowid())
                }
                None => None,
            };
            tx.execute(
                "INSERT OR REPLACE INTO tombstones (id, name, path, removed_at) VALUES (?1, ?2, ?3, ?4)",
                params![id, tombstone.name, tombstone.path.as_deref().map(path_bytes), tombstone.removed_at],
            )?;
            tx.execute("DELETE FROM tombstones WHERE removed_at < ?1", [tombstone.removed_at - TOMBSTONE_SECS])?;
            evict_layout(tx, tombstone.removed_at)?;
            Ok(key)
        })
    }

    /// The deletions on record that are not done, oldest first.
    pub fn deletions(&self) -> Result<Vec<Recorded>, Error> {
        let mut stmt = self
            .conn
            .prepare("SELECT rowid, job, play, device, inode, bytes FROM deletions ORDER BY rowid")
            .map_err(|e| self.fail(e))?;
        let rows = stmt
            .query_map([], |r| {
                let names = (r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?);
                Ok((names, r.get(3)?, r.get(4)?, r.get::<_, i64>(5)?))
            })
            .map_err(|e| self.fail(e))?;
        let mut recorded = Vec::new();
        for row in rows {
            let ((key, job, play), device, inode, bytes) = row.map_err(|e| self.fail(e))?;
            let bad = || corrupt(&self.path, &format!("deletion {key} is unreadable"));
            let play = PlayName::parse(&play).ok_or_else(bad)?;
            let job = job.parse().map_err(|_| bad())?;
            recorded.push(Recorded { key, job, play, folder: identity(device, inode), bytes: bytes as u64 });
        }
        Ok(recorded)
    }

    /// Notes that what is left of a deletion's folder takes up `bytes`.
    pub fn left_to_delete(&mut self, key: i64, bytes: u64) -> Result<(), Error> {
        self.write(|tx| {
            tx.execute("UPDATE deletions SET bytes = ?2 WHERE rowid = ?1", params![key, bytes as i64]).map(drop)
        })
    }

    pub fn deleted(&mut self, key: i64) -> Result<(), Error> {
        self.write(|tx| tx.execute("DELETE FROM deletions WHERE rowid = ?1", [key]).map(drop))
    }

    /// The plays folder the engine used last.
    pub fn plays_folder(&self) -> Result<Option<Identity>, Error> {
        self.conn
            .query_row("SELECT device, inode FROM plays_folder", [], |r| Ok(identity(r.get(0)?, r.get(1)?)))
            .optional()
            .map_err(|e| self.fail(e))
    }

    /// Notes the plays folder the engine uses. When it used another before, the plays it had
    /// become downloads in a folder the person chose, and their folders are no longer deleted.
    /// Those not done stop with `stopped`, since their paths may now lead into the new folder.
    pub fn use_plays_folder(&mut self, identity: Identity, stopped: &Error) -> Result<(), Error> {
        self.write(|tx| {
            let before: Option<(i64, i64)> = tx
                .query_row("SELECT device, inode FROM plays_folder", [], |r| Ok((r.get(0)?, r.get(1)?)))
                .optional()?;
            if before.is_some_and(|before| before != (identity.device as i64, identity.inode as i64)) {
                stop_plays(tx, stopped)?;
                tx.execute("DELETE FROM plays", [])?;
                tx.execute("DELETE FROM deletions", [])?;
            }
            tx.execute(
                "INSERT OR REPLACE INTO plays_folder (one, device, inode) VALUES (1, ?1, ?2)",
                params![identity.device as i64, identity.inode as i64],
            )
            .map(drop)
        })
    }

    /// Stops every play not done with `stopped`, as when the plays folder cannot be used.
    pub fn stop_plays(&mut self, stopped: &Error) -> Result<(), Error> {
        self.write(|tx| stop_plays(tx, stopped))
    }

    /// Notes that someone pressed play on a play, or streamed it, at `at`.
    pub fn watched(&mut self, id: DownloadId, at: SystemTime) -> Result<(), Error> {
        self.write(|tx| {
            tx.execute("UPDATE plays SET watched_at = ?2 WHERE job = ?1", params![id.to_hex(), unix(at)]).map(drop)
        })
    }

    /// Notes that the person asked for a play by hand, so it never leaves the plays folder by itself.
    pub fn by_hand(&mut self, id: DownloadId) -> Result<(), Error> {
        self.write(|tx| tx.execute("UPDATE plays SET by_hand = 1 WHERE job = ?1", [id.to_hex()]).map(drop))
    }

    /// Links a download that already existed to the title a play joined it for.
    pub fn link(&mut self, id: DownloadId, link: &TitleLink, now: i64) -> Result<(), Error> {
        self.write(|tx| insert_link(tx, id, link, now))
    }

    pub fn played(&mut self, id: DownloadId) -> Result<(), Error> {
        self.write(|tx| tx.execute("UPDATE job_titles SET played = 1 WHERE job = ?1", [id.to_hex()]).map(drop))
    }

    pub fn tombstones(&self, now: i64) -> Result<Vec<Tombstone>, Error> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, name, path, removed_at FROM tombstones WHERE removed_at >= ?1")
            .map_err(|e| self.fail(e))?;
        let rows = stmt
            .query_map([now - TOMBSTONE_SECS], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<Vec<u8>>>(2)?, r.get(3)?))
            })
            .map_err(|e| self.fail(e))?;
        let mut out = Vec::new();
        for row in rows {
            let (id, name, path, removed_at) = row.map_err(|e| self.fail(e))?;
            out.push(Tombstone {
                id: id.parse().map_err(|_| corrupt(&self.path, "a tombstone has an unreadable id"))?,
                name,
                path: path.map(path_from),
                removed_at,
            });
        }
        Ok(out)
    }

    pub fn record_results(&mut self, saved: &[(Release, Option<SavedLink>)], now: i64) -> Result<(), Error> {
        self.write(|tx| {
            for (release, link) in saved {
                tx.execute(
                    "INSERT OR REPLACE INTO results (id, release, seen_at, link) VALUES (?1, ?2, ?3, ?4)",
                    params![release.id.to_ref(), json(release), now, link.as_ref().map(json)],
                )?;
            }
            tx.execute("DELETE FROM results WHERE seen_at < ?1", [now - RESULTS_SECS]).map(drop)
        })
    }

    pub fn results(&self, now: i64) -> Result<Vec<(Release, Option<SavedLink>)>, Error> {
        let mut stmt = self
            .conn
            .prepare("SELECT release, link FROM results WHERE seen_at >= ?1 ORDER BY seen_at DESC")
            .map_err(|e| self.fail(e))?;
        let rows = stmt
            .query_map([now - RESULTS_SECS], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)))
            .map_err(|e| self.fail(e))?;
        let mut out = Vec::new();
        for row in rows {
            let (release, link) = row.map_err(|e| self.fail(e))?;
            let unreadable = |_| corrupt(&self.path, "a search result is unreadable");
            out.push((
                serde_json::from_str(&release).map_err(unreadable)?,
                link.map(|link| serde_json::from_str(&link)).transpose().map_err(unreadable)?,
            ));
        }
        Ok(out)
    }
}

/// A file's identity as the store keeps it, in two signed columns.
/// A path as the database keeps it: the system's own bytes on Unix, and UTF-8 on Windows.
#[cfg(unix)]
fn path_bytes(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}

#[cfg(unix)]
fn path_from(bytes: Vec<u8>) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    PathBuf::from(std::ffi::OsString::from_vec(bytes))
}

#[cfg(windows)]
fn path_bytes(path: &Path) -> Vec<u8> {
    path.to_string_lossy().into_owned().into_bytes()
}

#[cfg(windows)]
fn path_from(bytes: Vec<u8>) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(&bytes).into_owned())
}

fn identity(device: i64, inode: i64) -> Identity {
    Identity { device: device as u64, inode: inode as u64 }
}

fn json(value: &impl serde::Serialize) -> String {
    serde_json::to_string(value).expect("stored values serialize")
}

fn intent_columns(intent: &Intent) -> (&'static str, Option<i64>, Option<String>) {
    match intent {
        Intent::Running => ("running", None, None),
        Intent::Paused => ("paused", None, None),
        Intent::Done { finished_at } => ("done", Some(*finished_at), None),
        Intent::Failed { error } => ("failed", None, Some(json(error))),
    }
}

fn stop_plays(tx: &Transaction, stopped: &Error) -> rusqlite::Result<()> {
    let (intent, _, error) = intent_columns(&Intent::Failed { error: stopped.clone() });
    tx.execute(
        "UPDATE jobs SET intent = ?1, finished_at = NULL, error = ?2 \
         WHERE intent != 'done' AND id IN (SELECT job FROM plays)",
        params![intent, error],
    )
    .map(drop)
}

fn insert_link(tx: &Transaction, id: DownloadId, link: &TitleLink, now: i64) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT OR REPLACE INTO job_titles (job, title, span, release, played, linked_at) \
         VALUES (?1, ?2, ?3, ?4, 0, ?5)",
        params![
            id.to_hex(),
            link.title.as_str(),
            link.span.to_string(),
            link.release.as_ref().map(|r| r.to_ref()),
            now
        ],
    )
    .map(drop)
}

type LinkColumns = (Option<String>, Option<String>, Option<String>, Option<bool>);

fn linked(columns: LinkColumns) -> Result<Option<Linked>, ()> {
    let (Some(title), Some(span), release, Some(played)) = columns else { return Ok(None) };
    let release = release.map(|r| serde_json::from_value(serde_json::Value::String(r))).transpose().map_err(drop)?;
    let link = TitleLink { title: title.parse().map_err(drop)?, span: span.parse().map_err(drop)?, release };
    Ok(Some(Linked { link, played }))
}

fn upsert_layout(tx: &Transaction, id: DownloadId, bytes: &[u8], now: i64) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT OR REPLACE INTO layout (id, bytes, saved_at) VALUES (?1, ?2, ?3)",
        params![id.to_hex(), bytes, now],
    )?;
    evict_layout(tx, now)
}

fn evict_layout(tx: &Transaction, now: i64) -> rusqlite::Result<()> {
    tx.execute("DELETE FROM layout WHERE id NOT IN (SELECT id FROM jobs) AND saved_at < ?1", [now - LAYOUT_SECS])?;
    tx.execute(
        "DELETE FROM layout WHERE id IN (\
           SELECT id FROM (\
             SELECT id, sum(length(bytes)) OVER (ORDER BY saved_at DESC, id) AS newer \
             FROM layout WHERE id NOT IN (SELECT id FROM jobs)\
           ) WHERE newer > ?1\
         )",
        [LAYOUT_BYTES],
    )
    .map(drop)
}

fn save_resume(
    tx: &Transaction,
    id: DownloadId,
    bytes: &[u8],
    verified: u64,
    piece_map: Option<PieceMap>,
    files: Option<&[u64]>,
    now: i64,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT OR REPLACE INTO resume (job, bytes, saved_at) VALUES (?1, ?2, ?3)",
        params![id.to_hex(), bytes, now],
    )?;
    let piece_map = piece_map.map(|m| m.to_string());
    tx.execute(
        "UPDATE jobs SET verified = ?2, piece_map = ?3, files_verified = ?4 WHERE id = ?1",
        params![id.to_hex(), verified as i64, piece_map, files.map(|f| json(&f))],
    )
    .map(drop)
}

/// A time as the database keeps it, in whole Unix seconds.
fn unix(at: SystemTime) -> i64 {
    at.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// A time the database kept in whole Unix seconds.
fn at(seconds: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(seconds.max(0) as u64)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::daemon::db;
    use newsreel_core::ErrorCode;

    fn open(path: &Path) -> Store {
        Store::open(&db::connect(path).unwrap()).unwrap()
    }

    fn record(n: u8) -> JobRecord {
        JobRecord {
            id: DownloadId::from_bytes([n; 20]),
            name: format!("job {n}"),
            dir: PathBuf::from("/dl"),
            selection: Selection::Patterns(vec!["*.mkv".into()]),
            cap: Cap { bytes: 7, from_setup: true },
            added_at: 100,
            intent: Intent::Running,
            verified: 0,
            piece_map: None,
            files_verified: None,
            title: None,
            home: Home::Folder,
        }
    }

    #[test]
    fn jobs_round_trip_through_every_intent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let mut store = open(&path);
        store.insert(&record(1), b"manifest", 100).unwrap();
        store
            .set_intent(
                DownloadId::from_bytes([1; 20]),
                &Intent::Failed { error: Error::new(ErrorCode::DiskFull, "full") },
            )
            .unwrap();
        store.insert(&record(3), b"manifest", 100).unwrap();
        store.finish(DownloadId::from_bytes([3; 20]), 200, b"resume", 42).unwrap();
        let map: PieceMap = format!("{}{}", "8".repeat(32), "-".repeat(32)).parse().unwrap();
        store.insert(&record(5), b"manifest", 100).unwrap();
        store.save_resume(DownloadId::from_bytes([5; 20]), b"resume", 9, Some(map), Some(&[4, 5]), 150).unwrap();
        drop(store);
        let store = open(&path);
        let jobs = store.jobs().unwrap();
        assert_eq!(jobs[0].record.intent, Intent::Failed { error: Error::new(ErrorCode::DiskFull, "full") });
        assert_eq!(jobs[0].layout, b"manifest");
        assert_eq!(jobs[1].record, JobRecord { intent: Intent::Done { finished_at: 200 }, verified: 42, ..record(3) });
        let saved = JobRecord { verified: 9, piece_map: Some(map), files_verified: Some(vec![4, 5]), ..record(5) };
        assert_eq!(jobs[2].record, saved);
        assert_eq!(store.resume(DownloadId::from_bytes([3; 20])).unwrap().as_deref(), Some(&b"resume"[..]));
    }

    #[test]
    fn a_title_link_keeps_whether_it_played_and_goes_with_its_job() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let mut store = open(&path);
        let link = TitleLink {
            title: "tt9999901".parse().unwrap(),
            span: "s1".parse().unwrap(),
            release: Some(ResultId::from_bytes([1; 20])),
        };
        let made = Linked { link: link.clone(), played: false };
        store.insert(&JobRecord { title: Some(made.clone()), ..record(1) }, b"release", 100).unwrap();
        store.insert(&record(3), b"release", 100).unwrap();
        store.link(DownloadId::from_bytes([3; 20]), &link, 110).unwrap();
        store.played(DownloadId::from_bytes([3; 20])).unwrap();
        drop(store);
        let mut store = open(&path);
        let titles: Vec<Option<Linked>> = store.jobs().unwrap().into_iter().map(|j| j.record.title).collect();
        assert_eq!(titles, [Some(made), Some(Linked { link, played: true })]);

        let gone = Tombstone { id: DownloadId::from_bytes([1; 20]), name: None, path: None, removed_at: 120 };
        store.remove(&gone, None).unwrap();
        let left: Vec<DownloadId> = store.jobs().unwrap().into_iter().map(|j| j.record.id).collect();
        assert_eq!(left, [DownloadId::from_bytes([3; 20])]);
        let links: i64 = store.conn.query_row("SELECT count(*) FROM job_titles", [], |r| r.get(0)).unwrap();
        assert_eq!(links, 1);
    }

    #[test]
    fn a_failed_check_replaces_the_progress_and_drops_the_resume_data_it_contradicts() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open(&dir.path().join("state.db"));
        let id = DownloadId::from_bytes([6; 20]);
        store.insert(&record(6), b"manifest", 100).unwrap();
        store.finish(id, 200, b"every piece", 42).unwrap();
        let failed = Intent::Failed { error: Error::new(ErrorCode::VerificationFailed, "1 damaged piece") };
        store.record_check(id, Some(&failed), 30, None, &[20, 10]).unwrap();
        let jobs = store.jobs().unwrap();
        let checked = JobRecord { intent: failed, verified: 30, files_verified: Some(vec![20, 10]), ..record(6) };
        assert_eq!(jobs[0].record, checked);
        assert_eq!(store.resume(id).unwrap(), None);
    }

    #[test]
    fn removal_leaves_a_short_tombstone() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open(&dir.path().join("state.db"));
        store.insert(&record(1), b"manifest", 100).unwrap();
        let tomb = Tombstone {
            id: DownloadId::from_bytes([1; 20]),
            name: Some("job 1".into()),
            path: Some("/dl/job 1".into()),
            removed_at: 1_000,
        };
        store.remove(&tomb, None).unwrap();
        assert!(store.jobs().unwrap().is_empty());
        assert_eq!(store.tombstones(1_000 + 60).unwrap(), [tomb]);
        assert!(store.tombstones(1_000 + TOMBSTONE_SECS + 1).unwrap().is_empty());
    }

    #[test]
    fn a_play_keeps_its_place_and_its_deletion_outlives_the_engine_until_done() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let mut store = open(&path);
        let folder = Identity { device: u64::MAX, inode: 7 };
        let play = JobRecord {
            dir: "/th/data/plays/0101010101010101-0".into(),
            home: Home::Plays(Play { watched_at: at(100), by_hand: false, folder }),
            ..record(1)
        };
        store.insert(&play, b"release", 100).unwrap();
        store.insert(&record(2), b"release", 100).unwrap();
        store.watched(play.id, at(250)).unwrap();
        store.by_hand(play.id).unwrap();
        let homes: Vec<Home> = store.jobs().unwrap().into_iter().map(|j| j.record.home).collect();
        assert_eq!(homes, [Home::Plays(Play { watched_at: at(250), by_hand: true, folder }), Home::Folder]);
        let tomb = Tombstone { id: play.id, name: None, path: None, removed_at: 300 };
        let name = PlayName::parse("0101010101010101-0").unwrap();
        let key = store.remove(&tomb, Some((&name, folder, 300))).unwrap().unwrap();
        store.left_to_delete(key, 200).unwrap();
        drop(store);
        let mut store = open(&path);
        assert_eq!(store.deletions().unwrap(), [Recorded { key, job: play.id, play: name, folder, bytes: 200 }]);
        assert_eq!(store.jobs().unwrap().len(), 1);
        store.deleted(key).unwrap();
        assert_eq!(store.deletions().unwrap(), []);
    }

    #[test]
    fn a_plays_folder_other_than_the_one_used_before_turns_its_plays_into_downloads_in_a_folder() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open(&dir.path().join("state.db"));
        let first = Identity { device: 1, inode: 2 };
        let stopped = Error::new(ErrorCode::PathConflict, "the plays folder it was in was moved");
        assert_eq!(store.plays_folder().unwrap(), None);
        store.use_plays_folder(first, &stopped).unwrap();
        let home = Home::Plays(Play { watched_at: at(100), by_hand: false, folder: Identity { device: 1, inode: 9 } });
        let done = Intent::Done { finished_at: 150 };
        store.insert(&JobRecord { home, ..record(1) }, b"release", 100).unwrap();
        store.insert(&JobRecord { home, intent: done.clone(), ..record(2) }, b"release", 100).unwrap();
        store.use_plays_folder(first, &stopped).unwrap();
        let jobs = |store: &Store| -> Vec<(Home, Intent)> {
            store.jobs().unwrap().into_iter().map(|job| (job.record.home, job.record.intent)).collect()
        };
        assert_eq!(jobs(&store), [(home, Intent::Running), (home, done.clone())], "the same folder keeps its plays");
        let moved = Identity { device: 1, inode: 3 };
        store.use_plays_folder(moved, &stopped).unwrap();
        assert_eq!(store.plays_folder().unwrap(), Some(moved));
        assert_eq!(jobs(&store), [(Home::Folder, Intent::Failed { error: stopped }), (Home::Folder, done)]);
    }

    #[test]
    fn cached_file_lists_expire_and_fit_a_budget_but_a_downloads_stays() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open(&dir.path().join("state.db"));
        let id = DownloadId::from_bytes;
        let largest = vec![7; 16 << 20];
        let t0 = 1_000_000;
        let later = t0 + LAYOUT_SECS + 1;

        store.insert(&record(1), b"tracked", t0).unwrap();
        store.insert(&record(3), b"removed", t0).unwrap();
        store.cache_layout(id([2; 20]), b"old", t0).unwrap();
        store.cache_layout(id([4; 20]), b"recent", later).unwrap();
        for n in 5..10 {
            store.cache_layout(id([n; 20]), &largest, later + i64::from(n)).unwrap();
        }
        store.remove(&Tombstone { id: id([3; 20]), name: None, path: None, removed_at: later + 10 }, None).unwrap();

        let kept: Vec<u8> = (1..10).filter(|n| store.layout(id([*n; 20])).unwrap().is_some()).collect();
        assert_eq!(kept, [1, 6, 7, 8, 9]);
        assert_eq!(store.jobs().unwrap()[0].layout, b"tracked");
    }
}
