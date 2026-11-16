use std::cmp::{Ordering, Reverse};
use std::collections::HashSet;

use torrential_core::name::{self, year};

const STOPWORDS: &[&str] = &[
    "a", "an", "and", "at", "by", "de", "der", "die", "das", "el", "for", "in", "la", "le", "of", "on", "or", "the",
    "to", "with",
];

#[derive(Clone, Debug)]
pub(crate) struct Terms {
    text: String,
    words: Vec<String>,
    year: Option<u16>,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) enum YearMatch {
    Other,
    Absent,
    Same,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) struct Relevance {
    pub matched: u16,
    pub year: YearMatch,
}

impl Terms {
    pub fn parse(text: &str) -> Terms {
        let text = torrential_core::text::sanitize(text).to_lowercase();
        let mut words: Vec<String> = Vec::new();
        for word in tokens(&text) {
            if !words.contains(&word) {
                words.push(word);
            }
        }
        let year = words.iter().find_map(|w| year(w));
        Terms { text, words, year }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn words(&self) -> &[String] {
        &self.words
    }

    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }

    pub fn relevance(&self, name: &str) -> Relevance {
        let name_tokens = tokens(name);
        let matched = self.matches(name, &name_tokens).iter().filter(|hit| **hit).count();
        let year = match self.year {
            None => YearMatch::Absent,
            Some(wanted) => {
                let years: Vec<u16> = name_tokens.iter().filter_map(|t| year(t)).collect();
                if years.contains(&wanted) {
                    YearMatch::Same
                } else if years.is_empty() {
                    YearMatch::Absent
                } else {
                    YearMatch::Other
                }
            }
        };
        Relevance { matched: u16::try_from(matched).unwrap_or(u16::MAX), year }
    }

    pub fn extra_words(&self, name: &str) -> u16 {
        let name_tokens = tokens(name);
        let matched = self.matches(name, &name_tokens).iter().filter(|hit| **hit).count();
        u16::try_from(name_tokens.len().saturating_sub(matched)).unwrap_or(u16::MAX)
    }

    /// Whether a name is of the query's title: the words before its first year, season,
    /// episode or resolution are the query's, as `The.Bear.S03E01.1080p` is of `the bear s03e01`
    /// and `The.Island.with.Bear.Grylls.S03E01` is not.
    pub fn is_title_of(&self, name: &str) -> bool {
        let title = |s: &str| tokens(s).into_iter().take_while(|t| !is_marker(t)).collect::<String>();
        let wanted = title(&self.text);
        !wanted.is_empty() && title(name) == wanted
    }

    pub fn is_relevant(&self, name: &str) -> bool {
        let hits = self.matches(name, &tokens(name));
        let all_stopwords = self.words.iter().all(|w| STOPWORDS.contains(&w.as_str()));
        self.words.iter().zip(hits).any(|(word, hit)| hit && (all_stopwords || !STOPWORDS.contains(&word.as_str())))
    }

    fn matches(&self, name: &str, name_tokens: &[String]) -> Vec<bool> {
        let mut set: HashSet<String> = name_tokens.iter().cloned().collect();
        for run in 2..=3 {
            set.extend(name_tokens.windows(run).filter(|w| joinable(w)).map(|w| w.concat()));
        }
        let folded = name.to_lowercase();
        let mut hits: Vec<bool> = self
            .words
            .iter()
            .map(|w| if w.is_ascii() { set.contains(w) } else { folded.contains(w.as_str()) })
            .collect();
        for run in 2..=3 {
            for (start, window) in self.words.windows(run).enumerate() {
                if joinable(window) && set.contains(&window.concat()) {
                    hits[start..start + run].fill(true);
                }
            }
        }
        hits
    }
}

pub(crate) fn newer_first(a: &str, b: &str) -> Ordering {
    #[derive(PartialEq, Eq, PartialOrd, Ord)]
    enum Word {
        Number(Reverse<u64>),
        Text(String),
    }
    let words = |name: &str| -> Vec<Word> {
        tokens(name).into_iter().map(|t| t.parse().map_or(Word::Text(t), |n| Word::Number(Reverse(n)))).collect()
    };
    words(a).cmp(&words(b))
}

fn joinable(run: &[String]) -> bool {
    let number = |t: &String| t.bytes().all(|b| b.is_ascii_digit());
    run.windows(2).all(|pair| !(number(&pair[0]) && number(&pair[1])))
}

fn tokens(s: &str) -> Vec<String> {
    name::words(s).map(str::to_lowercase).collect()
}

/// A token that ends a release's title: a year, `s03`, `s03e01`, `3x01` or `1080p`.
fn is_marker(token: &str) -> bool {
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let season = token.strip_prefix('s').is_some_and(|rest| {
        let (season, episode) = rest.split_once('e').unwrap_or((rest, "1"));
        digits(season) && episode.split('e').all(digits)
    });
    let crossed = token.split_once('x').is_some_and(|(s, e)| digits(s) && digits(e));
    let resolution = token.strip_suffix('p').is_some_and(|n| digits(n) && (3..=4).contains(&n.len()));
    year(token).is_some() || season || crossed || resolution
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every query word is in the name.
    fn covers(terms: &Terms, name: &str) -> bool {
        !terms.words.is_empty() && usize::from(terms.relevance(name).matched) == terms.words.len()
    }

    #[test]
    fn counts_words_and_joined_words() {
        let terms = Terms::parse("Arch Linux ISO");
        let r = terms.relevance("archlinux-2026.10.01-x86_64.iso");
        assert_eq!(r, Relevance { matched: 3, year: YearMatch::Absent });
        assert!(covers(&terms, "archlinux-2026.10.01-x86_64.iso"));
        assert!(!covers(&terms, "ubuntu-24.04.5.1-desktop-amd64.iso"));
    }

    #[test]
    fn ranks_the_query_year_above_a_missing_one_above_another() {
        let terms = Terms::parse("night of the living dead 1968");
        let same = terms.relevance("Night of the Living Dead (1968) 1080p");
        let absent = terms.relevance("Night of the Living Dead 1080p");
        let other = terms.relevance("Night of the Living Dead (1990) 1080p");
        assert_eq!(same, Relevance { matched: 6, year: YearMatch::Same });
        assert_eq!(absent, Relevance { matched: 5, year: YearMatch::Absent });
        assert_eq!(other, Relevance { matched: 5, year: YearMatch::Other });
        assert!(same > absent && absent > other);
    }

    #[test]
    fn counts_the_words_a_name_adds_to_the_query() {
        let terms = Terms::parse("debian 13.7 netinst");
        assert_eq!(terms.extra_words("debian-13.7.0-amd64-netinst.iso"), 3);
        assert_eq!(terms.extra_words("debian-edu-13.7.0-amd64-netinst.iso"), 4);
        assert_eq!(Terms::parse("night of the living dead 1968").extra_words("Night of the Living Dead (1968)"), 0);
    }

    #[test]
    fn a_title_is_the_words_before_a_year_season_or_resolution() {
        let bear = Terms::parse("the bear s03e01");
        assert!(bear.is_title_of("The.Bear.S03E01.Tomorrow.1080p.DSNP.WEB-DL.DDP5.1.H.264-NTb"));
        assert!(bear.is_title_of("The.Bear.2024.S03E01.1080p.FX.WEB-DL.AVC.AAC-CBGWT"));
        assert!(!bear.is_title_of("The.Island.with.Bear.Grylls.S03E01.1080p.AMZN.WEB-DL-Cinefeel"));
        assert!(!bear.is_title_of("Masha.and.the.Bear.S03E01.1080p.HMAX.WEB-DL-playWEB"));
        let dune = Terms::parse("dune part two 2024");
        assert!(dune.is_title_of("Dune-Part.Two.[2024].[1080p.MA.WEB-DL-HONE]"));
        assert!(Terms::parse("spiderman").is_title_of("Spider-Man.2002.1080p"));
        assert!(Terms::parse("show").is_title_of("Show.1x02.720p"));
        assert!(!Terms::parse("s03e01").is_title_of("The.Bear.S03E01"), "a query of only markers names no title");
    }

    #[test]
    fn newer_versions_and_dates_sort_first() {
        let mut names = [
            "Stack Exchange Data Dump (2026-06-30)",
            "debian-13.7.0-arm64-netinst.iso",
            "Stack Exchange Data Dump (2026-09-30)",
            "debian-13.10.0-amd64-netinst.iso",
            "DEBIAN-13.7.0-AMD64-netinst.iso",
        ];
        names.sort_by(|a, b| newer_first(a, b));
        assert_eq!(
            names,
            [
                "debian-13.10.0-amd64-netinst.iso",
                "DEBIAN-13.7.0-AMD64-netinst.iso",
                "debian-13.7.0-arm64-netinst.iso",
                "Stack Exchange Data Dump (2026-09-30)",
                "Stack Exchange Data Dump (2026-06-30)",
            ]
        );
    }

    #[test]
    fn joined_name_words_match_one_query_word() {
        let terms = Terms::parse("spiderman");
        assert!(covers(&terms, "Spider-Man No Way Home 2021"));
        assert_eq!(Terms::parse("wifi driver").relevance("Wi-Fi Driver Pack").matched, 2);
        assert!(!covers(&terms, "Spider Monkey Documentary"));
        assert!(covers(&Terms::parse("h264"), "Movie 2024 1080p H.264"));
    }

    #[test]
    fn numbers_side_by_side_stay_apart() {
        let area51 = Terms::parse("area 51");
        assert!(!area51.is_relevant("Some Movie 2024 1080p BluRay DTS-HD MA 5.1 x264"));
        assert_eq!(area51.relevance("Area 52 2024 DD 5.1").matched, 1);
        assert_eq!(area51.relevance("Area 51 2015").matched, 2);
        assert!(!covers(&Terms::parse("cifar 10"), "CIFAR-100 python version 1.0"));
    }

    #[test]
    fn stopwords_alone_do_not_make_a_row_relevant() {
        let terms = Terms::parse("the office");
        assert!(terms.is_relevant("The Office S01 720p"));
        assert!(!terms.is_relevant("The Matrix 1999"));
        assert!(Terms::parse("the").is_relevant("The Matrix 1999"));
    }

    #[test]
    fn non_latin_words_match_inside_longer_runs() {
        let terms = Terms::parse("進撃の巨人");
        assert!(covers(&terms, "[Group] 進撃の巨人 The Final Season 01"));
        assert_eq!(Terms::parse("  Ubuntu\t24.04 ").text(), "ubuntu 24.04");
    }
}
