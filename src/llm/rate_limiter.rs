use std::{
    collections::VecDeque,
    sync::Mutex,
    time::{Duration, Instant},
};

/// Sliding-60-second-window request counter. `try_acquire` returns `false`
/// when the configured per-minute limit would be exceeded, so callers can
/// short-circuit instead of waiting in line.
pub struct RateLimiter {
    max_requests_per_minute: u32,
    window: Mutex<VecDeque<Instant>>,
}

impl RateLimiter {
    pub fn new(max_requests_per_minute: u32) -> Self {
        Self {
            max_requests_per_minute,
            window: Mutex::new(VecDeque::new()),
        }
    }

    pub fn try_acquire(&self) -> bool {
        let now = Instant::now();
        let cutoff = now - Duration::from_secs(60);
        let mut w = self.window.lock().expect("rate limiter poisoned");
        while w.front().is_some_and(|t| *t < cutoff) {
            w.pop_front();
        }
        if (w.len() as u32) >= self.max_requests_per_minute {
            return false;
        }
        w.push_back(now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lets_through_up_to_the_limit() {
        let rl = RateLimiter::new(3);
        assert!(rl.try_acquire());
        assert!(rl.try_acquire());
        assert!(rl.try_acquire());
        assert!(!rl.try_acquire());
    }

    #[test]
    fn zero_limit_blocks_everything() {
        let rl = RateLimiter::new(0);
        assert!(!rl.try_acquire());
    }
}
