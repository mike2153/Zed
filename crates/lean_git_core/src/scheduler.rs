use crate::models::RepoId;
#[cfg(test)]
use serde::{Deserialize, Serialize};
#[cfg(test)]
use std::collections::VecDeque;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Condvar, Mutex};

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub id: u64,
    pub repo_id: Option<RepoId>,
    pub kind: TaskKind,
    pub priority: TaskPriority,
    pub generation: u64,
}

#[cfg(test)]
impl Task {
    pub fn dedupe_key(&self) -> Option<DedupeKey> {
        match &self.kind {
            TaskKind::RefreshStatus => self
                .repo_id
                .as_ref()
                .map(|repo_id| DedupeKey(format!("refresh:{repo_id}"))),
            TaskKind::LoadHistory { repo_id } => Some(DedupeKey(format!("history:{repo_id}"))),
            TaskKind::ListDirectory { repo_id, path } => {
                Some(DedupeKey(format!("dir:{repo_id}:{path}")))
            }
            TaskKind::LoadDiff {
                repo_id,
                path,
                staged,
            } => Some(DedupeKey(format!("diff:{repo_id}:{staged}:{path}"))),
            TaskKind::MutateGit { .. } => None,
        }
    }

    pub fn is_mutation_for(&self, repo_id: &str) -> bool {
        self.repo_id.as_deref() == Some(repo_id) && matches!(self.kind, TaskKind::MutateGit { .. })
    }
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskKind {
    RefreshStatus,
    LoadHistory {
        repo_id: RepoId,
    },
    ListDirectory {
        repo_id: RepoId,
        path: String,
    },
    LoadDiff {
        repo_id: RepoId,
        path: String,
        staged: bool,
    },
    MutateGit {
        action: String,
    },
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum TaskPriority {
    Background = 0,
    Visible = 1,
    User = 2,
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct DedupeKey(pub String);

#[cfg(test)]
#[derive(Debug, Default)]
pub struct SchedulerQueue {
    next_id: u64,
    queued: VecDeque<Task>,
    dedupe: BTreeSet<DedupeKey>,
    running_mutations: BTreeSet<RepoId>,
}

#[derive(Debug, Default)]
pub struct RuntimeScheduler {
    state: Mutex<RuntimeState>,
    available: Condvar,
}

#[derive(Debug, Default)]
struct RuntimeState {
    running_workers: usize,
    running_by_repo: BTreeMap<RepoId, usize>,
    running_mutations: BTreeSet<RepoId>,
    waiting_mutations: BTreeMap<RepoId, usize>,
}

impl RuntimeScheduler {
    pub fn run<R>(
        &self,
        repo_id: Option<&str>,
        is_mutation: bool,
        max_workers: usize,
        work: impl FnOnce() -> R,
    ) -> R {
        let _permit = self.acquire(repo_id, is_mutation, max_workers);
        work()
    }

    pub fn running_workers(&self) -> usize {
        self.state
            .lock()
            .map(|state| state.running_workers)
            .unwrap_or(0)
    }

    fn acquire(
        &self,
        repo_id: Option<&str>,
        is_mutation: bool,
        max_workers: usize,
    ) -> RuntimePermit<'_> {
        let limit = max_workers.max(1);
        let repo_id = repo_id.map(ToString::to_string);
        let mut state = self.state.lock().expect("runtime scheduler lock poisoned");
        if is_mutation {
            if let Some(repo_id) = &repo_id {
                *state.waiting_mutations.entry(repo_id.clone()).or_insert(0) += 1;
            }
        }
        while runtime_blocked(&state, repo_id.as_deref(), is_mutation, limit) {
            state = self
                .available
                .wait(state)
                .expect("runtime scheduler lock poisoned");
        }
        if is_mutation {
            if let Some(repo_id) = &repo_id {
                if let Some(count) = state.waiting_mutations.get_mut(repo_id) {
                    *count = count.saturating_sub(1);
                    if *count == 0 {
                        state.waiting_mutations.remove(repo_id);
                    }
                }
            }
        }
        state.running_workers += 1;
        if let Some(repo_id) = &repo_id {
            *state.running_by_repo.entry(repo_id.clone()).or_insert(0) += 1;
        }
        if is_mutation {
            if let Some(repo_id) = &repo_id {
                state.running_mutations.insert(repo_id.clone());
            }
        }
        RuntimePermit {
            scheduler: self,
            repo_id,
            is_mutation,
        }
    }
}

fn runtime_blocked(
    state: &RuntimeState,
    repo_id: Option<&str>,
    is_mutation: bool,
    worker_limit: usize,
) -> bool {
    if state.running_workers >= worker_limit {
        return true;
    }
    let Some(repo_id) = repo_id else {
        return false;
    };
    if state.running_mutations.contains(repo_id) {
        return true;
    }
    if !is_mutation && state.waiting_mutations.get(repo_id).copied().unwrap_or(0) > 0 {
        return true;
    }
    is_mutation && state.running_by_repo.get(repo_id).copied().unwrap_or(0) > 0
}

struct RuntimePermit<'a> {
    scheduler: &'a RuntimeScheduler,
    repo_id: Option<RepoId>,
    is_mutation: bool,
}

impl Drop for RuntimePermit<'_> {
    fn drop(&mut self) {
        let mut state = self
            .scheduler
            .state
            .lock()
            .expect("runtime scheduler lock poisoned");
        state.running_workers = state.running_workers.saturating_sub(1);
        if self.is_mutation {
            if let Some(repo_id) = &self.repo_id {
                state.running_mutations.remove(repo_id);
            }
        }
        if let Some(repo_id) = &self.repo_id {
            if let Some(count) = state.running_by_repo.get_mut(repo_id) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    state.running_by_repo.remove(repo_id);
                }
            }
        }
        self.scheduler.available.notify_all();
    }
}

#[cfg(test)]
impl SchedulerQueue {
    pub fn enqueue(&mut self, mut task: Task) -> bool {
        if let Some(key) = task.dedupe_key() {
            if !self.dedupe.insert(key) {
                return false;
            }
        }
        if task.id == 0 {
            self.next_id += 1;
            task.id = self.next_id;
        }
        let insert_at = self
            .queued
            .iter()
            .position(|queued| queued.priority < task.priority)
            .unwrap_or(self.queued.len());
        self.queued.insert(insert_at, task);
        true
    }

    pub fn next(&mut self) -> Option<Task> {
        let index = self.queued.iter().position(|task| match &task.repo_id {
            Some(repo_id) if task.is_mutation_for(repo_id) => {
                !self.running_mutations.contains(repo_id)
            }
            _ => true,
        })?;
        let task = self.queued.remove(index)?;
        if let Some(key) = task.dedupe_key() {
            self.dedupe.remove(&key);
        }
        if let Some(repo_id) = &task.repo_id {
            if task.is_mutation_for(repo_id) {
                self.running_mutations.insert(repo_id.clone());
            }
        }
        Some(task)
    }

    pub fn finish(&mut self, task: &Task) {
        if let Some(repo_id) = &task.repo_id {
            if task.is_mutation_for(repo_id) {
                self.running_mutations.remove(repo_id);
            }
        }
    }

    pub fn len(&self) -> usize {
        self.queued.len()
    }

    pub fn is_empty(&self) -> bool {
        self.queued.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refresh(repo_id: &str, priority: TaskPriority) -> Task {
        Task {
            id: 0,
            repo_id: Some(repo_id.to_string()),
            kind: TaskKind::RefreshStatus,
            priority,
            generation: 0,
        }
    }

    fn mutation(repo_id: &str, action: &str) -> Task {
        Task {
            id: 0,
            repo_id: Some(repo_id.to_string()),
            kind: TaskKind::MutateGit {
                action: action.to_string(),
            },
            priority: TaskPriority::User,
            generation: 0,
        }
    }

    #[test]
    fn dedupes_status_refresh_by_repo() {
        let mut queue = SchedulerQueue::default();
        assert!(queue.enqueue(refresh("a", TaskPriority::Background)));
        assert!(!queue.enqueue(refresh("a", TaskPriority::User)));
        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn user_priority_runs_before_background() {
        let mut queue = SchedulerQueue::default();
        queue.enqueue(refresh("a", TaskPriority::Background));
        queue.enqueue(refresh("b", TaskPriority::User));
        assert_eq!(queue.next().unwrap().repo_id.as_deref(), Some("b"));
    }

    #[test]
    fn per_repo_mutation_lock_blocks_second_write() {
        let mut queue = SchedulerQueue::default();
        queue.enqueue(mutation("a", "commit"));
        queue.enqueue(mutation("a", "push"));
        let first = queue.next().unwrap();
        assert_eq!(
            first.kind,
            TaskKind::MutateGit {
                action: "commit".to_string()
            }
        );
        assert!(queue.next().is_none());
        queue.finish(&first);
        assert_eq!(
            queue.next().unwrap().kind,
            TaskKind::MutateGit {
                action: "push".to_string()
            }
        );
    }

    #[test]
    fn runtime_scheduler_enforces_worker_limit() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Barrier};
        use std::thread;
        use std::time::Duration;

        let scheduler = Arc::new(RuntimeScheduler::default());
        let barrier = Arc::new(Barrier::new(9));
        let current = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();

        for _ in 0..8 {
            let scheduler = Arc::clone(&scheduler);
            let barrier = Arc::clone(&barrier);
            let current = Arc::clone(&current);
            let peak = Arc::clone(&peak);
            handles.push(thread::spawn(move || {
                barrier.wait();
                scheduler.run(None, false, 2, || {
                    let now = current.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    thread::sleep(Duration::from_millis(20));
                    current.fetch_sub(1, Ordering::SeqCst);
                });
            }));
        }

        barrier.wait();
        for handle in handles {
            handle.join().unwrap();
        }

        assert!(peak.load(Ordering::SeqCst) <= 2);
    }

    #[test]
    fn runtime_scheduler_serializes_same_repo_mutations() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Barrier};
        use std::thread;
        use std::time::Duration;

        let scheduler = Arc::new(RuntimeScheduler::default());
        let barrier = Arc::new(Barrier::new(3));
        let current = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();

        for _ in 0..2 {
            let scheduler = Arc::clone(&scheduler);
            let barrier = Arc::clone(&barrier);
            let current = Arc::clone(&current);
            let peak = Arc::clone(&peak);
            handles.push(thread::spawn(move || {
                barrier.wait();
                scheduler.run(Some("repo-a"), true, 2, || {
                    let now = current.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    thread::sleep(Duration::from_millis(20));
                    current.fetch_sub(1, Ordering::SeqCst);
                });
            }));
        }

        barrier.wait();
        for handle in handles {
            handle.join().unwrap();
        }

        assert_eq!(peak.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn runtime_scheduler_blocks_reads_during_same_repo_mutation() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Barrier};
        use std::thread;
        use std::time::Duration;

        let scheduler = Arc::new(RuntimeScheduler::default());
        let barrier = Arc::new(Barrier::new(3));
        let current = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();

        for is_mutation in [true, false] {
            let scheduler = Arc::clone(&scheduler);
            let barrier = Arc::clone(&barrier);
            let current = Arc::clone(&current);
            let peak = Arc::clone(&peak);
            handles.push(thread::spawn(move || {
                barrier.wait();
                scheduler.run(Some("repo-a"), is_mutation, 2, || {
                    let now = current.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    thread::sleep(Duration::from_millis(20));
                    current.fetch_sub(1, Ordering::SeqCst);
                });
            }));
        }

        barrier.wait();
        for handle in handles {
            handle.join().unwrap();
        }

        assert_eq!(peak.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn runtime_scheduler_prioritizes_waiting_mutation_over_new_reads() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::thread;
        use std::time::Duration;

        let scheduler = Arc::new(RuntimeScheduler::default());
        let read_started = Arc::new(AtomicBool::new(false));
        let mutation_started = Arc::new(AtomicBool::new(false));
        let second_read_started = Arc::new(AtomicBool::new(false));

        let first_scheduler = Arc::clone(&scheduler);
        let first_started = Arc::clone(&read_started);
        let first_read = thread::spawn(move || {
            first_scheduler.run(Some("repo-a"), false, 2, || {
                first_started.store(true, Ordering::SeqCst);
                thread::sleep(Duration::from_millis(80));
            });
        });
        while !read_started.load(Ordering::SeqCst) {
            thread::yield_now();
        }

        let mutation_scheduler = Arc::clone(&scheduler);
        let mutation_flag = Arc::clone(&mutation_started);
        let mutation = thread::spawn(move || {
            mutation_scheduler.run(Some("repo-a"), true, 2, || {
                mutation_flag.store(true, Ordering::SeqCst);
                thread::sleep(Duration::from_millis(10));
            });
        });
        thread::sleep(Duration::from_millis(10));

        let second_scheduler = Arc::clone(&scheduler);
        let second_flag = Arc::clone(&second_read_started);
        let second_read = thread::spawn(move || {
            second_scheduler.run(Some("repo-a"), false, 2, || {
                second_flag.store(true, Ordering::SeqCst);
            });
        });

        thread::sleep(Duration::from_millis(20));
        assert!(!mutation_started.load(Ordering::SeqCst));
        assert!(!second_read_started.load(Ordering::SeqCst));

        first_read.join().unwrap();
        mutation.join().unwrap();
        assert!(mutation_started.load(Ordering::SeqCst));
        second_read.join().unwrap();
        assert!(second_read_started.load(Ordering::SeqCst));
    }
}
