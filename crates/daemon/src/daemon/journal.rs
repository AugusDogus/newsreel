//! Each release's landed pieces and the CRC32 of their bytes, appended as they land. When a
//! release is added again, after a crash or a removal, the Usenet engine reads its pieces back
//! from disk and keeps the ones that still match, so files already on disk are not fetched again. The
//! owner's resume data stays the record it trusts without reading; this only saves fetches.
//!
//! A piece no server sent intact is recorded too, so an engine that restarts while it repairs
//! the piece from the release's PAR2 files goes on repairing it without asking the servers again.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use newsreel_core::DownloadId;

/// File index, piece index, CRC32, each little-endian. A piece no server sent intact has the
/// top bit of its file index set, and no CRC.
const RECORD: usize = 12;
const LOST: u32 = 1 << 31;
/// Journals untouched this long are deleted when the engine starts.
pub const KEEP: Duration = Duration::from_secs(30 * 86_400);

/// What a journal last recorded of a piece.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Recorded {
    /// It landed with this CRC32.
    Landed(u32),
    /// No server sent it intact.
    Lost,
}

pub struct Journal {
    path: PathBuf,
    file: Mutex<Option<File>>,
}

impl Journal {
    pub fn new(dir: &Path, id: DownloadId) -> Self {
        Self { path: dir.join(format!("{}.pieces", id.to_hex())), file: Mutex::new(None) }
    }

    /// The last record of each piece of files with `pieces` pieces each.
    pub fn read(&self, pieces: &[usize]) -> Vec<Vec<Option<Recorded>>> {
        let mut recorded: Vec<Vec<Option<Recorded>>> = pieces.iter().map(|&n| vec![None; n]).collect();
        let bytes = std::fs::read(&self.path).unwrap_or_default();
        // A record cut short by a crash is the last one; chunks_exact leaves it out.
        for record in bytes.as_chunks::<RECORD>().0 {
            let word = |i: usize| u32::from_le_bytes([record[i], record[i + 1], record[i + 2], record[i + 3]]);
            let (f, p, crc) = (word(0), word(4) as usize, word(8));
            let (f, what) = match f & LOST {
                0 => (f as usize, Recorded::Landed(crc)),
                _ => ((f & !LOST) as usize, Recorded::Lost),
            };
            if let Some(slot) = recorded.get_mut(f).and_then(|file| file.get_mut(p)) {
                *slot = Some(what);
            }
        }
        recorded
    }

    /// Appends a landed piece. A journal that cannot be written only costs fetches later.
    pub fn record(&self, (f, p): (usize, usize), crc: u32) {
        self.append(f as u32, p, crc);
    }

    /// Appends a piece no server sent intact.
    pub fn lost(&self, (f, p): (usize, usize)) {
        self.append(f as u32 | LOST, p, 0);
    }

    fn append(&self, f: u32, p: usize, crc: u32) {
        let mut file = self.file.lock().unwrap();
        if file.is_none() {
            let opened = self
                .path
                .parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|()| OpenOptions::new().create(true).append(true).open(&self.path));
            match opened {
                Ok(opened) => *file = Some(opened),
                Err(e) => {
                    crate::log::log(&format!("cannot write {}: {e}", self.path.display()));
                    return;
                }
            }
        }
        let mut record = [0u8; RECORD];
        record[..4].copy_from_slice(&f.to_le_bytes());
        record[4..8].copy_from_slice(&(p as u32).to_le_bytes());
        record[8..].copy_from_slice(&crc.to_le_bytes());
        if let Some(opened) = file.as_mut()
            && let Err(e) = opened.write_all(&record)
        {
            crate::log::log(&format!("cannot write {}: {e}", self.path.display()));
            *file = None;
        }
    }
}

/// Deletes journals not written for `keep`.
pub fn prune(dir: &Path, keep: Duration) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let cutoff = SystemTime::now() - keep;
    for entry in entries.flatten() {
        let path = entry.path();
        let stale = entry.metadata().and_then(|m| m.modified()).is_ok_and(|modified| modified < cutoff);
        if stale && path.extension().is_some_and(|e| e == "pieces") {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_last_record_of_each_piece_wins_and_a_torn_one_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let id: DownloadId = "ab".repeat(20).parse().unwrap();
        let journal = Journal::new(dir.path(), id);
        assert_eq!(journal.read(&[2, 1]), [vec![None, None], vec![None]]);
        journal.record((0, 1), 7);
        journal.record((1, 0), 8);
        journal.record((0, 1), 9);
        journal.record((5, 5), 1);
        drop(journal);
        let path = dir.path().join(format!("{}.pieces", "ab".repeat(20)));
        let mut torn = std::fs::read(&path).unwrap();
        torn.extend([0, 0, 0, 0, 0, 0]);
        std::fs::write(&path, torn).unwrap();
        let read = Journal::new(dir.path(), id).read(&[2, 1]);
        assert_eq!(read, [vec![None, Some(Recorded::Landed(9))], vec![Some(Recorded::Landed(8))]]);
    }

    #[test]
    fn a_lost_piece_stays_lost_until_it_lands() {
        let dir = tempfile::tempdir().unwrap();
        let id: DownloadId = "cd".repeat(20).parse().unwrap();
        let journal = Journal::new(dir.path(), id);
        journal.record((0, 0), 3);
        journal.lost((0, 1));
        journal.lost((1, 0));
        journal.record((1, 0), 4);
        assert_eq!(
            journal.read(&[2, 1]),
            [vec![Some(Recorded::Landed(3)), Some(Recorded::Lost)], vec![Some(Recorded::Landed(4))]]
        );
    }
}
