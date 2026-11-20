//! 고정 크기 워커 풀 (SPEC §4.2). async 런타임 없이 스레드 + 채널.
//! 작업 패닉은 `catch_unwind`로 격리해 풀이 줄어들지 않게 한다 (§6.2).

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::thread;

use crossbeam_channel::{Sender, unbounded};

type Job = Box<dyn FnOnce() + Send + 'static>;

pub struct WorkerPool {
    tx: Option<Sender<Job>>,
    handles: Vec<thread::JoinHandle<()>>,
}

impl WorkerPool {
    pub fn new(threads: usize) -> Self {
        let (tx, rx) = unbounded::<Job>();
        let handles = (0..threads.max(1))
            .map(|i| {
                let rx = rx.clone();
                thread::Builder::new()
                    .name(format!("leaf-worker-{i}"))
                    .stack_size(256 * 1024)
                    .spawn(move || {
                        for job in rx {
                            let _ = catch_unwind(AssertUnwindSafe(job));
                        }
                    })
                    .expect("spawn worker")
            })
            .collect();
        Self {
            tx: Some(tx),
            handles,
        }
    }

    pub fn spawn(&self, job: impl FnOnce() + Send + 'static) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(Box::new(job));
        }
    }
}

impl Drop for WorkerPool {
    fn drop(&mut self) {
        self.tx.take();
        for h in self.handles.drain(..) {
            let _ = h.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn runs_jobs_and_survives_panics() {
        let n = Arc::new(AtomicUsize::new(0));
        {
            let pool = WorkerPool::new(2);
            pool.spawn(|| panic!("boom"));
            for _ in 0..10 {
                let n = n.clone();
                pool.spawn(move || {
                    n.fetch_add(1, Ordering::SeqCst);
                });
            }
        } // drop → join
        assert_eq!(n.load(Ordering::SeqCst), 10);
    }
}
