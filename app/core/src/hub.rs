//! Thread-safe holder of the latest frame, shared by one producer and many HTTP clients.

use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

/// A JPEG frame shared between clients without copying.
pub type Frame = Arc<Vec<u8>>;

/// Outcome of [`FrameHub::wait_newer`].
#[derive(Debug)]
pub enum Wait {
    Frame(u64, Frame),
    Timeout,
    Closed,
}

#[derive(Default)]
struct State {
    frame: Option<Frame>,
    seq: u64,
    paused: bool,
    closed: bool,
    clients: usize,
}

/// Keeps only the most recent frame; consumers block until a newer one arrives.
///
/// Each frame gets a monotonically increasing sequence number so a client can
/// ask for "anything newer than what I already sent", which drops frames for
/// slow clients instead of queueing them. The `paused` flag tells clients the
/// producer is intentionally stopped, so they may re-send the last frame as a
/// keepalive.
#[derive(Default)]
pub struct FrameHub {
    state: Mutex<State>,
    cond: Condvar,
}

/// Counts a connected client for as long as it is alive.
pub struct ClientGuard<'a>(&'a FrameHub);

impl Drop for ClientGuard<'_> {
    fn drop(&mut self) {
        self.0.lock().clients -= 1;
    }
}

impl FrameHub {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Replace the current frame, wake every waiting client, return its sequence number.
    pub fn publish(&self, frame: Vec<u8>) -> u64 {
        let mut state = self.lock();
        state.frame = Some(Arc::new(frame));
        state.seq += 1;
        self.cond.notify_all();
        state.seq
    }

    /// `(seq, frame)` of the current frame; `(0, None)` before the first one.
    pub fn latest(&self) -> (u64, Option<Frame>) {
        let state = self.lock();
        (state.seq, state.frame.clone())
    }

    /// Block until a frame newer than `after` exists, the timeout elapses, or the hub closes.
    pub fn wait_newer(&self, after: u64, timeout: Duration) -> Wait {
        let guard = self.lock();
        let (state, _) = self
            .cond
            .wait_timeout_while(guard, timeout, |s| !s.closed && s.seq <= after)
            .unwrap_or_else(PoisonError::into_inner);
        match (&state.frame, state.closed) {
            (_, true) => Wait::Closed,
            (Some(frame), false) if state.seq > after => Wait::Frame(state.seq, frame.clone()),
            _ => Wait::Timeout,
        }
    }

    pub fn set_paused(&self, paused: bool) {
        self.lock().paused = paused;
    }

    pub fn is_paused(&self) -> bool {
        self.lock().paused
    }

    /// Release every waiting client; later waits return [`Wait::Closed`].
    pub fn close(&self) {
        self.lock().closed = true;
        self.cond.notify_all();
    }

    pub fn client_guard(&self) -> ClientGuard<'_> {
        self.lock().clients += 1;
        ClientGuard(self)
    }

    pub fn clients(&self) -> usize {
        self.lock().clients
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn wait_newer_returns_latest_and_times_out() {
        let hub = FrameHub::new();
        assert!(matches!(hub.latest(), (0, None)));
        assert!(matches!(hub.wait_newer(0, Duration::from_millis(5)), Wait::Timeout));
        hub.publish(b"a".to_vec());
        hub.publish(b"b".to_vec());
        match hub.wait_newer(0, Duration::from_millis(5)) {
            Wait::Frame(2, f) => assert_eq!(f.as_slice(), b"b"),
            other => panic!("unexpected {other:?}"),
        }
        assert!(matches!(hub.wait_newer(2, Duration::from_millis(5)), Wait::Timeout));
    }

    #[test]
    fn publish_wakes_waiter() {
        let hub = Arc::new(FrameHub::new());
        let h = hub.clone();
        let waiter = thread::spawn(move || h.wait_newer(0, Duration::from_secs(5)));
        thread::sleep(Duration::from_millis(20));
        hub.publish(b"x".to_vec());
        assert!(matches!(waiter.join().unwrap(), Wait::Frame(1, _)));
    }

    #[test]
    fn close_releases_waiters() {
        let hub = Arc::new(FrameHub::new());
        let h = hub.clone();
        let waiter = thread::spawn(move || h.wait_newer(0, Duration::from_secs(5)));
        thread::sleep(Duration::from_millis(20));
        hub.close();
        assert!(matches!(waiter.join().unwrap(), Wait::Closed));
    }

    #[test]
    fn client_guard_counts() {
        let hub = FrameHub::new();
        let a = hub.client_guard();
        let b = hub.client_guard();
        assert_eq!(hub.clients(), 2);
        drop(a);
        drop(b);
        assert_eq!(hub.clients(), 0);
    }
}
