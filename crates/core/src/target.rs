//! What a person names: text read as a download ID, an ID prefix, an NZB link, an `.nzb` file,
//! or a film or episode.

use std::path::PathBuf;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize};

use crate::catalog::VideoRef;
use crate::error::{Error, ErrorCode};
use crate::id::{DownloadId, HexPrefix, decode_hex};

impl FromStr for DownloadId {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Error> {
        decode_hex::<20>(s).map(Self::from_bytes).ok_or_else(|| invalid_ref(s, "a download ID is 40 hex characters"))
    }
}

impl<'de> Deserialize<'de> for DownloadId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

impl FromStr for HexPrefix {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Error> {
        let lower = s.to_ascii_lowercase();
        let (result, hex) = match lower.strip_prefix('u') {
            Some(rest) => (true, rest.to_string()),
            None => (false, lower),
        };
        let valid = (Self::MIN_LEN..=40).contains(&hex.len()) && hex.bytes().all(|b| b.is_ascii_hexdigit());
        if valid {
            Ok(Self { result, hex })
        } else {
            Err(invalid_ref(s, "an ID is 4 to 40 hex characters, with a leading u for a search result"))
        }
    }
}

/// What a user named on the command line, parsed once at the boundary.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum Target {
    /// A prefix of a download ID or search result ID, up to the whole ID.
    Prefix(String),
    /// An `http` or `https` URL that serves an NZB.
    NzbUrl(String),
    /// An absolute path to an NZB file.
    NzbFile(PathBuf),
    /// A film or an episode, such as `tt0063350` or `tt0903747:3:5`. A title ID is never an ID
    /// prefix, since `t` is not hex.
    Title(VideoRef),
}

impl Target {
    /// Parses user input in a fixed order: URL, `.nzb` path, title, ID prefix, then any other
    /// existing file. `cwd` resolves relative paths.
    pub fn parse(input: &str, cwd: &std::path::Path) -> Result<Self, Error> {
        let input = input.trim();
        let lower = input.to_ascii_lowercase();
        if lower.starts_with("http://") || lower.starts_with("https://") {
            return Ok(Self::NzbUrl(input.to_string()));
        }
        let path = cwd.join(input);
        if lower.ends_with(".nzb") {
            return if path.is_file() {
                Ok(Self::NzbFile(path))
            } else {
                Err(Error::new(ErrorCode::InvalidRef, format!("no such file: {}", path.display())))
            };
        }
        if let Ok(video) = input.parse::<VideoRef>() {
            return Ok(Self::Title(video));
        }
        match input.parse::<HexPrefix>() {
            Ok(prefix) => Ok(Self::Prefix(if prefix.result { format!("u{}", prefix.hex) } else { prefix.hex })),
            Err(_) if path.is_file() => Ok(Self::NzbFile(path)),
            Err(err) => Err(err),
        }
    }
}

/// The error for a command or a flag that takes a release and was given a title. Only `play` and
/// `get` take a title.
pub fn not_a_release(video: &VideoRef, taker: &str) -> Error {
    Error::new(ErrorCode::Usage, format!("{taker} takes a release, not a title such as {video}"))
        .with_hint(format!("run newsreel sources {video} to list its releases, and pass the ID of one"))
        .with_next(format!("newsreel sources {video}"))
}

fn invalid_ref(input: &str, why: &str) -> Error {
    let message = if input.contains(':') {
        "a URL is not a release reference unless it is an http or https link to an NZB".to_string()
    } else {
        format!("{input:?} is not a release reference: {why}")
    };
    Error::new(ErrorCode::InvalidRef, message)
        .with_hint("pass an ID from `newsreel search`, an .nzb file, or an NZB URL")
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    const ARCH: &str = "9b521219bc48550140a666f74a29048ed78e933b";

    #[test]
    fn parses_each_kind_of_target() {
        let cwd = Path::new("/nonexistent");
        assert_eq!(Target::parse("9b5212", cwd).unwrap(), Target::Prefix("9b5212".into()));
        assert_eq!(Target::parse("U9B52", cwd).unwrap(), Target::Prefix("u9b52".into()));
        assert_eq!(Target::parse(ARCH, cwd).unwrap(), Target::Prefix(ARCH.into()), "a whole ID is a prefix too");
        assert_eq!(
            Target::parse("https://example.org/api?t=get&id=7", cwd).unwrap(),
            Target::NzbUrl("https://example.org/api?t=get&id=7".into())
        );
        assert_eq!(Target::parse("tt0063350", cwd).unwrap(), Target::Title("tt0063350".parse().unwrap()));
        assert_eq!(Target::parse(" tt0903747:3:5 ", cwd).unwrap(), Target::Title("tt0903747:3:5".parse().unwrap()));
        assert_eq!(Target::parse("9b5", cwd).unwrap_err().code, ErrorCode::InvalidRef);
        assert_eq!(Target::parse("tt12", cwd).unwrap_err().code, ErrorCode::InvalidRef);
        let missing = format!("no such file: {}", cwd.join("missing.nzb").display());
        assert_eq!(Target::parse("missing.nzb", cwd).unwrap_err().message, missing);
        assert_eq!(Target::parse("ftp://example.org/a.nzb", cwd).unwrap_err().code, ErrorCode::InvalidRef);
    }

    #[test]
    fn a_file_named_like_an_id_does_not_hide_the_id() {
        let cwd = std::env::temp_dir().join(format!("newsreel-id-{}", std::process::id()));
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(cwd.join("9b5212"), b"").unwrap();
        std::fs::write(cwd.join("film"), b"").unwrap();
        let parsed = (Target::parse("9b5212", &cwd), Target::parse("film", &cwd), Target::parse("./9b5212", &cwd));
        std::fs::remove_dir_all(&cwd).unwrap();
        assert_eq!(parsed.0.unwrap(), Target::Prefix("9b5212".into()));
        assert_eq!(parsed.1.unwrap(), Target::NzbFile(cwd.join("film")));
        assert_eq!(parsed.2.unwrap(), Target::NzbFile(cwd.join("./9b5212")));
    }

    #[test]
    fn multibyte_input_is_rejected_without_panicking() {
        let err = Target::parse("abcdefgé", Path::new("/nonexistent")).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidRef);
    }
}
