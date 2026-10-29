//! Admission regulation: how many "one job" runners this machine launches.
//!
//! Admitting a task *is* launching a runner that claims at most one job from Nestor and then
//! exits (contract v2 §2). There is no local admission token. A runner is launched only when the
//! mode is automatic, the machine is not blocked, fewer runners are alive than the ceiling, and
//! Nestor reports eligible jobs. Pausing or lowering the ceiling only stops *launching*: no busy
//! runner is interrupted. The regulator knows nothing about processor or memory use.
//!
//! This is good-faith regulation, not a security control (principle 7): the server enforces the
//! lease ceiling and the pause at claim time (S6).
//!
//! **Scopes (decision B1(b)).** While more than one runner may be alive, every live runner is
//! pinned to the same scope, and is launched with that scope as a restrictive filter for its
//! claim. A runner whose scope the service does not know (adopted after a restart, started by
//! hand without one) blocks further launches until it ends. Nestor must also refuse, at claim
//! time, a job of another scope than the one the machine holds leases in (S6, amendment A2):
//! the local rule alone cannot guarantee it, because the server picks the job.

use crate::model::{Admission, BlockReason, Mode, Queue, ScopeId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Denied {
    Paused,
    Blocked(BlockReason),
    AtCapacity {
        live: u32,
        ceiling: u32,
    },
    /// A live runner's scope is unknown, so another one could mix scopes.
    ScopeUnknown,
    /// A runner reported a job outside its scope; nothing more is launched until it ends.
    ScopeViolation,
}

/// What the supervisor may launch right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchPlan {
    pub count: u32,
    /// The scope the new runners are pinned to.
    pub scope: Option<ScopeId>,
}

impl LaunchPlan {
    pub fn none() -> Self {
        LaunchPlan { count: 0, scope: None }
    }
}

/// The scope a live runner is pinned to, as the supervisor tracks it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveScope {
    Pinned(ScopeId),
    /// Not known: adopted after a restart, or started by hand before any queue was seen.
    Unknown,
    /// Reported a job of another scope than its pin.
    Violated,
}

#[derive(Debug)]
pub struct Regulator {
    mode: Mode,
    ceiling: u32,
    blocked: Option<BlockReason>,
}

impl Regulator {
    pub fn new(mode: Mode, ceiling: u32) -> Self {
        Regulator { mode, ceiling: ceiling.max(1), blocked: None }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn ceiling(&self) -> u32 {
        self.ceiling
    }

    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
    }

    /// Takes effect at the next launch. Runners already alive keep running, so lowering the
    /// ceiling interrupts nothing.
    pub fn set_ceiling(&mut self, ceiling: u32) {
        self.ceiling = ceiling.max(1);
    }

    pub fn set_blocked(&mut self, reason: Option<BlockReason>) {
        self.blocked = reason;
    }

    pub fn admission(&self) -> Admission {
        if let Some(reason) = self.blocked {
            Admission::Blocked(reason)
        } else if self.mode == Mode::Paused {
            Admission::Paused
        } else {
            Admission::Open
        }
    }

    /// The scope every new runner must be pinned to, given the live ones, or why none may be
    /// launched.
    pub fn active_scope(live: &[LiveScope]) -> Result<Option<ScopeId>, Denied> {
        let mut active: Option<ScopeId> = None;
        for scope in live {
            match scope {
                LiveScope::Violated => return Err(Denied::ScopeViolation),
                LiveScope::Unknown => return Err(Denied::ScopeUnknown),
                LiveScope::Pinned(scope) => match &active {
                    // Two live runners already disagree: never add a third.
                    Some(current) if current != scope => return Err(Denied::ScopeViolation),
                    _ => active = Some(scope.clone()),
                },
            }
        }
        Ok(active)
    }

    /// Whether one more runner may be launched next to `live`, and in which scope. `queue`
    /// chooses the scope when no runner is alive; `None` without a queue (a manual start before
    /// any queue was seen) is allowed only when nothing else is alive.
    pub fn may_launch_one(&self, live: &[LiveScope], queue: Option<&Queue>) -> Result<Option<ScopeId>, Denied> {
        self.check_open(live.len() as u32)?;
        let active = Self::active_scope(live)?;
        Ok(active.or_else(|| queue.and_then(|q| q.busiest().map(|(scope, _)| scope.clone()))))
    }

    /// How many runners to launch for `queue`, and in which scope. Never more than the free
    /// room under the ceiling, never more than the eligible jobs of the chosen scope.
    pub fn plan(&self, live: &[LiveScope], queue: &Queue) -> Result<LaunchPlan, Denied> {
        let room = self.check_open(live.len() as u32)?;
        let (scope, eligible) = match Self::active_scope(live)? {
            Some(scope) => {
                let eligible = queue.in_scope(&scope);
                (scope, eligible)
            }
            None => match queue.busiest() {
                Some((scope, eligible)) => (scope.clone(), eligible),
                None => return Ok(LaunchPlan::none()),
            },
        };
        let count = room.min(eligible);
        Ok(if count == 0 { LaunchPlan::none() } else { LaunchPlan { count, scope: Some(scope) } })
    }

    /// Free room under the ceiling, or why there is none.
    fn check_open(&self, live: u32) -> Result<u32, Denied> {
        if let Some(reason) = self.blocked {
            return Err(Denied::Blocked(reason));
        }
        if self.mode == Mode::Paused {
            return Err(Denied::Paused);
        }
        if live >= self.ceiling {
            return Err(Denied::AtCapacity { live, ceiling: self.ceiling });
        }
        Ok(self.ceiling - live)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn queue(entries: &[(&str, u32)]) -> Queue {
        Queue { eligible: entries.iter().map(|(s, n)| (s.to_string(), *n)).collect::<BTreeMap<_, _>>() }
    }

    fn pinned(scope: &str) -> LiveScope {
        LiveScope::Pinned(scope.into())
    }

    #[test]
    fn launches_up_to_the_room_and_the_eligible_jobs() {
        let regulator = Regulator::new(Mode::Automatic, 3);
        assert_eq!(regulator.plan(&[], &queue(&[("a", 10)])), Ok(LaunchPlan { count: 3, scope: Some("a".into()) }));
        assert_eq!(regulator.plan(&[pinned("a")], &queue(&[("a", 1)])).unwrap().count, 1);
        assert_eq!(regulator.plan(&[], &Queue::empty()), Ok(LaunchPlan::none()));
        assert_eq!(
            regulator.plan(&[pinned("a"), pinned("a"), pinned("a")], &queue(&[("a", 5)])),
            Err(Denied::AtCapacity { live: 3, ceiling: 3 })
        );
    }

    #[test]
    fn paused_and_blocked_launch_nothing() {
        let mut regulator = Regulator::new(Mode::Paused, 4);
        assert_eq!(regulator.plan(&[], &queue(&[("a", 5)])), Err(Denied::Paused));
        regulator.set_mode(Mode::Automatic);
        regulator.set_blocked(Some(BlockReason::Revoked));
        assert_eq!(regulator.plan(&[], &queue(&[("a", 5)])), Err(Denied::Blocked(BlockReason::Revoked)));
        assert_eq!(regulator.admission(), Admission::Blocked(BlockReason::Revoked));
    }

    #[test]
    fn scopes_are_never_mixed() {
        let regulator = Regulator::new(Mode::Automatic, 4);
        // Nothing alive: the busiest scope is chosen, ties to the smallest identifier.
        assert_eq!(regulator.plan(&[], &queue(&[("b", 2), ("a", 2)])).unwrap().scope, Some("a".into()));
        assert_eq!(regulator.plan(&[], &queue(&[("b", 3), ("a", 2)])).unwrap().scope, Some("b".into()));
        // A runner of scope a is alive: only jobs of a count, those of b wait.
        assert_eq!(regulator.plan(&[pinned("a")], &queue(&[("b", 9)])), Ok(LaunchPlan::none()));
        assert_eq!(
            regulator.plan(&[pinned("a")], &queue(&[("a", 1), ("b", 9)])),
            Ok(LaunchPlan { count: 1, scope: Some("a".into()) })
        );
        // Unknown or violated scopes block launching altogether.
        assert_eq!(regulator.plan(&[LiveScope::Unknown], &queue(&[("a", 1)])), Err(Denied::ScopeUnknown));
        assert_eq!(regulator.plan(&[LiveScope::Violated], &queue(&[("a", 1)])), Err(Denied::ScopeViolation));
        assert_eq!(regulator.plan(&[pinned("a"), pinned("b")], &queue(&[("a", 1)])), Err(Denied::ScopeViolation));
    }

    #[test]
    fn a_manual_start_follows_the_same_rules() {
        let regulator = Regulator::new(Mode::Automatic, 2);
        assert_eq!(regulator.may_launch_one(&[], None), Ok(None));
        assert_eq!(regulator.may_launch_one(&[pinned("a")], None), Ok(Some("a".into())));
        assert_eq!(
            regulator.may_launch_one(&[pinned("a"), pinned("a")], None),
            Err(Denied::AtCapacity { live: 2, ceiling: 2 })
        );
        assert_eq!(regulator.may_launch_one(&[], Some(&queue(&[("c", 1)]))), Ok(Some("c".into())));
    }
}
