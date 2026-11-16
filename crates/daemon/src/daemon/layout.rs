//! A release's files as the daemon reads them from its manifest: checked to keep every path
//! inside its folder.

use torrential_core::text::sanitize;
use torrential_core::{DownloadId, Error, ErrorCode, ReleaseFile};

#[derive(Clone, PartialEq, Debug)]
pub struct Layout {
    pub id: DownloadId,
    pub name: String,
    pub files: Vec<LayoutFile>,
    pub single_file: bool,
    pub bytes: Vec<u8>,
    checked: (),
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LayoutFile {
    pub path: String,
    pub size: u64,
}

impl Layout {
    pub fn new(
        id: DownloadId,
        name: String,
        files: Vec<LayoutFile>,
        single_file: bool,
        bytes: Vec<u8>,
    ) -> Result<Self, Error> {
        let escapes = |p: &str| p.is_empty() || p.starts_with('/') || p.split('/').any(|c| c == ".." || c.is_empty());
        if escapes(&name) || name.contains('/') || files.iter().any(|f| escapes(&f.path)) {
            return Err(Error::new(ErrorCode::InvalidRelease, "the release has a file path outside its own folder")
                .with_hint("this release is malformed or malicious; do not download it"));
        }
        Ok(Self { id, name, files, single_file, bytes, checked: () })
    }

    pub fn total(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }

    /// Its files with sanitized paths, nothing selected and nothing verified, for choosing what
    /// to play.
    pub fn listing(&self) -> Vec<ReleaseFile> {
        (0..)
            .zip(&self.files)
            .map(|(index, f)| ReleaseFile {
                index,
                path: sanitize(&f.path),
                size: f.size,
                selected: false,
                verified: 0,
                risk: None,
            })
            .collect()
    }
}

pub type ParseLayout = fn(&[u8]) -> Result<Layout, Error>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::fake;

    #[test]
    fn paths_that_leave_the_folder_are_refused() {
        let with = |name: &str, path: &str| {
            fake::Posting { id: DownloadId::from_bytes([1; 20]), name: name.into(), files: vec![(path.into(), 1)] }
                .layout()
        };
        assert_eq!(with("set", "a/b.mkv").unwrap().files[0].path, "a/b.mkv");
        for (name, path) in
            [("set", "../etc/passwd"), ("set", "/etc/passwd"), ("..", "a"), ("a/b", "c"), ("set", "a//b")]
        {
            assert_eq!(with(name, path).unwrap_err().code, ErrorCode::InvalidRelease, "{name} {path}");
        }
    }
}
