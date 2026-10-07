use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::model::{ForkVersion, InstallSource, ProtocolError, FORMAT_VERSION};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Upgrade,
    Rollback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Preparing,
    Prepared,
    Quiescing,
    Captured,
    Installing,
    Restoring,
    Verifying,
    Committed,
    Cleanup,
    Failed,
    Recovering,
    Recovered,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Journal {
    pub(crate) format_version: u32,
    pub(crate) id: Uuid,
    pub(crate) point_id: Uuid,
    pub(crate) from: ForkVersion,
    pub(crate) to: ForkVersion,
    pub(crate) direction: Direction,
    pub(crate) source: InstallSource,
    pub(crate) phase: Phase,
}

impl Journal {
    pub(crate) fn new(
        point_id: Uuid,
        from: ForkVersion,
        to: ForkVersion,
        direction: Direction,
        source: InstallSource,
    ) -> Self {
        Self {
            format_version: FORMAT_VERSION,
            id: Uuid::new_v4(),
            point_id,
            from,
            to,
            direction,
            source,
            phase: Phase::Preparing,
        }
    }

    pub fn id(&self) -> Uuid {
        self.id
    }
    pub fn point_id(&self) -> Uuid {
        self.point_id
    }
    pub fn phase(&self) -> Phase {
        self.phase
    }
    pub fn direction(&self) -> Direction {
        self.direction
    }

    pub(crate) fn validate(&self) -> Result<(), ProtocolError> {
        if self.format_version != FORMAT_VERSION || self.id.is_nil() || self.point_id.is_nil() {
            return Err(ProtocolError::Invalid(
                "unsupported journal or missing identity".into(),
            ));
        }
        let valid_versions = match self.direction {
            Direction::Upgrade => self.to.is_newer_than(&self.from),
            Direction::Rollback => self.from.is_newer_than(&self.to),
        };
        if !valid_versions
            || (self.direction == Direction::Upgrade && self.phase == Phase::Restoring)
        {
            return Err(ProtocolError::Invalid(
                "journal direction does not match its versions or phase".into(),
            ));
        }
        Ok(())
    }

    pub(crate) fn advance(&mut self, next: Phase) -> Result<(), ProtocolError> {
        use Phase::*;
        let allowed = match (self.phase, next) {
            (Preparing, Prepared)
            | (Prepared, Quiescing)
            | (Quiescing, Captured)
            | (Captured, Installing)
            | (Verifying, Committed)
            | (Committed, Cleanup)
            | (Failed, Recovering)
            | (Recovering, Recovered) => true,
            (Installing, Verifying) => self.direction == Direction::Upgrade,
            (Installing, Restoring) | (Restoring, Verifying) => {
                self.direction == Direction::Rollback
            }
            (
                Preparing | Prepared | Quiescing | Captured | Installing | Restoring | Verifying,
                Failed,
            ) => true,
            _ => false,
        };
        if !allowed {
            return Err(ProtocolError::Phase);
        }
        self.phase = next;
        Ok(())
    }
}
