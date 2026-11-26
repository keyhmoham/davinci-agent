//! The economy mechanisms as runtime behaviour: repairing plans and failed
//! steps, and learning the cost model from finished tasks.

use super::repair;
use crate::engine::{Inner, TaskCtx};
use crate::planner::template_refs;
use dfae_core::*;
use std::sync::Arc;

impl Inner {
    /// Fix argument problems in a freshly parsed plan *before* validation, so a
    /// model is asked to repair a plan only for problems code cannot fix.
    pub(crate) async fn repair_plan_args(&self, ctx: &Arc<TaskCtx>, steps: &mut [Step], goal: &str) -> usize {
        if !self.cfg.economy.repair {
            return 0;
        }
        let entities = ctx.rec.lock().await.entities.clone();
        let mut fixed = Vec::new();
        for s in steps.iter_mut().filter(|s| s.kind == StepKind::Tool && !s.brief && template_refs(&s.args).is_empty()) {
            let Some(meta) = s.tool.as_deref().and_then(|t| self.tools.meta(t)) else { continue };
            if let Some(fix) = repair::fix_args(&meta, &s.args, goal, &entities) {
                s.args = fix.args;
                s.repairs += 1;
                fixed.push((s.id.clone(), fix.note));
            }
        }
        let n = fixed.len();
        for (step_id, note) in fixed {
            self.emit(ctx, EventKind::Repaired { step_id, note: format!("plan: {note}") }).await;
        }
        n
    }

    /// A step failed with an argument or path problem: repair it and let it run again.
    /// The retry does not consume the step's attempt budget (nothing flaky happened)
    /// and the repaired step is validated and policy-checked like any other.
    pub(crate) async fn repair_failed_step(&self, ctx: &Arc<TaskCtx>, step: &Step, kind: ErrorKind) -> bool {
        const MAX_REPAIRS: u32 = 2;
        if !self.cfg.economy.repair || step.kind != StepKind::Tool || step.repairs >= MAX_REPAIRS || !template_refs(&step.args).is_empty() {
            return false;
        }
        let Some(meta) = step.tool.as_deref().and_then(|t| self.tools.meta(t)) else { return false };
        let (goal, entities) = {
            let r = ctx.rec.lock().await;
            (r.goal.clone(), r.entities.clone())
        };
        let Some(fix) = repair::repair_after_failure(&meta, &step.args, kind, &goal, &entities, self.policy.workspace()) else { return false };
        {
            let mut r = ctx.rec.lock().await;
            let Some(s) = r.plan.as_mut().and_then(|p| p.step_mut(&step.id)) else { return false };
            s.args = fix.args;
            s.repairs += 1;
            s.attempts = s.attempts.saturating_sub(1);
            s.status = StepStatus::Pending;
            s.error = None;
        }
        self.emit(ctx, EventKind::Repaired { step_id: step.id.clone(), note: fix.note }).await;
        self.persist(ctx).await;
        true
    }

    /// Teach the cost model what this task's model calls really cost.
    pub(crate) async fn learn_costs(&self, rec: &TaskRecord) {
        let mut any = false;
        for e in rec.ledger.entries.iter().filter(|e| e.origin.is_none() && !e.cached && matches!(e.component, Component::Worker | Component::Planner)) {
            self.cost.observe(&e.purpose, e.prompt_tokens + e.completion_tokens);
            any = true;
        }
        if any {
            let _ = self.store.kv_set("costmodel", self.cost.state()).await;
        }
    }
}
