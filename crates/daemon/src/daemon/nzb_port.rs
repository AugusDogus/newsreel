//! Turns an NZB, from a file or a URL, into the bytes the engine adds: a manifest resolved
//! over NNTP, which [`super::engine_port::ParseLayout`] then reads.

use newsreel_core::Error;

use super::BoxFuture;

pub trait NzbPort: Send + Sync {
    fn resolve(&self, nzb: Vec<u8>) -> BoxFuture<'_, Result<Vec<u8>, Error>>;
}

/// For the fake engine, whose test releases are already in the form it parses.
pub struct Passthrough;

impl NzbPort for Passthrough {
    fn resolve(&self, nzb: Vec<u8>) -> BoxFuture<'_, Result<Vec<u8>, Error>> {
        Box::pin(async move { Ok(nzb) })
    }
}
