//! Where a video's keyframes are, read from its own index without reading the video: a Matroska
//! file's Cues, or an MP4 file's sample tables. A TV's HLS playlist is cut at these, so each
//! segment starts on a keyframe and the video is copied, not encoded again.

use std::future::Future;
use std::io;

/// The most of a Matroska file's head newsreel reads for its index, and the largest MP4 `moov` it
/// takes. A two-hour film's are a few megabytes.
const HEAD: u64 = 256 * 1024;
const MOST_INDEX: u64 = 64 * 1024 * 1024;

/// Reads bytes `start..end` of the file, waiting for the pieces that hold them.
pub trait Bytes {
    fn size(&self) -> u64;
    fn read(&mut self, start: u64, end: u64) -> impl Future<Output = io::Result<Vec<u8>>> + Send;
}

/// The keyframes of the file's video, in seconds and in order, or `None` for a file with no index
/// newsreel reads: an AVI, a transport stream, a fragmented MP4, or a Matroska file without Cues.
pub async fn keyframes(bytes: &mut impl Bytes, container: &str) -> io::Result<Option<Vec<f64>>> {
    let mut times = match container {
        "matroska" | "webm" => matroska(bytes).await?,
        "mp4" => mp4(bytes).await?,
        _ => None,
    };
    if let Some(times) = &mut times {
        times.retain(|t| t.is_finite() && *t >= 0.0);
        times.sort_by(f64::total_cmp);
        times.dedup();
    }
    Ok(times.filter(|times| !times.is_empty()))
}

fn bad(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what.to_string())
}

// Matroska.

const SEGMENT: u64 = 0x1853_8067;
const SEEK_HEAD: u64 = 0x114D_9B74;
const SEEK: u64 = 0x4DBB;
const SEEK_ID: u64 = 0x53AB;
const SEEK_POSITION: u64 = 0x53AC;
const INFO: u64 = 0x1549_A966;
const TIMESTAMP_SCALE: u64 = 0x2A_D7B1;
const TRACKS: u64 = 0x1654_AE6B;
const TRACK_ENTRY: u64 = 0xAE;
const TRACK_NUMBER: u64 = 0xD7;
const TRACK_TYPE: u64 = 0x83;
const CUES: u64 = 0x1C53_BB6B;
const CUE_POINT: u64 = 0xBB;
const CUE_TIME: u64 = 0xB3;
const CUE_TRACK_POSITIONS: u64 = 0xB7;
const CUE_TRACK: u64 = 0xF7;
const CLUSTER: u64 = 0x1F43_B675;

/// An element's ID, the length of its head, and the length of its body, `None` for an unknown
/// length.
struct Element {
    id: u64,
    head: usize,
    size: Option<u64>,
}

/// The variable-length integer at `at`: its value with the length marker kept, as IDs are
/// written, or taken off, as sizes are, and its length.
fn vint(data: &[u8], at: usize, keep_marker: bool) -> Option<(u64, usize)> {
    let first = *data.get(at)?;
    let length = first.leading_zeros() as usize + 1;
    if length > 8 {
        return None;
    }
    let bytes = data.get(at..at + length)?;
    let mut value = u64::from(if keep_marker { first } else { first & (0xFFu16 >> length) as u8 });
    for byte in &bytes[1..] {
        value = (value << 8) | u64::from(*byte);
    }
    Some((value, length))
}

fn element(data: &[u8], at: usize) -> Option<Element> {
    let (id, id_length) = vint(data, at, true)?;
    let (size, size_length) = vint(data, at + id_length, false)?;
    let unknown = size == (1u64 << (7 * size_length)) - 1;
    Some(Element { id, head: id_length + size_length, size: (!unknown).then_some(size) })
}

/// The children of an element's body, each as its ID and its body.
fn children(body: &[u8]) -> Vec<(u64, &[u8])> {
    let mut found = Vec::new();
    let mut at = 0;
    while let Some(element) = element(body, at) {
        let start = at + element.head;
        let Some(end) = element.size.and_then(|size| start.checked_add(usize::try_from(size).ok()?)) else { break };
        let Some(child) = body.get(start..end) else { break };
        found.push((element.id, child));
        at = end;
    }
    found
}

fn uint(body: &[u8]) -> u64 {
    body.iter().take(8).fold(0, |value, byte| (value << 8) | u64::from(*byte))
}

async fn matroska(bytes: &mut impl Bytes) -> io::Result<Option<Vec<f64>>> {
    let head = bytes.read(0, HEAD.min(bytes.size())).await?;
    // The EBML header, then the Segment, whose body the positions in its SeekHead count from.
    let ebml = element(&head, 0).ok_or_else(|| bad("the file does not start with an EBML header"))?;
    let at = ebml.head
        + usize::try_from(ebml.size.ok_or_else(|| bad("the EBML header has no length"))?)
            .map_err(|_| bad("the EBML header is too long"))?;
    let segment =
        element(&head, at).filter(|e| e.id == SEGMENT).ok_or_else(|| bad("no Segment follows the EBML header"))?;
    let base = (at + segment.head) as u64;
    let (mut scale, mut video, mut cues) = (1_000_000u64, None, None);
    let mut cues_at = None;
    // The top-level elements before the first Cluster, read from the head.
    let mut at = base as usize;
    while let Some(element) = element(&head, at) {
        let start = at + element.head;
        let Some(size) = element.size else { break };
        if element.id == CLUSTER {
            break;
        }
        let end = start.saturating_add(usize::try_from(size).unwrap_or(usize::MAX));
        let body = head.get(start..end);
        match (element.id, body) {
            (SEEK_HEAD, Some(body)) => {
                for (_, seek) in children(body).into_iter().filter(|(id, _)| *id == SEEK) {
                    let fields = children(seek);
                    let id = fields.iter().find(|(id, _)| *id == SEEK_ID).map(|(_, v)| uint(v));
                    let position = fields.iter().find(|(id, _)| *id == SEEK_POSITION).map(|(_, v)| uint(v));
                    if let (Some(CUES), Some(position)) = (id, position) {
                        cues_at = Some(base + position);
                    }
                }
            }
            (INFO, Some(body)) => {
                if let Some((_, value)) = children(body).into_iter().find(|(id, _)| *id == TIMESTAMP_SCALE) {
                    scale = uint(value).max(1);
                }
            }
            (TRACKS, Some(body)) => video = video_track(body),
            (CUES, Some(body)) => cues = Some(body.to_vec()),
            _ => {}
        }
        at = end;
    }
    let Some(video) = video else { return Ok(None) };
    let cues = match (cues, cues_at) {
        (Some(cues), _) => cues,
        (None, Some(position)) if position < bytes.size() => {
            let top = bytes.read(position, (position + 16).min(bytes.size())).await?;
            let Some(element) = element(&top, 0).filter(|e| e.id == CUES) else { return Ok(None) };
            let Some(size) = element.size.filter(|size| *size <= MOST_INDEX) else { return Ok(None) };
            let start = position + element.head as u64;
            bytes.read(start, (start + size).min(bytes.size())).await?
        }
        _ => return Ok(None),
    };
    let seconds = |time: u64| time as f64 * scale as f64 / 1e9;
    let times = children(&cues)
        .into_iter()
        .filter(|(id, _)| *id == CUE_POINT)
        .filter_map(|(_, point)| {
            let fields = children(point);
            let time = fields.iter().find(|(id, _)| *id == CUE_TIME).map(|(_, v)| uint(v))?;
            let on_video = fields
                .iter()
                .filter(|(id, _)| *id == CUE_TRACK_POSITIONS)
                .any(|(_, positions)| children(positions).iter().any(|(id, v)| *id == CUE_TRACK && uint(v) == video));
            on_video.then(|| seconds(time))
        })
        .collect();
    Ok(Some(times))
}

/// The number of the first video track.
fn video_track(tracks: &[u8]) -> Option<u64> {
    children(tracks).into_iter().filter(|(id, _)| *id == TRACK_ENTRY).find_map(|(_, entry)| {
        let fields = children(entry);
        let kind = fields.iter().find(|(id, _)| *id == TRACK_TYPE).map(|(_, v)| uint(v));
        let number = fields.iter().find(|(id, _)| *id == TRACK_NUMBER).map(|(_, v)| uint(v));
        number.filter(|_| kind == Some(1))
    })
}

// MP4.

/// The boxes in `data`, each as its type and its body.
pub fn boxes(data: &[u8]) -> Vec<([u8; 4], &[u8])> {
    let mut found = Vec::new();
    let mut at = 0usize;
    while let Some(head) = data.get(at..at + 8) {
        let size = u64::from(u32::from_be_bytes([head[0], head[1], head[2], head[3]]));
        let kind = [head[4], head[5], head[6], head[7]];
        let (size, header) = match size {
            1 => match data.get(at + 8..at + 16) {
                Some(large) => (u64::from_be_bytes(large.try_into().expect("eight bytes")), 16),
                None => break,
            },
            0 => ((data.len() - at) as u64, 8),
            size => (size, 8),
        };
        let Some(end) = usize::try_from(size).ok().and_then(|size| at.checked_add(size)) else { break };
        let Some(body) = data.get(at + header..end).filter(|_| size >= header as u64) else { break };
        found.push((kind, body));
        at = end;
    }
    found
}

pub fn child<'a>(data: &'a [u8], kind: &[u8; 4]) -> Option<&'a [u8]> {
    boxes(data).into_iter().find(|(k, _)| k == kind).map(|(_, body)| body)
}

fn be32(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(data.get(at..at + 4)?.try_into().ok()?))
}

fn be64(data: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_be_bytes(data.get(at..at + 8)?.try_into().ok()?))
}

async fn mp4(bytes: &mut impl Bytes) -> io::Result<Option<Vec<f64>>> {
    // The top-level boxes, by their heads alone, until the `moov`.
    let size = bytes.size();
    let mut at = 0u64;
    while at + 8 <= size {
        let head = bytes.read(at, (at + 16).min(size)).await?;
        let length = u64::from(be32(&head, 0).ok_or_else(|| bad("a box runs past the file"))?);
        let (length, header) = match length {
            1 => (be64(&head, 8).ok_or_else(|| bad("a box runs past the file"))?, 16),
            0 => (size - at, 8),
            length => (length, 8),
        };
        if length < header {
            return Err(bad("a box is shorter than its head"));
        }
        if &head[4..8] == b"moov" {
            if length > MOST_INDEX {
                return Ok(None);
            }
            let moov = bytes.read(at + header, (at + length).min(size)).await?;
            return Ok(moov_keyframes(&moov));
        }
        at = at.saturating_add(length);
    }
    Ok(None)
}

/// The keyframes of the first video track a `moov`'s body holds, by its sample tables. A
/// fragmented MP4's `moov` holds no samples, so it has none.
pub fn moov_keyframes(moov: &[u8]) -> Option<Vec<f64>> {
    let trak = boxes(moov).into_iter().filter(|(kind, _)| kind == b"trak").map(|(_, body)| body).find(|trak| {
        child(trak, b"mdia").and_then(|mdia| child(mdia, b"hdlr")).and_then(|hdlr| hdlr.get(8..12)) == Some(b"vide")
    })?;
    let mdia = child(trak, b"mdia")?;
    let mdhd = child(mdia, b"mdhd")?;
    let timescale = f64::from(if mdhd.first() == Some(&1) { be32(mdhd, 20)? } else { be32(mdhd, 12)? });
    if timescale <= 0.0 {
        return None;
    }
    let stbl = child(child(mdia, b"minf")?, b"stbl")?;
    // Decode times, from the runs of sample durations.
    let stts = child(stbl, b"stts")?;
    let mut decode = Vec::new();
    let mut time = 0u64;
    for entry in 0..be32(stts, 4)? as usize {
        let (count, delta) = (be32(stts, 8 + entry * 8)?, be32(stts, 12 + entry * 8)?);
        for _ in 0..count {
            decode.push(time);
            time += u64::from(delta);
            if decode.len() > 50_000_000 {
                return None;
            }
        }
    }
    if decode.is_empty() {
        return None;
    }
    // Composition offsets, which B-frames need.
    let mut offsets = vec![0i64; decode.len()];
    if let Some(ctts) = child(stbl, b"ctts") {
        let signed = ctts.first() == Some(&1);
        let mut sample = 0usize;
        for entry in 0..be32(ctts, 4)? as usize {
            let count = be32(ctts, 8 + entry * 8)? as usize;
            let raw = be32(ctts, 12 + entry * 8)?;
            let offset = if signed { i64::from(raw as i32) } else { i64::from(raw) };
            for slot in offsets.iter_mut().skip(sample).take(count) {
                *slot = offset;
            }
            sample += count;
        }
    }
    // The edit list's first edit that shows media moves where the video starts.
    let shift = child(trak, b"edts").and_then(|edts| child(edts, b"elst")).and_then(|elst| {
        let version = *elst.first()?;
        (0..be32(elst, 4)? as usize).find_map(|entry| {
            let media_time = if version == 1 {
                be64(elst, 8 + entry * 20 + 8)? as i64
            } else {
                i64::from(be32(elst, 8 + entry * 12 + 4)? as i32)
            };
            (media_time >= 0).then_some(media_time)
        })
    });
    let shift = shift.unwrap_or(0);
    let pts = |sample: usize| (decode[sample] as i64 + offsets[sample] - shift) as f64 / timescale;
    let times = match child(stbl, b"stss") {
        Some(stss) => (0..be32(stss, 4)? as usize)
            .filter_map(|entry| be32(stss, 8 + entry * 4))
            .filter_map(|number| (number as usize).checked_sub(1).filter(|sample| *sample < decode.len()))
            .map(pts)
            .collect(),
        // Every sample is a keyframe.
        None => (0..decode.len()).map(pts).collect(),
    };
    Some(times)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Memory(Vec<u8>);

    impl Bytes for Memory {
        fn size(&self) -> u64 {
            self.0.len() as u64
        }

        async fn read(&mut self, start: u64, end: u64) -> io::Result<Vec<u8>> {
            Ok(self.0[start as usize..end as usize].to_vec())
        }
    }

    /// A Matroska element, its size written in eight bytes as some muxers write them.
    fn ebml(id: u64, body: &[u8]) -> Vec<u8> {
        let mut out = id.to_be_bytes().iter().copied().skip_while(|b| *b == 0).collect::<Vec<_>>();
        out.push(0x01);
        out.extend_from_slice(&(body.len() as u64).to_be_bytes()[1..]);
        out.extend_from_slice(body);
        out
    }

    fn number(id: u64, value: u64) -> Vec<u8> {
        ebml(id, &value.to_be_bytes())
    }

    fn cue(time: u64, track: u64) -> Vec<u8> {
        ebml(CUE_POINT, &[number(CUE_TIME, time), ebml(CUE_TRACK_POSITIONS, &number(CUE_TRACK, track))].concat())
    }

    /// A Matroska file whose Cues come after its one Cluster, with the SeekHead pointing at them.
    fn matroska_file(cues_first: bool) -> Vec<u8> {
        let tracks = ebml(
            TRACKS,
            &[
                ebml(TRACK_ENTRY, &[number(TRACK_NUMBER, 1), number(TRACK_TYPE, 2)].concat()),
                ebml(TRACK_ENTRY, &[number(TRACK_NUMBER, 2), number(TRACK_TYPE, 1)].concat()),
            ]
            .concat(),
        );
        let info = ebml(INFO, &number(TIMESTAMP_SCALE, 1_000_000));
        let cues = ebml(CUES, &[cue(0, 2), cue(1000, 1), cue(2002, 2), cue(4004, 2)].concat());
        let cluster = ebml(CLUSTER, &vec![0u8; 5000]);
        // The SeekHead's own length does not change with the position it holds.
        let seek_head = |position: u64| {
            ebml(
                SEEK_HEAD,
                &ebml(SEEK, &[ebml(SEEK_ID, &[0x1C, 0x53, 0xBB, 0x6B]), number(SEEK_POSITION, position)].concat()),
            )
        };
        let before = seek_head(0).len() + info.len() + tracks.len();
        let body = if cues_first {
            [seek_head((before) as u64), info, tracks, cues, cluster].concat()
        } else {
            [seek_head((before + cluster.len()) as u64), info, tracks, cluster, cues].concat()
        };
        [ebml(0x1A45_DFA3, &number(0x4286, 1)), ebml(SEGMENT, &body)].concat()
    }

    #[tokio::test]
    async fn a_matroska_files_keyframes_are_its_videos_cues_wherever_they_are() {
        for cues_first in [true, false] {
            let mut file = Memory(matroska_file(cues_first));
            assert_eq!(keyframes(&mut file, "matroska").await.unwrap(), Some(vec![0.0, 2.002, 4.004]), "{cues_first}");
        }
        let mut cut = Memory(matroska_file(false)[..400].to_vec());
        assert_eq!(keyframes(&mut cut, "matroska").await.unwrap(), None, "the Cues are past the end");
    }

    fn mp4_box(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        [&((body.len() + 8) as u32).to_be_bytes()[..], kind, body].concat()
    }

    fn table(kind: &[u8; 4], version: u8, entries: &[&[u32]]) -> Vec<u8> {
        let mut body = vec![version, 0, 0, 0];
        body.extend_from_slice(&(entries.len() as u32).to_be_bytes());
        for entry in entries {
            for value in *entry {
                body.extend_from_slice(&value.to_be_bytes());
            }
        }
        mp4_box(kind, &body)
    }

    /// An MP4 of 24 frames a second at a timescale of 12288, a keyframe every 48, with B-frames
    /// that delay each frame two, which the edit list takes back off.
    fn mp4_file() -> Vec<u8> {
        let mut mdhd = vec![0u8; 20];
        mdhd[12..16].copy_from_slice(&12288u32.to_be_bytes());
        let mut hdlr = vec![0u8; 8];
        hdlr.extend_from_slice(b"vide");
        hdlr.extend_from_slice(&[0; 12]);
        let stbl = mp4_box(
            b"stbl",
            &[
                table(b"stts", 0, &[&[144, 512]]),
                table(b"ctts", 0, &[&[144, 1024]]),
                table(b"stss", 0, &[&[1], &[49], &[97]]),
            ]
            .concat(),
        );
        let mdia =
            mp4_box(b"mdia", &[mp4_box(b"mdhd", &mdhd), mp4_box(b"hdlr", &hdlr), mp4_box(b"minf", &stbl)].concat());
        let edts = mp4_box(b"edts", &table(b"elst", 0, &[&[6000, 1024, 0x0001_0000]]));
        let moov = mp4_box(b"moov", &mp4_box(b"trak", &[edts, mdia].concat()));
        [mp4_box(b"ftyp", b"isom\0\0\0\0"), mp4_box(b"mdat", &[0u8; 3000]), moov].concat()
    }

    #[tokio::test]
    async fn an_mp4s_keyframes_come_from_its_sample_tables_and_edit_list() {
        let mut file = Memory(mp4_file());
        assert_eq!(keyframes(&mut file, "mp4").await.unwrap(), Some(vec![0.0, 2.0, 4.0]));
        assert_eq!(keyframes(&mut Memory(mp4_file()), "avi").await.unwrap(), None);
    }
}
