use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A download: a hash of its release's segments, so the same release is always the same download,
/// even when an indexer varies the rest of the NZB on every fetch.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DownloadId([u8; 20]);

impl DownloadId {
    pub const fn from_bytes(bytes: [u8; 20]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 20] {
        &self.0
    }

    pub fn to_hex(&self) -> String {
        hex(&self.0)
    }

    pub fn has_prefix(&self, prefix: &HexPrefix) -> bool {
        !prefix.result && self.to_hex().starts_with(prefix.as_str())
    }
}

impl fmt::Display for DownloadId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for DownloadId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DownloadId({})", self.to_hex())
    }
}

impl Serialize for DownloadId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}

/// A search result: the SHA-1 of its NZB link, so the same row gets the same ID in every
/// search. Shown with a `u` prefix, which keeps it apart from download IDs.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct ResultId([u8; 20]);

impl ResultId {
    pub const fn from_bytes(bytes: [u8; 20]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 20] {
        &self.0
    }

    pub fn to_hex(&self) -> String {
        hex(&self.0)
    }

    /// The full ID as typed by users: `u` and 40 hex characters.
    pub fn to_ref(&self) -> String {
        format!("u{}", self.to_hex())
    }
}

impl Serialize for ResultId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_ref())
    }
}

impl<'de> Deserialize<'de> for ResultId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.strip_prefix('u')
            .and_then(decode_hex::<20>)
            .map(Self)
            .ok_or_else(|| serde::de::Error::custom(format!("invalid search result id {s:?}")))
    }
}

/// A unique-prefix reference: 4 to 40 lowercase hex characters, led by `u` for a search result.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct HexPrefix {
    pub(crate) result: bool,
    pub(crate) hex: String,
}

impl HexPrefix {
    pub const MIN_LEN: usize = 4;

    pub fn as_str(&self) -> &str {
        &self.hex
    }

    /// Names a search result rather than a download.
    pub fn is_result(&self) -> bool {
        self.result
    }

    pub fn matches(&self, id: &ResultId) -> bool {
        self.result && id.to_hex().starts_with(&self.hex)
    }
}

/// How many hex digits every screen shows of an ID, unless more tell two apart.
pub const SHORT_ID: usize = 6;

/// The shortest prefix of `id`, at least `min` characters, that no ID in `others` shares.
pub fn shortest_unique_prefix<'a>(
    id: &DownloadId,
    others: impl IntoIterator<Item = &'a DownloadId>,
    min: usize,
) -> String {
    let hex = id.to_hex();
    let longest_shared = others
        .into_iter()
        .filter(|other| *other != id)
        .map(|other| common_prefix_len(&hex, &other.to_hex()))
        .max()
        .unwrap_or(0);
    hex[..(longest_shared + 1).max(min).min(40)].to_string()
}

fn common_prefix_len(a: &str, b: &str) -> usize {
    a.bytes().zip(b.bytes()).take_while(|(x, y)| x == y).count()
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(DIGITS[(b >> 4) as usize] as char);
        out.push(DIGITS[(b & 0xf) as usize] as char);
    }
    out
}

pub(crate) fn decode_hex<const N: usize>(s: &str) -> Option<[u8; N]> {
    if s.len() != N * 2 {
        return None;
    }
    let mut out = [0; N];
    for (i, pair) in s.as_bytes().chunks(2).enumerate() {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out[i] = (hi * 16 + lo) as u8;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ARCH: &str = "9b521219bc48550140a666f74a29048ed78e933b";

    #[test]
    fn a_minimum_longer_than_an_id_returns_the_whole_id() {
        let id: DownloadId = ARCH.parse().unwrap();
        assert_eq!(shortest_unique_prefix(&id, [&id], 64), ARCH);
    }

    #[test]
    fn shortest_unique_prefix_grows_past_collisions() {
        let a: DownloadId = format!("9b5212{}", "0".repeat(34)).parse().unwrap();
        let b: DownloadId = format!("9b5212f{}", "0".repeat(33)).parse().unwrap();
        let c: DownloadId = format!("51e0c7{}", "0".repeat(34)).parse().unwrap();
        assert_eq!(shortest_unique_prefix(&a, [&a, &b, &c], 6), "9b52120");
        assert_eq!(shortest_unique_prefix(&c, [&a, &b, &c], 6), "51e0c7");
    }

    #[test]
    fn result_ids_round_trip_through_json() {
        let id = ResultId::from_bytes([0x0f; 20]);
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, format!("\"u{}\"", "0f".repeat(20)));
        assert_eq!(serde_json::from_str::<ResultId>(&json).unwrap(), id);
        assert!(serde_json::from_str::<ResultId>(&format!("\"{}\"", "0f".repeat(20))).is_err(), "needs its u");
    }
}
