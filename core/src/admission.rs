//! Admission regulation: how many tasks this machine accepts at the same time.
//!
//! A runner must hold a token from the regulator before it claims a task from Nestor, and gives
//! it back when the task ends. The regulator never stops a task: pausing or lowering the ceiling
//! only refuses *new* tokens. It knows nothing about processor or memory use.

use std::collections::BTreeMap;

use crate::model::{Admission, BlockReason, Mode, RunnerId};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub id: u64,
    pub runner_id: RunnerId,
    pub generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Denied {
    Paused,
    Blocked(BlockReason),
    AtCapacity {
        busy: u32,
        ceiling: u32,
    },
    /// The runner already holds a token: one task per runner.
    AlreadyHolding,
}

#[derive(Debug)]
pub struct Regulator {
    mode: Mode,
    ceiling: u32,
    blocked: Option<BlockReason>,
    holders: BTreeMap<RunnerId, Token>,
    next_id: u64,
}

impl Regulator {
    pub fn new(mode: Mode, ceiling: u32) -> Self {
        Regulator { mode, ceiling: ceiling.max(1), blocked: None, holders: BTreeMap::new(), next_id: 1 }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn ceiling(&self) -> u32 {
        self.ceiling
    }

    pub fn busy(&self) -> u32 {
        self.holders.len() as u32
    }

    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
    }

    /// Takes effect on the next admission. Tokens already given stay valid, so lowering the
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

    /// Room for new admissions right now (0 when paused, blocked or at or above the ceiling).
    pub fn free_slots(&self) -> u32 {
        match self.admission() {
            Admission::Open => self.ceiling.saturating_sub(self.busy()),
            _ => 0,
        }
    }

    pub fn try_acquire(&mut self, runner_id: &str, generation: u64) -> Result<Token, Denied> {
        if let Some(reason) = self.blocked {
            return Err(Denied::Blocked(reason));
        }
        if self.mode == Mode::Paused {
            return Err(Denied::Paused);
        }
        if self.holders.contains_key(runner_id) {
            return Err(Denied::AlreadyHolding);
        }
        if self.busy() >= self.ceiling {
            return Err(Denied::AtCapacity { busy: self.busy(), ceiling: self.ceiling });
        }
        let token = Token { id: self.next_id, runner_id: runner_id.to_string(), generation };
        self.next_id += 1;
        self.holders.insert(runner_id.to_string(), token.clone());
        Ok(token)
    }

    /// Gives a token back. A stale token (already released, or from an earlier generation of the
    /// runner) is ignored and returns `false`.
    pub fn release(&mut self, token: &Token) -> bool {
        match self.holders.get(&token.runner_id) {
            Some(held) if held == token => {
                self.holders.remove(&token.runner_id);
                true
            }
            _ => false,
        }
    }

    /// Counts a runner found running after a restart of the service as busy, whatever the
    /// ceiling, until it reports that it is idle. This keeps the ceiling honest when the service
    /// does not know yet whether the runner is in the middle of a task.
    pub fn adopt(&mut self, runner_id: &str, generation: u64) -> Token {
        let token = Token { id: self.next_id, runner_id: runner_id.to_string(), generation };
        self.next_id += 1;
        self.holders.insert(runner_id.to_string(), token.clone());
        token
    }

    /// Releases whatever token the runner holds (the runner died or was removed).
    pub fn release_runner(&mut self, runner_id: &str) -> Option<Token> {
        self.holders.remove(runner_id)
    }

    pub fn token_of(&self, runner_id: &str) -> Option<&Token> {
        self.holders.get(runner_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ceiling_paused_and_blocked_refuse_new_tokens_only() {
        let mut regulator = Regulator::new(Mode::Automatic, 2);
        let a = regulator.try_acquire("a", 1).unwrap();
        let _b = regulator.try_acquire("b", 1).unwrap();
        assert_eq!(regulator.try_acquire("c", 1), Err(Denied::AtCapacity { busy: 2, ceiling: 2 }));
        assert_eq!(regulator.try_acquire("a", 1), Err(Denied::AlreadyHolding));

        regulator.set_ceiling(1);
        assert_eq!(regulator.busy(), 2, "lowering the ceiling keeps the tokens already given");
        assert!(regulator.release(&a));
        assert!(!regulator.release(&a), "a token is released once");
        assert_eq!(regulator.try_acquire("c", 1), Err(Denied::AtCapacity { busy: 1, ceiling: 1 }));

        regulator.set_mode(Mode::Paused);
        assert_eq!(regulator.busy(), 1);
        assert_eq!(regulator.admission(), Admission::Paused);
        regulator.set_blocked(Some(BlockReason::NotAssociated));
        assert_eq!(regulator.admission(), Admission::Blocked(BlockReason::NotAssociated));
        assert_eq!(regulator.free_slots(), 0);
    }

    #[test]
    fn stale_generation_token_does_not_release_the_new_one() {
        let mut regulator = Regulator::new(Mode::Automatic, 1);
        let old = regulator.try_acquire("a", 1).unwrap();
        regulator.release_runner("a");
        let new = regulator.try_acquire("a", 2).unwrap();
        assert!(!regulator.release(&old));
        assert_eq!(regulator.token_of("a"), Some(&new));
    }
}
