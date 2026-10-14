//! Background jobs: long operations (discovery, scans, parsing big saves)
//! run on worker threads so the UI thread never blocks; results come back
//! through a channel polled by [`crate::App::tick`].

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::Instant;

use savelens_core::{Node, ScanSession};
use savelens_discovery::DiscoveryReport;

/// Name prefix of worker threads (the panic hook leaves the terminal alone
/// when one of them panics: the job is reported as failed instead).
pub const WORKER_THREAD_PREFIX: &str = "savelens-job";

/// Kind of a background job (at most one of each kind runs at a time).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobKind {
    Discovery,
    Scan,
    AstLoad,
}

/// A loaded AST for the explorer.
#[derive(Debug, Clone)]
pub struct LoadedTree {
    pub root: Node,
    pub badges: Vec<String>,
    pub codec: String,
}

/// Result of a finished job.
pub enum JobOutput {
    Discovery(Result<DiscoveryReport, String>),
    Scan {
        session: Box<ScanSession>,
        snapshot: Option<Box<Node>>,
        result: Result<usize, String>,
        label: String,
    },
    AstLoad {
        key: (i64, String, String),
        result: Result<LoadedTree, String>,
    },
    /// The job panicked or its thread could not be spawned.
    Failed {
        kind: JobKind,
        message: String,
    },
}

impl JobOutput {
    fn kind(&self) -> JobKind {
        match self {
            JobOutput::Discovery(_) => JobKind::Discovery,
            JobOutput::Scan { .. } => JobKind::Scan,
            JobOutput::AstLoad { .. } => JobKind::AstLoad,
            JobOutput::Failed { kind, .. } => *kind,
        }
    }
}

/// A running job.
#[derive(Debug, Clone)]
pub struct Running {
    pub kind: JobKind,
    pub label: String,
    pub started: Instant,
}

/// Job queue.
pub struct Jobs {
    tx: Sender<JobOutput>,
    rx: Receiver<JobOutput>,
    running: Vec<Running>,
}

impl Default for Jobs {
    fn default() -> Self {
        let (tx, rx) = channel();
        Jobs { tx, rx, running: Vec::new() }
    }
}

impl Jobs {
    /// True if a job of this kind is running.
    pub fn is_running(&self, kind: JobKind) -> bool {
        self.running.iter().any(|r| r.kind == kind)
    }

    /// True if any job is running.
    pub fn busy(&self) -> bool {
        !self.running.is_empty()
    }

    /// Running jobs.
    pub fn running(&self) -> &[Running] {
        &self.running
    }

    /// Starts `work` on a worker thread.
    pub fn spawn<F>(&mut self, kind: JobKind, label: impl Into<String>, work: F)
    where
        F: FnOnce() -> JobOutput + Send + 'static,
    {
        let label = label.into();
        let tx = self.tx.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("{WORKER_THREAD_PREFIX}-{kind:?}").to_lowercase())
            .spawn(move || {
                let out = match catch_unwind(AssertUnwindSafe(work)) {
                    Ok(out) => out,
                    Err(payload) => {
                        let message = payload
                            .downcast_ref::<&str>()
                            .map(|s| s.to_string())
                            .or_else(|| payload.downcast_ref::<String>().cloned())
                            .unwrap_or_else(|| "worker panicked".to_string());
                        JobOutput::Failed { kind, message }
                    }
                };
                let _ = tx.send(out);
            });
        match spawned {
            Ok(_) => self.running.push(Running { kind, label, started: Instant::now() }),
            Err(e) => {
                let _ = self.tx.send(JobOutput::Failed { kind, message: format!("cannot start worker thread: {e}") });
                self.running.push(Running { kind, label, started: Instant::now() });
            }
        }
    }

    /// Finished jobs (non-blocking).
    pub fn poll(&mut self) -> Vec<JobOutput> {
        let mut out = Vec::new();
        while let Ok(o) = self.rx.try_recv() {
            let kind = o.kind();
            if let Some(i) = self.running.iter().position(|r| r.kind == kind) {
                self.running.remove(i);
            }
            out.push(o);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn wait(jobs: &mut Jobs) -> Vec<JobOutput> {
        let start = Instant::now();
        let mut all = Vec::new();
        while jobs.busy() && start.elapsed() < Duration::from_secs(10) {
            all.extend(jobs.poll());
            std::thread::sleep(Duration::from_millis(5));
        }
        all
    }

    #[test]
    fn panicking_job_is_reported_as_failed() {
        let mut jobs = Jobs::default();
        jobs.spawn(JobKind::Discovery, "boom", || panic!("kaboom"));
        assert!(jobs.is_running(JobKind::Discovery));
        let out = wait(&mut jobs);
        assert!(!jobs.busy());
        match out.as_slice() {
            [JobOutput::Failed { kind, message }] => {
                assert_eq!(*kind, JobKind::Discovery);
                assert!(message.contains("kaboom"));
            }
            _ => panic!("expected one failed job"),
        }
    }

    #[test]
    fn successful_job_clears_running_state() {
        let mut jobs = Jobs::default();
        jobs.spawn(JobKind::Discovery, "discover", || JobOutput::Discovery(Err("none".into())));
        let out = wait(&mut jobs);
        assert_eq!(out.len(), 1);
        assert!(matches!(&out[0], JobOutput::Discovery(Err(m)) if m == "none"));
        assert!(jobs.running().is_empty());
    }
}
