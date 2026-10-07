//! Point-scoped ownership receipts. Only the audited application's resource
//! writer calls these APIs; frontend-supplied paths must never reach them.
use std::collections::BTreeSet;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::windows_database::hash_reader;
use crate::windows_resources::normalized_path;
use crate::windows_store::{invalid, lock_regular_file};
use crate::{
    CaptureSlot, Catalog, Digest, Point, ProtocolError, ResourceKind, ResourceRole, ResourceState,
    StoreError, StoreLease,
};

const MAX_WRITE_PATHS: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum ManagedFileOutcome {
    Missing,
    File { bytes: u64, sha256: Digest },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PendingWrite {
    id: Uuid,
    before: ManagedFileOutcome,
    after: ManagedFileOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ManagedFileWrite {
    point_id: Uuid,
    snapshot_digest: Digest,
    normalized_path: String,
    role: ResourceRole,
    created_here: bool,
    completed: Option<ManagedFileOutcome>,
    pub(crate) pending: Option<PendingWrite>,
}

impl StoreLease {
    /// Persist intent BEFORE the audited writer creates/replaces/deletes the
    /// file. This first implementation covers paths present in the old inventory;
    /// a newly introduced tree member requires the tree ownership executor.
    pub fn begin_managed_file_write(
        &mut self,
        path: &Path,
        role: ResourceRole,
        after: ManagedFileOutcome,
    ) -> Result<Uuid, StoreError> {
        validate_outcome(&after)?;
        let mut catalog = self
            .load()?
            .ok_or_else(|| invalid("managed writer has no catalog"))?;
        require_writer_idle(&catalog)?;
        let point = catalog.previous().cloned().ok_or(ProtocolError::NoPoint)?;
        let manifest = self.verify_snapshot(&point)?;
        let inventory = self.read_resource_inventory(
            manifest.transaction_id,
            point.id,
            CaptureSlot::Previous,
            &manifest.resource_inventory_sha256,
        )?;
        let normalized = normalized_path(path)?;
        let origin = inventory
            .resources
            .iter()
            .find(|resource| {
                normalized_path(&resource.path).is_ok_and(|value| value == normalized)
                    && resource.role == role
            })
            .ok_or_else(|| {
                invalid("managed writer path or role is outside the sealed inventory")
            })?;
        if !matches!(
            origin.state,
            ResourceState::File { .. }
                | ResourceState::Missing {
                    kind: ResourceKind::File
                }
        ) {
            return Err(invalid("managed writer requires a regular-file origin"));
        }
        let before = live_outcome(path)?;
        let id = Uuid::new_v4();
        if let Some(write) = catalog
            .managed_file_writes
            .iter_mut()
            .find(|write| write.normalized_path == normalized)
        {
            if write.role != role || write.completed.as_ref() != Some(&before) {
                return Err(invalid("managed file changed outside its recorded writer"));
            }
            write.pending = Some(PendingWrite { id, before, after });
        } else {
            if catalog.managed_file_writes.len() >= MAX_WRITE_PATHS {
                return Err(invalid("managed write path limit exceeded"));
            }
            catalog.managed_file_writes.push(ManagedFileWrite {
                point_id: point.id,
                snapshot_digest: point.snapshot_digest.clone(),
                normalized_path: normalized,
                role,
                created_here: before == ManagedFileOutcome::Missing
                    && matches!(origin.state, ResourceState::Missing { .. }),
                completed: None,
                pending: Some(PendingWrite { id, before, after }),
            });
        }
        // Receipts and the previous pointer are one atomic active.json write.
        self.save(&catalog)?;
        Ok(id)
    }

    /// Never trusts a writer's success flag: independently read and hash its
    /// result while refusing links and already-open writers.
    pub fn complete_managed_file_write(&mut self, id: Uuid) -> Result<(), StoreError> {
        self.finish_managed_file_write(id, false)
    }

    /// Cancel only if the file still matches the pre-write bytes/missing state.
    pub fn cancel_managed_file_write(&mut self, id: Uuid) -> Result<(), StoreError> {
        self.finish_managed_file_write(id, true)
    }

    /// After a writer process dies, classify its exact bytes against the intent.
    /// Unknown/partial/external content stays blocked for explicit repair.
    pub fn reconcile_pending_managed_file_write(&mut self) -> Result<(), StoreError> {
        let catalog = self
            .load()?
            .ok_or_else(|| invalid("managed writer has no catalog"))?;
        if catalog.journal().is_some() || catalog.cleanup_pending().is_some() {
            return Err(ProtocolError::Busy.into());
        }
        let Some(write) = catalog
            .managed_file_writes
            .iter()
            .find(|write| write.pending.is_some())
        else {
            return Ok(());
        };
        let pending = write.pending.as_ref().unwrap();
        let live = live_outcome(Path::new(&write.normalized_path))?;
        if live == pending.after {
            self.complete_managed_file_write(pending.id)
        } else if live == pending.before {
            self.cancel_managed_file_write(pending.id)
        } else {
            Err(invalid(
                "pending managed write has unknown or partially written content",
            ))
        }
    }

    fn finish_managed_file_write(&mut self, id: Uuid, cancel: bool) -> Result<(), StoreError> {
        let mut catalog = self
            .load()?
            .ok_or_else(|| invalid("managed writer has no catalog"))?;
        if catalog.journal().is_some() || catalog.cleanup_pending().is_some() {
            return Err(ProtocolError::Busy.into());
        }
        let index = catalog
            .managed_file_writes
            .iter()
            .position(|write| {
                write
                    .pending
                    .as_ref()
                    .is_some_and(|pending| pending.id == id)
            })
            .ok_or_else(|| invalid("managed write token is missing or already consumed"))?;
        let write = &catalog.managed_file_writes[index];
        let pending = write.pending.as_ref().unwrap();
        let expected = if cancel {
            &pending.before
        } else {
            &pending.after
        };
        if live_outcome(Path::new(&write.normalized_path))? != *expected {
            return Err(invalid(
                "managed write result differs from its durable intent",
            ));
        }
        let outcome = expected.clone();
        let write = &mut catalog.managed_file_writes[index];
        if cancel && write.completed.is_none() {
            catalog.managed_file_writes.remove(index);
        } else {
            if !cancel {
                write.completed = Some(outcome);
            }
            write.pending = None;
        }
        self.save(&catalog)
    }
}

fn require_writer_idle(catalog: &Catalog) -> Result<(), StoreError> {
    if catalog.journal().is_some()
        || catalog.cleanup_pending().is_some()
        || catalog
            .managed_file_writes
            .iter()
            .any(|write| write.pending.is_some())
    {
        return Err(ProtocolError::Busy.into());
    }
    Ok(())
}

fn live_outcome(path: &Path) -> Result<ManagedFileOutcome, StoreError> {
    let mut file = match lock_regular_file(path) {
        Ok(file) => file,
        Err(StoreError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(ManagedFileOutcome::Missing)
        }
        Err(error) => return Err(error),
    };
    let bytes = file.metadata()?.len();
    let result = ManagedFileOutcome::File {
        bytes,
        sha256: hash_reader(&mut file)?,
    };
    validate_outcome(&result)?;
    Ok(result)
}

fn validate_outcome(outcome: &ManagedFileOutcome) -> Result<(), ProtocolError> {
    if matches!(outcome, ManagedFileOutcome::File { bytes, .. } if *bytes > 512 * 1024 * 1024) {
        return Err(ProtocolError::Invalid(
            "managed write file is too large".into(),
        ));
    }
    Ok(())
}

pub(crate) fn validate_writes(
    writes: &[ManagedFileWrite],
    point: Option<&Point>,
) -> Result<(), ProtocolError> {
    if writes.len() > MAX_WRITE_PATHS || (!writes.is_empty() && point.is_none()) {
        return Err(ProtocolError::Invalid(
            "managed writes have no single previous point or exceed the limit".into(),
        ));
    }
    let mut paths = BTreeSet::new();
    let mut pending_ids = BTreeSet::new();
    for write in writes {
        let point = point.unwrap();
        if write.point_id != point.id
            || write.snapshot_digest != point.snapshot_digest
            || !paths.insert(&write.normalized_path)
            || normalized_path(Path::new(&write.normalized_path))
                .map_err(|_| ProtocolError::Invalid("invalid managed write path".into()))?
                != write.normalized_path
            || (write.completed.is_none() && write.pending.is_none())
        {
            return Err(ProtocolError::Invalid(
                "managed write identity, path or progress changed".into(),
            ));
        }
        if let Some(completed) = &write.completed {
            validate_outcome(completed)?;
        }
        if let Some(pending) = &write.pending {
            if pending.id.is_nil() || !pending_ids.insert(pending.id) {
                return Err(ProtocolError::Invalid(
                    "managed write token is invalid or duplicated".into(),
                ));
            }
            validate_outcome(&pending.before)?;
            validate_outcome(&pending.after)?;
            if write
                .completed
                .as_ref()
                .is_some_and(|completed| completed != &pending.before)
            {
                return Err(ProtocolError::Invalid("managed write chain changed".into()));
            }
        }
    }
    Ok(())
}

pub(crate) fn owns_addition(
    catalog: &Catalog,
    point: &Point,
    path: &Path,
    role: ResourceRole,
    rescue: &ResourceState,
) -> Result<bool, StoreError> {
    let normalized = normalized_path(path)?;
    let Some(write) = catalog
        .managed_file_writes
        .iter()
        .find(|write| write.normalized_path == normalized && write.role == role)
    else {
        return Ok(false);
    };
    let ResourceState::File { bytes, sha256, .. } = rescue else {
        return Ok(false);
    };
    Ok(write.point_id == point.id
        && write.snapshot_digest == point.snapshot_digest
        && write.created_here
        && write.pending.is_none()
        && write.completed
            == Some(ManagedFileOutcome::File {
                bytes: *bytes,
                sha256: sha256.clone(),
            }))
}
