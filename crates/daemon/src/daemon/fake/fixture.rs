use std::collections::BTreeMap;
use std::path::Path;

use torrential_core::{Category, DownloadId, Error, ErrorCode, Flag, Release, ResultId, Selection, SourceOutcome};

use super::{Network, Posting, Search, Source, land};
use crate::daemon::admission::Cap;
use crate::daemon::book::{Intent, JobRecord};
use crate::daemon::store::Store;

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;
const DAY: i64 = 86_400;

fn id(short: &str) -> DownloadId {
    let hex = format!("{short}{}", "5d3b8e2f1c4a".repeat(3));
    hex[..40].parse().expect("fixture IDs are hex")
}

fn posting(short: &str, name: &str, files: &[(&str, u64)]) -> Posting {
    Posting {
        id: id(short),
        name: name.into(),
        files: files.iter().map(|(path, size)| (path.to_string(), *size)).collect(),
    }
}

struct Listing {
    release: Posting,
    title: &'static str,
    source: &'static str,
    flags: &'static [Flag],
    grabs: Option<u64>,
    age_days: i64,
    category: Category,
}

fn night() -> Vec<Listing> {
    let listing = |release, title, source, flags, grabs, age_days, category| Listing {
        release,
        title,
        source,
        flags,
        grabs,
        age_days,
        category,
    };
    let video = Category::Video;
    vec![
        listing(
            posting(
                "4c1e9a",
                "night_of_the_living_dead",
                &[
                    ("night_of_the_living_dead.mp4", 747 * MIB),
                    ("night_of_the_living_dead.ogv", 407 * MIB),
                    ("night_of_the_living_dead_512kb.mp4", 212 * MIB),
                    ("night_of_the_living_dead.srt", 49 * KIB),
                    ("night_of_the_living_dead_meta.xml", 3 * KIB),
                ],
            ),
            "Night of the Living Dead (1968)",
            "geek",
            &[],
            Some(1_204),
            3_300,
            video,
        ),
        listing(
            posting(
                "b7f20d",
                "night_of_the_living_dead_1968_dvd",
                &[
                    ("Night.mp4", 596_399_542),
                    ("Night.asr.srt", 52_633),
                    ("Night.gif", 325_837),
                    ("VTS_01_1.mp4", 1_072_064_102),
                    ("VTS_01_1_512Kb.mp4", 211_078_758),
                    ("VTS_01_2.mp4", 1_035_573_657),
                    ("VTS_01_2_512Kb.mp4", 204_157_747),
                    ("VTS_01_3.mp4", 432_640_819),
                    ("VTS_01_3_512Kb.mp4", 86_402_662),
                    ("night_of_the_living_dead_files.xml", 10_035),
                    ("extras/night_of_the_living_dead_meta.xml", 4_300),
                    ("extras/setup.exe", 2_202_009),
                ],
            ),
            "Night of the Living Dead (1968) [DVD]",
            "geek",
            &[],
            Some(860),
            4_400,
            video,
        ),
        listing(
            posting(
                "e3a915",
                "Night.of.the.Living.Dead.1968.1080p.BluRay.x264",
                &[
                    ("Night.of.the.Living.Dead.1968.1080p.BluRay.x264.mkv", 8_375_186_227),
                    ("Night.of.the.Living.Dead.1968.1080p.BluRay.x264.srt", 62_464),
                    ("Night.of.the.Living.Dead.1968.1080p.BluRay.x264.nfo", 2_458),
                ],
            ),
            "Night.of.the.Living.Dead.1968.1080p.BluRay.x264",
            "hydra",
            &[],
            Some(214),
            1_100,
            video,
        ),
        listing(
            posting(
                "09cd47",
                "Night of the Living Dead 4K Restoration",
                &[
                    ("Night of the Living Dead 4K Restoration.mkv", 22_870_444_441),
                    ("Restoration notes.pdf", 1_887_437),
                ],
            ),
            "Night of the Living Dead 4K Restoration",
            "hydra",
            &[],
            Some(88),
            700,
            video,
        ),
        listing(
            posting(
                "f61b2e",
                "Night.of.the.Living.Dead.2025.CAM.x264",
                &[("Night.of.the.Living.Dead.2025.CAM.x264.mkv", 1_503_238_553), ("Sample.mkv", 19_084_083)],
            ),
            "Night.of.the.Living.Dead.2025.CAM.x264",
            "planet",
            &[Flag::Cam],
            Some(41),
            60,
            video,
        ),
        listing(
            posting(
                "7a0c3f",
                "night_of_the_living_dead_colorized",
                &[
                    ("night_of_the_living_dead_colorized.mp4", 769_654_784),
                    ("night_of_the_living_dead_colorized.srt", 49_049),
                ],
            ),
            "Night of the Living Dead (1968) colorized",
            "geek",
            &[],
            Some(377),
            2_200,
            video,
        ),
        listing(
            posting(
                "c48e01",
                "Night of the Living Dead score",
                &[
                    ("01 Main title.flac", 43_201_331),
                    ("02 The cemetery.flac", 40_579_891),
                    ("03 The farmhouse.flac", 54_525_952),
                    ("04 Dawn.flac", 47_080_653),
                    ("cover.jpg", 1_258_291),
                ],
            ),
            "Night of the Living Dead, original score FLAC",
            "hydra",
            &[],
            Some(12),
            1_500,
            Category::Audio,
        ),
        listing(
            posting(
                "2d9b6a",
                "Night of the Living Dead 1968 720p WEB",
                &[("Night of the Living Dead 1968 720p WEB.mkv", 2_254_857_830), ("English.srt", 53_555)],
            ),
            "Night of the Living Dead 1968 720p WEB",
            "planet",
            &[],
            Some(0),
            1_800,
            video,
        ),
    ]
}

/// A Linux ISO every fake search knows, and the indexer that lists it in the fake engine.
struct Iso {
    id: &'static str,
    name: &'static str,
    size: u64,
    grabs: u64,
    source: &'static str,
}

const ISOS: [Iso; 2] = [
    Iso {
        id: "9b521219bc48550140a666f74a29048ed78e933b",
        name: "archlinux-2026.10.01-x86_64.iso",
        size: 1_640_497_152,
        grabs: 412,
        source: "hydra",
    },
    Iso {
        id: "e3a1f06d2b1c8a4f5e6d7c8b9a0f1e2d3c4b5a69",
        name: "ubuntu-24.04.5-desktop-amd64.iso",
        size: 6_345_887_744,
        grabs: 1873,
        source: "geek",
    },
];

impl Iso {
    /// This ISO as `source` lists it.
    fn listed_by(&self, source: &str) -> Release {
        Release { sources: vec![source.into()], ..release(self.id, self.name, self.size, self.grabs) }
    }
}

/// A row the fake indexer lists. Its `fake://` NZB link serves a fake release of one file
/// whose download ID has the same hex as the row's ID.
pub fn release(hex: &str, name: &str, size: u64, grabs: u64) -> Release {
    let id: DownloadId = hex.parse().expect("fake IDs are 40 hex characters");
    Release {
        id: ResultId::from_bytes(*id.as_bytes()),
        name: name.to_string(),
        size: Some(size),
        grabs: Some(grabs),
        published: None,
        category: Some(Category::Software),
        flags: Default::default(),
        sources: vec!["fake".into()],
        nzb_url: Some(format!("fake://{hex}")),
        span: None,
        failed: None,
    }
}

/// The Linux ISOs as the one source `fake` lists them, for a test that searches only them.
#[cfg(test)]
pub fn isos() -> Vec<Release> {
    ISOS.iter().map(|iso| iso.listed_by("fake")).collect()
}

fn nzb_url(release: &Posting) -> String {
    format!("fake://{}", release.id.to_hex())
}

pub fn search(now: i64) -> Search {
    let mut catalog: Vec<Release> = ISOS.iter().map(|iso| iso.listed_by(iso.source)).collect();
    let mut nzbs = BTreeMap::new();
    for listing in night() {
        let t = &listing.release;
        nzbs.insert(nzb_url(t), t.bytes());
        catalog.push(Release {
            id: ResultId::from_bytes(*t.id.as_bytes()),
            name: listing.title.into(),
            size: Some(t.files.iter().map(|(_, size)| size).sum()),
            grabs: listing.grabs,
            published: Some(now - listing.age_days * DAY),
            category: Some(listing.category),
            flags: listing.flags.iter().copied().collect(),
            sources: vec![listing.source.into()],
            nzb_url: Some(nzb_url(t)),
            span: None,
            failed: None,
        });
    }
    let script = vec![
        Source::answers("hydra", 200),
        Source::answers("geek", 450),
        Source::fails("dognzb", 600, SourceOutcome::Http { status: 503 }),
        Source::answers("planet", 800),
        Source::fails("slowznab", 1_500, SourceOutcome::Timeout),
    ];
    Search::scripted(catalog, script, nzbs)
}

pub fn network(rate: u64) -> Network {
    Network::new(rate)
}

struct Seed {
    release: Posting,
    intent: Intent,
    done_percent: u64,
    added_days_ago: i64,
}

pub fn seed(store: &mut Store, downloads: &Path, now: i64) -> Result<(), Error> {
    let stopped =
        Error::new(ErrorCode::StorageError, "cannot write openwebtext-2019/urlsf_subset07.tar: permission denied")
            .with_hint("check that the download folder is writable, then press p to resume");
    let seeds = [
        Seed {
            release: posting(
                "6f2c81",
                "wikipedia-en-2026-09-text",
                &[
                    ("enwiki-20260901-pages-articles-multistream.xml.bz2", 23_514_217_267),
                    ("enwiki-20260901-pages-articles-multistream-index.txt.bz2", 266_338_304),
                    ("enwiki-20260901-abstract.xml.gz", 1_181_116_006),
                ],
            ),
            intent: Intent::Running,
            done_percent: 23,
            added_days_ago: 0,
        },
        Seed {
            release: posting(
                "a90d44",
                "openwebtext-2019",
                &[
                    ("urlsf_subset05.tar", 3_328_599_654),
                    ("urlsf_subset06.tar", 3_306_087_219),
                    ("urlsf_subset07.tar", 3_341_789_593),
                    ("urlsf_subset08.tar", 3_298_538_701),
                ],
            ),
            intent: Intent::Failed { error: stopped },
            done_percent: 37,
            added_days_ago: 1,
        },
        Seed {
            release: posting(
                "91fd3b",
                "stackexchange",
                &[
                    ("stackoverflow.com-Posts.7z", 22_978_867_609),
                    ("stackoverflow.com-Comments.7z", 6_657_199_308),
                    ("stackoverflow.com-Votes.7z", 1_503_238_553),
                    ("stackoverflow.com-Users.7z", 966_367_641),
                    ("stackoverflow.com-Badges.7z", 429_496_729),
                    ("math.stackexchange.com.7z", 3_435_973_836),
                    ("superuser.com.7z", 1_717_986_918),
                    ("askubuntu.com.7z", 1_395_864_371),
                    ("serverfault.com.7z", 1_288_490_188),
                    ("unix.stackexchange.com.7z", 858_993_459),
                    ("Readme.txt", 2_150),
                ],
            ),
            intent: Intent::Paused,
            done_percent: 34,
            added_days_ago: 3,
        },
        Seed {
            release: posting(
                "5e02c8",
                "big_buck_bunny_1080p_h264.mov",
                &[("big_buck_bunny_1080p_h264.mov", 725_106_140)],
            ),
            intent: Intent::Done { finished_at: now - 5 * DAY },
            done_percent: 100,
            added_days_ago: 5,
        },
        Seed {
            release: posting(
                "c7b9e3",
                "archlinux-2026.09.01-x86_64.iso",
                &[("archlinux-2026.09.01-x86_64.iso", 1_438_646_272)],
            ),
            intent: Intent::Done { finished_at: now - 30 * DAY },
            done_percent: 100,
            added_days_ago: 31,
        },
    ];
    for seed in seeds {
        let size: u64 = seed.release.files.iter().map(|(_, size)| size).sum();
        let arrived = size / 100 * seed.done_percent;
        let layout = seed.release.layout()?;
        let landing = land(&layout, &vec![true; layout.files.len()], arrived, 0);
        let id = seed.release.id;
        let added_at = now - seed.added_days_ago * DAY - 3_600;
        let record = JobRecord {
            id,
            name: seed.release.name.clone(),
            dir: downloads.to_path_buf(),
            selection: Selection::All,
            cap: Cap::new(None, None),
            added_at,
            intent: seed.intent,
            verified: landing.verified,
            piece_map: landing.piece_map,
            files_verified: Some(landing.files),
            title: None,
            home: crate::daemon::book::Home::Folder,
        };
        store.insert(&record, &seed.release.bytes(), added_at)?;
        let resume = arrived.to_string();
        let files = record.files_verified.as_deref();
        store.save_resume(id, resume.as_bytes(), record.verified, record.piece_map, files, added_at)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::super::parse;
    use super::*;
    use crate::daemon::book::{JobEntry, Observed};
    use torrential_core::{PieceMap, Slice};

    fn filled(map: PieceMap) -> f64 {
        let wanted: Vec<u8> = map
            .slices()
            .iter()
            .filter_map(|s| match s {
                Slice::Wanted(eighths) => Some(eighths.get()),
                Slice::Unwanted => None,
            })
            .collect();
        wanted.iter().map(|e| f64::from(*e)).sum::<f64>() / (8.0 * wanted.len() as f64)
    }

    #[test]
    fn each_unfinished_seed_draws_its_progress_with_pieces_out_of_order() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&crate::daemon::db::connect(&dir.path().join("state.db")).unwrap()).unwrap();
        seed(&mut store, dir.path(), 0).unwrap();
        let mut mapped = Vec::new();
        for stored in store.jobs().unwrap() {
            let layout = parse(&stored.layout).unwrap();
            let job = JobEntry::new(stored.record, Arc::new(layout), Observed::Absent).view();
            let name = job.name;
            let Some(map) = job.progress.piece_map else {
                assert_eq!(job.status.name(), "done", "{name} has no map");
                continue;
            };
            let progress = job.progress.verified as f64 / job.progress.size as f64;
            assert!((filled(map) - progress).abs() < 0.005, "{name} draws {map} for {progress:.3}");
            let text = map.to_string();
            assert!(text.as_bytes().windows(2).any(|w| w[0] < w[1]), "{text} does not fill from the start");
            mapped.push(name);
        }
        assert_eq!(mapped, ["stackexchange", "openwebtext-2019", "wikipedia-en-2026-09-text"]);
    }
}
