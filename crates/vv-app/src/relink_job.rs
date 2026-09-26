//! Relinking in the background: walking a large folder and probing the
//! files found takes long enough for the desktop to flag the window as hung.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use vv_core::{MediaId, MediaMeta};

pub(crate) enum RelinkRequest {
    /// Media looked up by file name under `base_dir`; those still reachable
    /// at their path are left alone.
    ByName {
        base_dir: PathBuf,
        targets: Vec<(MediaId, PathBuf)>,
    },
    /// Files already chosen (forced relink): a `None` meta is probed.
    Chosen(Vec<(MediaId, PathBuf, Option<MediaMeta>)>),
}

pub(crate) struct PreparedRelink {
    pub(crate) media_id: MediaId,
    pub(crate) path: PathBuf,
    pub(crate) content_hash: u64,
    pub(crate) meta: Option<MediaMeta>,
}

pub(crate) struct RelinkOutcome {
    pub(crate) relinks: Vec<PreparedRelink>,
    /// Not found by name, with the folder searched: offered to the forced relink.
    pub(crate) not_found: Option<(PathBuf, Vec<MediaId>)>,
}

/// `total` stays 0 while the folder is being scanned.
#[derive(Default)]
pub(crate) struct RelinkProgress {
    pub(crate) done: AtomicUsize,
    pub(crate) total: AtomicUsize,
    pub(crate) cancel: AtomicBool,
}

pub(crate) struct RelinkJob {
    pub(crate) progress: Arc<RelinkProgress>,
    pub(crate) handle: std::thread::JoinHandle<Option<RelinkOutcome>>,
}

pub(crate) fn spawn(request: RelinkRequest) -> RelinkJob {
    let progress = Arc::new(RelinkProgress::default());
    let handle = std::thread::spawn({
        let progress = progress.clone();
        move || run(request, &progress)
    });
    RelinkJob { progress, handle }
}

/// `None` if cancelled.
pub(crate) fn run(request: RelinkRequest, progress: &RelinkProgress) -> Option<RelinkOutcome> {
    let (chosen, not_found) = match request {
        RelinkRequest::Chosen(chosen) => (chosen, None),
        RelinkRequest::ByName { base_dir, targets } => {
            let offline: Vec<(MediaId, PathBuf)> =
                targets.into_iter().filter(|(_, p)| !p.exists()).collect();
            let index = if offline.is_empty() {
                Default::default()
            } else {
                crate::project_io::index_media_by_filename(&base_dir)
            };
            if progress.cancel.load(Ordering::Relaxed) {
                return None;
            }
            let mut chosen = Vec::new();
            let mut failed = Vec::new();
            for (media_id, path) in offline {
                match path.file_name().and_then(|name| index.get(name)) {
                    Some(found) => chosen.push((media_id, found.clone(), None)),
                    None => failed.push(media_id),
                }
            }
            (chosen, (!failed.is_empty()).then_some((base_dir, failed)))
        }
    };
    progress.total.store(chosen.len(), Ordering::Relaxed);
    let mut relinks = Vec::with_capacity(chosen.len());
    for (media_id, path, meta) in chosen {
        if progress.cancel.load(Ordering::Relaxed) {
            return None;
        }
        relinks.push(PreparedRelink {
            media_id,
            content_hash: vv_media::content_fingerprint(&path).unwrap_or(0),
            meta: meta.or_else(|| vv_media::probe_media(&path).ok()),
            path,
        });
        progress.done.fetch_add(1, Ordering::Relaxed);
    }
    Some(RelinkOutcome { relinks, not_found })
}

#[cfg(test)]
#[path = "tests/relink_job.rs"]
mod tests;
