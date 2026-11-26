//! In-memory local writer leases. Restart revokes every token; runtime lifetime is independent.
use crate::model::{
    INPUT_CHUNK_LIMIT, INPUT_LEASE_TTL as TTL, PASTE_LIMIT, PasteBinding, PasteResult, ResizeEpoch,
    TerminalView,
};
use anyhow::{Result, ensure};
use std::{collections::HashMap, time::Instant};
use uuid::Uuid;

#[derive(Debug)]
pub(crate) struct ResizeDeadlineReached;

impl std::fmt::Display for ResizeDeadlineReached {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Resize authority changed")
    }
}

impl std::error::Error for ResizeDeadlineReached {}

struct Lease {
    token: Uuid,
    deadline: Instant,
    next_sequence: u64,
    next_resize: u64,
    terminal: Option<TerminalView>,
    epoch: Option<ResizeEpoch>,
    paste_incarnation: Option<Uuid>,
    paste_high_water: u64,
    stage: Option<Stage>,
}

struct Stage {
    binding: PasteBinding,
    bytes: Vec<u8>,
}

impl Lease {
    fn advance_paste(&mut self, binding: &PasteBinding) -> bool {
        if binding.operation <= self.paste_high_water {
            return false;
        }
        self.paste_incarnation = Some(binding.incarnation);
        self.paste_high_water = binding.operation;
        self.stage = None;
        true
    }
}

#[derive(Default)]
pub struct Ownership {
    leases: HashMap<Uuid, Lease>,
}
impl Ownership {
    pub fn revoke(&mut self, runtime: Uuid) {
        self.leases.remove(&runtime);
    }
    pub fn unowned(&self, runtime: Uuid, now: Instant) -> Result<()> {
        ensure!(
            !self
                .leases
                .get(&runtime)
                .is_some_and(|lease| lease.deadline > now),
            "Інший клієнт уже керує цією сесією"
        );
        Ok(())
    }

    #[cfg(test)]
    pub fn acquire(&mut self, runtime: Uuid, now: Instant) -> Result<Uuid> {
        self.grant(runtime, None, None, now)
    }

    pub fn grant(
        &mut self,
        runtime: Uuid,
        terminal: Option<TerminalView>,
        epoch: Option<ResizeEpoch>,
        now: Instant,
    ) -> Result<Uuid> {
        self.unowned(runtime, now)?;
        let token = Uuid::new_v4();
        self.leases.insert(
            runtime,
            Lease {
                token,
                deadline: now + TTL,
                next_sequence: 0,
                next_resize: 0,
                terminal,
                epoch,
                paste_incarnation: None,
                paste_high_water: 0,
                stage: None,
            },
        );
        Ok(token)
    }
    pub fn resize(
        &mut self,
        terminal: &TerminalView,
        token: Uuid,
        epoch: ResizeEpoch,
        sequence: u64,
        now: Instant,
    ) -> Result<()> {
        let lease = self
            .leases
            .get_mut(&terminal.target.runtime_id)
            .ok_or_else(|| anyhow::anyhow!("Керування втрачено"))?;
        ensure!(
            lease.token == token
                && lease.terminal.as_ref() == Some(terminal)
                && lease.epoch == Some(epoch),
            "Resize authority changed"
        );
        if now >= lease.deadline {
            if lease.next_resize == sequence && sequence < u64::MAX {
                return Err(ResizeDeadlineReached.into());
            }
            anyhow::bail!("Resize authority changed");
        }
        ensure!(lease.next_resize == sequence, "Resize sequence rejected");
        lease.next_resize = sequence
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("Resize sequence exhausted"))?;
        Ok(())
    }
    pub fn renew(&mut self, runtime: Uuid, token: Uuid, now: Instant) -> Result<()> {
        let lease = self
            .leases
            .get_mut(&runtime)
            .ok_or_else(|| anyhow::anyhow!("Керування втрачено"))?;
        ensure!(
            lease.token == token && lease.deadline > now,
            "Керування втрачено"
        );
        lease.deadline = now + TTL;
        Ok(())
    }
    pub fn input(&mut self, runtime: Uuid, token: Uuid, sequence: u64, now: Instant) -> Result<()> {
        self.renew(runtime, token, now)?;
        let lease = self.leases.get_mut(&runtime).expect("validated lease");
        ensure!(
            lease.next_sequence == sequence,
            "Input sequence rejected; do not replay"
        );
        // Consume before sending: ambiguous failures must never replay a command.
        lease.next_sequence = sequence
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("Input sequence exhausted"))?;
        Ok(())
    }
    pub fn deadline(&self, runtime: Uuid, token: Uuid) -> Option<Instant> {
        self.leases
            .get(&runtime)
            .filter(|lease| lease.token == token)
            .map(|lease| lease.deadline)
    }

    pub fn epoch(&self, runtime: Uuid, token: Uuid, now: Instant) -> Option<ResizeEpoch> {
        self.leases
            .get(&runtime)
            .filter(|lease| lease.token == token && lease.deadline > now)
            .and_then(|lease| lease.epoch)
    }

    fn paste_lease(&mut self, binding: &PasteBinding, now: Instant) -> Option<&mut Lease> {
        let lease = self.leases.get_mut(&binding.terminal.target.runtime_id)?;
        if lease.token != binding.token {
            return None;
        }
        if lease.deadline <= now {
            lease.stage = None;
            return None;
        }
        (lease.terminal.as_ref() == Some(&binding.terminal)
            && lease.epoch == Some(binding.epoch)
            && lease.next_sequence == binding.sequence
            && binding.sequence < u64::MAX
            && !binding.incarnation.is_nil()
            && lease
                .paste_incarnation
                .is_none_or(|id| id == binding.incarnation))
        .then_some(lease)
    }

    pub fn paste_live(&mut self, binding: &PasteBinding, now: Instant) -> bool {
        self.paste_lease(binding, now).is_some_and(|lease| {
            lease.paste_incarnation == Some(binding.incarnation)
                && lease.paste_high_water == binding.operation
        })
    }

    pub fn stage(
        &mut self,
        binding: PasteBinding,
        offset: usize,
        data: &[u8],
        now: Instant,
    ) -> PasteResult {
        let Some(lease) = self.paste_lease(&binding, now) else {
            return PasteResult::Stale {};
        };
        if lease.advance_paste(&binding) {
            if offset != 0 {
                return PasteResult::Invalid {};
            }
            lease.stage = Some(Stage {
                binding: binding.clone(),
                bytes: Vec::with_capacity(PASTE_LIMIT),
            });
        }
        let Some(stage) = lease
            .stage
            .as_mut()
            .filter(|stage| stage.binding == binding)
        else {
            return PasteResult::Stale {};
        };
        if offset != stage.bytes.len()
            || !(1..=INPUT_CHUNK_LIMIT).contains(&data.len())
            || offset + data.len() > PASTE_LIMIT
        {
            lease.stage = None;
            return PasteResult::Invalid {};
        }
        stage.bytes.extend_from_slice(data);
        PasteResult::Staged {}
    }

    pub fn take_stage(&mut self, binding: &PasteBinding) -> Option<Vec<u8>> {
        let runtime = binding.terminal.target.runtime_id;
        let lease = self.leases.get_mut(&runtime)?;
        let staged = &lease.stage.as_ref()?.binding;
        if staged == binding && lease.next_sequence == binding.sequence {
            return lease.stage.take().map(|stage| stage.bytes);
        }
        let original_sequence = PasteBinding {
            sequence: staged.sequence,
            ..binding.clone()
        };
        if staged == &original_sequence {
            self.leases.remove(&runtime);
        }
        None
    }

    pub fn discard_stage(&mut self, runtime: Uuid, token: Uuid) {
        if let Some(lease) = self
            .leases
            .get_mut(&runtime)
            .filter(|lease| lease.token == token)
        {
            lease.stage = None;
        }
    }

    pub fn discard_rejected_stage(&mut self, binding: &PasteBinding, now: Instant) {
        let Some(lease) = self.paste_lease(binding, now) else {
            return;
        };
        if lease.advance_paste(binding)
            || lease
                .stage
                .as_ref()
                .is_some_and(|stage| stage.binding == *binding)
        {
            lease.stage = None;
        }
    }

    pub fn confirm_paste(&mut self, binding: &PasteBinding) -> bool {
        let Some(lease) = self.leases.get_mut(&binding.terminal.target.runtime_id) else {
            return false;
        };
        if lease.token != binding.token || lease.next_sequence != binding.sequence {
            return false;
        }
        let Some(next) = binding.sequence.checked_add(1) else {
            return false;
        };
        lease.next_sequence = next;
        true
    }

    #[cfg(test)]
    pub fn input_sequence(&self, binding: &PasteBinding) -> Option<u64> {
        self.leases
            .get(&binding.terminal.target.runtime_id)
            .filter(|lease| lease.token == binding.token)
            .map(|lease| lease.next_sequence)
    }
    pub fn release(&mut self, runtime: Uuid, token: Uuid) {
        if self
            .leases
            .get(&runtime)
            .is_some_and(|lease| lease.token == token)
        {
            self.leases.remove(&runtime);
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    #[test]
    fn exclusive_expiring_leases_reject_stale_release_and_replayed_input() {
        let mut owners = Ownership::default();
        let id = Uuid::new_v4();
        let now = Instant::now();
        let first = owners.acquire(id, now).unwrap();
        assert!(owners.acquire(id, now).is_err());
        assert!(owners.input(id, Uuid::new_v4(), 0, now).is_err());
        owners.input(id, first, 0, now).unwrap();
        assert!(owners.input(id, first, 0, now).is_err());
        assert!(owners.input(id, first, 2, now).is_err());
        let later = now + TTL + Duration::from_secs(1);
        assert!(owners.renew(id, first, later).is_err());
        let second = owners.acquire(id, later).unwrap();
        owners.release(id, first);
        owners.input(id, second, 0, later).unwrap();
        assert!(Ownership::default().input(id, second, 1, later).is_err());
        owners.release(id, second);
        assert!(owners.acquire(id, later).is_ok());
    }
}
