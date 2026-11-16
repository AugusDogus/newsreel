//! Each release's landed pieces and the CRC32 of their bytes, appended as they land. When a
//! release is added again, after a crash or a removal, the Usenet engine reads its pieces back
//! from disk and keeps the ones that still match, so files already on disk are not fetched again. The
//! owner's resume data stays the record it trusts without reading; this only saves fetches.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use torrential_core::DownloadId;

/// File index, piece index, CRC32, each little-endian.
const RECORD: usize = 12;
/// Journals untouched this long are deleted when the engine starts.
pub const KEEP: Duration = Duration::from_secs(30 * 86_400);

pub struct Journal {
    path: PathBuf,
    file: Mutex<Option<File>>,
}

impl Journal {
    pub fn new(dir: &Path, id: DownloadId) -> Self {
        Self { path: dir.join(format!("{}.pieces", id.to_hex())), file: Mutex::new(None) }
    }

    /// The last CRC recorded for each piece of files with `pieces` pieces each.
    pub fn read(&self, pieces: &[usize]) -> Vec<Vec<Option<u32>>> {
        let mut recorded: Vec<Vec<Option<u32>>> = pieces.iter().map(|&n| vec![None; n]).collect();
        let bytes = std::fs::read(&self.path).unwrap_or_default();
        // A record cut short by a crash is the last one; chunks_exact leaves it out.
        for record in bytes.as_chunks::<RECORD>().0 {
            let word = |i: usize| u32::from_le_bytes([record[i], record[i + 1], record[i + 2], record[i + 3]]);
            let (f, p, crc) = (word(0) as usize, word(4) as usize, word(8));
            if let Some(slot) = recorded.get_mut(f).and_then(|file| file.get_mut(p)) {
                *slot = Some(crc);
            }
        }
        recorded
    }

    /// Appends a landed piece. A journal that cannot be written only costs fetches later.
    pub fn record(&self, (f, p): (usize, usize), crc: u32) {
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
        record[..4].copy_from_slice(&(f as u32).to_le_bytes());
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
        assert_eq!(Journal::new(dir.path(), id).read(&[2, 1]), [vec![None, Some(9)], vec![Some(8)]]);
    }
}
