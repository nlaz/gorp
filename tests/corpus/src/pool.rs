//! Database connection pool with a bounded wait queue.

use std::time::Duration;

pub struct PoolOptions {
    pub max_connections: usize,
    pub acquire_timeout: Duration,
    pub idle_timeout: Duration,
}

impl Default for PoolOptions {
    fn default() -> Self {
        Self {
            max_connections: 16,
            acquire_timeout: Duration::from_secs(5),
            idle_timeout: Duration::from_secs(600),
        }
    }
}

pub struct Connection {
    pub id: usize,
    pub idle_since: Option<u64>,
}

pub struct ConnectionPool {
    opts: PoolOptions,
    idle: Vec<Connection>,
    leased: usize,
}

impl ConnectionPool {
    pub fn new(opts: PoolOptions) -> Self {
        Self { opts, idle: Vec::new(), leased: 0 }
    }

    /// Hand out an idle connection, or open a new one while under the cap.
    /// Returns `None` when the pool is saturated — callers back off rather
    /// than growing the pool without bound.
    pub fn acquire(&mut self) -> Option<Connection> {
        if let Some(conn) = self.idle.pop() {
            self.leased += 1;
            return Some(conn);
        }
        if self.leased < self.opts.max_connections {
            self.leased += 1;
            return Some(Connection { id: self.leased, idle_since: None });
        }
        None
    }

    pub fn release(&mut self, mut conn: Connection, now: u64) {
        conn.idle_since = Some(now);
        self.leased = self.leased.saturating_sub(1);
        self.idle.push(conn);
    }

    /// Close connections idle past the timeout so the database is not holding
    /// backends open for a pool that has gone quiet.
    pub fn reap_idle(&mut self, now: u64) -> usize {
        let cutoff = self.opts.idle_timeout.as_secs();
        let before = self.idle.len();
        self.idle.retain(|c| c.idle_since.is_none_or(|t| now.saturating_sub(t) < cutoff));
        before - self.idle.len()
    }

    pub fn in_use(&self) -> usize {
        self.leased
    }
}
