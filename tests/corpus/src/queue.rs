//! In-process job queue with priority lanes and visibility timeouts.

use std::collections::VecDeque;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    Low,
    Normal,
    Urgent,
}

#[derive(Debug, Clone)]
pub struct Job {
    pub id: u64,
    pub payload: String,
    pub priority: Priority,
    pub attempts: u32,
}

/// Three lanes drained urgent-first, so a flood of low-priority work cannot
/// starve an urgent job behind it.
#[derive(Default)]
pub struct JobQueue {
    urgent: VecDeque<Job>,
    normal: VecDeque<Job>,
    low: VecDeque<Job>,
    in_flight: Vec<Job>,
}

impl JobQueue {
    pub fn enqueue(&mut self, job: Job) {
        match job.priority {
            Priority::Urgent => self.urgent.push_back(job),
            Priority::Normal => self.normal.push_back(job),
            Priority::Low => self.low.push_back(job),
        }
    }

    /// Take the next job and hold it in flight until acked or returned.
    pub fn dequeue(&mut self) -> Option<Job> {
        let job = self
            .urgent
            .pop_front()
            .or_else(|| self.normal.pop_front())
            .or_else(|| self.low.pop_front())?;
        self.in_flight.push(job.clone());
        Some(job)
    }

    pub fn ack(&mut self, id: u64) -> bool {
        let before = self.in_flight.len();
        self.in_flight.retain(|j| j.id != id);
        self.in_flight.len() < before
    }

    /// Return an unacked job to its lane with its attempt count incremented.
    pub fn requeue_expired(&mut self, id: u64) {
        if let Some(pos) = self.in_flight.iter().position(|j| j.id == id) {
            let mut job = self.in_flight.remove(pos);
            job.attempts += 1;
            self.enqueue(job);
        }
    }

    pub fn depth(&self) -> usize {
        self.urgent.len() + self.normal.len() + self.low.len()
    }
}
