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

use crate::windows_database::{
    check_integrity, hash_reader, open_read_only, pragma_i64, require_free_space,
};
use crate::windows_store::{invalid, lock_regular_file, validate_ntfs_path, wide_path};
use crate::{
    CaptureSlot, DatabaseImage, Digest, Direction, FileDacl, Phase, StoreError, StoreLease,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RestoreMode {
    Previous,
    Rescue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RestorePhase {
    Staging,
    Staged,
    RemovingSidecars,
    SidecarsRemoved,
    Replacing,
    Replaced,
    Verifying,
    Complete,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DatabaseRestoreLedger {
    format_version: u32,
    transaction_id: Uuid,
    point_id: Uuid,
    destination: PathBuf,
    image_sha256: Digest,
    rescue_metadata_sha256: Digest,
    mode: RestoreMode,
    phase: RestorePhase,
}

impl StoreLease {
    /// The independent helper holds the executable launch fence throughout
    /// restoration. This API never launches/migrates a GUI or consumes a point.
    pub fn restore_previous_database(&mut self) -> Result<(), StoreError> {
        self.restore_database_inner(RestoreMode::Previous, |_| Ok(()))
    }

    /// Program/package compensation is coordinated separately by the helper.
    /// Restoring rescue bytes alone does not mark the global transaction recovered.
    pub fn restore_rescue_database(&mut self) -> Result<(), StoreError> {
        self.restore_database_inner(RestoreMode::Rescue, |_| Ok(()))
    }

    fn restore_database_inner(
        &mut self,
        mode: RestoreMode,
        mut after_checkpoint: impl FnMut(RestorePhase) -> Result<(), StoreError>,
    ) -> Result<(), StoreError> {
        let catalog = self
            .load()?
            .ok_or_else(|| invalid("database restore has no catalog"))?;
        let journal = catalog
            .journal()
            .cloned()
            .ok_or_else(|| invalid("database restore has no transaction"))?;
        match mode {
            RestoreMode::Previous
                if journal.direction() == Direction::Rollback
                    && journal.phase() == Phase::Restoring => {}
            RestoreMode::Rescue if journal.phase() == Phase::Recovering => {}
            _ => return Err(crate::ProtocolError::Phase.into()),
        }
        let bound_rescue = journal
            .rescue_database_sha256()
            .ok_or_else(|| invalid("no bound emergency database exists"))?;
        let rescue_directory =
            self.database_directory(journal.id(), journal.point_id(), CaptureSlot::Rescue);
        let metadata = read_bounded(&rescue_directory.join("database.json"), 1024 * 1024)?;
        if Digest::parse(&format!("{:x}", Sha256::digest(&metadata)))? != *bound_rescue {
            return Err(invalid("emergency database metadata changed"));
        }
        let rescue: DatabaseImage = serde_json::from_slice(&metadata)?;
        if rescue.transaction_id != journal.id()
            || rescue.point_id != journal.point_id()
            || rescue.slot != CaptureSlot::Rescue
        {
            return Err(invalid(
                "emergency database belongs to a different transaction",
            ));
        }
        self.verify_database(&rescue)?;
        let image = match mode {
            RestoreMode::Previous => {
                let point = catalog.previous().ok_or(crate::ProtocolError::NoPoint)?;
                self.verify_snapshot(point)?.database
            }
            RestoreMode::Rescue => rescue.clone(),
        };
        if image.source_path != rescue.source_path {
            return Err(invalid(
                "changed business directory requires separate destination rescue material",
            ));
        }
        let directory = self
            .root
            .join("transactions")
            .join(journal.id().to_string());
        validate_ntfs_path(&directory)?;
        fs::create_dir_all(&directory)?;
        validate_ntfs_path(&directory)?;
        let name = match mode {
            RestoreMode::Previous => "previous",
            RestoreMode::Rescue => "rescue",
        };
        let ledger_path = directory.join(format!("database-restore-{name}.json"));
        let mut ledger = match read_bounded(&ledger_path, 1024 * 1024) {
            Ok(bytes) => serde_json::from_slice::<DatabaseRestoreLedger>(&bytes)?,
            Err(StoreError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
                DatabaseRestoreLedger {
                    format_version: 1,
                    transaction_id: journal.id(),
                    point_id: journal.point_id(),
                    destination: image.source_path.clone(),
                    image_sha256: image.sha256.clone(),
                    rescue_metadata_sha256: bound_rescue.clone(),
                    mode,
                    phase: RestorePhase::Staging,
                }
            }
            Err(error) => return Err(error),
        };
        if ledger.format_version != 1
            || ledger.transaction_id != journal.id()
            || ledger.point_id != journal.point_id()
            || ledger.destination != image.source_path
            || ledger.image_sha256 != image.sha256
            || ledger.rescue_metadata_sha256 != *bound_rescue
            || ledger.mode != mode
        {
            return Err(invalid(
                "database restore intent changed or belongs to another transaction",
            ));
        }
        let checkpoint =
            |this: &StoreLease,
             ledger: &mut DatabaseRestoreLedger,
             phase,
             callback: &mut dyn FnMut(RestorePhase) -> Result<(), StoreError>| {
                ledger.phase = phase;
                this.write_private_file(&ledger_path, &serde_json::to_vec_pretty(ledger)?)?;
                callback(phase)
            };
        let target = &image.source_path;
        validate_ntfs_path(target)?;
        let stage =
            target.with_file_name(format!("cc-switch-rollback-{}-{name}.db.tmp", journal.id()));
        let material = self
            .database_directory(image.transaction_id, image.point_id, image.slot)
            .join("cc-switch.db");
        if ledger.phase == RestorePhase::Staging {
            checkpoint(
                self,
                &mut ledger,
                RestorePhase::Staging,
                &mut after_checkpoint,
            )?;
            // Before the first destructive action, current raw DB/WAL must
            // still match the emergency capture taken in the stopped state.
            if mode == RestoreMode::Previous {
                verify_live_source(&rescue, true)?;
            }
            require_free_space(
                target.parent().unwrap(),
                image
                    .bytes
                    .checked_add(64 * 1024 * 1024)
                    .ok_or_else(|| invalid("database restore space estimate overflow"))?,
            )?;
            let mut input = lock_regular_file(&material)?;
            if !stage.try_exists()? {
                let mut output = OpenOptions::new()
                    .create_new(true)
                    .read(true)
                    .write(true)
                    .access_mode(0xc004_0000)
                    .open(&stage)?; // GENERIC_READ | GENERIC_WRITE | WRITE_DAC
                io::copy(&mut input, &mut output)?;
                output.sync_all()?;
                image.source_dacl.apply(&output)?;
            }
            verify_material(&stage, &image)?;
            checkpoint(
                self,
                &mut ledger,
                RestorePhase::Staged,
                &mut after_checkpoint,
            )?;
        }
        if ledger.phase == RestorePhase::Staged {
            verify_material(&stage, &image)?;
            if mode == RestoreMode::Previous {
                verify_live_source(&rescue, true)?;
            }
            // The intent is durable BEFORE WAL/SHM removal. A crash here is
            // resumed under the launch fence; no GUI may see the half-state.
            checkpoint(
                self,
                &mut ledger,
                RestorePhase::RemovingSidecars,
                &mut after_checkpoint,
            )?;
        }
        if ledger.phase == RestorePhase::RemovingSidecars {
            if mode == RestoreMode::Previous {
                verify_live_source(&rescue, false)?;
            }
            for suffix in ["-wal", "-shm"] {
                let path = sidecar(target, suffix);
                match lock_regular_file(&path) {
                    Ok(file) => {
                        drop(file);
                        fs::remove_file(&path)?;
                    }
                    Err(StoreError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
            }
            checkpoint(
                self,
                &mut ledger,
                RestorePhase::SidecarsRemoved,
                &mut after_checkpoint,
            )?;
        }
        if ledger.phase == RestorePhase::SidecarsRemoved {
            checkpoint(
                self,
                &mut ledger,
                RestorePhase::Replacing,
                &mut after_checkpoint,
            )?;
        }
        if ledger.phase == RestorePhase::Replacing {
            // A crash after MoveFileEx but before its checkpoint leaves the
            // desired bytes in place; detect that and do not require a stage.
            if !material_matches(target, &image)? {
                verify_material(&stage, &image)?;
                match lock_regular_file(target) {
                    Ok(file) => {
                        let mut permissions = file.metadata()?.permissions();
                        drop(file);
                        if permissions.readonly() {
                            permissions.set_readonly(false);
                            fs::set_permissions(target, permissions)?;
                        }
                    }
                    Err(StoreError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
                let source = wide_path(&stage);
                let destination = wide_path(target);
                // SAFETY: same-volume terminated paths, original target and
                // staging validated; the intent is durably bound to this DB.
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
            checkpoint(
                self,
                &mut ledger,
                RestorePhase::Replaced,
                &mut after_checkpoint,
            )?;
        }
        if ledger.phase == RestorePhase::Replaced {
            // WRITE_DAC handle changes this exact file; no path-following ACL
            // restore and no owner/SACL changes. Reapply after a resumed rename.
            let file = OpenOptions::new()
                .read(true)
                .access_mode(0x8004_0000)
                .open(target)?;
            image.source_dacl.apply(&file)?;
            let mut permissions = file.metadata()?.permissions();
            drop(file);
            permissions.set_readonly(image.source_readonly);
            fs::set_permissions(target, permissions)?;
            checkpoint(
                self,
                &mut ledger,
                RestorePhase::Verifying,
                &mut after_checkpoint,
            )?;
        }
        if matches!(
            ledger.phase,
            RestorePhase::Verifying | RestorePhase::Complete
        ) {
            verify_material(target, &image)?;
            let file = lock_regular_file(target)?;
            if FileDacl::capture(&file)? != image.source_dacl
                || file.metadata()?.permissions().readonly() != image.source_readonly
            {
                return Err(invalid("restored database permissions changed"));
            }
            let database = open_read_only(target)?;
            check_integrity(&database)?;
            if pragma_i64(&database, "user_version")? != image.user_version {
                return Err(invalid(
                    "restored database schema differs from the source version",
                ));
            }
            drop(database);
            drop(file);
            // An interrupted first attempt can leave a matching redundant
            // stage when the live image already matched. Remove only verified
            // bytes in this transaction's reserved staging name.
            if stage.try_exists()? {
                verify_material(&stage, &image)?;
                fs::remove_file(&stage)?;
            }
            if ledger.phase != RestorePhase::Complete {
                checkpoint(
                    self,
                    &mut ledger,
                    RestorePhase::Complete,
                    &mut after_checkpoint,
                )?;
            }
        }
        Ok(())
    }
}

fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>, StoreError> {
    let mut file = lock_regular_file(path)?;
    let mut bytes = Vec::new();
    file.by_ref().take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(invalid("restore metadata exceeds its size limit"));
    }
    Ok(bytes)
}

fn material_matches(path: &Path, image: &DatabaseImage) -> Result<bool, StoreError> {
    let mut file = match lock_regular_file(path) {
        Ok(file) => file,
        Err(StoreError::Io(error)) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    Ok(file.metadata()?.len() == image.bytes && hash_reader(&mut file)? == image.sha256)
}
fn verify_material(path: &Path, image: &DatabaseImage) -> Result<(), StoreError> {
    if !material_matches(path, image)? {
        return Err(invalid("restored or staged database digest changed"));
    }
    Ok(())
}
fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    name.into()
}
fn verify_live_source(image: &DatabaseImage, include_wal: bool) -> Result<(), StoreError> {
    let mut file = lock_regular_file(&image.source_path)?;
    if hash_reader(&mut file)? != image.source_main_sha256 {
        return Err(invalid("live database changed after its emergency capture"));
    }
    if include_wal {
        let now = match lock_regular_file(&sidecar(&image.source_path, "-wal")) {
            Ok(mut file) => Some(hash_reader(&mut file)?),
            Err(StoreError::Io(error)) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        if now != image.source_wal_sha256 {
            return Err(invalid("live WAL changed after its emergency capture"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Catalog, ForkVersion, InstallSource, PrivateRoot, ResourceKind, ResourceRequest,
        ResourceRole,
    };

    fn ready_rollback(temp: &Path) -> (StoreLease, PathBuf) {
        let install = temp.join("installation");
        fs::create_dir(&install).unwrap();
        fs::write(install.join("cc-switch.exe"), b"MZ fixture").unwrap();
        let source = temp.join("business.db");
        let database = rusqlite::Connection::open(&source).unwrap();
        database.execute_batch("PRAGMA user_version=19; CREATE TABLE sentinel(value TEXT); INSERT INTO sentinel VALUES ('old');").unwrap();
        drop(database);
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
        let image = lease
            .capture_database(&source, CaptureSlot::Previous)
            .unwrap();
        let (resources, digest) = lease
            .capture_resources(
                &[ResourceRequest {
                    path: temp.join("missing.json"),
                    role: ResourceRole::Settings,
                    kind: ResourceKind::File,
                }],
                CaptureSlot::Previous,
            )
            .unwrap();
        let point = lease
            .seal_snapshot(&install, &image, &resources, &digest, None)
            .unwrap();
        let mut catalog = lease.load().unwrap().unwrap();
        catalog.advance(Phase::Installing).unwrap();
        catalog.advance(Phase::Verifying).unwrap();
        catalog.commit_upgrade(point).unwrap();
        catalog.finish_cleanup(None).unwrap();
        lease.save(&catalog).unwrap();
        let database = rusqlite::Connection::open(&source).unwrap();
        database
            .execute_batch("PRAGMA user_version=20; UPDATE sentinel SET value='new';")
            .unwrap();
        drop(database);
        catalog.begin_rollback().unwrap();
        catalog.advance(Phase::Prepared).unwrap();
        catalog.advance(Phase::Quiescing).unwrap();
        lease.save(&catalog).unwrap();
        lease
            .capture_database(&source, CaptureSlot::Rescue)
            .unwrap();
        let mut catalog = lease.load().unwrap().unwrap();
        catalog.advance(Phase::Captured).unwrap();
        catalog.advance(Phase::Installing).unwrap();
        catalog.advance(Phase::Restoring).unwrap();
        lease.save(&catalog).unwrap();
        (lease, source)
    }

    #[test]
    fn old_schema_and_rows_restore_without_migration_and_point_is_not_consumed_early() {
        let temp = tempfile::tempdir().unwrap();
        let (mut lease, source) = ready_rollback(temp.path());
        lease.restore_previous_database().unwrap();
        let database = open_read_only(&source).unwrap();
        assert_eq!(pragma_i64(&database, "user_version").unwrap(), 19);
        assert_eq!(
            database
                .query_row("SELECT value FROM sentinel", [], |row| row
                    .get::<_, String>(0))
                .unwrap(),
            "old"
        );
        assert!(lease.load().unwrap().unwrap().previous().is_some());
        assert_eq!(
            lease.load().unwrap().unwrap().journal().unwrap().phase(),
            Phase::Restoring
        );
    }

    #[test]
    fn interruption_at_each_durable_checkpoint_can_resume_after_the_os_lease_reopens() {
        for interrupted in [
            RestorePhase::Staging,
            RestorePhase::Staged,
            RestorePhase::RemovingSidecars,
            RestorePhase::SidecarsRemoved,
            RestorePhase::Replacing,
            RestorePhase::Replaced,
            RestorePhase::Verifying,
        ] {
            let temp = tempfile::tempdir().unwrap();
            let (mut lease, source) = ready_rollback(temp.path());
            assert!(lease
                .restore_database_inner(RestoreMode::Previous, |phase| {
                    if phase == interrupted {
                        Err(invalid("simulated helper interruption"))
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
            lease.restore_previous_database().unwrap();
            let database = open_read_only(&source).unwrap();
            assert_eq!(
                pragma_i64(&database, "user_version").unwrap(),
                19,
                "{interrupted:?}"
            );
            assert_eq!(
                database
                    .query_row("SELECT value FROM sentinel", [], |row| row
                        .get::<_, String>(0))
                    .unwrap(),
                "old"
            );
        }
    }

    #[test]
    fn failed_old_restore_can_restore_new_data_only_under_global_recovery_phase() {
        let temp = tempfile::tempdir().unwrap();
        let (mut lease, source) = ready_rollback(temp.path());
        lease.restore_previous_database().unwrap();
        assert!(lease.restore_rescue_database().is_err());
        let mut catalog = lease.load().unwrap().unwrap();
        catalog.advance(Phase::Failed).unwrap();
        catalog.advance(Phase::Recovering).unwrap();
        lease.save(&catalog).unwrap();
        lease.restore_rescue_database().unwrap();
        let database = open_read_only(&source).unwrap();
        assert_eq!(pragma_i64(&database, "user_version").unwrap(), 20);
        assert_eq!(
            database
                .query_row("SELECT value FROM sentinel", [], |row| row
                    .get::<_, String>(0))
                .unwrap(),
            "new"
        );
        assert_eq!(
            lease.load().unwrap().unwrap().journal().unwrap().phase(),
            Phase::Recovering
        );
    }

    #[test]
    fn changed_live_data_or_emergency_metadata_aborts_before_overwriting_the_database() {
        let temp = tempfile::tempdir().unwrap();
        let (mut lease, source) = ready_rollback(temp.path());
        let database = rusqlite::Connection::open(&source).unwrap();
        database
            .execute("UPDATE sentinel SET value='external'", [])
            .unwrap();
        drop(database);
        assert!(lease.restore_previous_database().is_err());
        assert_eq!(
            open_read_only(&source)
                .unwrap()
                .query_row("SELECT value FROM sentinel", [], |row| row
                    .get::<_, String>(0))
                .unwrap(),
            "external"
        );
        let journal = lease.load().unwrap().unwrap().journal().unwrap().clone();
        fs::write(
            lease
                .database_directory(journal.id(), journal.point_id(), CaptureSlot::Rescue)
                .join("database.json"),
            b"{}",
        )
        .unwrap();
        assert!(lease.restore_previous_database().is_err());
    }
}
