//! Exact regular-file executor with point-scoped write-ownership checks.
//! Tree/link and inventory-union restoration require their own executors.
//! Reject an unsupported plan before writing any live resource.
use std::fs::{self, OpenOptions};
use std::io::{self, Read};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use uuid::Uuid;
use windows_sys::Win32::Storage::FileSystem::{
    MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
};

use crate::windows_database::{hash_reader, require_free_space};
use crate::windows_resources::normalized_path;
use crate::windows_store::{invalid, lock_regular_file, validate_ntfs_path, wide_path};
use crate::{
    CaptureSlot, Catalog, Digest, Direction, FileDacl, Journal, Phase, Point, ResourceInventory,
    ResourceKind, ResourceState, StoreError, StoreLease,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Mode {
    Previous,
    Rescue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum FilePhase {
    Staging,
    Staged,
    Replacing,
    Replaced,
    Verifying,
    Complete,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileRestoreLedger {
    format_version: u32,
    transaction_id: Uuid,
    point_id: Uuid,
    previous_digest: Digest,
    rescue_digest: Digest,
    ownership_digest: Digest,
    mode: Mode,
    index: usize,
    phase: FilePhase,
}

impl StoreLease {
    /// Requires the independent helper's launch fence and all writers stopped.
    /// This deliberately does not advance the global journal or consume a point.
    /// An inventory containing trees/links needs the corresponding executor.
    pub fn restore_previous_resource_files(&mut self) -> Result<(), StoreError> {
        self.restore_resource_files(Mode::Previous, |_, _| Ok(()))
    }

    pub fn restore_rescue_resource_files(&mut self) -> Result<(), StoreError> {
        self.restore_resource_files(Mode::Rescue, |_, _| Ok(()))
    }

    fn restore_resource_files(
        &mut self,
        mode: Mode,
        mut after_checkpoint: impl FnMut(usize, FilePhase) -> Result<(), StoreError>,
    ) -> Result<(), StoreError> {
        let catalog = self
            .load()?
            .ok_or_else(|| invalid("resource restore has no catalog"))?;
        let journal = catalog
            .journal()
            .cloned()
            .ok_or_else(|| invalid("resource restore has no journal"))?;
        match mode {
            Mode::Previous
                if journal.direction() == Direction::Rollback
                    && journal.phase() == Phase::Restoring => {}
            Mode::Rescue
                if journal.direction() == Direction::Rollback
                    && journal.phase() == Phase::Recovering => {}
            _ => return Err(crate::ProtocolError::Phase.into()),
        }
        let point = catalog.previous().ok_or(crate::ProtocolError::NoPoint)?;
        let manifest = self.verify_snapshot(point)?;
        let previous = self.read_resource_inventory(
            manifest.transaction_id,
            point.id,
            CaptureSlot::Previous,
            &manifest.resource_inventory_sha256,
        )?;
        let rescue_digest = journal
            .rescue_resources_sha256()
            .ok_or_else(|| invalid("no bound emergency resources exist"))?;
        let rescue = self.read_resource_inventory(
            journal.id(),
            point.id,
            CaptureSlot::Rescue,
            rescue_digest,
        )?;
        validate_file_plan(&previous, &rescue, &catalog, point)?;
        let desired = if mode == Mode::Previous {
            &previous
        } else {
            &rescue
        };
        let directory = self
            .root
            .join("transactions")
            .join(journal.id().to_string());
        validate_ntfs_path(&directory)?;
        fs::create_dir_all(&directory)?;
        validate_ntfs_path(&directory)?;
        let ledger_path = file_ledger_path(&directory, mode);
        let expected = FileRestoreLedger {
            format_version: 1,
            transaction_id: journal.id(),
            point_id: journal.point_id(),
            previous_digest: manifest.resource_inventory_sha256.clone(),
            rescue_digest: rescue_digest.clone(),
            ownership_digest: Digest::parse(&format!(
                "{:x}",
                Sha256::digest(serde_json::to_vec(&catalog.managed_file_writes)?)
            ))?,
            mode,
            index: 0,
            phase: FilePhase::Staging,
        };
        let existing = read_ledger(&ledger_path)?;
        let mut ledger = existing.clone().unwrap_or_else(|| expected.clone());
        validate_ledger(&ledger, &expected, desired.resources.len())?;
        let forward = read_ledger(&file_ledger_path(&directory, Mode::Previous))?;
        if let Some(forward) = &forward {
            let mut forward_expected = expected.clone();
            forward_expected.mode = Mode::Previous;
            validate_ledger(forward, &forward_expected, desired.resources.len())?;
        }
        // Preflight every resource, including completed and untouched entries.
        // A late mismatch must not permit earlier items to be overwritten first.
        for index in 0..desired.resources.len() {
            let resource = &desired.resources[index];
            if existing.is_none() {
                match fs::symlink_metadata(stage_path(&resource.path, &journal, mode, index)) {
                    Ok(_) => {
                        return Err(invalid(
                            "resource stage already exists without a durable owner",
                        ))
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
            validate_ntfs_path(
                resource
                    .path
                    .parent()
                    .ok_or_else(|| invalid("resource has no parent"))?,
            )?;
            if !resource.path.parent().unwrap().is_dir() {
                return Err(invalid(
                    "missing resource parent requires a managed-directory executor",
                ));
            }
            if index < ledger.index || ledger.phase == FilePhase::Complete {
                verify_state(&resource.path, &resource.state, false)?;
                continue;
            }
            let own_install = existing.is_some()
                && index == ledger.index
                && matches!(
                    ledger.phase,
                    FilePhase::Replacing | FilePhase::Replaced | FilePhase::Verifying
                );
            if own_install && state_matches(&resource.path, &resource.state, true)? {
                continue;
            }
            if mode == Mode::Previous {
                verify_state(&resource.path, &rescue.resources[index].state, own_install)?;
            } else {
                let forward_install = forward.as_ref().is_some_and(|forward| {
                    index < forward.index
                        || (index == forward.index
                            && matches!(
                                forward.phase,
                                FilePhase::Replacing
                                    | FilePhase::Replaced
                                    | FilePhase::Verifying
                                    | FilePhase::Complete
                            ))
                });
                if !state_matches(
                    &resource.path,
                    &rescue.resources[index].state,
                    forward_install,
                )? && !(forward_install
                    && state_matches(&resource.path, &previous.resources[index].state, true)?)
                {
                    return Err(invalid(
                        "resource changed outside this restoration transaction",
                    ));
                }
            }
        }
        let checkpoint =
            |this: &StoreLease,
             ledger: &mut FileRestoreLedger,
             callback: &mut dyn FnMut(usize, FilePhase) -> Result<(), StoreError>| {
                this.write_private_file(&ledger_path, &serde_json::to_vec_pretty(ledger)?)?;
                callback(ledger.index, ledger.phase)
            };
        while ledger.index < desired.resources.len() {
            let resource = &desired.resources[ledger.index];
            let target = &resource.path;
            let stage = stage_path(target, &journal, mode, ledger.index);
            if ledger.phase == FilePhase::Staging {
                checkpoint(self, &mut ledger, &mut after_checkpoint)?;
                if let ResourceState::File {
                    material_id,
                    bytes,
                    dacl,
                    ..
                } = &resource.state
                {
                    require_free_space(
                        target.parent().unwrap(),
                        bytes
                            .checked_add(64 * 1024 * 1024)
                            .ok_or_else(|| invalid("resource space estimate overflow"))?,
                    )?;
                    let material = self
                        .resource_directory(desired.transaction_id, desired.point_id, desired.slot)
                        .join(format!("{material_id}.bin"));
                    let mut input = lock_regular_file(&material)?;
                    match OpenOptions::new()
                        .create_new(true)
                        .read(true)
                        .write(true)
                        .access_mode(0xc004_0000)
                        .share_mode(0)
                        .open(&stage)
                    {
                        Ok(mut output) => {
                            // Apply the original DACL BEFORE writing sensitive bytes.
                            dacl.apply(&output)?;
                            io::copy(&mut input, &mut output)?;
                            output.sync_all()?;
                        }
                        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                        Err(error) => return Err(error.into()),
                    }
                    verify_state(&stage, &resource.state, true)?;
                }
                ledger.phase = FilePhase::Staged;
                checkpoint(self, &mut ledger, &mut after_checkpoint)?;
            }
            if ledger.phase == FilePhase::Staged {
                if mode == Mode::Previous {
                    verify_state(target, &rescue.resources[ledger.index].state, false)?;
                }
                ledger.phase = FilePhase::Replacing;
                checkpoint(self, &mut ledger, &mut after_checkpoint)?;
            }
            if ledger.phase == FilePhase::Replacing {
                if !state_matches(target, &resource.state, true)? {
                    // Recheck at the last boundary before mutation, including
                    // recovery after readonly was cleared before a process died.
                    if mode == Mode::Previous {
                        verify_state(target, &rescue.resources[ledger.index].state, true)?;
                    } else {
                        let allowed = forward.as_ref().is_some_and(|forward| {
                            ledger.index < forward.index
                                || (ledger.index == forward.index
                                    && matches!(
                                        forward.phase,
                                        FilePhase::Replacing
                                            | FilePhase::Replaced
                                            | FilePhase::Verifying
                                            | FilePhase::Complete
                                    ))
                        });
                        if !state_matches(target, &rescue.resources[ledger.index].state, true)?
                            && !(allowed
                                && state_matches(
                                    target,
                                    &previous.resources[ledger.index].state,
                                    true,
                                )?)
                        {
                            return Err(invalid("resource changed before emergency compensation"));
                        }
                    }
                    match &resource.state {
                        ResourceState::File { .. } => {
                            verify_state(&stage, &resource.state, true)?;
                            clear_readonly(target)?;
                            let source = wide_path(&stage);
                            let destination = wide_path(target);
                            // SAFETY: validated same-parent local paths; both
                            // immutable selections are bound to this journal.
                            if unsafe {
                                MoveFileExW(
                                    source.as_ptr(),
                                    destination.as_ptr(),
                                    MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
                                )
                            } == 0
                            {
                                return Err(io::Error::last_os_error().into());
                            }
                        }
                        ResourceState::Missing { .. } => {
                            // Forward deletion was authorized by this point's
                            // completed application write receipt during preflight.
                            // Compensation can undo only this executor's own file.
                            let owned_state = if mode == Mode::Previous {
                                &rescue.resources[ledger.index].state
                            } else {
                                &previous.resources[ledger.index].state
                            };
                            if !matches!(owned_state, ResourceState::File { .. }) {
                                return Err(invalid("missing-state deletion lacks file ownership"));
                            }
                            verify_state(target, owned_state, true)?;
                            clear_readonly(target)?;
                            fs::remove_file(target)?;
                        }
                        _ => return Err(invalid("unsupported resource executor")),
                    }
                }
                ledger.phase = FilePhase::Replaced;
                checkpoint(self, &mut ledger, &mut after_checkpoint)?;
            }
            if ledger.phase == FilePhase::Replaced {
                if let ResourceState::File { dacl, readonly, .. } = &resource.state {
                    verify_state(target, &resource.state, true)?;
                    let file = OpenOptions::new()
                        .read(true)
                        .access_mode(0x8004_0000)
                        .share_mode(1)
                        .open(target)?;
                    dacl.apply(&file)?;
                    let mut permissions = file.metadata()?.permissions();
                    drop(file);
                    permissions.set_readonly(*readonly);
                    fs::set_permissions(target, permissions)?;
                }
                ledger.phase = FilePhase::Verifying;
                checkpoint(self, &mut ledger, &mut after_checkpoint)?;
            }
            verify_state(target, &resource.state, false)?;
            match fs::symlink_metadata(&stage) {
                Ok(_) => {
                    verify_state(&stage, &resource.state, true)?;
                    clear_readonly(&stage)?;
                    fs::remove_file(&stage)?;
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            ledger.index += 1;
            ledger.phase = if ledger.index == desired.resources.len() {
                FilePhase::Complete
            } else {
                FilePhase::Staging
            };
            checkpoint(self, &mut ledger, &mut after_checkpoint)?;
        }
        // Recheck the entire desired set; never mark the global restore complete.
        for resource in &desired.resources {
            verify_state(&resource.path, &resource.state, false)?;
        }
        Ok(())
    }
}

fn validate_file_plan(
    previous: &ResourceInventory,
    rescue: &ResourceInventory,
    catalog: &Catalog,
    point: &Point,
) -> Result<(), StoreError> {
    if previous.resources.len() != rescue.resources.len() {
        return Err(invalid(
            "resource union requires write-ownership reconciliation",
        ));
    }
    for (previous, rescue) in previous.resources.iter().zip(&rescue.resources) {
        if normalized_path(&previous.path)? != normalized_path(&rescue.path)?
            || previous.role != rescue.role
        {
            return Err(invalid(
                "emergency resource selection differs from the sealed point",
            ));
        }
        for resource in [previous, rescue] {
            if !matches!(
                resource.state,
                ResourceState::File { .. }
                    | ResourceState::Missing {
                        kind: ResourceKind::File
                    }
            ) {
                return Err(invalid(
                    "resource inventory requires a tree or link executor",
                ));
            }
        }
        if matches!(previous.state, ResourceState::Missing { .. })
            && matches!(rescue.state, ResourceState::File { .. })
            && !crate::windows_ownership::owns_addition(
                catalog,
                point,
                &previous.path,
                previous.role,
                &rescue.state,
            )?
        {
            return Err(invalid(
                "deleting a post-upgrade addition requires write-ownership evidence",
            ));
        }
    }
    Ok(())
}

fn file_ledger_path(directory: &Path, mode: Mode) -> PathBuf {
    directory.join(match mode {
        Mode::Previous => "resource-files-previous.json",
        Mode::Rescue => "resource-files-rescue.json",
    })
}

fn read_ledger(path: &Path) -> Result<Option<FileRestoreLedger>, StoreError> {
    let mut file = match lock_regular_file(path) {
        Ok(file) => file,
        Err(StoreError::Io(error)) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut bytes = Vec::new();
    file.by_ref()
        .take(1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 1024 * 1024 {
        return Err(invalid("resource restore ledger exceeds its limit"));
    }
    Ok(Some(serde_json::from_slice(&bytes)?))
}

fn validate_ledger(
    ledger: &FileRestoreLedger,
    expected: &FileRestoreLedger,
    count: usize,
) -> Result<(), StoreError> {
    if ledger.format_version != 1
        || ledger.transaction_id != expected.transaction_id
        || ledger.point_id != expected.point_id
        || ledger.previous_digest != expected.previous_digest
        || ledger.rescue_digest != expected.rescue_digest
        || ledger.ownership_digest != expected.ownership_digest
        || ledger.mode != expected.mode
        || ledger.index > count
        || (ledger.phase == FilePhase::Complete) != (ledger.index == count)
    {
        return Err(invalid(
            "resource restore ledger identity or progress changed",
        ));
    }
    Ok(())
}

fn stage_path(target: &Path, journal: &Journal, mode: Mode, index: usize) -> PathBuf {
    target.with_file_name(format!(
        "cc-switch-rollback-{}-{mode:?}-{index}.resource.tmp",
        journal.id()
    ))
}

fn state_matches(path: &Path, state: &ResourceState, relaxed: bool) -> Result<bool, StoreError> {
    match state {
        ResourceState::Missing { .. } => match fs::symlink_metadata(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
            Err(error) => Err(error.into()),
            Ok(_) => {
                let _guard = lock_regular_file(path)?;
                Ok(false)
            }
        },
        ResourceState::File {
            bytes,
            sha256,
            dacl,
            readonly,
            ..
        } => {
            let mut file = match lock_regular_file(path) {
                Ok(file) => file,
                Err(StoreError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
                    return Ok(false)
                }
                Err(error) => return Err(error),
            };
            Ok(file.metadata()?.len() == *bytes
                && hash_reader(&mut file)? == *sha256
                && FileDacl::capture(&file)? == *dacl
                && (relaxed || file.metadata()?.permissions().readonly() == *readonly))
        }
        _ => Err(invalid("unsupported resource state in file executor")),
    }
}

fn verify_state(path: &Path, state: &ResourceState, relaxed: bool) -> Result<(), StoreError> {
    if !state_matches(path, state, relaxed)? {
        return Err(invalid(
            "live or staged resource differs from its bound state",
        ));
    }
    Ok(())
}

fn clear_readonly(path: &Path) -> Result<(), StoreError> {
    let file = match lock_regular_file(path) {
        Ok(file) => file,
        Err(StoreError::Io(error)) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let mut permissions = file.metadata()?.permissions();
    drop(file);
    if permissions.readonly() {
        permissions.set_readonly(false);
        fs::set_permissions(path, permissions)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Catalog, ForkVersion, InstallSource, PrivateRoot, ResourceRequest, ResourceRole};

    fn ready(temp: &Path, create_unowned_addition: bool) -> (StoreLease, Vec<PathBuf>) {
        ready_with_writer(temp, create_unowned_addition, false)
    }

    fn ready_with_writer(
        temp: &Path,
        create_addition: bool,
        owned_addition: bool,
    ) -> (StoreLease, Vec<PathBuf>) {
        let install = temp.join("安装 路径");
        fs::create_dir(&install).unwrap();
        fs::write(install.join("cc-switch.exe"), b"MZ fixture").unwrap();
        let database_path = temp.join("business.db");
        let database = rusqlite::Connection::open(&database_path).unwrap();
        database
            .execute_batch("PRAGMA user_version=19; CREATE TABLE sentinel(value TEXT);")
            .unwrap();
        drop(database);
        let paths = vec![
            temp.join("配置.json"),
            temp.join("deleted.toml"),
            temp.join("absent.json"),
        ];
        fs::write(&paths[0], b"old settings\r\n").unwrap();
        let mut permissions = fs::metadata(&paths[0]).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&paths[0], permissions).unwrap();
        fs::write(&paths[1], b"old provider").unwrap();
        let requests: Vec<_> = paths
            .iter()
            .map(|path| ResourceRequest {
                path: path.clone(),
                role: ResourceRole::Provider,
                kind: ResourceKind::File,
            })
            .collect();
        let mut lease = PrivateRoot::create_at(temp.join("private"))
            .unwrap()
            .try_lease()
            .unwrap();
        let mut catalog = Catalog::new(ForkVersion::parse("3.20.4-fork.3").unwrap());
        catalog
            .begin_upgrade(
                ForkVersion::parse("3.20.4-fork.4").unwrap(),
                InstallSource::ManualSetup,
            )
            .unwrap();
        catalog.advance(Phase::Prepared).unwrap();
        catalog.advance(Phase::Quiescing).unwrap();
        lease.save(&catalog).unwrap();
        let database = lease
            .capture_database(&database_path, CaptureSlot::Previous)
            .unwrap();
        let (inventory, digest) = lease
            .capture_resources(&requests, CaptureSlot::Previous)
            .unwrap();
        let point = lease
            .seal_snapshot(&install, &database, &inventory, &digest, None)
            .unwrap();
        let mut catalog = lease.load().unwrap().unwrap();
        catalog.advance(Phase::Installing).unwrap();
        catalog.advance(Phase::Verifying).unwrap();
        catalog.commit_upgrade(point).unwrap();
        catalog.finish_cleanup(None).unwrap();
        lease.save(&catalog).unwrap();
        clear_readonly(&paths[0]).unwrap();
        fs::write(&paths[0], b"new settings").unwrap();
        fs::remove_file(&paths[1]).unwrap();
        let receipt = if owned_addition {
            Some(
                lease
                    .begin_managed_file_write(
                        &paths[2],
                        ResourceRole::Provider,
                        outcome(b"external addition"),
                    )
                    .unwrap(),
            )
        } else {
            None
        };
        if create_addition {
            fs::write(&paths[2], b"external addition").unwrap();
        }
        if let Some(receipt) = receipt {
            lease.complete_managed_file_write(receipt).unwrap();
        }
        let mut catalog = lease.load().unwrap().unwrap();
        catalog.begin_rollback().unwrap();
        catalog.advance(Phase::Prepared).unwrap();
        catalog.advance(Phase::Quiescing).unwrap();
        lease.save(&catalog).unwrap();
        lease
            .capture_resources(&requests, CaptureSlot::Rescue)
            .unwrap();
        let mut catalog = lease.load().unwrap().unwrap();
        catalog.advance(Phase::Captured).unwrap();
        catalog.advance(Phase::Installing).unwrap();
        catalog.advance(Phase::Restoring).unwrap();
        lease.save(&catalog).unwrap();
        (lease, paths)
    }

    fn start_recovery(lease: &mut StoreLease) {
        let mut catalog = lease.load().unwrap().unwrap();
        catalog.advance(Phase::Failed).unwrap();
        catalog.advance(Phase::Recovering).unwrap();
        lease.save(&catalog).unwrap();
    }

    fn outcome(bytes: &[u8]) -> crate::ManagedFileOutcome {
        crate::ManagedFileOutcome::File {
            bytes: bytes.len() as u64,
            sha256: Digest::parse(&format!("{:x}", Sha256::digest(bytes))).unwrap(),
        }
    }

    fn idle_writer(temp: &Path) -> (StoreLease, Vec<PathBuf>) {
        let (mut lease, paths) = ready(temp, false);
        start_recovery(&mut lease);
        let mut catalog = lease.load().unwrap().unwrap();
        let id = catalog.journal().unwrap().id();
        let version = catalog.current_version().clone();
        catalog.confirm_recovered(&version).unwrap();
        catalog.finish_recovery_cleanup(id).unwrap();
        lease.save(&catalog).unwrap();
        (lease, paths)
    }

    fn assert_old(paths: &[PathBuf]) {
        assert_eq!(fs::read(&paths[0]).unwrap(), b"old settings\r\n");
        assert!(fs::metadata(&paths[0]).unwrap().permissions().readonly());
        assert_eq!(fs::read(&paths[1]).unwrap(), b"old provider");
        assert!(!paths[2].exists());
    }

    fn assert_rescue(paths: &[PathBuf]) {
        assert_eq!(fs::read(&paths[0]).unwrap(), b"new settings");
        assert!(!fs::metadata(&paths[0]).unwrap().permissions().readonly());
        assert!(!paths[1].exists());
        assert!(!paths[2].exists());
    }

    #[test]
    fn exact_bytes_readonly_and_deleted_file_restore_without_consuming_the_point() {
        let temp = tempfile::tempdir().unwrap();
        let (mut lease, paths) = ready(temp.path(), false);
        lease.restore_previous_resource_files().unwrap();
        assert_old(&paths);
        lease.restore_previous_resource_files().unwrap();
        assert_old(&paths);
        let catalog = lease.load().unwrap().unwrap();
        assert!(catalog.previous().is_some());
        assert_eq!(catalog.journal().unwrap().phase(), Phase::Restoring);
        clear_readonly(&paths[0]).unwrap();
    }

    #[test]
    fn every_resource_checkpoint_resumes_after_reopening_the_os_lease() {
        for index in 0..2 {
            for phase in [
                FilePhase::Staging,
                FilePhase::Staged,
                FilePhase::Replacing,
                FilePhase::Replaced,
                FilePhase::Verifying,
            ] {
                let temp = tempfile::tempdir().unwrap();
                let (mut lease, paths) = ready(temp.path(), false);
                assert!(lease
                    .restore_resource_files(Mode::Previous, |now_index, now_phase| {
                        if now_index == index && now_phase == phase {
                            Err(invalid("simulated interrupted helper"))
                        } else {
                            Ok(())
                        }
                    })
                    .is_err());
                drop(lease);
                let mut lease = PrivateRoot::create_at(temp.path().join("private"))
                    .unwrap()
                    .try_lease()
                    .unwrap();
                lease.restore_previous_resource_files().unwrap();
                assert_old(&paths);
                clear_readonly(&paths[0]).unwrap();
            }
        }
    }

    #[test]
    fn compensation_restores_new_bytes_and_only_removes_its_own_created_file() {
        let temp = tempfile::tempdir().unwrap();
        let (mut lease, paths) = ready(temp.path(), false);
        lease.restore_previous_resource_files().unwrap();
        assert!(lease.restore_rescue_resource_files().is_err());
        start_recovery(&mut lease);
        lease.restore_rescue_resource_files().unwrap();
        assert_rescue(&paths);
        lease.restore_rescue_resource_files().unwrap();
        assert_rescue(&paths);
        assert_eq!(
            lease.load().unwrap().unwrap().journal().unwrap().phase(),
            Phase::Recovering
        );
    }

    #[test]
    fn partial_forward_restore_can_compensate_at_each_checkpoint() {
        for index in 0..2 {
            for phase in [
                FilePhase::Staging,
                FilePhase::Staged,
                FilePhase::Replacing,
                FilePhase::Replaced,
                FilePhase::Verifying,
            ] {
                let temp = tempfile::tempdir().unwrap();
                let (mut lease, paths) = ready(temp.path(), false);
                assert!(lease
                    .restore_resource_files(Mode::Previous, |now_index, now_phase| {
                        if now_index == index && now_phase == phase {
                            Err(invalid("simulated forward failure"))
                        } else {
                            Ok(())
                        }
                    })
                    .is_err());
                start_recovery(&mut lease);
                lease.restore_rescue_resource_files().unwrap();
                assert_rescue(&paths);
            }
        }
    }

    #[test]
    fn compensation_including_owned_deletion_resumes_at_each_checkpoint() {
        for index in 0..2 {
            for phase in [
                FilePhase::Staging,
                FilePhase::Staged,
                FilePhase::Replacing,
                FilePhase::Replaced,
                FilePhase::Verifying,
            ] {
                let temp = tempfile::tempdir().unwrap();
                let (mut lease, paths) = ready(temp.path(), false);
                lease.restore_previous_resource_files().unwrap();
                start_recovery(&mut lease);
                assert!(lease
                    .restore_resource_files(Mode::Rescue, |now_index, now_phase| {
                        if now_index == index && now_phase == phase {
                            Err(invalid("simulated compensation interruption"))
                        } else {
                            Ok(())
                        }
                    })
                    .is_err());
                drop(lease);
                let mut lease = PrivateRoot::create_at(temp.path().join("private"))
                    .unwrap()
                    .try_lease()
                    .unwrap();
                lease.restore_rescue_resource_files().unwrap();
                assert_rescue(&paths);
            }
        }
    }

    #[test]
    fn rename_completed_before_the_checkpoint_is_recognized_on_resume() {
        let temp = tempfile::tempdir().unwrap();
        let (mut lease, paths) = ready(temp.path(), false);
        let catalog = lease.load().unwrap().unwrap();
        let journal = catalog.journal().unwrap().clone();
        assert!(lease
            .restore_resource_files(Mode::Previous, |index, phase| {
                if index == 0 && phase == FilePhase::Replacing {
                    let stage = stage_path(&paths[0], &journal, Mode::Previous, 0);
                    let source = wide_path(&stage);
                    let destination = wide_path(&paths[0]);
                    // SAFETY: this simulates the same validated move as the executor
                    // but interrupts before the Replaced checkpoint can be persisted.
                    assert_ne!(
                        unsafe {
                            MoveFileExW(
                                source.as_ptr(),
                                destination.as_ptr(),
                                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
                            )
                        },
                        0
                    );
                    return Err(invalid("simulated crash after rename"));
                }
                Ok(())
            })
            .is_err());
        drop(lease);
        let mut lease = PrivateRoot::create_at(temp.path().join("private"))
            .unwrap()
            .try_lease()
            .unwrap();
        lease.restore_previous_resource_files().unwrap();
        assert_old(&paths);
        clear_readonly(&paths[0]).unwrap();
    }

    #[test]
    fn unowned_stage_and_tampered_progress_are_rejected_before_live_writes() {
        let temp = tempfile::tempdir().unwrap();
        let (mut lease, paths) = ready(temp.path(), false);
        let catalog = lease.load().unwrap().unwrap();
        let journal = catalog.journal().unwrap();
        let stage = stage_path(&paths[0], journal, Mode::Previous, 0);
        fs::write(&stage, b"unowned").unwrap();
        assert!(lease.restore_previous_resource_files().is_err());
        assert_eq!(fs::read(&paths[0]).unwrap(), b"new settings");
        assert_eq!(fs::read(&stage).unwrap(), b"unowned");
        fs::remove_file(&stage).unwrap();
        let ledger = file_ledger_path(
            &lease
                .root
                .join("transactions")
                .join(journal.id().to_string()),
            Mode::Previous,
        );
        assert!(lease
            .restore_resource_files(Mode::Previous, |_, _| Err(invalid("simulated crash")))
            .is_err());
        let mut progress: serde_json::Value =
            serde_json::from_slice(&fs::read(&ledger).unwrap()).unwrap();
        progress["point_id"] = serde_json::json!(Uuid::new_v4());
        fs::write(&ledger, serde_json::to_vec(&progress).unwrap()).unwrap();
        assert!(lease.restore_previous_resource_files().is_err());
        assert_eq!(fs::read(&paths[0]).unwrap(), b"new settings");
    }

    #[test]
    fn resource_added_without_ownership_or_changed_after_rescue_aborts_the_entire_plan() {
        let temp = tempfile::tempdir().unwrap();
        let (mut lease, paths) = ready(temp.path(), true);
        assert!(lease.restore_previous_resource_files().is_err());
        assert_eq!(fs::read(&paths[0]).unwrap(), b"new settings");
        assert_eq!(fs::read(&paths[2]).unwrap(), b"external addition");
        let temp = tempfile::tempdir().unwrap();
        let (mut lease, paths) = ready(temp.path(), false);
        fs::write(&paths[1], b"external after rescue").unwrap();
        assert!(lease.restore_previous_resource_files().is_err());
        assert_eq!(fs::read(&paths[0]).unwrap(), b"new settings");
        assert_eq!(fs::read(&paths[1]).unwrap(), b"external after rescue");
    }

    #[test]
    fn completed_point_scoped_receipt_allows_added_file_deletion_and_rescue_restores_it() {
        for interrupted in [
            FilePhase::Staging,
            FilePhase::Staged,
            FilePhase::Replacing,
            FilePhase::Replaced,
            FilePhase::Verifying,
            FilePhase::Complete,
        ] {
            let temp = tempfile::tempdir().unwrap();
            let (mut lease, paths) = ready_with_writer(temp.path(), true, true);
            assert!(lease
                .restore_resource_files(Mode::Previous, |index, phase| {
                    if (index == 2 && phase == interrupted) || phase == FilePhase::Complete {
                        Err(invalid("simulated delete boundary"))
                    } else {
                        Ok(())
                    }
                })
                .is_err());
            drop(lease);
            let mut lease = PrivateRoot::create_at(temp.path().join("private"))
                .unwrap()
                .try_lease()
                .unwrap();
            lease.restore_previous_resource_files().unwrap();
            assert_old(&paths);
            start_recovery(&mut lease);
            lease.restore_rescue_resource_files().unwrap();
            assert_eq!(fs::read(&paths[0]).unwrap(), b"new settings");
            assert!(!paths[1].exists());
            assert_eq!(fs::read(&paths[2]).unwrap(), b"external addition");
        }
    }

    #[test]
    fn write_intent_blocks_installation_until_independent_result_verification() {
        let temp = tempfile::tempdir().unwrap();
        let (mut lease, paths) = idle_writer(temp.path());
        assert!(lease
            .begin_managed_file_write(&paths[2], ResourceRole::Skill, outcome(b"owned"))
            .is_err());
        let id = lease
            .begin_managed_file_write(&paths[2], ResourceRole::Provider, outcome(b"owned"))
            .unwrap();
        assert!(lease.complete_managed_file_write(id).is_err());
        let mut catalog = lease.load().unwrap().unwrap();
        assert!(catalog.begin_rollback().is_err());
        assert!(catalog
            .begin_upgrade(
                ForkVersion::parse("3.20.4-fork.5").unwrap(),
                InstallSource::ManualSetup
            )
            .is_err());
        fs::write(&paths[2], b"owned").unwrap();
        assert!(lease.cancel_managed_file_write(id).is_err());
        lease.complete_managed_file_write(id).unwrap();
        assert!(lease.complete_managed_file_write(id).is_err());
        fs::write(&paths[2], b"external").unwrap();
        assert!(lease
            .begin_managed_file_write(&paths[2], ResourceRole::Provider, outcome(b"replacement"))
            .is_err());
    }

    #[test]
    fn crashed_managed_writer_reconciles_only_exact_before_or_after_bytes() {
        for result in [None, Some(b"owned".as_slice()), Some(b"partial".as_slice())] {
            let temp = tempfile::tempdir().unwrap();
            let (mut lease, paths) = idle_writer(temp.path());
            lease
                .begin_managed_file_write(&paths[2], ResourceRole::Provider, outcome(b"owned"))
                .unwrap();
            if let Some(bytes) = result {
                fs::write(&paths[2], bytes).unwrap();
            }
            drop(lease);
            let mut lease = PrivateRoot::create_at(temp.path().join("private"))
                .unwrap()
                .try_lease()
                .unwrap();
            if result == Some(b"partial".as_slice()) {
                assert!(lease.reconcile_pending_managed_file_write().is_err());
                assert!(lease.load().unwrap().unwrap().begin_rollback().is_err());
            } else {
                lease.reconcile_pending_managed_file_write().unwrap();
                let mut catalog = lease.load().unwrap().unwrap();
                assert_eq!(catalog.managed_file_writes.is_empty(), result.is_none());
                catalog.begin_rollback().unwrap();
            }
        }
    }

    #[test]
    fn ownership_record_cannot_be_reused_for_a_different_point() {
        let temp = tempfile::tempdir().unwrap();
        let (mut lease, paths) = idle_writer(temp.path());
        let id = lease
            .begin_managed_file_write(&paths[2], ResourceRole::Provider, outcome(b"owned"))
            .unwrap();
        fs::write(&paths[2], b"owned").unwrap();
        lease.complete_managed_file_write(id).unwrap();
        let mut bytes: serde_json::Value =
            serde_json::from_slice(&fs::read(lease.root.join("active.json")).unwrap()).unwrap();
        bytes["managed_file_writes"][0]["point_id"] = serde_json::json!(Uuid::new_v4());
        fs::write(
            lease.root.join("active.json"),
            serde_json::to_vec(&bytes).unwrap(),
        )
        .unwrap();
        assert!(lease.load().is_err());
    }

    #[test]
    fn next_successful_upgrade_replaces_the_single_point_and_clears_old_write_receipts() {
        let temp = tempfile::tempdir().unwrap();
        let (mut lease, paths) = idle_writer(temp.path());
        let id = lease
            .begin_managed_file_write(&paths[2], ResourceRole::Provider, outcome(b"owned"))
            .unwrap();
        fs::write(&paths[2], b"owned").unwrap();
        lease.complete_managed_file_write(id).unwrap();
        let mut catalog = lease.load().unwrap().unwrap();
        let old_id = catalog.previous().unwrap().id;
        catalog
            .begin_upgrade(
                ForkVersion::parse("3.20.4-fork.5").unwrap(),
                InstallSource::ManualSetup,
            )
            .unwrap();
        catalog.advance(Phase::Prepared).unwrap();
        catalog.advance(Phase::Quiescing).unwrap();
        lease.save(&catalog).unwrap();
        let database = lease
            .capture_database(&temp.path().join("business.db"), CaptureSlot::Previous)
            .unwrap();
        let requests: Vec<_> = paths
            .iter()
            .map(|path| ResourceRequest {
                path: path.clone(),
                role: ResourceRole::Provider,
                kind: ResourceKind::File,
            })
            .collect();
        let (resources, digest) = lease
            .capture_resources(&requests, CaptureSlot::Previous)
            .unwrap();
        let point = lease
            .seal_snapshot(
                &temp.path().join("安装 路径"),
                &database,
                &resources,
                &digest,
                None,
            )
            .unwrap();
        let mut catalog = lease.load().unwrap().unwrap();
        catalog.advance(Phase::Installing).unwrap();
        catalog.advance(Phase::Verifying).unwrap();
        catalog.commit_upgrade(point).unwrap();
        assert_eq!(catalog.cleanup_pending(), Some(old_id));
        assert_ne!(catalog.previous().unwrap().id, old_id);
        assert_eq!(
            catalog.previous().unwrap().source_version.as_string(),
            "3.20.4-fork.4"
        );
        assert!(catalog.managed_file_writes.is_empty());
        catalog.finish_cleanup(Some(old_id)).unwrap();
        lease.save(&catalog).unwrap();
        assert!(lease.complete_managed_file_write(id).is_err());
    }

    #[test]
    fn tampered_rescue_material_or_metadata_is_rejected_before_any_live_write() {
        for tamper_metadata in [true, false] {
            let temp = tempfile::tempdir().unwrap();
            let (mut lease, paths) = ready(temp.path(), false);
            let catalog = lease.load().unwrap().unwrap();
            let journal = catalog.journal().unwrap();
            let directory =
                lease.resource_directory(journal.id(), journal.point_id(), CaptureSlot::Rescue);
            if tamper_metadata {
                fs::write(directory.join("inventory.json"), b"{}").unwrap();
            } else {
                let inventory = lease
                    .read_resource_inventory(
                        journal.id(),
                        journal.point_id(),
                        CaptureSlot::Rescue,
                        journal.rescue_resources_sha256().unwrap(),
                    )
                    .unwrap();
                let ResourceState::File { material_id, .. } = inventory.resources[0].state else {
                    panic!("expected file");
                };
                fs::write(directory.join(format!("{material_id}.bin")), b"changed").unwrap();
            }
            assert!(lease.restore_previous_resource_files().is_err());
            assert_eq!(fs::read(&paths[0]).unwrap(), b"new settings");
            assert!(!paths[1].exists());
        }
    }
}
