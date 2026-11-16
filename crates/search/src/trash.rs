//! How release names stand in TRaSH Guides (https://trash-guides.info): its Radarr and Sonarr
//! custom formats list groups and titles to avoid, and rank groups in tiers. They are bundled
//! at one commit by `scripts/trash/update.py`, under the MIT license in `trash/LICENSE`.
//!
//! Only release group and title conditions are kept. A tier format in Radarr also checks the
//! source and resolution; here a group in any tier list counts at its best tier. Names that
//! look like a season or an episode are judged by Sonarr's formats, all others by Radarr's.

use std::sync::LazyLock;

use fancy_regex::Regex;
use serde::Deserialize;

const FORMATS: &str = include_str!("trash/formats.json");

/// How a release's name stands in the guides.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Standing {
    /// Its group or title is on a list of releases to avoid.
    Avoid,
    /// On no list.
    Unlisted,
    /// Its group is ranked; 1 is the best tier.
    Tier(u8),
}

impl Standing {
    pub fn is_avoid(self) -> bool {
        self == Self::Avoid
    }

    /// Higher is a better tier; zero for anything not ranked.
    pub fn tier_rank(self) -> u8 {
        match self {
            Self::Tier(n) => 4u8.saturating_sub(n),
            Self::Avoid | Self::Unlisted => 0,
        }
    }
}

#[derive(Deserialize)]
struct Document {
    formats: Vec<FormatEntry>,
}

#[derive(Deserialize)]
struct FormatEntry {
    app: App,
    format: String,
    kind: Kind,
    tier: Option<u8>,
    specs: Vec<SpecEntry>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum App {
    Radarr,
    Sonarr,
}

#[derive(Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Kind {
    Penalty,
    Tier,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum On {
    Group,
    Title,
}

#[derive(Deserialize)]
struct SpecEntry {
    on: On,
    required: bool,
    negate: bool,
    name: String,
    pattern: String,
}

struct Spec {
    on: On,
    required: bool,
    negate: bool,
    regex: Regex,
}

struct Format {
    app: App,
    standing: Standing,
    specs: Vec<Spec>,
}

struct Compiled {
    formats: Vec<Format>,
    #[cfg_attr(not(test), expect(dead_code, reason = "read by the test that every format compiles"))]
    /// Formats left out, each with why. A format is kept whole or not at all, since one
    /// without some of its conditions could match releases it never meant.
    skipped: Vec<String>,
}

/// The formats that compiled. A test checks that none were skipped.
static FORMATS_COMPILED: LazyLock<Vec<Format>> = LazyLock::new(|| compile().formats);

fn compile() -> Compiled {
    let document: Document = serde_json::from_str(FORMATS).expect("the bundled TRaSH formats parse");
    let (mut formats, mut skipped) = (Vec::new(), Vec::new());
    for entry in document.formats {
        match format(&entry) {
            Ok(f) => formats.push(f),
            Err(why) => skipped.push(format!("{:?} {}: {why}", entry.app, entry.format)),
        }
    }
    Compiled { formats, skipped }
}

fn format(entry: &FormatEntry) -> Result<Format, String> {
    let standing = match (entry.kind, entry.tier) {
        (Kind::Penalty, _) => Standing::Avoid,
        (Kind::Tier, Some(n)) => Standing::Tier(n),
        (Kind::Tier, None) => return Err("a tier format with no tier".into()),
    };
    if entry.specs.is_empty() {
        return Err("no release group or title conditions".into());
    }
    let specs = entry.specs.iter().map(|s| {
        // Radarr and Sonarr match without regard to case.
        let regex = Regex::new(&format!("(?i){}", s.pattern)).map_err(|e| format!("{}: {e}", s.name))?;
        Ok(Spec { on: s.on, required: s.required, negate: s.negate, regex })
    });
    Ok(Format { app: entry.app, standing, specs: specs.collect::<Result<_, String>>()? })
}

impl Format {
    /// As Radarr reads a custom format: every required condition holds, and for each kind of
    /// condition with optional ones, at least one of those holds.
    fn matches(&self, group: &str, title: &str) -> bool {
        let holds = |s: &Spec| {
            let input = match s.on {
                On::Group => group,
                On::Title => title,
            };
            s.regex.is_match(input).unwrap_or(false) != s.negate
        };
        if !self.specs.iter().filter(|s| s.required).all(holds) {
            return false;
        }
        [On::Group, On::Title].into_iter().all(|on| {
            let mut optional = self.specs.iter().filter(|s| !s.required && s.on == on).peekable();
            optional.peek().is_none() || optional.any(holds)
        })
    }
}

/// Where a release's name stands: avoided if any list to avoid names it, else its best tier.
pub(crate) fn standing(name: &str) -> Standing {
    let app = if SERIES.is_match(name).unwrap_or(false) { App::Sonarr } else { App::Radarr };
    let group = release_group(name).unwrap_or_default();
    let matching = FORMATS_COMPILED.iter().filter(|f| f.app == app && f.matches(&group, name));
    matching.fold(Standing::Unlisted, |best, f| match (best, f.standing) {
        (Standing::Avoid, _) | (_, Standing::Avoid) => Standing::Avoid,
        (Standing::Tier(a), Standing::Tier(b)) => Standing::Tier(a.min(b)),
        (Standing::Unlisted, other) | (other, Standing::Unlisted) => other,
    })
}

/// A season or an episode, as in `S01`, `S01E02`, `1x02` or `Season 1`.
static SERIES: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bS\d{1,3}(?:[ ._-]?E\d{1,4})*\b|\b\d{1,2}x\d{2,3}\b|\bSeason[ ._-]?\d+\b")
        .expect("a valid pattern")
});

/// Endings posters add after the group, which Radarr strips before it reads one.
const AFTER_GROUP: [&str; 9] =
    ["postbot", "xpost", "obfuscated", "scrambled", "asrequested", "repackpost", "rakuv", "whiterev", "buymore"];

/// The group that made a release: the last `-` part of its name, as in `Movie.2024.1080p.WEB-DL.x264-GRP`.
/// None when the name has no such part.
pub(crate) fn release_group(name: &str) -> Option<String> {
    let mut rest = name.trim();
    for extension in [".mkv", ".mp4", ".avi", ".m4v", ".ts", ".nzb"] {
        if rest.len() > extension.len() && rest[rest.len() - extension.len()..].eq_ignore_ascii_case(extension) {
            rest = &rest[..rest.len() - extension.len()];
        }
    }
    // A trailing `[site]` tag names where it was posted, not who made it.
    while let Some(open) = rest.strip_suffix(']').and_then(|r| r.rfind('[')) {
        rest = rest[..open].trim_end();
    }
    loop {
        let (head, last) = rest.rsplit_once('-')?;
        if AFTER_GROUP.iter().any(|a| last.eq_ignore_ascii_case(a)) {
            rest = head;
            continue;
        }
        let word = |w: &str| !w.is_empty() && w.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '@');
        // A few groups have dots, as `Pahe.in` and `C.A.A`; a long dotted part is the rest of a
        // name with no group, as in `Dune-Part.Two.2024.1080p`.
        let dotted = last.len() <= 8 && last.split('.').count() <= 3 && last.split('.').all(word);
        let plain = word(last) || dotted;
        let first = last.split('.').next().unwrap_or(last);
        let web_dl = first.eq_ignore_ascii_case("dl") && head.to_ascii_lowercase().ends_with("web");
        return (plain && !web_dl).then(|| last.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_group_is_the_last_dash_part_without_endings_posters_add() {
        let group = |name| release_group(name);
        assert_eq!(group("Sintel.2010.1080p.WEB.H264-CLASSiCALHD").as_deref(), Some("CLASSiCALHD"));
        assert_eq!(group("Movie.2024.2160p.UHD.BluRay.x265-GRP-Obfuscated.mkv").as_deref(), Some("GRP"));
        assert_eq!(group("Movie 2024 1080p WEB-DL x264-FLUX [rartv]").as_deref(), Some("FLUX"));
        assert_eq!(group("Movie.2024.1080p.WEB-DL"), None, "WEB-DL is not a group");
        assert_eq!(group("Movie.2024.1080p.WEB-DL.x264"), None);
        assert_eq!(group("Oppenheimer.2023.1080p.UHD.BluRay.x264.DD.5.1-Pahe.in").as_deref(), Some("Pahe.in"));
        assert_eq!(group("Movie.2024.1080p.BluRay.x264-C.A.A").as_deref(), Some("C.A.A"));
        assert_eq!(group("Dune-Part.Two.2024.1080p.AV1.DDP.Atmos"), None);
        assert_eq!(group("Tears Of Steel (2012) 1080p x264 AAC"), None);
        assert_eq!(group("Movie.2024.720p.HDTV.x264-"), None);
    }

    #[test]
    fn lists_to_avoid_win_and_ranked_groups_take_their_best_tier() {
        assert_eq!(standing("Night.of.the.Living.Dead.1968.1080p.BluRay.x264-aXXo"), Standing::Avoid);
        assert_eq!(standing("Movie.2024.1080p.WEB-DL.DDP5.1.H.264-FLUX"), Standing::Tier(1));
        assert_eq!(standing("Movie.2024.1080p.BluRay.x264-UnknownCrew"), Standing::Unlisted);
        assert_eq!(standing("Movie.2024.1080p.BluRay.x264-RARBG"), Standing::Avoid, "a retag group");
        assert_eq!(standing("Oppenheimer.2023.1080p.UHD.BluRay.x264.DD.5.1-Pahe.in"), Standing::Avoid);
        assert!(Standing::Tier(1).tier_rank() > Standing::Tier(3).tier_rank());
        assert_eq!(Standing::Unlisted.tier_rank(), Standing::Avoid.tier_rank());
    }

    #[test]
    fn every_bundled_format_compiles() {
        let compiled = compile();
        assert_eq!(compiled.skipped, Vec::<String>::new());
        assert_eq!(compiled.formats.len(), 36);
    }

    #[test]
    fn conditions_with_unbounded_lookbehind_still_apply() {
        // Upscaled's `(?<=\b[12]\d{3}\b).*\bUpscaled\b` needs a lookbehind .NET allows.
        assert_eq!(standing("Movie.2010.2160p.Upscaled.UHD.HEVC-GRP"), Standing::Avoid);
        assert_eq!(standing("Movie.2010.1080p.BluRay.x264-EVO"), Standing::Avoid);
        assert_eq!(standing("Movie.2010.1080p.WEB-DL.x264-EVO"), Standing::Unlisted, "EVO is fine on WEB-DL");
    }

    #[test]
    fn series_are_judged_by_sonarr_and_films_by_radarr() {
        // Sonarr lists EVO whatever the source; Radarr only off WEB-DL.
        assert_eq!(standing("Show.S01E02.1080p.WEB-DL.x264-EVO"), Standing::Avoid);
        assert_eq!(standing("Show.S01.1080p.WEB-DL.x264-EVO"), Standing::Avoid);
        assert_eq!(standing("Show.Season.1.1080p.WEB-DL.x264-EVO"), Standing::Avoid);
        assert_eq!(standing("Movie.2010.1080p.WEB-DL.x264-EVO"), Standing::Unlisted);
    }
}
