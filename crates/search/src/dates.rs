use time::OffsetDateTime;
use time::format_description::well_known::{Rfc2822, Rfc3339};

pub(crate) fn rfc3339(s: &str) -> Option<i64> {
    OffsetDateTime::parse(s.trim(), &Rfc3339).ok().map(OffsetDateTime::unix_timestamp)
}

pub(crate) fn rfc2822(s: &str) -> Option<i64> {
    OffsetDateTime::parse(s.trim(), &Rfc2822).ok().map(OffsetDateTime::unix_timestamp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_each_source_format() {
        assert_eq!(rfc2822("Thu, 15 Oct 2026 14:00:32 -0000"), Some(1_792_072_832));
    }
}
