//! The two scrollbacks: the process's own output, and procwatch's activity log
//! (one line per sample, plus events).

use std::collections::VecDeque;
use std::collections::vec_deque;

use crate::render::Segment;

/// A FIFO that drops its oldest item once full.
pub struct Ring<T> {
    items: VecDeque<T>,
    capacity: usize,
}

impl<T> Ring<T> {
    pub fn new(capacity: usize) -> Self {
        Self { items: VecDeque::with_capacity(capacity.min(1024)), capacity }
    }

    pub fn push(&mut self, item: T) {
        if self.items.len() == self.capacity {
            self.items.pop_front();
        }
        self.items.push_back(item);
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn clear(&mut self) {
        self.items.clear();
    }

    pub fn iter(&self) -> vec_deque::Iter<'_, T> {
        self.items.iter()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Stdout,
    Stderr,
}

pub struct OutputLine {
    pub source: Source,
    pub segments: Vec<Segment>,
}

/// One sample as it appears in the activity log.
pub struct Sample {
    pub cpu: f32,
    pub cpu_spike: bool,
    pub memory: u64,
    pub memory_delta: i64,
    pub memory_jump: bool,
}

pub enum Activity {
    Sample(Sample),
    Event(String),
    Alert(String),
}

pub struct ActivityLine {
    pub at: u64,
    pub activity: Activity,
}

impl ActivityLine {
    /// Spikes, jumps and events: what the "spikes only" view keeps.
    pub fn is_notable(&self) -> bool {
        match &self.activity {
            Activity::Sample(s) => s.cpu_spike || s.memory_jump,
            Activity::Event(_) | Activity::Alert(_) => true,
        }
    }
}
