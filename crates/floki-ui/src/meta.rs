//! Size and dates for the rows on screen, read off the UI thread.
//!
//! Searches come back without metadata (a page of 1000 rows would cost 1000
//! disk reads before anything shows). Instead the list asks for the rows it
//! is drawing; one background thread reads them with `GetFileAttributesExW`
//! and posts the answers back. Requests from an older search are dropped
//! unread, and the newest requests go first, so fast scrolling never queues
//! up a backlog of rows that already left the screen.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;

use floki_ntfs::FileMeta;

/// One row to read: the search it belongs to, its index, its full path.
#[derive(Debug)]
pub struct MetaRequest {
    pub seq: u64,
    pub idx: usize,
    pub path: String,
}

/// The answer; `meta` is `None` when the file is gone or unreadable.
#[derive(Debug)]
pub struct MetaReply {
    pub seq: u64,
    pub idx: usize,
    pub path: String,
    pub meta: Option<FileMeta>,
}

pub struct MetaWorker {
    tx: Sender<MetaRequest>,
    pub rx: Receiver<MetaReply>,
    /// Search the UI is showing; older requests are skipped.
    current: Arc<AtomicU64>,
}

impl MetaWorker {
    #[must_use]
    pub fn spawn() -> Self {
        let (req_tx, req_rx) = mpsc::channel::<MetaRequest>();
        let (rep_tx, rep_rx) = mpsc::channel::<MetaReply>();
        let current = Arc::new(AtomicU64::new(0));
        let seen = Arc::clone(&current);
        std::thread::Builder::new()
            .name("floki-meta".to_owned())
            .spawn(move || run(&req_rx, &rep_tx, &seen, floki_ntfs::file_meta))
            .expect("meta thread must spawn");
        Self {
            tx: req_tx,
            rx: rep_rx,
            current,
        }
    }

    /// The list now shows search `seq`; pending reads for others are dropped.
    pub fn set_current(&self, seq: u64) {
        self.current.store(seq, Ordering::Relaxed);
    }

    pub fn request(&self, req: MetaRequest) {
        let _ = self.tx.send(req);
    }
}

/// Worker loop: take everything queued, keep the current search's requests,
/// answer newest first. Ends when the UI side hangs up.
fn run(
    rx: &Receiver<MetaRequest>,
    tx: &Sender<MetaReply>,
    current: &AtomicU64,
    stat: impl Fn(&str) -> Option<FileMeta>,
) {
    while let Ok(first) = rx.recv() {
        let mut batch: Vec<MetaRequest> = std::iter::once(first).chain(rx.try_iter()).collect();
        while let Some(req) = batch.pop() {
            if req.seq != current.load(Ordering::Relaxed) {
                continue;
            }
            let meta = stat(&req.path);
            let reply = MetaReply {
                seq: req.seq,
                idx: req.idx,
                path: req.path,
                meta,
            };
            if tx.send(reply).is_err() {
                return;
            }
            // Newer requests (the user kept scrolling) jump the queue.
            batch.extend(rx.try_iter());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_newest_first_and_skips_old_searches() {
        let (req_tx, req_rx) = mpsc::channel();
        let (rep_tx, rep_rx) = mpsc::channel();
        let current = AtomicU64::new(2);
        let req = |seq, idx| MetaRequest {
            seq,
            idx,
            path: format!("p{idx}"),
        };
        req_tx.send(req(1, 0)).unwrap();
        req_tx.send(req(2, 1)).unwrap();
        req_tx.send(req(2, 2)).unwrap();
        drop(req_tx);
        let stat = |_: &str| {
            Some(FileMeta {
                is_dir: false,
                size: Some(1),
                modified_ms: None,
                created_ms: None,
            })
        };
        run(&req_rx, &rep_tx, &current, stat);
        let order: Vec<usize> = rep_rx.try_iter().map(|r| r.idx).collect();
        assert_eq!(order, [2, 1]);
    }
}
