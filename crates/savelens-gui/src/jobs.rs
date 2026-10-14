//! Background execution so the UI thread never blocks:
//!
//! * [`Job`] — one long, independent computation (discovery, process hunt,
//!   scans over big files) on its own thread.
//! * [`Worker`] — a single thread that executes engine operations
//!   (history loading, mutations, cheat-table refreshes) **sequentially**, so
//!   two mutations of the same save can never race.

use std::panic::AssertUnwindSafe;
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, unbounded, Receiver, RecvTimeoutError, Sender, TryRecvError};
use savelens_watcher::Engine;

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    }
}

/// A computation running on its own thread.
pub struct Job<T> {
    label: String,
    rx: Receiver<Result<T, String>>,
    started: Instant,
    expected: Option<Duration>,
}

impl<T: Send + 'static> Job<T> {
    /// Starts `f` on a new thread. `expected` (if known) drives a progress bar.
    pub fn spawn(
        label: impl Into<String>,
        expected: Option<Duration>,
        f: impl FnOnce() -> T + Send + 'static,
    ) -> anyhow::Result<Self> {
        let label = label.into();
        let (tx, rx) = bounded(1);
        let thread_name = format!("savelens-gui-{}", label.to_ascii_lowercase().replace(' ', "-"));
        std::thread::Builder::new().name(thread_name).spawn(move || {
            let out = std::panic::catch_unwind(AssertUnwindSafe(f))
                .map_err(|p| format!("background task panicked: {}", panic_message(p.as_ref())));
            // The receiver is gone when the UI dropped the job; nothing to report then.
            let _ = tx.send(out);
        })?;
        Ok(Job { label, rx, started: Instant::now(), expected })
    }

    /// The result, once available.
    pub fn poll(&mut self) -> Option<Result<T, String>> {
        match self.rx.try_recv() {
            Ok(r) => Some(r),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err(format!("{}: worker thread terminated", self.label))),
        }
    }

    /// Waits up to `timeout` for the result.
    pub fn wait(&mut self, timeout: Duration) -> Option<Result<T, String>> {
        match self.rx.recv_timeout(timeout) {
            Ok(r) => Some(r),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => Some(Err(format!("{}: worker thread terminated", self.label))),
        }
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// Fraction of the expected duration elapsed (capped below 1 while running).
    pub fn progress(&self) -> Option<f32> {
        self.expected.map(
            |e| {
                if e.is_zero() {
                    0.99
                } else {
                    (self.elapsed().as_secs_f32() / e.as_secs_f32()).min(0.99)
                }
            },
        )
    }
}

type Task<M> = Box<dyn FnOnce(&Engine) -> M + Send>;

/// Sequential engine worker producing messages of type `M`.
pub struct Worker<M> {
    tx: Sender<Task<M>>,
    rx: Receiver<M>,
    pending: usize,
}

impl<M: Send + 'static> Worker<M> {
    /// Spawns the worker thread. `on_panic` converts a panicking task into a message.
    pub fn spawn(engine: Engine, on_panic: fn(String) -> M) -> anyhow::Result<Self> {
        let (tx, task_rx) = unbounded::<Task<M>>();
        let (msg_tx, rx) = unbounded::<M>();
        std::thread::Builder::new().name("savelens-gui-worker".into()).spawn(move || {
            for task in task_rx {
                let msg = match std::panic::catch_unwind(AssertUnwindSafe(|| task(&engine))) {
                    Ok(m) => m,
                    Err(p) => on_panic(panic_message(p.as_ref())),
                };
                if msg_tx.send(msg).is_err() {
                    break;
                }
            }
        })?;
        Ok(Worker { tx, rx, pending: 0 })
    }

    /// Queues a task. Returns false when the worker thread is gone.
    pub fn submit(&mut self, task: impl FnOnce(&Engine) -> M + Send + 'static) -> bool {
        if self.tx.send(Box::new(task)).is_ok() {
            self.pending += 1;
            true
        } else {
            false
        }
    }

    /// Next finished message, if any.
    pub fn try_recv(&mut self) -> Option<M> {
        match self.rx.try_recv() {
            Ok(m) => {
                self.pending = self.pending.saturating_sub(1);
                Some(m)
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                self.pending = 0;
                None
            }
        }
    }

    /// Waits up to `timeout` for the next finished message.
    pub fn recv_timeout(&mut self, timeout: Duration) -> Option<M> {
        match self.rx.recv_timeout(timeout) {
            Ok(m) => {
                self.pending = self.pending.saturating_sub(1);
                Some(m)
            }
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => {
                self.pending = 0;
                None
            }
        }
    }

    /// Number of queued or running tasks.
    pub fn pending(&self) -> usize {
        self.pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_returns_value_and_reports_panics() {
        let mut j = Job::spawn("sum", Some(Duration::from_secs(10)), || 2 + 2).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(j.wait(Duration::from_secs(5)), Some(Ok(4)));
        assert!(j.progress().is_some_and(|p| p < 1.0));
        let mut bad: Job<u8> = Job::spawn("bad", None, || panic!("kaboom")).unwrap_or_else(|e| panic!("{e}"));
        let r = bad.wait(Duration::from_secs(5));
        assert!(matches!(r, Some(Err(ref m)) if m.contains("kaboom")), "{r:?}");
        assert!(bad.progress().is_none());
    }
}
