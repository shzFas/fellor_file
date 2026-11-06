use crate::{TaskId, TransferEvent};
use fellor_vfs::VPath;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

const WINDOW: Duration = Duration::from_secs(3);

/// Per-task progress state with throttled emission and a moving-window
/// throughput estimate.
pub(crate) struct Progress {
    pub task: TaskId,
    pub done_files: u64,
    pub done_bytes: u64,
    pub total_files: u64,
    pub total_bytes: u64,
    pub current: Option<VPath>,
    interval: Duration,
    last_emit: Option<Instant>,
    samples: VecDeque<(Instant, u64)>,
}

impl Progress {
    pub fn new(task: TaskId, interval: Duration) -> Self {
        Self {
            task,
            done_files: 0,
            done_bytes: 0,
            total_files: 0,
            total_bytes: 0,
            current: None,
            interval,
            last_emit: None,
            samples: VecDeque::new(),
        }
    }

    pub fn add_bytes(&mut self, delta: i64) {
        self.done_bytes = if delta >= 0 { self.done_bytes.saturating_add(delta as u64) } else { self.done_bytes.saturating_sub(delta.unsigned_abs()) };
    }

    fn bps(&mut self, now: Instant) -> f64 {
        self.samples.push_back((now, self.done_bytes));
        while self.samples.len() > 2 && now.duration_since(self.samples[0].0) > WINDOW {
            self.samples.pop_front();
        }
        let (t0, b0) = self.samples[0];
        let dt = now.duration_since(t0).as_secs_f64();
        if dt < 0.05 {
            return 0.0;
        }
        (self.done_bytes.saturating_sub(b0)) as f64 / dt
    }

    /// Returns an event if at least `interval` passed since the last one (or `force`).
    pub fn tick(&mut self, force: bool) -> Option<TransferEvent> {
        let now = Instant::now();
        if !force {
            if let Some(t) = self.last_emit {
                if now.duration_since(t) < self.interval {
                    return None;
                }
            }
        }
        self.last_emit = Some(now);
        let bytes_per_sec = self.bps(now);
        Some(TransferEvent::Progress {
            task: self.task,
            done_files: self.done_files,
            done_bytes: self.done_bytes,
            total_files: self.total_files,
            total_bytes: self.total_bytes,
            bytes_per_sec,
            current: self.current.clone(),
        })
    }
}
