use std::fs::{self, OpenOptions};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::ptr;
use std::time::Duration;

use rusqlite::{backup::Backup, backup::StepResult, Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use uuid::Uuid;
use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

use crate::windows_store::{invalid, lock_regular_file, validate_ntfs_path, wide_path};
use crate::{Digest, Direction, Phase, StoreError, StoreLease};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureSlot {
    Previous,
    Rescue,
}

/// No executable SQL or migration instructions are persisted in this image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseImage {
    pub transaction_id: Uuid,
    pub point_id: Uuid,
    pub slot: CaptureSlot,
    pub user_version: i64,
    pub page_count: u64,
    pub bytes: u64,
    pub sha256: Digest,
}

impl StoreLease {
    /// The coordinator must already have persisted Quiescing and stopped all
    /// writers. Holding the OS lease throughout this method binds the capture
    /// to that intent. A partial image never becomes an active rollback point.
    pub fn capture_database(
        &mut self,
        source: &Path,
        slot: CaptureSlot,
    ) -> Result<DatabaseImage, StoreError> {
        let catalog = self
            .load()?
            .ok_or_else(|| invalid("capture has no persisted catalog"))?;
        let journal = catalog
            .journal()
            .ok_or_else(|| invalid("capture has no transaction"))?;
        if journal.phase() != Phase::Quiescing
            || (slot == CaptureSlot::Previous && journal.direction() != Direction::Upgrade)
        {
            return Err(crate::ProtocolError::Phase.into());
        }
        let directory = self.database_directory(journal.id(), journal.point_id(), slot);
        validate_ntfs_path(&directory)?;
        let parent = directory.parent().unwrap();
        fs::create_dir_all(parent)?;
        validate_ntfs_path(parent)?;
        // Refuse to overwrite an earlier attempt, even after process death.
        fs::create_dir(&directory)?;
        let cleanup = IncompleteImage {
            path: directory.clone(),
            completed: false,
        };
        let image = capture_sqlite(source, &directory.join("cc-switch.db"))?;
        let image = DatabaseImage {
            transaction_id: journal.id(),
            point_id: journal.point_id(),
            slot,
            user_version: image.user_version,
            page_count: image.page_count,
            bytes: image.bytes,
            sha256: image.sha256,
        };
        let metadata = serde_json::to_vec_pretty(&image)?;
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(directory.join("database.json"))?;
        use std::io::Write;
        file.write_all(&metadata)?;
        file.sync_all()?;
        cleanup.complete();
        Ok(image)
    }

    /// Validate the bound immutable image before presenting a confirmation and
    /// again immediately before a restore. This never opens the live database.
    pub fn verify_database(&self, image: &DatabaseImage) -> Result<(), StoreError> {
        if image.transaction_id.is_nil() || image.point_id.is_nil() {
            return Err(invalid("database image has no identity"));
        }
        let directory = self.database_directory(image.transaction_id, image.point_id, image.slot);
        validate_ntfs_path(&directory)?;
        let path = directory.join("cc-switch.db");
        let mut guard = lock_regular_file(&path)?;
        if guard.metadata()?.len() != image.bytes || hash_reader(&mut guard)? != image.sha256 {
            return Err(invalid("database snapshot digest or length changed"));
        }
        let database = open_read_only(&path)?;
        check_integrity(&database)?;
        if pragma_i64(&database, "user_version")? != image.user_version
            || pragma_i64(&database, "page_count")? as u64 != image.page_count
        {
            return Err(invalid("database snapshot schema or page count changed"));
        }
        Ok(())
    }

    fn database_directory(&self, transaction: Uuid, point: Uuid, slot: CaptureSlot) -> PathBuf {
        match slot {
            CaptureSlot::Previous => self
                .root
                .join("points")
                .join(point.to_string())
                .join("database"),
            CaptureSlot::Rescue => self
                .root
                .join("transactions")
                .join(transaction.to_string())
                .join("rescue")
                .join("database"),
        }
    }
}

struct IncompleteImage {
    path: PathBuf,
    completed: bool,
}
impl IncompleteImage {
    fn complete(mut self) {
        // The directory is now immutable material owned by the transaction.
        // Its deletion is controlled by catalog commit/recovery cleanup.
        self.completed = true;
    }
}
impl Drop for IncompleteImage {
    fn drop(&mut self) {
        if !self.completed && validate_ntfs_path(&self.path).is_ok() {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

struct CapturedDatabase {
    user_version: i64,
    page_count: u64,
    bytes: u64,
    sha256: Digest,
}

fn capture_sqlite(source: &Path, destination: &Path) -> Result<CapturedDatabase, StoreError> {
    let _source_guard = lock_regular_file(source)?;
    // A crashed writer can leave committed pages only in WAL. Do not copy just
    // the main DB, and do not use immutable=1 (which would ignore those pages).
    let mut wal_name = source.as_os_str().to_owned();
    wal_name.push("-wal");
    let wal_path = PathBuf::from(wal_name);
    let _wal_guard = match lock_regular_file(&wal_path) {
        Ok(file) => Some(file),
        Err(StoreError::Io(error)) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    let source_db = open_read_only(source)?;
    let user_version = pragma_i64(&source_db, "user_version")?;
    let page_size = pragma_i64(&source_db, "page_size")?;
    let page_count = pragma_i64(&source_db, "page_count")?;
    if user_version < 0 || page_size <= 0 || page_count <= 0 {
        return Err(invalid("invalid source SQLite header"));
    }
    let estimated_bytes = (page_count as u64)
        .checked_mul(page_size as u64)
        .ok_or_else(|| invalid("database size overflow"))?;
    // Account for an output journal and a reserve, before creating the DB.
    require_free_space(
        destination.parent().unwrap(),
        estimated_bytes
            .checked_mul(2)
            .and_then(|bytes| bytes.checked_add(64 * 1024 * 1024))
            .ok_or_else(|| invalid("database space estimate overflow"))?,
    )?;
    OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(destination)?
        .sync_all()?;
    let mut output = Connection::open_with_flags(
        destination,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    output.busy_timeout(Duration::from_secs(2))?;
    {
        let backup = Backup::new(&source_db, &mut output)?;
        // BUSY, LOCKED and MORE are incomplete, never a successful snapshot.
        if !matches!(backup.step(-1)?, StepResult::Done) {
            return Err(invalid(
                "SQLite capture did not complete; no snapshot was published",
            ));
        }
    }
    output.pragma_update(None, "journal_mode", "DELETE")?;
    check_integrity(&output)?;
    if pragma_i64(&output, "user_version")? != user_version {
        return Err(invalid("SQLite backup changed the source schema version"));
    }
    let page_count = pragma_i64(&output, "page_count")? as u64;
    output.close().map_err(|(_, error)| error)?;
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(destination)?;
    file.sync_all()?;
    let bytes = file.metadata()?.len();
    let sha256 = hash_reader(&mut file)?;
    Ok(CapturedDatabase {
        user_version,
        page_count,
        bytes,
        sha256,
    })
}

fn open_read_only(path: &Path) -> Result<Connection, StoreError> {
    let database = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    database.busy_timeout(Duration::from_secs(2))?;
    database.pragma_update(None, "query_only", true)?;
    Ok(database)
}

fn pragma_i64(database: &Connection, name: &str) -> Result<i64, StoreError> {
    Ok(database.pragma_query_value(None, name, |row| row.get(0))?)
}

fn check_integrity(database: &Connection) -> Result<(), StoreError> {
    let mut statement = database.prepare("PRAGMA integrity_check")?;
    let mut rows = statement.query([])?;
    let Some(row) = rows.next()? else {
        return Err(invalid("SQLite integrity check returned no result"));
    };
    if row.get::<_, String>(0)? != "ok" || rows.next()?.is_some() {
        return Err(invalid("SQLite integrity check failed"));
    }
    Ok(())
}

pub(crate) fn hash_reader(reader: &mut impl Read) -> Result<Digest, StoreError> {
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(Digest::parse(&format!("{:x}", hash.finalize()))?)
}

pub(crate) fn require_free_space(path: &Path, minimum: u64) -> Result<(), StoreError> {
    validate_ntfs_path(path)?;
    let path = wide_path(path);
    let mut available = 0;
    // SAFETY: path is terminated and available is a writable u64. We use the
    // caller's available quota, not the total volume free-byte count.
    if unsafe {
        GetDiskFreeSpaceExW(
            path.as_ptr(),
            &mut available,
            ptr::null_mut(),
            ptr::null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error().into());
    }
    if available < minimum {
        return Err(invalid("insufficient disk space for rollback materials"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Catalog, ForkVersion, InstallSource, PrivateRoot};

    fn capturing_store(root: &Path) -> StoreLease {
        let private = PrivateRoot::create_at(root.join("private")).unwrap();
        let mut lease = private.try_lease().unwrap();
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
        lease
    }

    #[test]
    fn committed_wal_is_captured_without_migrating_the_original_schema() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("legacy.db");
        let writer = Connection::open(&source).unwrap();
        writer.execute_batch("PRAGMA user_version=19; PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE sentinel(value TEXT); PRAGMA wal_checkpoint(TRUNCATE); INSERT INTO sentinel VALUES ('committed WAL');").unwrap();
        // Preserve the exact durable state a terminated process leaves behind.
        let main = fs::read(&source).unwrap();
        let wal = fs::read(source.with_file_name("legacy.db-wal")).unwrap();
        drop(writer);
        fs::write(&source, main).unwrap();
        fs::write(source.with_file_name("legacy.db-wal"), wal).unwrap();
        let mut lease = capturing_store(temp.path());
        let image = lease
            .capture_database(&source, CaptureSlot::Previous)
            .unwrap();
        assert_eq!(image.user_version, 19);
        lease.verify_database(&image).unwrap();
        let path = lease
            .database_directory(image.transaction_id, image.point_id, image.slot)
            .join("cc-switch.db");
        let snapshot = open_read_only(&path).unwrap();
        let value: String = snapshot
            .query_row("SELECT value FROM sentinel", [], |row| row.get(0))
            .unwrap();
        assert_eq!(value, "committed WAL");
        assert_eq!(
            pragma_i64(&open_read_only(&source).unwrap(), "user_version").unwrap(),
            19
        );
    }

    #[test]
    fn missing_or_corrupt_live_database_never_creates_a_usable_image() {
        let temp = tempfile::tempdir().unwrap();
        let mut lease = capturing_store(temp.path());
        let source = temp.path().join("missing.db");
        assert!(lease
            .capture_database(&source, CaptureSlot::Previous)
            .is_err());
        assert!(!source.exists());
        fs::write(&source, b"this is not a database").unwrap();
        assert!(lease
            .capture_database(&source, CaptureSlot::Previous)
            .is_err());
        assert!(lease.load().unwrap().unwrap().previous().is_none());
    }

    #[test]
    fn snapshot_corruption_is_detected_before_restore() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source.db");
        let database = Connection::open(&source).unwrap();
        database
            .execute_batch("PRAGMA user_version=19; CREATE TABLE sentinel(value TEXT);")
            .unwrap();
        drop(database);
        let mut lease = capturing_store(temp.path());
        let image = lease
            .capture_database(&source, CaptureSlot::Previous)
            .unwrap();
        let path = lease
            .database_directory(image.transaction_id, image.point_id, image.slot)
            .join("cc-switch.db");
        fs::write(path, b"truncated").unwrap();
        assert!(lease.verify_database(&image).is_err());
    }

    #[test]
    fn capture_rejects_linked_database_and_an_already_open_writer() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source.db");
        let database = Connection::open(&source).unwrap();
        database
            .execute_batch("CREATE TABLE sentinel(value TEXT);")
            .unwrap();
        let mut lease = capturing_store(temp.path());
        assert!(lease
            .capture_database(&source, CaptureSlot::Previous)
            .is_err());
        drop(database);
        let linked = temp.path().join("linked.db");
        fs::hard_link(&source, &linked).unwrap();
        assert!(lease
            .capture_database(&linked, CaptureSlot::Previous)
            .is_err());
        assert!(lease
            .capture_database(&source, CaptureSlot::Previous)
            .is_err());
    }
}
