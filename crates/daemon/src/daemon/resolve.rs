use newsreel_core::error::{Candidate, CandidateKind, Details};
use newsreel_core::id::shortest_unique_prefix;
use newsreel_core::{DownloadId, Error, ErrorCode, HexPrefix, Release};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Found<'a> {
    Job(DownloadId),
    Release(&'a Release),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scope {
    Downloads,
    DownloadsAndResults,
}

/// What a prefix names: a download, or with a leading `u` a recent search result.
pub fn prefix<'a>(
    prefix: &HexPrefix,
    jobs: &[(DownloadId, &str)],
    results: &'a [Release],
    scope: Scope,
    command: &str,
) -> Result<Found<'a>, Error> {
    // The hex of each match, what it is, its name, and its kind.
    let mut found: Vec<(DownloadId, Found<'a>, String, CandidateKind)> = Vec::new();
    if !prefix.is_result() {
        for (id, name) in jobs {
            if id.has_prefix(prefix) {
                found.push((*id, Found::Job(*id), name.to_string(), CandidateKind::Download));
            }
        }
    } else if scope == Scope::DownloadsAndResults {
        for release in results {
            if prefix.matches(&release.id) {
                let hex = DownloadId::from_bytes(*release.id.as_bytes());
                found.push((hex, Found::Release(release), release.name.clone(), CandidateKind::SearchResult));
            }
        }
    }
    let lead = if prefix.is_result() { "u" } else { "" };
    let typed = format!("{lead}{}", prefix.as_str());
    match found.len() {
        0 => {
            let error = match scope {
                Scope::Downloads => no_download(&typed),
                Scope::DownloadsAndResults => {
                    Error::new(ErrorCode::NotFound, format!("no download or recent search result matches {typed}"))
                        .with_hint("run newsreel search <words> to find it, or pass an .nzb file or URL")
                }
            };
            Err(error)
        }
        1 => Ok(found.remove(0).1),
        n => {
            let ids: Vec<DownloadId> = found.iter().map(|(id, ..)| *id).collect();
            let candidates: Vec<Candidate> = found
                .iter()
                .map(|(id, _, name, kind)| Candidate {
                    id: format!("{lead}{}", shortest_unique_prefix(id, &ids, 6)),
                    name: name.clone(),
                    kind: *kind,
                })
                .collect();
            let first = candidates[0].id.clone();
            let thing = if prefix.is_result() { "search result" } else { "download" };
            Err(Error::new(
                ErrorCode::AmbiguousRef,
                format!("{typed} matches {}", newsreel_core::units::count(n, thing)),
            )
            .with_details(Details::Candidates(candidates))
            .with_hint(format!("use a longer prefix, such as: newsreel {command} {first}")))
        }
    }
}

/// The error for a reference that names no download.
pub fn no_download(reference: &str) -> Error {
    Error::new(ErrorCode::NotFound, format!("no download matches {reference}"))
        .with_hint("run newsreel status to list downloads")
        .with_next("newsreel status")
}

#[cfg(test)]
mod tests {
    use super::*;
    use newsreel_core::ResultId;
    use std::collections::BTreeSet;

    fn bytes(hex_prefix: &str) -> [u8; 20] {
        let id: DownloadId = format!("{hex_prefix}{}", "0".repeat(40 - hex_prefix.len())).parse().unwrap();
        *id.as_bytes()
    }

    fn release(hex_prefix: &str, name: &str) -> Release {
        Release {
            id: ResultId::from_bytes(bytes(hex_prefix)),
            name: name.into(),
            size: None,
            grabs: None,
            published: None,
            category: None,
            flags: BTreeSet::new(),
            sources: vec!["hydra".into()],
            nzb_url: None,
            span: None,
            failed: None,
        }
    }

    #[test]
    fn a_plain_prefix_names_a_download_and_a_u_prefix_a_search_result() {
        let job = DownloadId::from_bytes(bytes("9b5212"));
        let jobs = [(job, "arch")];
        let results = [release("9b5212", "arch"), release("51e0c7", "older arch")];
        let p = "9b52".parse().unwrap();
        assert_eq!(prefix(&p, &jobs, &results, Scope::DownloadsAndResults, "get").unwrap(), Found::Job(job));
        let p = "u9b52".parse().unwrap();
        assert_eq!(
            prefix(&p, &jobs, &results, Scope::DownloadsAndResults, "get").unwrap(),
            Found::Release(&results[0])
        );
        let p = "u51e0".parse().unwrap();
        assert_eq!(prefix(&p, &jobs, &results, Scope::Downloads, "pause").unwrap_err().code, ErrorCode::NotFound);
    }

    #[test]
    fn ambiguity_lists_candidates_with_a_longer_prefix() {
        let results = [release("e3a1f0", "ubuntu-24.04.5-desktop-amd64.iso"), release("e3a1c4", "Ubuntu repack")];
        let err = prefix(&"ue3a1".parse().unwrap(), &[], &results, Scope::DownloadsAndResults, "get").unwrap_err();
        assert_eq!(err.code, ErrorCode::AmbiguousRef);
        assert_eq!(err.message, "ue3a1 matches 2 search results");
        assert_eq!(err.hint.as_deref(), Some("use a longer prefix, such as: newsreel get ue3a1f0"));
        let Some(Details::Candidates(c)) = err.details else { panic!("expected candidates") };
        assert_eq!(
            c.iter().map(|c| (c.id.as_str(), c.kind.as_str())).collect::<Vec<_>>(),
            [("ue3a1f0", "search result"), ("ue3a1c4", "search result")]
        );
    }
}
