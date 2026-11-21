use super::*;

fn mp4_box(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    [&((body.len() + 8) as u32).to_be_bytes()[..], kind, body].concat()
}

fn full(kind: &[u8; 4], version: u8, body: &[u8]) -> Vec<u8> {
    mp4_box(kind, &[&[version, 0, 0, 0][..], body].concat())
}

/// The `ftyp` and `moov` of a fragmented MP4 whose video is track 1 at a timescale of 1000, and
/// whose audio is track 2.
fn init() -> Vec<u8> {
    let trak = |id: u32, handler: &[u8; 4], timescale: u32| {
        let mut tkhd = vec![0u8; 8];
        tkhd.extend_from_slice(&id.to_be_bytes());
        let mut mdhd = vec![0u8; 8];
        mdhd.extend_from_slice(&timescale.to_be_bytes());
        let hdlr = [&[0u8; 4][..], handler, &[0; 12]].concat();
        let mdia = mp4_box(b"mdia", &[full(b"mdhd", 0, &mdhd), full(b"hdlr", 0, &hdlr)].concat());
        mp4_box(b"trak", &[full(b"tkhd", 0, &tkhd), mdia].concat())
    };
    [mp4_box(b"ftyp", b"isom\0\0\0\0"), mp4_box(b"moov", &[trak(2, b"soun", 48000), trak(1, b"vide", 1000)].concat())]
        .concat()
}

/// A fragment whose video starts showing at `ms` milliseconds: decoded 80 ms before, with an
/// 80 ms composition offset, as B-frames make.
fn fragment(ms: u32) -> Vec<u8> {
    let traf = |id: u32, decode: u32, offset: Option<u32>| {
        let tfhd = full(b"tfhd", 0, &id.to_be_bytes());
        let tfdt = full(b"tfdt", 1, &u64::from(decode).to_be_bytes());
        let trun = match offset {
            // Data offset, then each sample's duration and composition offset.
            Some(offset) => {
                let body = [&1u32.to_be_bytes()[..], &0u32.to_be_bytes(), &40u32.to_be_bytes(), &offset.to_be_bytes()];
                mp4_box(b"trun", &[&[0u8, 0, 0x09, 0x01][..], &body.concat()].concat())
            }
            None => mp4_box(b"trun", &[&[0u8, 0, 0, 0][..], &0u32.to_be_bytes()].concat()),
        };
        mp4_box(b"traf", &[tfhd, tfdt, trun].concat())
    };
    let moof = mp4_box(b"moof", &[traf(2, ms * 48, None), traf(1, ms - 80, Some(80))].concat());
    [moof, mp4_box(b"mdat", &ms.to_be_bytes())].concat()
}

#[test]
fn a_plan_starts_segments_on_keyframes_at_least_six_seconds_apart() {
    let keyframes = [0.08, 2.08, 4.08, 6.08, 6.2, 8.08, 12.0, 12.3, 18.5, 59.6];
    let plan = Plan::on(&keyframes, 60.0).unwrap();
    // 12.0 comes too soon after 6.08, 12.3 too near 12.0 for the clocks to tell apart, and 59.6
    // would make a last segment of less than a second.
    assert_eq!(plan.starts, [0.0, 6.08, 18.5]);
    assert_eq!(plan.segment_of(0.0), 0);
    assert_eq!(plan.segment_of(6.0), 1, "a fragment a little early is the keyframe's");
    assert_eq!(plan.segment_of(18.3), 2);
    assert_eq!(plan.segment_of(1e9), 2);
    assert_eq!(Plan::every(13.5).unwrap().starts, [0.0, 6.0, 12.0]);
    assert_eq!(Plan::on(&keyframes, f64::NAN), None);
}

#[test]
fn a_playlist_lists_every_segment_of_the_whole_film() {
    let plan = Plan { starts: vec![0.0, 6.08, 12.0], duration: 20.5 };
    assert_eq!(
        plan.playlist(),
        "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:9\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-MEDIA-SEQUENCE:0\n\
         #EXT-X-INDEPENDENT-SEGMENTS\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:6.080000,\n0.m4s\n#EXTINF:5.920000,\n1.m4s\n\
         #EXTINF:8.500000,\n2.m4s\n#EXT-X-ENDLIST\n"
    );
    assert_eq!(
        ["film.m3u8", "init.mp4", "12.m4s", "x.m4s", "film.mkv"].map(Asked::of),
        [Some(Asked::Playlist), Some(Asked::Init), Some(Asked::Segment(12)), None, None]
    );
}

#[test]
fn a_run_is_cut_where_its_fragments_reach_the_next_segment() {
    let plan = Plan { starts: vec![0.0, 6.0, 12.0, 18.0], duration: 24.0 };
    let stream = [init(), fragment(4000), fragment(6000), fragment(8000), fragment(12000), fragment(14000)].concat();
    // A run from segment 1, whose seek landed on the keyframe at 4 s.
    let mut cutter = Cutter::new(1);
    // In two reads, the second cutting a box in two.
    let cut = init().len() + fragment(4000).len() + 10;
    let used = cutter.take(&stream[..cut], &plan).unwrap();
    assert_eq!(used, cut - 10);
    assert_eq!(cutter.init.as_deref(), Some(init().as_slice()));
    let rest = [&stream[used..cut], &stream[cut..]].concat();
    assert_eq!(cutter.take(&rest, &plan).unwrap(), rest.len());
    let finished: Vec<(usize, Vec<u8>)> = std::mem::take(&mut cutter.finished);
    assert_eq!(finished, [(1, [fragment(6000), fragment(8000)].concat())], "the fragment at 4 s is segment 0's");
    cutter.finish();
    assert_eq!(cutter.finished, [(2, [fragment(12000), fragment(14000)].concat())]);
}

/// A film whose "ffmpeg" is `sh`, writing `stream` whatever time it is asked to start at, and
/// noting each start in `starts.txt`.
#[cfg(unix)]
fn film(dir: &Path, stream: &[u8], plan: Plan) -> Film {
    std::fs::write(dir.join("stream.mp4"), stream).unwrap();
    let log = dir.join("starts.txt");
    let file = dir.join("stream.mp4");
    let arguments: Arguments =
        Box::new(move |t| vec!["-c".into(), format!("echo {t} >> '{}'; cat '{}'", log.display(), file.display())]);
    Film::new(plan, "sh".into(), arguments, dir.join("film"), 0.0).unwrap()
}

#[cfg(unix)]
#[tokio::test]
async fn a_tv_gets_each_segment_from_one_run_and_a_new_run_for_a_seek_behind_it() {
    let dir = tempfile::tempdir().unwrap();
    let plan = Plan { starts: vec![0.0, 6.0, 12.0, 18.0], duration: 24.0 };
    let stream = [init(), fragment(80), fragment(6000), fragment(12000), fragment(18000), fragment(20000)].concat();
    let film = film(dir.path(), &stream, plan);
    assert_eq!(*film.init().await.unwrap(), init());
    assert_eq!(film.segment(0).await.unwrap(), fragment(80));
    assert_eq!(film.segment(3).await.unwrap(), [fragment(18000), fragment(20000)].concat());
    assert_eq!(film.segment(1).await.unwrap(), fragment(6000), "kept from the same run");
    let starts = || std::fs::read_to_string(dir.path().join("starts.txt")).unwrap();
    assert_eq!(starts(), "0\n", "one run made them all");
    // The run is over, and a segment it made but that is gone from disk comes from a new run.
    std::fs::remove_file(dir.path().join("film/2.m4s")).unwrap();
    film.shared.state().made.remove(&2);
    assert_eq!(film.segment(2).await.unwrap(), fragment(12000));
    assert_eq!(starts(), "0\n12.01\n", "the new run starts just past the segment's keyframe");
    assert!(film.segment(4).await.is_err(), "the film has four segments");
    let folder = dir.path().join("film");
    drop(film);
    assert!(!folder.exists(), "the segments go with the film");
}
