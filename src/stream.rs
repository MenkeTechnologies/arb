//! Shared live-stream state. In M1 every stdin-bound widget renders from one
//! shared buffer (per-widget transforms/queries arrive with the query engine).

use std::collections::VecDeque;
use std::time::Instant;

pub struct StreamState {
    pub lines: VecDeque<String>,
    pub total: u64,
    pub start: Instant,
    cap: usize,
}

impl StreamState {
    pub fn new() -> Self {
        Self::with_cap(5000)
    }

    /// A stream buffer with a custom retention cap. `usize::MAX` = keep every
    /// line (fzf select mode, where dropping lines would lose marks and shift the
    /// cursor as the stream grows).
    pub fn with_cap(cap: usize) -> Self {
        Self {
            lines: VecDeque::new(),
            total: 0,
            start: Instant::now(),
            cap,
        }
    }

    /// Append a line, dropping the oldest beyond the retention cap.
    pub fn push(&mut self, line: String) {
        self.lines.push_back(line);
        self.total += 1;
        while self.lines.len() > self.cap {
            self.lines.pop_front();
        }
    }

    /// Lines per second since start.
    pub fn rate(&self) -> f64 {
        let secs = self.start.elapsed().as_secs_f64().max(0.001);
        self.total as f64 / secs
    }
}

impl Default for StreamState {
    fn default() -> Self {
        Self::new()
    }
}
