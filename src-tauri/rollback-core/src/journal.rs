use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::model::{Digest, ForkVersion, InstallSource, Point, ProtocolError, FORMAT_VERSION};

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
    #[serde(default)]
    pub(crate) rescue_database_sha256: Option<Digest>,
    #[serde(default)]
    pub(crate) rescue_resources_sha256: Option<Digest>,
    #[serde(default)]
    pub(crate) captured_candidate: Option<Point>,
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
            rescue_database_sha256: None,
            rescue_resources_sha256: None,
            captured_candidate: None,
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
    pub fn rescue_database_sha256(&self) -> Option<&Digest> {
        self.rescue_database_sha256.as_ref()
    }
    pub fn rescue_resources_sha256(&self) -> Option<&Digest> {
        self.rescue_resources_sha256.as_ref()
    }
    pub fn captured_candidate(&self) -> Option<&Point> {
        self.captured_candidate.as_ref()
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
        if let Some(candidate) = &self.captured_candidate {
            candidate.validate()?;
            if self.direction != Direction::Upgrade
                || candidate.id != self.point_id
                || candidate.transaction_id != self.id
                || candidate.source_version != self.from
                || candidate.installed_version != self.to
                || candidate.source != self.source
                || matches!(self.phase, Phase::Preparing | Phase::Prepared)
            {
                return Err(ProtocolError::Invalid(
                    "captured candidate does not match its journal".into(),
                ));
            }
        }
        if (self.rescue_database_sha256.is_some() || self.rescue_resources_sha256.is_some())
            && matches!(self.phase, Phase::Preparing | Phase::Prepared)
        {
            return Err(ProtocolError::Invalid(
                "emergency database was bound before quiescing".into(),
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
