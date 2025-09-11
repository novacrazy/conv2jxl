use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;

/// A task that can be executed by a thread in the thread pool.
///
/// The task will be repeatedly executed as long as the thread pool is running.
pub trait Task: Send + 'static {
    fn run(&self, thread: &ThreadData);
}

impl<F> Task for F
where
    F: Fn(&ThreadData) + Send + 'static,
{
    #[inline(always)]
    fn run(&self, thread: &ThreadData) {
        (self)(thread);
    }
}

#[derive(Debug)]
pub struct PoolState {
    pub running: AtomicBool,
}

pub struct SimpleThreadPool {
    pub threads: Vec<thread::JoinHandle<()>>,
    pub state: Arc<PoolState>,
}

#[derive(Debug, Clone)]
pub struct ThreadData {
    pub idx: usize,
    pub pool: Arc<PoolState>,
}

impl SimpleThreadPool {
    pub fn push(&mut self, task: impl Task) {
        let data = ThreadData {
            idx: self.threads.len(),
            pool: self.state.clone(),
        };

        self.threads.push(thread::spawn(move || {
            while data.pool.running.load(Ordering::Relaxed) {
                task.run(&data);
            }
        }));
    }
}
