//! In-memory local writer leases. Restart revokes every token; runtime lifetime is independent.
use anyhow::{Result, ensure};
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};
use uuid::Uuid;

const TTL: Duration = Duration::from_secs(15);
struct Lease {
    token: Uuid,
    deadline: Instant,
    next_sequence: u64,
}
#[derive(Default)]
pub struct Ownership {
    leases: HashMap<Uuid, Lease>,
}
impl Ownership {
    pub fn revoke(&mut self, runtime: Uuid) {
        self.leases.remove(&runtime);
    }
    pub fn acquire(&mut self, runtime: Uuid, now: Instant) -> Result<Uuid> {
        self.leases.retain(|_, lease| lease.deadline > now);
        ensure!(
            !self.leases.contains_key(&runtime),
            "Інший клієнт уже керує цією сесією"
        );
        let token = Uuid::new_v4();
        self.leases.insert(
            runtime,
            Lease {
                token,
                deadline: now + TTL,
                next_sequence: 0,
            },
        );
        Ok(token)
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
