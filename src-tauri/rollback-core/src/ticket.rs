use std::sync::Mutex;
use std::time::{Duration, Instant};
use uuid::Uuid;

use crate::{Digest, ForkVersion, ProtocolError};

/// Supplied by the coordinator only after package and snapshot preflight.
/// This is a binding of that result, not a substitute for minisign verification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedSelection {
    pub point_id: Uuid,
    pub current_version: ForkVersion,
    pub snapshot_digest: Digest,
    pub setup_digest: Digest,
}

struct Confirmation {
    nonce: Uuid,
    selection: PreparedSelection,
    issued_at: Instant,
}

/// Backend gate between the two confirmations. A new preparation replaces the
/// previous one; cancellation, target changes, expiration or process restart
/// invalidate it. Actual helper execution still needs the transaction lease.
pub struct TicketGate {
    pending: Mutex<Option<Confirmation>>,
    ttl: Duration,
}

impl TicketGate {
    pub fn new(ttl: Duration) -> Result<Self, ProtocolError> {
        if ttl.is_zero() || ttl > Duration::from_secs(300) {
            return Err(ProtocolError::Invalid(
                "confirmation lifetime must be 1ns..5min".into(),
            ));
        }
        Ok(Self {
            pending: Mutex::new(None),
            ttl,
        })
    }

    /// Called after the first confirmation and a fresh preflight, never merely
    /// because the About page displays a previous-version entry.
    pub fn issue(&self, selection: PreparedSelection) -> Result<Uuid, ProtocolError> {
        self.issue_at(selection, Instant::now())
    }

    fn issue_at(
        &self,
        selection: PreparedSelection,
        issued_at: Instant,
    ) -> Result<Uuid, ProtocolError> {
        if selection.point_id.is_nil() {
            return Err(ProtocolError::Invalid(
                "confirmation has no rollback point".into(),
            ));
        }
        let nonce = Uuid::new_v4();
        *self
            .pending
            .lock()
            .map_err(|_| ProtocolError::TicketUnavailable)? = Some(Confirmation {
            nonce,
            selection,
            issued_at,
        });
        Ok(nonce)
    }

    pub fn cancel(&self, nonce: Uuid) -> Result<(), ProtocolError> {
        let mut pending = self
            .pending
            .lock()
            .map_err(|_| ProtocolError::TicketUnavailable)?;
        if pending
            .as_ref()
            .is_some_and(|confirmation| confirmation.nonce == nonce)
        {
            *pending = None;
        }
        Ok(())
    }

    /// The second confirmation consumes the nonce atomically before a helper
    /// may start. Repeated or concurrent IPC calls can get at most one grant.
    pub fn consume(
        &self,
        nonce: Uuid,
        freshly_verified: &PreparedSelection,
    ) -> Result<PreparedSelection, ProtocolError> {
        self.consume_at(nonce, freshly_verified, Instant::now())
    }

    fn consume_at(
        &self,
        nonce: Uuid,
        freshly_verified: &PreparedSelection,
        now: Instant,
    ) -> Result<PreparedSelection, ProtocolError> {
        let mut pending = self
            .pending
            .lock()
            .map_err(|_| ProtocolError::TicketUnavailable)?;
        let matching = pending
            .as_ref()
            .is_some_and(|confirmation| confirmation.nonce == nonce);
        if !matching {
            return Err(ProtocolError::TicketUnavailable);
        }
        let confirmation = pending.take().ok_or(ProtocolError::TicketUnavailable)?;
        // Instant makes TTL independent of changes to the wall clock.
        if now
            .checked_duration_since(confirmation.issued_at)
            .is_none_or(|age| age >= self.ttl)
        {
            return Err(ProtocolError::TicketUnavailable);
        }
        if &confirmation.selection != freshly_verified {
            return Err(ProtocolError::SelectionChanged);
        }
        Ok(confirmation.selection)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn selection() -> PreparedSelection {
        PreparedSelection {
            point_id: Uuid::new_v4(),
            current_version: ForkVersion::parse("3.20.4-fork.4").unwrap(),
            snapshot_digest: Digest::parse(&"a".repeat(64)).unwrap(),
            setup_digest: Digest::parse(&"b".repeat(64)).unwrap(),
        }
    }

    #[test]
    fn concurrent_final_confirmations_get_exactly_one_execution_grant() {
        let gate = Arc::new(TicketGate::new(Duration::from_secs(60)).unwrap());
        let selection = selection();
        let nonce = gate.issue(selection.clone()).unwrap();
        let results = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    let gate = gate.clone();
                    let selection = selection.clone();
                    scope.spawn(move || gate.consume(nonce, &selection).is_ok())
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert_eq!(results.into_iter().filter(|granted| *granted).count(), 1);
    }

    #[test]
    fn cancel_or_new_preparation_invalidates_the_prior_confirmation() {
        let gate = TicketGate::new(Duration::from_secs(60)).unwrap();
        let selection = selection();
        let cancelled = gate.issue(selection.clone()).unwrap();
        gate.cancel(cancelled).unwrap();
        assert!(gate.consume(cancelled, &selection).is_err());
        let obsolete = gate.issue(selection.clone()).unwrap();
        let current = gate.issue(selection.clone()).unwrap();
        assert!(gate.consume(obsolete, &selection).is_err());
        // A stale dialog must not cancel a newer confirmation.
        gate.cancel(obsolete).unwrap();
        assert!(gate.consume(current, &selection).is_ok());
    }

    #[test]
    fn changed_setup_snapshot_version_or_point_requires_both_confirmations_again() {
        let gate = TicketGate::new(Duration::from_secs(60)).unwrap();
        let original = selection();
        let mut changed_setup = original.clone();
        changed_setup.setup_digest = Digest::parse(&"c".repeat(64)).unwrap();
        let mut changed_snapshot = original.clone();
        changed_snapshot.snapshot_digest = Digest::parse(&"d".repeat(64)).unwrap();
        let mut changed_version = original.clone();
        changed_version.current_version = ForkVersion::parse("3.20.4-fork.5").unwrap();
        let mut changed_point = original.clone();
        changed_point.point_id = Uuid::new_v4();
        for changed in [
            changed_setup,
            changed_snapshot,
            changed_version,
            changed_point,
        ] {
            let nonce = gate.issue(original.clone()).unwrap();
            assert!(matches!(
                gate.consume(nonce, &changed),
                Err(ProtocolError::SelectionChanged)
            ));
            assert!(gate.consume(nonce, &original).is_err());
        }
    }

    #[test]
    fn expired_confirmation_is_rejected_without_sleep_or_wall_clock_dependency() {
        let gate = TicketGate::new(Duration::from_secs(30)).unwrap();
        let selection = selection();
        let now = Instant::now();
        let nonce = gate.issue_at(selection.clone(), now).unwrap();
        assert!(gate
            .consume_at(nonce, &selection, now + Duration::from_secs(30))
            .is_err());
        assert!(gate
            .consume_at(nonce, &selection, now + Duration::from_secs(1))
            .is_err());
    }
}
