use std::path::{Path, PathBuf};

use torrential_core::filetype::{self, FileType};
use torrential_core::text::sanitize;
use torrential_core::{Error, FileRisk, Selection};

use super::layout::{Layout, LayoutFile};
use crate::selection::{compile, some_match};

pub fn wanted(layout: &Layout, selection: &Selection) -> Result<Vec<bool>, Error> {
    let patterns = match selection {
        Selection::All => return Ok(vec![true; layout.files.len()]),
        Selection::Patterns(patterns) => patterns,
    };
    let set = compile(patterns)?;
    let wanted: Vec<bool> =
        layout.files.iter().map(|f| set.is_match(&f.path) || set.is_match(sanitize(&f.path))).collect();
    some_match(wanted, patterns, &layout.name, &layout.id.to_hex())
}

pub fn targets(layout: &Layout, dir: &Path, wanted: &[bool]) -> Vec<PathBuf> {
    layout.files.iter().zip(wanted).filter(|(_, w)| **w).map(|(f, _)| path(layout, f, dir)).collect()
}

/// Where `file` of a download in `dir` is on disk.
pub fn path(layout: &Layout, file: &LayoutFile, dir: &Path) -> PathBuf {
    dir.join(relative(layout, file))
}

/// Where `file` is inside the download's folder.
pub fn relative(layout: &Layout, file: &LayoutFile) -> PathBuf {
    if layout.single_file { PathBuf::from(&layout.name) } else { Path::new(&layout.name).join(&file.path) }
}

/// The release's folder or single file in `dir`.
pub fn root(dir: &Path, name: &str) -> PathBuf {
    dir.join(name)
}

/// Where a download writes in `dir`: at or below the release's folder or single file.
pub fn footprint(layout: &Layout, dir: &Path) -> [PathBuf; 1] {
    [root(dir, &layout.name)]
}

const EXECUTABLE: [&str; 25] = [
    "exe", "msi", "bat", "cmd", "com", "scr", "pif", "ps1", "vbs", "vbe", "js", "jse", "wsf", "hta", "jar", "sh",
    "command", "app", "dmg", "pkg", "apk", "lnk", "cpl", "run", "reg",
];

pub fn risks(layout: &Layout) -> Vec<Option<FileRisk>> {
    let media: u64 =
        layout.files.iter().filter(|f| FileType::of(&f.path).is_some_and(|t| t.media)).map(|f| f.size).sum();
    let mostly_media = media * 2 > layout.total();
    let executable = |path: &str| EXECUTABLE.contains(&filetype::extension(path).as_str());
    layout.files.iter().map(|f| (mostly_media && executable(&f.path)).then_some(FileRisk::Executable)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::fake;
    use torrential_core::{DownloadId, ErrorCode};

    fn film() -> Layout {
        fake::Posting {
            id: DownloadId::from_bytes([0x6b; 20]),
            name: "night_of_the_living_dead_dvd".into(),
            files: vec![
                ("Night.mp4".into(), 596_399_542),
                ("Night.asr.srt".into(), 52_633),
                ("extras/VTS_01_1.MP4".into(), 262_144_000),
                ("Codec Installer.exe".into(), 1_200_000),
            ],
        }
        .layout()
        .unwrap()
    }

    #[test]
    fn globs_match_across_folders_and_ignore_case() {
        let pick =
            |p: &[&str]| wanted(&film(), &Selection::Patterns(p.iter().map(|s| s.to_string()).collect())).unwrap();
        assert_eq!(pick(&["*.mp4"]), [true, false, true, false]);
        assert_eq!(pick(&["Night.mp4", "night.asr.srt"]), [true, true, false, false]);
        assert_eq!(wanted(&film(), &Selection::All).unwrap(), [true; 4]);
    }

    #[test]
    fn a_path_as_the_file_list_shows_it_picks_the_file() {
        let odd = fake::Posting {
            id: DownloadId::from_bytes([0x6c; 20]),
            name: "show".into(),
            files: vec![("Ep  01\u{202e}.mkv".into(), 10), ("Ep 02.mkv".into(), 10)],
        }
        .layout()
        .unwrap();
        let shown = globset::escape(&sanitize("Ep  01\u{202e}.mkv"));
        assert_eq!(shown, "Ep 01.mkv");
        assert_eq!(wanted(&odd, &Selection::Patterns(vec![shown])).unwrap(), [true, false]);
    }

    #[test]
    fn patterns_that_match_nothing_are_refused() {
        let err = wanted(&film(), &Selection::Patterns(vec!["*.mkv".into()])).unwrap_err();
        assert_eq!(err.code, ErrorCode::NoSelection);
        assert_eq!(err.message, "no file in night_of_the_living_dead_dvd matches \"*.mkv\"");
        assert_eq!(err.hint.as_deref(), Some("run torrential files 6b6b6b to see the paths"));
        assert_eq!(compile(&["[".into()]).unwrap_err().code, ErrorCode::Usage);
    }

    #[test]
    fn executables_in_media_releases_are_marked() {
        assert_eq!(risks(&film()), [None, None, None, Some(FileRisk::Executable)]);
        let mut tools = film();
        tools.files.retain(|f| f.path.ends_with(".exe"));
        assert_eq!(risks(&tools), [None]);
    }

    #[test]
    fn a_download_writes_at_its_folder_or_single_file() {
        let release = |hex: &str, name: &str, files: &[&str]| {
            let files = files.iter().map(|path| (path.to_string(), 1_000)).collect();
            fake::Posting { id: hex.repeat(20).parse().unwrap(), name: name.into(), files }.layout().unwrap()
        };
        let dir = Path::new("/d");
        let pack = release("ab", "Pack", &["a.mkv", "extras/b.mkv", "c.nfo"]);
        assert_eq!(footprint(&pack, dir), [PathBuf::from("/d/Pack")]);
        let single = release("11", "film.mp4", &["film.mp4"]);
        assert_eq!(footprint(&single, dir), [PathBuf::from("/d/film.mp4")]);
    }

    #[test]
    fn single_files_land_beside_folders() {
        let dir = Path::new("/d");
        assert_eq!(
            targets(&film(), dir, &[true, false, true, false]),
            [
                PathBuf::from("/d/night_of_the_living_dead_dvd/Night.mp4"),
                PathBuf::from("/d/night_of_the_living_dead_dvd/extras/VTS_01_1.MP4")
            ]
        );
        let iso = fake::Posting {
            id: DownloadId::from_bytes([1; 20]),
            name: "a.iso".into(),
            files: vec![("a.iso".into(), 5)],
        };
        assert_eq!(targets(&iso.layout().unwrap(), dir, &[true]), [PathBuf::from("/d/a.iso")]);
    }
}
