use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::model::{Digest, ForkVersion, InstallSource, Point, ProtocolError, FORMAT_VERSION};
use crate::{Direction, Journal, Phase};

/// Serializable catalog, changed only by the installation coordinator while it
/// holds the cross-process lease. Installer/file verification belongs to the
/// helper; it must persist each returned state before the next external action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Catalog {
    format_version: u32,
    current_version: ForkVersion,
    previous: Option<Point>,
    transaction: Option<Journal>,
    cleanup_pending: Option<Uuid>,
    #[cfg(windows)]
    #[serde(default)]
    pub(crate) managed_file_writes: Vec<crate::windows_ownership::ManagedFileWrite>,
}

impl Catalog {
    pub fn new(current_version: ForkVersion) -> Self {
        Self {
            format_version: FORMAT_VERSION,
            current_version,
            previous: None,
            transaction: None,
            cleanup_pending: None,
            #[cfg(windows)]
            managed_file_writes: Vec::new(),
        }
    }

    pub fn current_version(&self) -> &ForkVersion {
        &self.current_version
    }
    pub fn previous(&self) -> Option<&Point> {
        self.previous.as_ref()
    }
    pub fn journal(&self) -> Option<&Journal> {
        self.transaction.as_ref()
    }
    pub fn cleanup_pending(&self) -> Option<Uuid> {
        self.cleanup_pending
    }

    pub fn validate(&self) -> Result<(), ProtocolError> {
        #[cfg(windows)]
        crate::windows_ownership::validate_writes(
            &self.managed_file_writes,
            self.previous.as_ref(),
        )?;
        if self.format_version != FORMAT_VERSION {
            return Err(ProtocolError::Invalid("unsupported catalog format".into()));
        }
        if self.cleanup_pending.is_some_and(|id| id.is_nil()) {
            return Err(ProtocolError::Invalid("missing cleanup identity".into()));
        }
        if self.cleanup_pending.is_some()
            && !self
                .transaction
                .as_ref()
                .is_some_and(|journal| matches!(journal.phase, Phase::Committed | Phase::Cleanup))
        {
            return Err(ProtocolError::Invalid(
                "cleanup has no committed transaction".into(),
            ));
        }
        if let Some(point) = &self.previous {
            point.validate()?;
            if point.installed_version != self.current_version
                || Some(point.id) == self.cleanup_pending
            {
                return Err(ProtocolError::Invalid(
                    "active point does not match installation or cleanup".into(),
                ));
            }
        }
        if let Some(journal) = &self.transaction {
            journal.validate()?;
            let committed = matches!(journal.phase, Phase::Committed | Phase::Cleanup);
            let expected_current = if committed {
                &journal.to
            } else {
                &journal.from
            };
            if &self.current_version != expected_current {
                return Err(ProtocolError::Invalid(
                    "catalog and journal disagree on installed version".into(),
                ));
            }
            if !committed && journal.direction == Direction::Rollback {
                let point = self.previous.as_ref().ok_or(ProtocolError::NoPoint)?;
                if point.id != journal.point_id || point.source_version != journal.to {
                    return Err(ProtocolError::Invalid(
                        "rollback journal target changed".into(),
                    ));
                }
            }
            if committed {
                match journal.direction {
                    Direction::Upgrade => {
                        let point = self.previous.as_ref().ok_or(ProtocolError::NoPoint)?;
                        if point.id != journal.point_id
                            || point.transaction_id != journal.id
                            || point.source_version != journal.from
                            || point.source != journal.source
                        {
                            return Err(ProtocolError::Invalid(
                                "committed upgrade lost its matching point".into(),
                            ));
                        }
                    }
                    Direction::Rollback => {
                        if self.previous.is_some() || self.cleanup_pending != Some(journal.point_id)
                        {
                            return Err(ProtocolError::Invalid(
                                "committed rollback must consume its only point".into(),
                            ));
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn require_idle(&self) -> Result<(), ProtocolError> {
        self.validate()?;
        #[cfg(windows)]
        if self
            .managed_file_writes
            .iter()
            .any(|write| write.pending.is_some())
        {
            return Err(ProtocolError::Busy);
        }
        if self.transaction.is_some() || self.cleanup_pending.is_some() {
            return Err(ProtocolError::Busy);
        }
        Ok(())
    }

    /// A same-version reinstall is a no-op and does not replace its old point.
    pub fn begin_upgrade(
        &mut self,
        next: ForkVersion,
        source: InstallSource,
    ) -> Result<bool, ProtocolError> {
        self.require_idle()?;
        if next == self.current_version {
            return Ok(false);
        }
        if !next.is_newer_than(&self.current_version) {
            return Err(ProtocolError::Invalid(
                "manual downgrade is not an upgrade transaction".into(),
            ));
        }
        self.transaction = Some(Journal::new(
            Uuid::new_v4(),
            self.current_version.clone(),
            next,
            Direction::Upgrade,
            source,
        ));
        Ok(true)
    }

    pub fn begin_rollback(&mut self) -> Result<(), ProtocolError> {
        self.require_idle()?;
        let point = self.previous.as_ref().ok_or(ProtocolError::NoPoint)?;
        self.transaction = Some(Journal::new(
            point.id,
            self.current_version.clone(),
            point.source_version.clone(),
            Direction::Rollback,
            point.source,
        ));
        Ok(())
    }

    /// Commit phases must go through the version-specific catalog operations.
    pub fn advance(&mut self, next: Phase) -> Result<(), ProtocolError> {
        self.validate()?;
        if matches!(next, Phase::Committed | Phase::Cleanup | Phase::Recovered) {
            return Err(ProtocolError::Phase);
        }
        self.transaction
            .as_mut()
            .ok_or(ProtocolError::Phase)?
            .advance(next)
    }

    pub fn bind_rescue_database(&mut self, digest: Digest) -> Result<(), ProtocolError> {
        self.validate()?;
        let journal = self.transaction.as_mut().ok_or(ProtocolError::Phase)?;
        if journal.phase != Phase::Quiescing || journal.rescue_database_sha256.is_some() {
            return Err(ProtocolError::Phase);
        }
        journal.rescue_database_sha256 = Some(digest);
        Ok(())
    }

    pub fn bind_rescue_resources(&mut self, digest: Digest) -> Result<(), ProtocolError> {
        self.validate()?;
        let journal = self.transaction.as_mut().ok_or(ProtocolError::Phase)?;
        if journal.phase != Phase::Quiescing || journal.rescue_resources_sha256.is_some() {
            return Err(ProtocolError::Phase);
        }
        journal.rescue_resources_sha256 = Some(digest);
        Ok(())
    }

    /// Make the sealed candidate recoverable after helper/installer death.
    /// It stays a transaction candidate, never a second selectable point.
    pub fn bind_captured_candidate(&mut self, point: Point) -> Result<(), ProtocolError> {
        self.validate()?;
        point.validate()?;
        let journal = self.transaction.as_mut().ok_or(ProtocolError::Phase)?;
        if journal.direction != Direction::Upgrade
            || journal.phase != Phase::Quiescing
            || journal.captured_candidate.is_some()
            || point.id != journal.point_id
            || point.transaction_id != journal.id
            || point.source_version != journal.from
            || point.installed_version != journal.to
            || point.source != journal.source
        {
            return Err(ProtocolError::Invalid(
                "candidate binding changed or was made outside capture".into(),
            ));
        }
        journal.captured_candidate = Some(point);
        Ok(())
    }

    pub fn commit_upgrade(&mut self, point: Point) -> Result<(), ProtocolError> {
        self.validate()?;
        point.validate()?;
        let journal = self.transaction.as_ref().ok_or(ProtocolError::Phase)?;
        if journal.direction != Direction::Upgrade || journal.phase != Phase::Verifying {
            return Err(ProtocolError::Phase);
        }
        if point.id != journal.point_id
            || point.transaction_id != journal.id
            || point.source_version != journal.from
            || point.installed_version != journal.to
            || point.source != journal.source
            || journal
                .captured_candidate
                .as_ref()
                .is_some_and(|captured| captured != &point)
        {
            return Err(ProtocolError::Invalid(
                "snapshot is not bound to this upgrade".into(),
            ));
        }
        self.transaction
            .as_mut()
            .unwrap()
            .advance(Phase::Committed)?;
        self.cleanup_pending = self.previous.as_ref().map(|previous| previous.id);
        self.current_version = point.installed_version.clone();
        self.previous = Some(point);
        #[cfg(windows)]
        self.managed_file_writes.clear();
        Ok(())
    }

    pub fn commit_rollback(&mut self) -> Result<(), ProtocolError> {
        self.validate()?;
        let journal = self.transaction.as_ref().ok_or(ProtocolError::Phase)?;
        if journal.direction != Direction::Rollback || journal.phase != Phase::Verifying {
            return Err(ProtocolError::Phase);
        }
        let restored_version = journal.to.clone();
        self.transaction
            .as_mut()
            .unwrap()
            .advance(Phase::Committed)?;
        self.cleanup_pending = self.previous.take().map(|previous| previous.id);
        #[cfg(windows)]
        self.managed_file_writes.clear();
        self.current_version = restored_version;
        Ok(())
    }

    /// Only after the helper has verified restoration of the pre-transaction
    /// executable AND data can the old catalog become usable again.
    pub fn confirm_recovered(
        &mut self,
        restored_version: &ForkVersion,
    ) -> Result<(), ProtocolError> {
        self.validate()?;
        let journal = self.transaction.as_mut().ok_or(ProtocolError::Phase)?;
        if journal.phase != Phase::Recovering || &journal.from != restored_version {
            return Err(ProtocolError::Phase);
        }
        journal.advance(Phase::Recovered)?;
        Ok(())
    }

    /// The restored installation may run, but another transaction stays blocked
    /// until its failed candidate and rescue files have actually been removed.
    pub fn finish_recovery_cleanup(&mut self, transaction_id: Uuid) -> Result<(), ProtocolError> {
        self.validate()?;
        let journal = self.transaction.as_ref().ok_or(ProtocolError::Phase)?;
        if journal.phase != Phase::Recovered || journal.id != transaction_id {
            return Err(ProtocolError::Phase);
        }
        self.transaction = None;
        Ok(())
    }

    /// Cleanup includes transaction rescue/staging files even when no older
    /// point existed. Until it finishes another update or rollback stays busy.
    pub fn finish_cleanup(&mut self, removed_point: Option<Uuid>) -> Result<(), ProtocolError> {
        self.validate()?;
        if removed_point != self.cleanup_pending {
            return Err(ProtocolError::Invalid("cleanup identity changed".into()));
        }
        let journal = self.transaction.as_mut().ok_or(ProtocolError::Phase)?;
        if journal.phase == Phase::Committed {
            journal.advance(Phase::Cleanup)?;
        }
        if journal.phase != Phase::Cleanup {
            return Err(ProtocolError::Phase);
        }
        self.cleanup_pending = None;
        self.transaction = None;
        Ok(())
    }
}
