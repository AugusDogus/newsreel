//! `POST /api/play`: starts or joins a download and fetches one video first, in playing order.

use crate::playback::stream::Playable;
use newsreel_core::text::sanitize;
use newsreel_core::{DownloadId, Error, ErrorCode, Job, ReleaseFile};
use serde::{Deserialize, Serialize};

use super::super::ctx::Ctx;
use super::super::downloads::{self, Served, served};
use super::super::owner::{Command, FilesInfo};

#[derive(Deserialize)]
pub struct Play {
    /// A download or search result ID, or an NZB URL.
    #[serde(rename = "ref")]
    reference: String,
    /// The index of the file to watch. Without it, the engine picks the main video.
    file: Option<u32>,
}

#[derive(Serialize)]
#[cfg_attr(test, derive(ts_rs::TS), ts(rename = "PlayInfo"))]
pub struct Playing {
    id: String,
    name: String,
    job: Job,
    files: Vec<ReleaseFile>,
    total_size: u64,
    video: Served,
    subtitles: Vec<Served>,
}

pub async fn play(ctx: &Ctx, request: Play) -> Result<Playing, Error> {
    let reference = request.reference.as_str();
    let (source, _, playable) =
        downloads::prepare(ctx, super::target(reference)?, request.file, Some(reference)).await?;
    let (job, info) = downloads::begin(ctx, source, &playable).await?;
    Ok(playing(ctx, job, &info, &playable).await)
}

/// How the web player plays `file` of a download that is already playing, as a title play
/// answers once a source starts.
pub async fn playing_file(ctx: &Ctx, id: DownloadId, file: u32) -> Result<Playing, Error> {
    let info = downloads::download_files(ctx, id).await?;
    let playable = downloads::choose(&info.layout, Some(file), None)?;
    let mut jobs = ctx.owner.ask(|reply| Command::Status { refs: vec![id.to_hex()], reply }).await?;
    let job = jobs.pop().ok_or_else(|| Error::new(ErrorCode::NotFound, "the download that played is gone"))?;
    Ok(playing(ctx, job, &info, &playable).await)
}

/// Where a download's video and its subtitles are served, and its files. `POST /api/play` and a
/// title play's `playing` line both answer with it.
async fn playing(ctx: &Ctx, job: Job, info: &FilesInfo, playable: &Playable) -> Playing {
    let layout = &info.layout;
    let streamed = |index: u32| served(job.id, layout, index);
    Playing {
        id: job.id.to_hex(),
        name: sanitize(&layout.name),
        files: downloads::listed(ctx, info).await,
        total_size: layout.total(),
        video: streamed(playable.video),
        subtitles: playable.subtitles.iter().map(|&index| streamed(index)).collect(),
        job,
    }
}
