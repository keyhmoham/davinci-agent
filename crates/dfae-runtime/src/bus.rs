use dfae_core::Event;
use parking_lot::Mutex;
use std::collections::{HashMap, VecDeque};
use tokio::sync::broadcast;

/// In-process event bus: live broadcast plus a bounded per-task backlog so a
/// UI that attaches mid-task can replay what it missed.
pub struct EventBus {
    tx: broadcast::Sender<Event>,
    backlog: Mutex<Backlog>,
}

type Backlog = (HashMap<String, Vec<Event>>, VecDeque<String>);

const MAX_TASKS: usize = 200;
const MAX_EVENTS_PER_TASK: usize = 3000;

impl EventBus {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(2048);
        Self { tx, backlog: Mutex::new((HashMap::new(), VecDeque::new())) }
    }

    pub fn publish(&self, e: Event) {
        {
            let mut g = self.backlog.lock();
            if !g.0.contains_key(&e.task_id) {
                g.1.push_back(e.task_id.clone());
                if g.1.len() > MAX_TASKS
                    && let Some(old) = g.1.pop_front()
                {
                    g.0.remove(&old);
                }
            }
            let v = g.0.entry(e.task_id.clone()).or_default();
            if v.len() < MAX_EVENTS_PER_TASK {
                v.push(e.clone());
            }
        }
        let _ = self.tx.send(e);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }

    pub fn backlog(&self, task_id: &str) -> Vec<Event> {
        self.backlog.lock().0.get(task_id).cloned().unwrap_or_default()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}
