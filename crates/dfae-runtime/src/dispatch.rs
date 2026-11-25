//! Task dispatcher: admission control for top-level tasks and bounded,
//! role-specific worker pools for subagents.
//!
//! * **Admission** — at most `max_concurrent_tasks` top-level tasks run; the rest
//!   wait in a bounded queue and are rejected (load shedding) beyond it.
//! * **Worker pools** — an agent holds one global slot *and* one slot of its
//!   role's pool while it works. `operator=1` therefore serialises every
//!   agent that may change state, while researchers fan out freely.
//!
//! Deadlock freedom: coordinators (top-level tasks) never hold agent slots, and
//! agents cannot spawn agents (depth is limited to one level), so an agent
//! never waits on a slot while holding the resources another agent needs.

use dfae_core::{AgentRole, TeamConfig};
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

#[derive(Default)]
struct Gauge {
    running: AtomicUsize,
    peak: AtomicUsize,
}

impl Gauge {
    fn enter(self: &Arc<Self>) -> GaugeGuard {
        let now = self.running.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
        GaugeGuard(self.clone())
    }
    fn stat(&self, capacity: usize) -> PoolStat {
        PoolStat { running: self.running.load(Ordering::SeqCst), capacity, peak: self.peak.load(Ordering::SeqCst) }
    }
}

struct GaugeGuard(Arc<Gauge>);
impl Drop for GaugeGuard {
    fn drop(&mut self) {
        self.0.running.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Held for the lifetime of a running top-level task.
pub struct TaskPermit {
    _slot: OwnedSemaphorePermit,
    _gauge: GaugeGuard,
}

/// Held for the lifetime of a running agent.
pub struct AgentPermit {
    _role: OwnedSemaphorePermit,
    _global: OwnedSemaphorePermit,
    _gauges: [GaugeGuard; 2],
}

#[derive(Debug, Clone, Serialize, Default, PartialEq)]
pub struct PoolStat {
    pub running: usize,
    pub capacity: usize,
    /// Highest concurrency observed since start.
    pub peak: usize,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct DispatchSnapshot {
    pub tasks: PoolStat,
    pub queued: usize,
    pub max_queued: usize,
    pub agents: PoolStat,
    pub roles: BTreeMap<String, PoolStat>,
    pub agents_spawned: u64,
}

pub struct Dispatcher {
    admission: Arc<Semaphore>,
    task_cap: usize,
    max_queued: usize,
    queued: AtomicUsize,
    task_gauge: Arc<Gauge>,
    agents: Arc<Semaphore>,
    agent_cap: usize,
    agent_gauge: Arc<Gauge>,
    roles: BTreeMap<AgentRole, (Arc<Semaphore>, usize, Arc<Gauge>)>,
    spawned: AtomicU64,
}

impl Dispatcher {
    pub fn new(cfg: &TeamConfig) -> Self {
        let roles = AgentRole::WORKERS
            .iter()
            .map(|r| {
                let n = cfg.slots_for(r.as_str());
                (*r, (Arc::new(Semaphore::new(n)), n, Arc::new(Gauge::default())))
            })
            .collect();
        Self {
            admission: Arc::new(Semaphore::new(cfg.max_concurrent_tasks)),
            task_cap: cfg.max_concurrent_tasks,
            max_queued: cfg.max_queued_tasks,
            queued: AtomicUsize::new(0),
            task_gauge: Arc::default(),
            agents: Arc::new(Semaphore::new(cfg.max_agents)),
            agent_cap: cfg.max_agents,
            agent_gauge: Arc::default(),
            roles,
            spawned: AtomicU64::new(0),
        }
    }

    /// Would a new top-level task be rejected right now?
    pub fn saturated(&self) -> bool {
        self.admission.available_permits() == 0 && self.queued.load(Ordering::SeqCst) >= self.max_queued
    }

    /// A slot if one is free right now.
    pub fn try_admit(&self) -> Option<TaskPermit> {
        let slot = self.admission.clone().try_acquire_owned().ok()?;
        Some(TaskPermit { _slot: slot, _gauge: self.task_gauge.enter() })
    }

    /// Wait for a slot. Returns the permit and, if the task had to wait, its
    /// queue position at the time it joined.
    pub async fn admit(&self) -> (TaskPermit, Option<usize>) {
        let (slot, position) = match self.admission.clone().try_acquire_owned() {
            Ok(p) => (p, None),
            Err(_) => {
                let pos = self.queued.fetch_add(1, Ordering::SeqCst) + 1;
                let p = self.admission.clone().acquire_owned().await.expect("admission semaphore is never closed");
                self.queued.fetch_sub(1, Ordering::SeqCst);
                (p, Some(pos))
            }
        };
        (TaskPermit { _slot: slot, _gauge: self.task_gauge.enter() }, position)
    }

    /// Wait for a worker of `role`. Role slot first, then the global slot —
    /// always in that order, so two agents can never hold each other's needs.
    pub async fn acquire_agent(&self, role: AgentRole) -> AgentPermit {
        let (sem, _, gauge) = self.roles.get(&role).unwrap_or_else(|| &self.roles[&AgentRole::Researcher]);
        let r = sem.clone().acquire_owned().await.expect("role semaphore is never closed");
        let g = self.agents.clone().acquire_owned().await.expect("agent semaphore is never closed");
        self.spawned.fetch_add(1, Ordering::SeqCst);
        AgentPermit { _role: r, _global: g, _gauges: [gauge.enter(), self.agent_gauge.enter()] }
    }

    pub fn snapshot(&self) -> DispatchSnapshot {
        DispatchSnapshot {
            tasks: self.task_gauge.stat(self.task_cap),
            queued: self.queued.load(Ordering::SeqCst),
            max_queued: self.max_queued,
            agents: self.agent_gauge.stat(self.agent_cap),
            roles: self.roles.iter().map(|(r, (_, cap, g))| (r.as_str().to_string(), g.stat(*cap))).collect(),
            agents_spawned: self.spawned.load(Ordering::SeqCst),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn cfg() -> TeamConfig {
        let mut c = dfae_core::Config::default().team;
        c.max_concurrent_tasks = 1;
        c.max_queued_tasks = 1;
        c.max_agents = 3;
        c.role_slots = [("operator".to_string(), 1), ("researcher".to_string(), 3)].into_iter().collect();
        c
    }

    #[tokio::test]
    async fn admission_queues_then_sheds_load() {
        let d = Arc::new(Dispatcher::new(&cfg()));
        let (first, pos) = d.admit().await;
        assert!(pos.is_none());
        assert!(!d.saturated());
        let d2 = d.clone();
        let waiter = tokio::spawn(async move { d2.admit().await.1 });
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(d.snapshot().queued, 1);
        assert!(d.saturated(), "queue is full: new tasks are rejected");
        drop(first);
        assert_eq!(waiter.await.unwrap(), Some(1));
    }

    #[tokio::test]
    async fn operators_are_serialised_and_researchers_run_in_parallel() {
        let d = Arc::new(Dispatcher::new(&cfg()));
        let op1 = d.acquire_agent(AgentRole::Operator).await;
        let d2 = d.clone();
        let op2 = tokio::spawn(async move { d2.acquire_agent(AgentRole::Operator).await });
        let r1 = d.acquire_agent(AgentRole::Researcher).await;
        let r2 = d.acquire_agent(AgentRole::Researcher).await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(!op2.is_finished(), "the second operator waits for the first");
        let s = d.snapshot();
        assert_eq!(s.roles["operator"].running, 1);
        assert_eq!(s.roles["researcher"].running, 2);
        assert_eq!(s.agents.running, 3);
        drop(op1);
        let _op2 = op2.await.unwrap();
        drop((r1, r2));
        let s = d.snapshot();
        assert_eq!(s.roles["operator"].peak, 1, "never more than one operator at a time");
        assert_eq!(s.agents_spawned, 4);
    }
}
