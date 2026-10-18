use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

pub struct RateLimiter {
    max_messages: usize,
    window_ms: u64,
    max_concurrent: usize,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    connections: HashMap<String, usize>,
    message_log: HashMap<String, VecDeque<u64>>,
    last_sweep_ms: u64,
}

#[derive(Debug, PartialEq)]
pub enum MessageResult {
    Ok,
    RateLimited { retry_after_ms: u64 },
}

impl RateLimiter {
    pub fn new(max_messages: usize, window_ms: u64, max_concurrent: usize) -> Self {
        Self {
            max_messages,
            window_ms,
            max_concurrent,
            inner: Mutex::default(),
        }
    }

    pub fn acquire_connection(&self, ip: &str) -> bool {
        let mut inner = self.inner.lock().unwrap();
        let current = inner.connections.get(ip).copied().unwrap_or(0);
        if current >= self.max_concurrent {
            return false;
        }
        inner.connections.insert(ip.to_owned(), current + 1);
        true
    }

    pub fn release_connection(&self, ip: &str) {
        let mut inner = self.inner.lock().unwrap();
        match inner.connections.get(ip).copied().unwrap_or(0) {
            0 | 1 => {
                inner.connections.remove(ip);
            }
            n => {
                inner.connections.insert(ip.to_owned(), n - 1);
            }
        }
    }

    pub fn record_message(&self, ip: &str, now_ms: u64) -> MessageResult {
        let mut inner = self.inner.lock().unwrap();
        let cutoff = now_ms.saturating_sub(self.window_ms);
        // Sweep idle IPs so the map does not grow with every visitor ever seen.
        if inner.message_log.len() > 10_000
            && now_ms.saturating_sub(inner.last_sweep_ms) >= self.window_ms
        {
            inner.last_sweep_ms = now_ms;
            inner
                .message_log
                .retain(|_, h| h.back().is_some_and(|ts| *ts > cutoff));
        }
        let history = inner.message_log.entry(ip.to_owned()).or_default();
        while history.front().is_some_and(|ts| *ts <= cutoff) {
            history.pop_front();
        }
        if history.len() >= self.max_messages {
            let oldest = *history.front().unwrap();
            let retry_after_ms = (oldest + self.window_ms).saturating_sub(now_ms).max(1);
            return MessageResult::RateLimited { retry_after_ms };
        }
        history.push_back(now_ms);
        MessageResult::Ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_concurrent_connections_per_ip() {
        let rl = RateLimiter::new(10, 1000, 2);
        assert!(rl.acquire_connection("a"));
        assert!(rl.acquire_connection("a"));
        assert!(!rl.acquire_connection("a"));
        assert!(rl.acquire_connection("b"));
        rl.release_connection("a");
        assert!(rl.acquire_connection("a"));
    }

    #[test]
    fn sliding_window_limits_messages_with_retry_after() {
        let rl = RateLimiter::new(2, 1000, 1);
        assert_eq!(rl.record_message("a", 10_000), MessageResult::Ok);
        assert_eq!(rl.record_message("a", 10_100), MessageResult::Ok);
        assert_eq!(
            rl.record_message("a", 10_500),
            MessageResult::RateLimited {
                retry_after_ms: 500
            }
        );
        assert_eq!(rl.record_message("a", 11_001), MessageResult::Ok);
    }
}
