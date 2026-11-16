use std::collections::BTreeSet;
use std::sync::Arc;

use newsreel_core::error::Details;
use newsreel_core::{
    Arch, Category, Decoders, Decoding, Display, ErrorCode, Flag, Language, Machine, Os, Playback, Player, Preferences,
    Profile, Release, ResultId, Screen, SearchQuery, Sort, SourceOutcome, SourceReport, Viewer,
};

use super::*;
use crate::observation::Observation;

fn source(name: &str, authority: u16) -> Arc<SourceMeta> {
    Arc::new(SourceMeta { name: name.into(), authority })
}

fn hydra() -> Arc<SourceMeta> {
    source("hydra", 3)
}
fn geek() -> Arc<SourceMeta> {
    source("geek", 5)
}
fn planet() -> Arc<SourceMeta> {
    source("planet", 6)
}

/// The NZB link a row tagged `tag` carries.
fn nzb(tag: &str) -> String {
    format!("https://indexer.example/getnzb/{tag}.nzb")
}

fn row(tag: &str, name: &str) -> Observation {
    Observation::new(nzb(tag), name).unwrap()
}

fn with(mut o: Observation, grabs: Option<u64>, size: Option<u64>, published: Option<i64>) -> Observation {
    o.grabs = grabs;
    o.size = size;
    o.published = published;
    o
}

fn fresh(source: &Arc<SourceMeta>, rows: Vec<Observation>) -> Run {
    Run { source: source.clone(), answer: Answer::Fresh(Arc::new(rows)), elapsed_ms: 40 }
}

fn failed(source: &Arc<SourceMeta>, failure: Failure) -> Run {
    Run { source: source.clone(), answer: Answer::Failed(failure), elapsed_ms: 6000 }
}

fn search(query: &SearchQuery, runs: &[Run]) -> SearchResult {
    assemble(query, query.profile.clone(), &Terms::parse(&query.text), runs, 1200).unwrap().0
}

fn names(result: &SearchResult) -> Vec<&str> {
    result.releases.iter().map(|r| r.name.as_str()).collect()
}

#[test]
fn merges_one_nzb_link_listed_by_two_sources() {
    let mut from_geek = with(row("aa", "Ubuntu 24.04 Desktop"), Some(50), Some(100), Some(1000));
    from_geek.category = Some(Category::Software);
    let from_planet = with(row("aa", "Ubuntu 24.04 Desktop amd64 ISO"), Some(80), Some(101), Some(900));
    let elsewhere = row("ab", "Ubuntu 24.04 Desktop");

    let result = search(
        &SearchQuery::new("ubuntu 24.04"),
        &[fresh(&planet(), vec![from_planet]), fresh(&geek(), vec![from_geek, elsewhere])],
    );

    assert_eq!(
        result.releases[0],
        Release {
            id: result_id(&nzb("aa")),
            name: "Ubuntu 24.04 Desktop amd64 ISO".into(),
            size: Some(100),
            grabs: Some(80),
            published: Some(1000),
            category: Some(Category::Software),
            flags: BTreeSet::new(),
            sources: vec!["geek".into(), "planet".into()],
            nzb_url: None,
            span: None,
            failed: None,
        }
    );
    assert_eq!(result.total, 2, "another link to the same name stays its own result");
}

#[test]
fn a_cut_title_loses_to_the_whole_one() {
    let cut = row("ab", "Linux Command Line (Hands-On) Ubuntu Linux & Linux Scripting ...");
    let whole = row("ab", "Linux Command Line (Hands-On) Ubuntu Linux & Linux Scripting");
    let result = search(&SearchQuery::new("ubuntu"), &[fresh(&geek(), vec![cut, whole])]);
    assert_eq!(names(&result), ["Linux Command Line (Hands-On) Ubuntu Linux & Linux Scripting"]);
}

#[test]
fn nzb_links_stay_out_of_results_and_with_the_indexer_that_issued_them() {
    let keyed = "http://127.0.0.1:5076/getnzb/api/a1?apikey=k3y";
    let from_hydra = Observation::new(keyed.into(), "Debian 13 netinst").unwrap();
    let from_geek = Observation::new(keyed.into(), "Debian 13 netinst").unwrap();
    let public = row("dvd", "Debian 13 DVD");
    let q = SearchQuery::new("debian 13");

    let (shown, linked) = assemble(
        &q,
        Profile::default(),
        &Terms::parse(&q.text),
        &[fresh(&geek(), vec![from_geek, public]), fresh(&hydra(), vec![from_hydra])],
        0,
    )
    .unwrap();

    let links = |releases: Vec<Release>| -> Vec<Option<String>> { releases.into_iter().map(|r| r.nzb_url).collect() };
    assert_eq!(links(shown.releases), [None, None]);
    let issued: Vec<(Option<String>, String)> = linked.into_iter().map(|(r, source)| (r.nzb_url, source)).collect();
    assert_eq!(issued, [(Some(nzb("dvd")), "geek".into()), (Some(keyed.into()), "hydra".into())]);
}

#[test]
fn a_row_gets_a_stable_u_ref_from_its_nzb_link() {
    let url = "http://127.0.0.1:5076/getnzb/api/a1?apikey=k3y";
    let listed = || Observation::new(url.into(), "Ubuntu 26.04 LTS").unwrap();
    let q = SearchQuery::new("ubuntu");
    let (first, linked) =
        assemble(&q, Profile::default(), &Terms::parse(&q.text), &[fresh(&hydra(), vec![listed()])], 0).unwrap();
    let again = search(&q, &[fresh(&hydra(), vec![listed(), listed()])]);
    let digest: [u8; 20] = Sha1::digest(url.as_bytes()).into();
    assert_eq!(first.releases[0].id, ResultId::from_bytes(digest));
    assert!(first.releases[0].id.to_ref().starts_with('u'));
    assert_eq!(first.releases[0].nzb_url, None);
    assert_eq!((linked[0].0.nzb_url.as_deref(), linked[0].1.as_str()), (Some(url), "hydra"));
    assert_eq!(again.releases, first.releases);
}

#[test]
fn the_guard_drops_a_source_that_answered_another_question() {
    let terms = Terms::parse("ubuntu");
    let latest = vec![
        row("1", "Some Movie 2026 1080p"),
        row("2", "Another Show S01E01"),
        row("3", "Ubuntu 24.04"),
        row("4", "Album 2026 FLAC"),
    ];
    assert_eq!(vet(&terms, latest), Err(Failure::Irrelevant));

    let search_rows = vec![row("1", "Ubuntu 24.04"), row("2", "ubuntu-26.04-desktop"), row("3", "Unrelated")];
    let kept: Vec<String> = vet(&terms, search_rows).unwrap().iter().map(|o| o.name().to_string()).collect();
    assert_eq!(kept, ["Ubuntu 24.04", "ubuntu-26.04-desktop"]);
}

fn films() -> Vec<Run> {
    let mut ungrabbed = with(row("a1", "Night of the Living Dead (1968)"), None, Some(5_000), Some(100));
    ungrabbed.category = Some(Category::Video);
    let bluray = with(row("b2", "Night of the Living Dead 1968 1080p BluRay"), Some(900), Some(4_000), Some(500));
    let remake = with(row("c3", "Night of the Living Dead (1990)"), Some(5_000), Some(1_000), Some(300));
    let partial = with(row("d4", "Living Dead Night Collection"), Some(10_000), Some(3_000), None);
    let mirror = with(row("e5", "Night of the Living Dead 1968"), Some(900), Some(2_000), Some(500));
    vec![
        fresh(&hydra(), vec![ungrabbed]),
        fresh(&geek(), vec![bluray, remake, partial]),
        fresh(&planet(), vec![mirror]),
    ]
}

#[test]
fn ranks_relevance_before_popularity_then_grabs_then_extra_words() {
    let result = search(&SearchQuery::new("night of the living dead 1968"), &films());
    assert_eq!(
        names(&result),
        [
            "Night of the Living Dead 1968",
            "Night of the Living Dead 1968 1080p BluRay",
            "Night of the Living Dead (1968)",
            "Night of the Living Dead (1990)",
            "Living Dead Night Collection",
        ]
    );
}

const GB: u64 = 1_000_000_000;

fn remasters() -> Vec<Run> {
    let film = |tag, tags: &str, grabs, size| {
        let mut o = with(row(tag, &format!("Night of the Living Dead 1968 {tags}")), Some(grabs), Some(size), None);
        o.category = Some(Category::Video);
        o
    };
    let mut ungrabbed = with(row("10", "Night of the Living Dead 1968 4K Remaster"), None, Some(58 * GB), None);
    ungrabbed.category = Some(Category::Video);
    vec![
        fresh(&hydra(), vec![ungrabbed]),
        fresh(
            &geek(),
            vec![
                film("11", "2160p UHD BluRay HDR10 x265", 500, 20 * GB),
                film("12", "2160p WEB x265", 300, 9 * GB),
                film("13", "1080p BluRay x264", 200, 8 * GB),
                film("14", "1080p WEB x265", 150, 2 * GB),
                film("15", "720p BluRay x264", 100, 1_500_000_000),
                film("16", "DVDRip", 80, 700_000_000),
                film("17", "Restored", 600, GB),
            ],
        ),
    ]
}

fn machine(locale: &str, display: Option<Display>, arch: Arch) -> Machine {
    Machine { os: Os::Macos, display, arch: Some(arch), language: Language::from_locale(locale) }
}

fn mac() -> Profile {
    let panel = Display { width: 3024, height: 1964, hdr: true };
    Profile { detected: Some(machine("en-IN", Some(panel), Arch::Arm64)), ..Profile::default() }
}

fn preferring(preferences: Preferences) -> Profile {
    Profile { preferences, ..Profile::default() }
}

fn ranked(profile: Profile) -> Vec<String> {
    let q = SearchQuery { profile, ..SearchQuery::new("night of the living dead 1968") };
    names(&search(&q, &remasters()))
        .iter()
        .map(|n| n.trim_start_matches("Night of the Living Dead 1968 ").to_string())
        .collect()
}

#[test]
fn without_a_profile_grabs_lead_and_unknown_grabs_follow() {
    assert_eq!(
        ranked(Profile::default()),
        [
            "Restored",
            "2160p UHD BluRay HDR10 x265",
            "2160p WEB x265",
            "1080p BluRay x264",
            "1080p WEB x265",
            "720p BluRay x264",
            "DVDRip",
            "4K Remaster",
        ]
    );
}

#[test]
fn a_screen_ranks_its_resolution_first_and_unknown_resolutions_last() {
    assert_eq!(
        ranked(mac()),
        [
            "1080p BluRay x264",
            "1080p WEB x265",
            "720p BluRay x264",
            "2160p UHD BluRay HDR10 x265",
            "2160p WEB x265",
            "4K Remaster",
            "DVDRip",
            "Restored",
        ]
    );
    let on = |screen| {
        ranked(Profile { preferences: Preferences { screen: Some(screen), ..Preferences::default() }, ..mac() })
    };
    assert_eq!(
        on(Screen::Tv4k),
        [
            "2160p UHD BluRay HDR10 x265",
            "2160p WEB x265",
            "4K Remaster",
            "1080p BluRay x264",
            "1080p WEB x265",
            "720p BluRay x264",
            "DVDRip",
            "Restored",
        ]
    );
    assert_eq!(
        on(Screen::Phone),
        [
            "720p BluRay x264",
            "DVDRip",
            "1080p BluRay x264",
            "1080p WEB x265",
            "2160p UHD BluRay HDR10 x265",
            "2160p WEB x265",
            "4K Remaster",
            "Restored",
        ]
    );
}

#[test]
fn an_sdr_display_ranks_hdr_after_everything_it_shows_well() {
    let monitor = Display { width: 1920, height: 1080, hdr: false };
    let linux = Profile { detected: Some(machine("en_US.UTF-8", Some(monitor), Arch::X86_64)), ..Profile::default() };
    assert_eq!(
        ranked(linux),
        [
            "1080p BluRay x264",
            "1080p WEB x265",
            "720p BluRay x264",
            "2160p WEB x265",
            "4K Remaster",
            "DVDRip",
            "Restored",
            "2160p UHD BluRay HDR10 x265",
        ]
    );
}

#[test]
fn a_size_cap_or_a_full_disk_ranks_what_fits_first() {
    let expected = [
        "Restored",
        "1080p BluRay x264",
        "1080p WEB x265",
        "720p BluRay x264",
        "DVDRip",
        "2160p UHD BluRay HDR10 x265",
        "2160p WEB x265",
        "4K Remaster",
    ];
    assert_eq!(ranked(preferring(Preferences { max_size: Some(8 * GB), ..Preferences::default() })), expected);
    assert_eq!(ranked(Profile { room: Some(8 * GB), ..Profile::default() }), expected);
}

#[test]
fn a_player_ranks_what_it_may_not_play_last() {
    let for_player = |playback| ranked(preferring(Preferences { playback: Some(playback), ..Preferences::default() }));
    assert_eq!(for_player(Playback::HevcHdr), ranked(Profile::default()));
    assert_eq!(
        for_player(Playback::Hevc),
        [
            "Restored",
            "2160p WEB x265",
            "1080p BluRay x264",
            "1080p WEB x265",
            "720p BluRay x264",
            "DVDRip",
            "4K Remaster",
            "2160p UHD BluRay HDR10 x265",
        ]
    );
    assert_eq!(
        for_player(Playback::H264),
        [
            "Restored",
            "1080p BluRay x264",
            "720p BluRay x264",
            "DVDRip",
            "4K Remaster",
            "2160p UHD BluRay HDR10 x265",
            "2160p WEB x265",
            "1080p WEB x265",
        ]
    );
}

#[test]
fn every_preference_together_ranks_fit_then_screen() {
    let laptop = Preferences { screen: Some(Screen::Laptop), max_size: Some(2 * GB), playback: Some(Playback::H264) };
    assert_eq!(
        ranked(Profile { preferences: laptop, ..mac() }),
        [
            "720p BluRay x264",
            "DVDRip",
            "Restored",
            "1080p BluRay x264",
            "1080p WEB x265",
            "2160p UHD BluRay HDR10 x265",
            "2160p WEB x265",
            "4K Remaster",
        ]
    );
}

#[test]
fn a_tagged_trailer_ranks_after_the_film_on_a_screen() {
    let film = with(row("20", "Night of the Living Dead 1968"), Some(40), Some(GB), None);
    let trailer = with(row("21", "Night of the Living Dead 1968 Trailer HD 1080p"), Some(40), Some(GB / 10), None);
    let runs = [fresh(&geek(), vec![trailer, film])];
    let ranked = |profile| {
        let q = SearchQuery { profile, ..SearchQuery::new("night of the living dead 1968") };
        names(&search(&q, &runs)).iter().map(|n| n.to_string()).collect::<Vec<_>>()
    };
    let film_first = ["Night of the Living Dead 1968", "Night of the Living Dead 1968 Trailer HD 1080p"];
    assert_eq!(ranked(Profile::default()), film_first);
    assert_eq!(ranked(mac()), film_first);
}

#[test]
fn a_screen_leaves_releases_that_are_not_video_to_grabs() {
    let mut episode = with(
        row("50", "Celtics City S01E08 Chapter VIII Ubuntu 1080p AMZN WEB-DL DDP5 1 H 264-Kits"),
        Some(1),
        Some(4 * GB),
        None,
    );
    episode.category = Some(Category::Video);
    let iso = with(row("51", "ubuntu-26.04-desktop-amd64.iso"), Some(143), Some(6 * GB), None);
    let runs = [fresh(&geek(), vec![episode, iso])];
    let q = SearchQuery { profile: mac(), ..SearchQuery::new("ubuntu") };
    assert_eq!(
        names(&search(&q, &runs)),
        [
            "ubuntu-26.04-desktop-amd64.iso",
            "Celtics City S01E08 Chapter VIII Ubuntu 1080p AMZN WEB-DL DDP5 1 H 264-Kits"
        ]
    );
}

#[test]
fn the_machines_architecture_breaks_a_tie_before_grabs() {
    let runs = [fresh(
        &geek(),
        vec![
            with(row("30", "debian-13.7.0-amd64-netinst.iso"), Some(50), Some(GB), None),
            with(row("31", "debian-13.7.0-arm64-netinst.iso"), Some(10), Some(GB), None),
            with(row("32", "debian-13.7.0-source-netinst.iso"), Some(30), Some(GB), None),
        ],
    )];
    let ranked = |profile| {
        let q = SearchQuery { profile, ..SearchQuery::new("debian 13.7 netinst") };
        names(&search(&q, &runs)).iter().map(|n| n.to_string()).collect::<Vec<_>>()
    };
    let pc = Profile { detected: Some(machine("en", None, Arch::X86_64)), ..Profile::default() };
    assert_eq!(
        ranked(mac()),
        ["debian-13.7.0-arm64-netinst.iso", "debian-13.7.0-amd64-netinst.iso", "debian-13.7.0-source-netinst.iso"]
    );
    assert_eq!(
        ranked(pc),
        ["debian-13.7.0-amd64-netinst.iso", "debian-13.7.0-source-netinst.iso", "debian-13.7.0-arm64-netinst.iso"]
    );
    assert_eq!(
        ranked(Profile::default()),
        ["debian-13.7.0-amd64-netinst.iso", "debian-13.7.0-source-netinst.iso", "debian-13.7.0-arm64-netinst.iso"]
    );
}

#[test]
fn the_system_language_breaks_a_tie_before_grabs() {
    let runs = [fresh(
        &hydra(),
        vec![
            with(row("40", "Night Of The Living Dead ( 1968) Doblada Al Español FULL HD"), Some(9), None, None),
            with(row("41", "Night Of The Living Dead ( 1968) English FULL HD"), Some(3), None, None),
        ],
    )];
    let ranked = |profile| {
        let q = SearchQuery { profile, ..SearchQuery::new("night of the living dead 1968") };
        names(&search(&q, &runs)).iter().map(|n| n.to_string()).collect::<Vec<_>>()
    };
    let english_first = [
        "Night Of The Living Dead ( 1968) English FULL HD",
        "Night Of The Living Dead ( 1968) Doblada Al Español FULL HD",
    ];
    let spanish_first = [english_first[1], english_first[0]];
    let mexico = Profile { detected: Some(machine("es-MX", None, Arch::Arm64)), ..Profile::default() };
    assert_eq!(ranked(mac()), english_first);
    assert_eq!(ranked(mexico), spanish_first);
    assert_eq!(ranked(Profile::default()), spanish_first);
}

#[test]
fn a_file_too_small_for_its_tag_ranks_as_untagged_on_a_screen() {
    let laptop = Preferences { screen: Some(Screen::Laptop), ..Preferences::default() };
    let ranked = |text: &str, rows: Vec<Observation>| {
        let q = SearchQuery { profile: preferring(laptop), ..SearchQuery::new(text) };
        names(&search(&q, &[fresh(&geek(), rows)])).iter().map(|n| n.to_string()).collect::<Vec<_>>()
    };
    let sized = |tag, name, size| with(row(tag, name), Some(40), Some(size), None);
    let clip = sized("22", "Night of the Living Dead |1968| WebRip US (HD 1080p)", 8_690_000);
    let film = sized("23", "Night of the Living Dead (1968)", GB);
    assert_eq!(
        ranked("night of the living dead 1968", vec![clip, film]),
        ["Night of the Living Dead (1968)", "Night of the Living Dead |1968| WebRip US (HD 1080p)"]
    );
    let episodes = vec![
        sized("24", "Bobs Burgers S16E03", 300_000_000),
        sized("25", "Bobs Burgers S16E03 1080p HEVC x265-ELiTE", 112_500_000),
        sized("26", "Bobs Burgers S16E03 720p WEB x265-MiNX", 81_400_000),
    ];
    assert_eq!(
        ranked("bobs burgers s16e03", episodes),
        ["Bobs Burgers S16E03 1080p HEVC x265-ELiTE", "Bobs Burgers S16E03 720p WEB x265-MiNX", "Bobs Burgers S16E03"]
    );
}

#[test]
fn alternate_sorts_keep_the_same_tie_breaks() {
    let sorted = |sort| {
        let mut q = SearchQuery::new("night of the living dead 1968");
        q.sort = sort;
        names(&search(&q, &films())).into_iter().map(str::to_string).collect::<Vec<_>>()
    };
    assert_eq!(
        sorted(Sort::Grabs),
        [
            "Living Dead Night Collection",
            "Night of the Living Dead (1990)",
            "Night of the Living Dead 1968",
            "Night of the Living Dead 1968 1080p BluRay",
            "Night of the Living Dead (1968)",
        ]
    );
    assert_eq!(
        sorted(Sort::Size),
        [
            "Night of the Living Dead (1968)",
            "Night of the Living Dead 1968 1080p BluRay",
            "Living Dead Night Collection",
            "Night of the Living Dead 1968",
            "Night of the Living Dead (1990)",
        ]
    );
    assert_eq!(
        sorted(Sort::Age),
        [
            "Night of the Living Dead 1968",
            "Night of the Living Dead 1968 1080p BluRay",
            "Night of the Living Dead (1990)",
            "Night of the Living Dead (1968)",
            "Living Dead Night Collection",
        ]
    );
}

#[test]
fn filters_exclude_unknown_values_and_count_before_the_limit() {
    let mut q = SearchQuery::new("night of the living dead 1968");
    let mut runs = films();
    runs.push(fresh(
        &source("slug", 7),
        vec![with(row("f6", "Night of the Living Dead 1968 REMUX"), Some(40), None, None)],
    ));

    q.max_size = Some(3_000);
    let small = search(&q, &runs);
    assert_eq!(
        names(&small),
        ["Night of the Living Dead 1968", "Night of the Living Dead (1990)", "Living Dead Night Collection"]
    );

    q.max_size = None;
    q.min_grabs = Some(1_000);
    q.limit = 1;
    let grabbed = search(&q, &runs);
    assert_eq!(names(&grabbed), ["Night of the Living Dead (1990)"]);
    assert_eq!(grabbed.total, 2);

    q.min_grabs = None;
    q.limit = 10;
    q.category = Some(Category::Video);
    assert_eq!(names(&search(&q, &runs)), ["Night of the Living Dead (1968)"]);
}

#[test]
fn hides_adult_releases_without_eating_lookalike_titles() {
    let mut film = row("01", "xXx (2002) 1080p BluRay");
    film.category = Some(Category::Video);
    let by_category = row("02", "xXx 2002 scene pack").flag(Flag::Adult, true);
    let by_name = row("03", "Brazzers xXx 2002");
    let show = row("04", "Sex Education S04 1080p");
    let runs = [fresh(&geek(), vec![film, by_category, by_name, show])];

    let hidden = search(&SearchQuery::new("xxx 2002 sex education"), &runs);
    assert_eq!(names(&hidden), ["xXx (2002) 1080p BluRay", "Sex Education S04 1080p"]);
    assert_eq!((hidden.total, hidden.hidden_adult), (2, 2));

    let mut q = SearchQuery::new("xxx 2002 sex education");
    q.adult = true;
    let shown = search(&q, &runs);
    let adult: Vec<&str> =
        shown.releases.iter().filter(|r| r.flags.contains(&Flag::Adult)).map(|r| r.name.as_str()).collect();
    assert_eq!(adult, ["Brazzers xXx 2002", "xXx 2002 scene pack"]);
    assert_eq!((shown.total, shown.hidden_adult), (4, 0));
}

#[test]
fn flags_camera_releases_from_any_listing_name() {
    let mut cam = row("05", "Some Movie 2024 HDCAM x264");
    cam.category = Some(Category::Video);
    let result = search(&SearchQuery::new("some movie 2024"), &[fresh(&geek(), vec![cam])]);
    assert_eq!(result.releases[0].flags, BTreeSet::from([Flag::Cam]));
}

#[test]
fn trash_guides_standings_rank_groups_above_more_grabs() {
    let avoided = with(row("06", "Sintel 2010 1080p BluRay x264-aXXo"), Some(900), None, None);
    let unlisted = with(row("07", "Sintel 2010 1080p BluRay x264-NoOne"), Some(50), None, None);
    let tiered = with(row("08", "Sintel 2010 1080p WEB-DL DDP5 1 H 264-FLUX"), Some(5), None, None);
    let result = search(&SearchQuery::new("sintel 2010"), &[fresh(&geek(), vec![avoided, unlisted, tiered])]);
    assert_eq!(
        names(&result),
        [
            "Sintel 2010 1080p WEB-DL DDP5 1 H 264-FLUX",
            "Sintel 2010 1080p BluRay x264-NoOne",
            "Sintel 2010 1080p BluRay x264-aXXo"
        ]
    );
    let flags: Vec<bool> = result.releases.iter().map(|r| r.flags.contains(&Flag::Avoid)).collect();
    assert_eq!(flags, [false, false, true]);
}

#[test]
fn a_ranked_group_lifts_only_releases_of_the_query_title() {
    let other_show = with(
        row("09", "The.Island.with.Bear.Grylls.S03E01.1080p.AMZN.WEB-DL.DD.2.0.H.264-Cinefeel"),
        Some(168),
        None,
        None,
    );
    let popular = with(row("10", "The.Bear.S03E01.1080p.WEB.H264-SuccessfulCrab"), Some(37833), None, None);
    let tiered =
        with(row("11", "The.Bear.S03E01.Tomorrow.1080p.DSNP.WEB-DL.DDP5.1.H.264-NTb"), Some(11637), None, None);
    let result = search(&SearchQuery::new("the bear s03e01"), &[fresh(&geek(), vec![other_show, popular, tiered])]);
    assert_eq!(
        names(&result),
        [
            "The.Bear.S03E01.Tomorrow.1080p.DSNP.WEB-DL.DDP5.1.H.264-NTb",
            "The.Bear.S03E01.1080p.WEB.H264-SuccessfulCrab",
            "The.Island.with.Bear.Grylls.S03E01.1080p.AMZN.WEB-DL.DD.2.0.H.264-Cinefeel"
        ]
    );
}

#[test]
fn every_source_failing_is_an_error_not_an_empty_list() {
    let q = SearchQuery::new("ubuntu");
    let runs = [
        failed(&planet(), Failure::Http(503)),
        failed(&geek(), Failure::Timeout),
        failed(&hydra(), Failure::Irrelevant),
    ];
    let err = assemble(&q, Profile::default(), &Terms::parse("ubuntu"), &runs, 6000).unwrap_err();
    assert_eq!(err.code, ErrorCode::ProvidersUnavailable);
    assert_eq!(err.message, "all sources failed (hydra, geek, planet)");
    assert_eq!(
        err.details,
        Some(Details::Sources(vec![
            SourceReport { source: "hydra".into(), outcome: SourceOutcome::Irrelevant, elapsed_ms: 6000 },
            SourceReport { source: "geek".into(), outcome: SourceOutcome::Timeout, elapsed_ms: 6000 },
            SourceReport { source: "planet".into(), outcome: SourceOutcome::Http { status: 503 }, elapsed_ms: 6000 },
        ]))
    );
}

#[test]
fn one_empty_answer_is_a_successful_search_with_no_results() {
    let runs = [failed(&planet(), Failure::Http(503)), fresh(&geek(), Vec::new())];
    let result = search(&SearchQuery::new("zzqqxx"), &runs);
    assert_eq!(result.releases, []);
    assert_eq!(
        result.sources,
        [
            SourceReport { source: "geek".into(), outcome: SourceOutcome::Ok { results: 0 }, elapsed_ms: 40 },
            SourceReport { source: "planet".into(), outcome: SourceOutcome::Http { status: 503 }, elapsed_ms: 6000 },
        ]
    );
}

struct XorShift(u64);

impl XorShift {
    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % n as u64) as usize
    }

    fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            items.swap(i, self.below(i + 1));
        }
    }
}

fn crowded() -> Vec<(Arc<SourceMeta>, Vec<Observation>)> {
    let titles = [
        "Night of the Living Dead 1968",
        "Night of the Living Dead (1968) 1080p",
        "Night of the Living Dead 1990",
        "Living Dead",
        "Night of the Living Dead",
    ];
    let sources = [hydra(), geek(), planet(), source("slug", 8)];
    let mut by_source: Vec<(Arc<SourceMeta>, Vec<Observation>)> =
        sources.iter().map(|s| (s.clone(), Vec::new())).collect();
    for i in 0..60usize {
        let mut o = row(&(i % 23 + 1).to_string(), titles[i % titles.len()]);
        o.grabs = [None, Some(10), Some(10)][i % 3];
        o.size = [None, Some(700), Some(700), Some(1400)][i % 4];
        o.published = [None, Some(50), Some(50)][i % 3];
        by_source[i % sources.len()].1.push(o);
    }
    by_source
}

#[test]
fn a_thousand_shuffles_give_one_order() {
    let query = SearchQuery { limit: 100, ..SearchQuery::new("night of the living dead 1968") };
    let terms = Terms::parse(&query.text);
    let base = crowded();
    let runs_from = |groups: &[(Arc<SourceMeta>, Vec<Observation>)]| -> Vec<Run> {
        groups.iter().map(|(s, rows)| fresh(s, rows.clone())).collect()
    };
    let (expected, expected_linked) = assemble(&query, Profile::default(), &terms, &runs_from(&base), 0).unwrap();
    assert_eq!(expected.releases.len(), 23);
    let thrice = expected.releases.iter().find(|r| r.id == result_id(&nzb("1"))).unwrap();
    assert_eq!(thrice.name, "Night of the Living Dead (1968) 1080p");
    assert_eq!(thrice.sources, ["hydra", "planet", "slug"]);

    let mut rng = XorShift(0x9e37_79b9_7f4a_7c15);
    for _ in 0..1000 {
        let mut groups = base.clone();
        rng.shuffle(&mut groups);
        for (_, rows) in &mut groups {
            rng.shuffle(rows);
        }
        let (shuffled, linked) = assemble(&query, Profile::default(), &terms, &runs_from(&groups), 0).unwrap();
        assert_eq!(shuffled.releases, expected.releases);
        assert_eq!(linked, expected_linked);
        assert_eq!(shuffled.sources, expected.sources);
    }
}

fn night(namesakes: Namesakes, year: u16) -> Wanted {
    Wanted {
        id: "tt0063350".parse().unwrap(),
        name: "Night of the Living Dead".into(),
        year: Some(year),
        episode: None,
        namesakes,
        runtime: Some(std::time::Duration::from_secs(96 * 60)),
        season_episodes: None,
        countries: Default::default(),
    }
}

fn video(byte: u8, name: &str, grabs: Option<u64>, size: u64) -> Observation {
    let mut o = with(row(&format!("{byte:02x}"), name), grabs, Some(size), None);
    o.category = Some(Category::Video);
    o
}

fn bluray() -> Observation {
    video(0x60, "Night.of.the.Living.Dead.1968.1080p.BluRay.x264-GRP", Some(120), 8 * GB)
}

fn dvdrip() -> Observation {
    video(0x62, "Night of the Living Dead 1968 DVDRip XviD", Some(3), 700_000_000)
}

fn for_title(wanted: &Wanted, profile: Profile) -> SearchQuery {
    SearchQuery { wanted: Some(wanted.clone()), profile, ..SearchQuery::new(wanted.queries()[0].clone()) }
}

fn played(wanted: &Wanted, profile: Profile, runs: &[Run]) -> Vec<String> {
    names(&search(&for_title(wanted, profile), runs)).iter().map(|n| n.to_string()).collect()
}

#[test]
fn a_cam_ranks_last_even_with_the_most_grabs() {
    let cam = video(0x61, "Night of the Living Dead 1968 HDCAM x264", Some(300), 1_400_000_000);
    let runs = [fresh(&geek(), vec![cam, dvdrip(), bluray()])];
    assert_eq!(
        played(&night(Namesakes::Leading, 1968), Profile::default(), &runs),
        [
            "Night.of.the.Living.Dead.1968.1080p.BluRay.x264-GRP",
            "Night of the Living Dead 1968 DVDRip XviD",
            "Night of the Living Dead 1968 HDCAM x264",
        ]
    );
}

#[test]
fn hevc_ranks_after_h264_for_a_player_that_may_not_play_it() {
    let hevc = video(0x66, "Night.of.the.Living.Dead.1968.1080p.BluRay.x265-GRP2", Some(200), 3 * GB);
    let runs = [fresh(&geek(), vec![hevc, bluray()])];
    let for_player = |playback| {
        let profile = preferring(Preferences { playback: Some(playback), ..Preferences::default() });
        played(&night(Namesakes::Leading, 1968), profile, &runs)
    };
    assert_eq!(
        for_player(Playback::H264),
        ["Night.of.the.Living.Dead.1968.1080p.BluRay.x264-GRP", "Night.of.the.Living.Dead.1968.1080p.BluRay.x265-GRP2"]
    );
    assert_eq!(
        for_player(Playback::Hevc),
        ["Night.of.the.Living.Dead.1968.1080p.BluRay.x265-GRP2", "Night.of.the.Living.Dead.1968.1080p.BluRay.x264-GRP"]
    );
}

#[test]
fn a_remux_ranks_after_an_encode_in_its_band() {
    let remux = video(0x67, "Night.of.the.Living.Dead.1968.1080p.BluRay.REMUX.AVC.DTS-HD.MA-GRP", Some(150), 30 * GB);
    let runs = [fresh(&geek(), vec![remux, bluray()])];
    let wanted = night(Namesakes::Leading, 1968);
    assert_eq!(
        played(&wanted, Profile::default(), &runs),
        [
            "Night.of.the.Living.Dead.1968.1080p.BluRay.x264-GRP",
            "Night.of.the.Living.Dead.1968.1080p.BluRay.REMUX.AVC.DTS-HD.MA-GRP",
        ]
    );
    let raw = SearchQuery { wanted: None, ..for_title(&wanted, Profile::default()) };
    assert_eq!(
        names(&search(&raw, &runs)),
        [
            "Night.of.the.Living.Dead.1968.1080p.BluRay.REMUX.AVC.DTS-HD.MA-GRP",
            "Night.of.the.Living.Dead.1968.1080p.BluRay.x264-GRP",
        ]
    );
}

fn watching_on(width: u32, height: u32, scale: f64) -> Profile {
    Profile { player: Player::Device(Viewer::measured(width, height, scale, Playback::Hevc)), ..mac() }
}

#[test]
fn a_browser_plays_what_it_decodes_in_hardware_then_in_software_then_old_codecs() {
    let named = |byte: u8, codec: &str| {
        video(byte, &format!("Night.of.the.Living.Dead.1968.1080p.WEB.{codec}"), Some(40), 3 * GB)
    };
    let runs =
        [fresh(&geek(), vec![named(0x80, "XviD"), named(0x81, "AV1"), named(0x82, "x265"), named(0x83, "x264")])];
    let night = night(Namesakes::Leading, 1968);
    let order = |profile: Profile| {
        let result = search(&for_title(&night, profile), &runs);
        result.releases.iter().map(|r| r.name.rsplit('.').next().unwrap().to_string()).collect::<Vec<_>>()
    };
    let reported = Decoders { h264: Decoding::Software, hevc: Decoding::Hardware, av1: Decoding::No };
    let browser = |decoders| Profile {
        player: Player::Browser { playback: Playback::HevcHdr, measured: None, decoders },
        ..mac()
    };
    assert_eq!(order(browser(Some(reported))), ["x265", "x264", "AV1", "XviD"]);
    assert_eq!(order(browser(None))[3], "XviD", "a browser that reported nothing still meets XviD last");
}

#[test]
fn a_phone_plays_720p_first_a_4k_tv_2160p_and_this_mac_its_1080p() {
    let uhd = "Night of the Living Dead (1968) 2160p BRRip 5.1 10Bit x265-GRP";
    let hd = "Night of the Living Dead (1968) 1080p BRRip x264-GRP";
    let small = "Night Of The Living Dead 1968 720p BRRip x264-GRP";
    let runs = [fresh(
        &geek(),
        vec![
            video(0x93, uhd, Some(50), 4_700_000_000),
            video(0x94, hd, Some(232), 1_600_000_000),
            video(0x92, small, Some(22), 1_700_000_000),
        ],
    )];
    let wanted = night(Namesakes::Leading, 1968);
    assert_eq!(played(&wanted, watching_on(1170, 2532, 3.0), &runs), [small, hd, uhd]);
    assert_eq!(played(&wanted, watching_on(3840, 2160, 1.0), &runs), [uhd, hd, small]);
    assert_eq!(played(&wanted, mac(), &runs), [hd, small, uhd]);
}

#[test]
fn a_phone_plays_a_copy_its_screen_shows_before_a_remux_with_more_grabs() {
    let remux = "Night.of.the.Living.Dead.1968.1080p.BluRay.REMUX.AVC.DTS-HD.MA-GRP";
    let small = "Night Of The Living Dead 1968 720p BRRip x264-GRP";
    let runs =
        [fresh(&geek(), vec![video(0x97, remux, Some(150), 20 * GB), video(0x98, small, Some(3), 1_700_000_000)])];
    let wanted = night(Namesakes::Leading, 1968);
    assert_eq!(played(&wanted, watching_on(1170, 2532, 3.0), &runs), [small, remux]);
    // A Usenet release has no swarm to weigh, so the copy that streams within its band leads
    // on every screen.
    assert_eq!(played(&wanted, watching_on(3840, 2160, 1.0), &runs), [small, remux], "28 Mbit/s is past the band");
    assert_eq!(played(&wanted, mac(), &runs), [small, remux]);
}

fn friday() -> Wanted {
    Wanted {
        id: "tt0032599".parse().unwrap(),
        name: "His Girl Friday".into(),
        year: Some(1940),
        episode: None,
        namesakes: Namesakes::None,
        runtime: Some(std::time::Duration::from_secs(92 * 60)),
        season_episodes: None,
        countries: Default::default(),
    }
}

#[test]
fn a_small_encode_that_streams_comfortably_is_in_band() {
    let criterion = "His Girl Friday (1940) Criterion + Extras (1080p BluRay x265 HEVC 10bit AAC 1.0";
    let runs = [fresh(
        &geek(),
        vec![
            video(0x80, criterion, Some(31), 5_600_000_000),
            video(0x81, "His Girl Friday (1940) 1080p BRRip x264-GRP", Some(54), 1_500_000_000),
        ],
    )];
    assert_eq!(
        played(&friday(), Profile::default(), &runs),
        ["His Girl Friday (1940) 1080p BRRip x264-GRP", criterion]
    );
    let runs = [fresh(
        &geek(),
        vec![
            video(0x92, "Night Of The Living Dead 1968 720p BRRip x264-GRP", Some(22), 1_700_000_000),
            video(0x93, "Night of the Living Dead (1968) 2160p BRRip 5.1 10Bit x265-GRP", Some(50), 4_700_000_000),
            video(0x94, "Night of the Living Dead (1968) 1080p BRRip x264-GRP", Some(232), 1_600_000_000),
        ],
    )];
    assert_eq!(
        played(&night(Namesakes::Leading, 1968), Profile::default(), &runs),
        [
            "Night of the Living Dead (1968) 1080p BRRip x264-GRP",
            "Night of the Living Dead (1968) 2160p BRRip 5.1 10Bit x265-GRP",
            "Night Of The Living Dead 1968 720p BRRip x264-GRP",
        ]
    );
}

#[test]
fn a_remux_that_fits_the_screen_plays_after_a_copy_that_streams() {
    let remux = "Night of the Living Dead (1968) 4K UHD Blu-ray Remux";
    let runs = [fresh(
        &hydra(),
        vec![video(0x95, remux, None, 59 * GB), video(0x96, "Night of the Living Dead (1968)", None, 1_200_000_000)],
    )];
    let on_4k = preferring(Preferences { screen: Some(Screen::Tv4k), ..Preferences::default() });
    assert_eq!(played(&night(Namesakes::Leading, 1968), on_4k, &runs), ["Night of the Living Dead (1968)", remux]);
}

fn sintel() -> Wanted {
    Wanted {
        id: "tt1727587".parse().unwrap(),
        name: "Sintel".into(),
        year: Some(2010),
        episode: None,
        namesakes: Namesakes::None,
        runtime: Some(std::time::Duration::from_secs(15 * 60)),
        season_episodes: None,
        countries: Default::default(),
    }
}

#[test]
fn the_more_grabbed_release_wins_a_tie_in_play_order() {
    let runs = [
        fresh(&geek(), vec![video(0x82, "Sintel", Some(3), 500_000_000)]),
        fresh(&planet(), vec![video(0x83, "Sintel.2010.HDRip.XviD.AC3-ViSiON", Some(1), 300_000_000)]),
    ];
    assert_eq!(played(&sintel(), Profile::default(), &runs), ["Sintel", "Sintel.2010.HDRip.XviD.AC3-ViSiON"]);
}

#[test]
fn a_trailer_is_not_the_film() {
    let runs = [fresh(
        &hydra(),
        vec![
            video(0x84, "Night Of The Living Dead (1968) Trailer HD 1080p", None, 300_000_000),
            video(0x64, "Night of the Living Dead (1968)", None, 1_200_000_000),
        ],
    )];
    assert_eq!(
        played(&night(Namesakes::Leading, 1968), Profile::default(), &runs),
        ["Night of the Living Dead (1968)"]
    );
    let runs = [fresh(
        &hydra(),
        vec![
            video(0x85, "His Girl Friday (1940) Trailer HD", None, 800_000_000),
            video(0x86, "His Girl Friday (1940)", None, 2_300_000_000),
        ],
    )];
    assert_eq!(played(&friday(), Profile::default(), &runs), ["His Girl Friday (1940)"]);
    let boys = Wanted {
        id: "tt0290988".parse().unwrap(),
        name: "Trailer Park Boys".into(),
        year: Some(2001),
        episode: Some(newsreel_core::catalog::Episode { season: 1, episode: 1 }),
        namesakes: Namesakes::None,
        runtime: Some(std::time::Duration::from_secs(25 * 60)),
        season_episodes: Some(6),
        countries: Default::default(),
    };
    let runs = [fresh(&geek(), vec![video(0x87, "Trailer.Park.Boys.S01E01.720p.WEB.x264", Some(40), 500_000_000)])];
    assert_eq!(played(&boys, Profile::default(), &runs), ["Trailer.Park.Boys.S01E01.720p.WEB.x264"]);
}

#[test]
fn a_plainly_named_release_plays_before_an_alternate_version() {
    let flesh = "Night of the Living Dead (1968) - The Flesh Harvest 4k re-sound design (2024)";
    let runs = [fresh(
        &hydra(),
        vec![
            video(0x88, flesh, None, 15 * GB),
            video(0x89, "Night of the Living Dead (1968) [colorised, 4k, 60FPS]", None, 6_900_000_000),
            video(0x8a, "Night Of The Living Dead (1968) Synthwave Horror Cut", None, 1_900_000_000),
            video(0x8b, "Night Of The Living Dead 1968 Svengoolie", None, 2_700_000_000),
            video(0x8c, "Night Of The Living Dead (1968) Colorized", None, 1_200_000_000),
            video(0x64, "Night of the Living Dead (1968)", None, 1_200_000_000),
        ],
    )];
    let screen = preferring(Preferences { screen: Some(Screen::Tv), ..Preferences::default() });
    assert_eq!(
        played(&night(Namesakes::Leading, 1968), screen, &runs),
        [
            "Night of the Living Dead (1968)",
            "Night of the Living Dead (1968) [colorised, 4k, 60FPS]",
            "Night Of The Living Dead (1968) Colorized",
            "Night Of The Living Dead 1968 Svengoolie",
            "Night Of The Living Dead (1968) Synthwave Horror Cut",
            flesh,
        ]
    );
}

#[test]
fn a_release_in_another_language_plays_after_the_original() {
    let english = Profile { detected: Some(machine("en-US", None, Arch::Arm64)), ..Profile::default() };
    let dub = "Night Of The Living Dead ( 1968) Doblada Al Español FULL HD";
    let runs = [
        fresh(&hydra(), vec![video(0x8d, dub, None, 3_600_000_000)]),
        fresh(
            &geek(),
            vec![
                video(0x8e, "Night of the Living Dead (1968) 1080p Ita Eng MIRCrew", Some(6), 2_200_000_000),
                video(0x8f, "Night of the Living Dead 1968 1080p BluRay x264-GRP", Some(7), 4_300_000_000),
            ],
        ),
    ];
    assert_eq!(
        played(&night(Namesakes::Leading, 1968), english.clone(), &runs),
        [
            "Night of the Living Dead 1968 1080p BluRay x264-GRP",
            "Night of the Living Dead (1968) 1080p Ita Eng MIRCrew",
            dub,
        ]
    );
    let runs = [fresh(
        &geek(),
        vec![
            video(0x90, "Sintel.2010.720p.SWESUB.x264-SMS", Some(25), 400_000_000),
            video(0x91, "Sintel.2010.720p.WEB.H264-CLASSiCALHD", Some(5), 400_000_000),
        ],
    )];
    assert_eq!(
        played(&sintel(), english, &runs),
        ["Sintel.2010.720p.WEB.H264-CLASSiCALHD", "Sintel.2010.720p.SWESUB.x264-SMS"]
    );
}

#[test]
fn a_release_too_small_for_the_runtime_is_dropped_as_a_fake() {
    let fake = video(0x6a, "Night.of.the.Living.Dead.1968.1080p.WEB-DL.x264", Some(900), 150_000_000);
    let runs = [fresh(&geek(), vec![fake, bluray()])];
    assert_eq!(
        played(&night(Namesakes::Leading, 1968), Profile::default(), &runs),
        ["Night.of.the.Living.Dead.1968.1080p.BluRay.x264-GRP"]
    );
}

#[test]
fn an_episode_ranks_before_its_season_pack() {
    let wanted = Wanted {
        id: "tt0903747".parse().unwrap(),
        name: "Breaking Bad".into(),
        year: Some(2008),
        episode: Some(newsreel_core::catalog::Episode { season: 3, episode: 5 }),
        namesakes: Namesakes::None,
        runtime: Some(std::time::Duration::from_secs(47 * 60)),
        season_episodes: Some(13),
        countries: Default::default(),
    };
    let runs = [fresh(
        &geek(),
        vec![
            video(0x70, "Breaking.Bad.S03.1080p.BluRay.x264-ROVERS", Some(300), 19_500_000_000),
            video(0x71, "Breaking.Bad.S03E05.1080p.WEB-DL.DD5.1.H.264", Some(25), 1_600_000_000),
            video(0x72, "Breaking.Bad.S01-S05.1080p.BluRay.x265", Some(900), 60 * GB),
            video(0x73, "Breaking.Bad.The.Complete.Series.720p.BluRay", Some(200), 40 * GB),
            video(0x74, "Breaking.Bad.S03E06.1080p.WEB-DL.DD5.1.H.264", Some(80), 1_600_000_000),
        ],
    )];
    let result = search(&for_title(&wanted, Profile::default()), &runs);
    let spans: Vec<(&str, String)> =
        result.releases.iter().map(|r| (r.name.as_str(), r.span.map(|s| s.to_string()).unwrap_or_default())).collect();
    assert_eq!(
        spans,
        [
            ("Breaking.Bad.S03E05.1080p.WEB-DL.DD5.1.H.264", "s3e5".to_string()),
            ("Breaking.Bad.S03.1080p.BluRay.x264-ROVERS", "s3".to_string()),
            ("Breaking.Bad.S01-S05.1080p.BluRay.x265", "s1-5".to_string()),
            ("Breaking.Bad.The.Complete.Series.720p.BluRay", "complete".to_string()),
        ]
    );
}

#[test]
fn a_remake_with_another_year_is_absent() {
    let remake = video(0x68, "Night of the Living Dead (1990) 1080p BluRay", Some(500), 9 * GB);
    let dated = video(0x65, "Night Of The Living Dead 1968 720p BRRip x264-GRP", Some(22), 1_700_000_000);
    let yearless = video(0x6b, "night_of_the_living_dead", Some(40), 1_100_000_000);
    let runs = [fresh(&hydra(), vec![yearless]), fresh(&geek(), vec![remake, dated])];
    assert_eq!(
        played(&night(Namesakes::Leading, 1968), Profile::default(), &runs),
        ["Night Of The Living Dead 1968 720p BRRip x264-GRP", "night_of_the_living_dead"],
        "beside a namesake, a release that names the year leads a more grabbed one that does not"
    );
    assert_eq!(
        played(&night(Namesakes::None, 1968), Profile::default(), &runs),
        ["night_of_the_living_dead", "Night Of The Living Dead 1968 720p BRRip x264-GRP"],
        "without one, the more grabbed release leads"
    );
    assert_eq!(
        played(&night(Namesakes::Shadowed, 1990), Profile::default(), &runs),
        ["Night of the Living Dead (1990) 1080p BluRay"]
    );
}

#[test]
fn a_search_without_a_wanted_title_ranks_and_writes_as_before() {
    let remake = video(0x68, "Night of the Living Dead (1990) 1080p BluRay", Some(500), 9 * GB);
    let cam = video(0x61, "Night of the Living Dead 1968 HDCAM x264", Some(300), 1_400_000_000);
    let runs = [fresh(&geek(), vec![remake, cam, dvdrip(), bluray()])];
    let result = search(&SearchQuery::new("night of the living dead 1968"), &runs);
    assert_eq!(
        names(&result),
        [
            "Night of the Living Dead 1968 HDCAM x264",
            "Night.of.the.Living.Dead.1968.1080p.BluRay.x264-GRP",
            "Night of the Living Dead 1968 DVDRip XviD",
            "Night of the Living Dead (1990) 1080p BluRay",
        ]
    );
    assert!(result.releases.iter().all(|r| r.span.is_none()));
}

#[test]
fn each_source_reports_once_over_a_titles_queries() {
    let yearless = video(0x69, "Night.of.the.Living.Dead.1080p.WEB-DL", Some(400), 3 * GB);
    let fair = video(0x65, "Night Of The Living Dead 1968 720p BRRip x264", Some(12), 1_200_000_000);
    let runs = [
        fresh(&geek(), vec![bluray()]),
        fresh(&geek(), vec![bluray(), yearless]),
        failed(&planet(), Failure::Timeout),
        fresh(&planet(), vec![fair]),
        failed(&hydra(), Failure::Http(503)),
        failed(&hydra(), Failure::Timeout),
    ];
    let result = search(&for_title(&night(Namesakes::Leading, 1968), Profile::default()), &runs);
    assert_eq!(
        result.sources,
        [
            SourceReport { source: "hydra".into(), outcome: SourceOutcome::Http { status: 503 }, elapsed_ms: 6000 },
            SourceReport { source: "geek".into(), outcome: SourceOutcome::Ok { results: 3 }, elapsed_ms: 40 },
            SourceReport { source: "planet".into(), outcome: SourceOutcome::Ok { results: 1 }, elapsed_ms: 6000 },
        ]
    );
    let listed: Vec<(&str, &[String])> = result.releases.iter().map(|r| (r.name.as_str(), &r.sources[..])).collect();
    let geek = ["geek".to_string()];
    let planet = ["planet".to_string()];
    // With a namesake, a release that names the year leads one that does not, whatever its grabs.
    assert_eq!(
        listed,
        [
            ("Night.of.the.Living.Dead.1968.1080p.BluRay.x264-GRP", &geek[..]),
            ("Night Of The Living Dead 1968 720p BRRip x264", &planet[..]),
            ("Night.of.the.Living.Dead.1080p.WEB-DL", &geek[..]),
        ]
    );
}

#[test]
fn a_ranked_groups_remux_past_the_screens_band_plays_after_a_copy_that_streams() {
    let remux = "Night.of.the.Living.Dead.1968.UHD.BluRay.2160p.FLAC.1.0.SDR.HEVC.REMUX-FraMeSToR";
    let encode = "Night.Of.The.Living.Dead.1968.1080p.Bluray.x264-GRP";
    let runs = [fresh(&geek(), vec![video(0x9a, remux, Some(1381), 57 * GB), video(0x9b, encode, Some(1615), 9 * GB)])];
    assert_eq!(played(&night(Namesakes::Leading, 1968), mac(), &runs), [encode, remux]);
}
