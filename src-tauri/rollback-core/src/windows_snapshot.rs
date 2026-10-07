use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use uuid::Uuid;

use crate::windows_database::hash_reader;
use crate::windows_store::{current_account_sid, invalid, lock_regular_file, validate_ntfs_path};
use crate::{
    CaptureSlot, DatabaseImage, Digest, Direction, FixedReleaseSetup, ForkVersion, InstallSource,
    Phase, Point, ResourceInventory, StoreError, StoreLease,
};

const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;

pub struct CachedSourceSetup<'a> {
    pub selection: &'a FixedReleaseSetup,
    pub path: &'a Path,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CachedSetupBinding {
    relative_path: PathBuf,
    sha256: Digest,
    bytes: u64,
    signature: String,
    release_url: String,
    public_key_sha256: Digest,
}

/// The digest of this immutable manifest is stored in the catalog's one Point.
/// DB and resource metadata are transitively bound by it. It deliberately does
/// not contain a Point with its own digest, avoiding a circular hash definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotManifest {
    format_version: u32,
    product: String,
    architecture: String,
    installation_scope: String,
    pub transaction_id: Uuid,
    pub point_id: Uuid,
    pub source_version: ForkVersion,
    pub installed_version: ForkVersion,
    pub install_directory: PathBuf,
    pub account_sid: String,
    pub captured_at_unix_ms: u64,
    pub source: InstallSource,
    pub source_binary_sha256: Digest,
    pub database: DatabaseImage,
    pub resource_inventory_sha256: Digest,
    cached_source_setup: Option<CachedSetupBinding>,
}

impl StoreLease {
    /// Persist a complete candidate before allowing installer replacement. The
    /// GUI health handshake must still pass before Catalog::commit_upgrade.
    /// The helper separately authenticates the HKCU/PE installation identity.
    pub fn seal_snapshot(
        &mut self,
        install_directory: &Path,
        database: &DatabaseImage,
        resources: &ResourceInventory,
        resource_digest: &Digest,
        cached_source: Option<CachedSourceSetup<'_>>,
    ) -> Result<Point, StoreError> {
        let mut catalog = self
            .load()?
            .ok_or_else(|| invalid("snapshot has no persisted catalog"))?;
        let journal = catalog
            .journal()
            .cloned()
            .ok_or_else(|| invalid("snapshot has no transaction"))?;
        if journal.direction() != Direction::Upgrade
            || journal.phase() != Phase::Quiescing
            || database.slot != CaptureSlot::Previous
            || resources.slot != CaptureSlot::Previous
            || database.transaction_id != journal.id()
            || resources.transaction_id != journal.id()
            || database.point_id != journal.point_id()
            || resources.point_id != journal.point_id()
        {
            return Err(invalid(
                "snapshot components are not bound to this capturing upgrade",
            ));
        }
        validate_ntfs_path(install_directory)?;
        self.verify_database(database)?;
        self.verify_resources(resources, resource_digest)?;
        let mut main_binary = lock_regular_file(&install_directory.join("cc-switch.exe"))?;
        let source_binary_sha256 = hash_reader(&mut main_binary)?;
        let cached_source_setup = match cached_source {
            Some(source) => {
                if source.selection.version() != &journal.from
                    || !journal.source.uses_cached_setup()
                {
                    return Err(invalid("source package is not bound to a protocol update"));
                }
                let relative_path = source
                    .path
                    .strip_prefix(&self.root)
                    .map_err(|_| invalid("source package is outside private rollback storage"))?
                    .to_path_buf();
                let mut file = lock_regular_file(source.path)?;
                let verified = source.selection.verify(&mut file, None).map_err(|error| {
                    invalid(&format!("source package failed verification: {error}"))
                })?;
                Some(CachedSetupBinding {
                    relative_path,
                    sha256: verified.sha256().clone(),
                    bytes: verified.bytes(),
                    signature: source.selection.signature().to_owned(),
                    release_url: source.selection.url().to_owned(),
                    public_key_sha256: verified.public_key_sha256().clone(),
                })
            }
            None if journal.source.uses_cached_setup() => {
                return Err(invalid(
                    "protocol update lacks its verified source package cache",
                ))
            }
            None => None,
        };
        let captured_at_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| invalid("capture clock precedes the epoch"))?
            .as_millis()
            .try_into()
            .map_err(|_| invalid("capture timestamp overflow"))?;
        let manifest = SnapshotManifest {
            format_version: 1,
            product: "com.ccswitch.desktop".into(),
            architecture: "windows-x86_64".into(),
            installation_scope: "current_user_nsis".into(),
            transaction_id: journal.id(),
            point_id: journal.point_id(),
            source_version: journal.from.clone(),
            installed_version: journal.to.clone(),
            install_directory: install_directory.to_owned(),
            account_sid: current_account_sid()?,
            captured_at_unix_ms,
            source: journal.source,
            source_binary_sha256,
            database: database.clone(),
            resource_inventory_sha256: resource_digest.clone(),
            cached_source_setup,
        };
        manifest.validate()?;
        let bytes = serde_json::to_vec_pretty(&manifest)?;
        let digest = Digest::parse(&format!("{:x}", Sha256::digest(&bytes)))?;
        let point = manifest.point(digest);
        let destination = self.snapshot_manifest_path(point.id);
        if destination.try_exists()? {
            return Err(invalid(
                "snapshot was already sealed; recover the existing intent",
            ));
        }
        self.write_private_file(&destination, &bytes)?;
        catalog.advance(Phase::Captured)?;
        self.save(&catalog)?;
        Ok(point)
    }

    pub fn verify_snapshot(&self, point: &Point) -> Result<SnapshotManifest, StoreError> {
        point.validate()?;
        let mut file = lock_regular_file(&self.snapshot_manifest_path(point.id))?;
        let mut bytes = Vec::new();
        file.by_ref()
            .take(MAX_MANIFEST_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_MANIFEST_BYTES
            || Digest::parse(&format!("{:x}", Sha256::digest(&bytes)))? != point.snapshot_digest
        {
            return Err(invalid("snapshot manifest changed or exceeds its limit"));
        }
        let manifest: SnapshotManifest = serde_json::from_slice(&bytes)?;
        manifest.validate()?;
        if manifest.point(point.snapshot_digest.clone()) != *point
            || manifest.account_sid != current_account_sid()?
        {
            return Err(invalid(
                "snapshot identity, source installation or Windows account changed",
            ));
        }
        self.verify_database(&manifest.database)?;
        let inventory_path = self
            .resource_directory(
                manifest.transaction_id,
                manifest.point_id,
                CaptureSlot::Previous,
            )
            .join("inventory.json");
        let mut file = lock_regular_file(&inventory_path)?;
        let mut bytes = Vec::new();
        file.by_ref()
            .take(16 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > 16 * 1024 * 1024 {
            return Err(invalid("resource inventory exceeds its size limit"));
        }
        let inventory: ResourceInventory = serde_json::from_slice(&bytes)?;
        if inventory.transaction_id != manifest.transaction_id
            || inventory.point_id != manifest.point_id
            || inventory.slot != CaptureSlot::Previous
        {
            return Err(invalid("snapshot inventory identity changed"));
        }
        self.verify_resources(&inventory, &manifest.resource_inventory_sha256)?;
        Ok(manifest)
    }

    fn snapshot_manifest_path(&self, point: Uuid) -> PathBuf {
        self.root
            .join("points")
            .join(point.to_string())
            .join("manifest.json")
    }
}

impl SnapshotManifest {
    fn validate(&self) -> Result<(), StoreError> {
        if self.format_version != 1
            || self.product != "com.ccswitch.desktop"
            || self.architecture != "windows-x86_64"
            || self.installation_scope != "current_user_nsis"
            || self.transaction_id.is_nil()
            || self.point_id.is_nil()
            || self.captured_at_unix_ms == 0
            || !self.installed_version.is_newer_than(&self.source_version)
            || !self.account_sid.starts_with("S-1-")
            || self.database.slot != CaptureSlot::Previous
            || self.database.transaction_id != self.transaction_id
            || self.database.point_id != self.point_id
            || self.source.uses_cached_setup() != self.cached_source_setup.is_some()
        {
            return Err(invalid("invalid or incomplete snapshot manifest binding"));
        }
        validate_ntfs_path(&self.install_directory)?;
        if let Some(cache) = &self.cached_source_setup {
            if cache.relative_path.is_absolute()
                || cache
                    .relative_path
                    .components()
                    .any(|part| !matches!(part, std::path::Component::Normal(_)))
                || cache.bytes == 0
                || cache.release_url != FixedReleaseSetup::setup_url(&self.source_version)
                || cache.signature.len() > 16 * 1024
            {
                return Err(invalid("invalid cached source package binding"));
            }
        }
        Ok(())
    }

    fn point(&self, snapshot_digest: Digest) -> Point {
        Point {
            id: self.point_id,
            transaction_id: self.transaction_id,
            source_version: self.source_version.clone(),
            installed_version: self.installed_version.clone(),
            captured_at_unix_ms: self.captured_at_unix_ms,
            snapshot_digest,
            source_setup_digest: self
                .cached_source_setup
                .as_ref()
                .map(|cache| cache.sha256.clone()),
            source: self.source,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Catalog, PrivateRoot, ResourceKind, ResourceRequest, ResourceRole};
    use std::fs;

    #[test]
    fn complete_candidate_is_bound_before_install_but_not_published_until_health_verification() {
        let temp = tempfile::tempdir().unwrap();
        let install = temp.path().join("installation");
        fs::create_dir(&install).unwrap();
        fs::write(install.join("cc-switch.exe"), b"MZ fixture binary").unwrap();
        let source = temp.path().join("business.db");
        let database = rusqlite::Connection::open(&source).unwrap();
        database
            .execute_batch("PRAGMA user_version=19; CREATE TABLE sentinel(value TEXT);")
            .unwrap();
        drop(database);
        let settings = temp.path().join("settings.json");
        fs::write(&settings, b"{\"fixture\":true}").unwrap();
        let mut lease = PrivateRoot::create_at(temp.path().join("private"))
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
            .capture_database(&source, CaptureSlot::Previous)
            .unwrap();
        let (resources, digest) = lease
            .capture_resources(
                &[ResourceRequest {
                    path: settings,
                    role: ResourceRole::Settings,
                    kind: ResourceKind::File,
                }],
                CaptureSlot::Previous,
            )
            .unwrap();
        let mut foreign = database.clone();
        foreign.point_id = Uuid::new_v4();
        assert!(lease
            .seal_snapshot(&install, &foreign, &resources, &digest, None)
            .is_err());
        assert_eq!(
            lease.load().unwrap().unwrap().journal().unwrap().phase(),
            Phase::Quiescing
        );
        let point = lease
            .seal_snapshot(&install, &database, &resources, &digest, None)
            .unwrap();
        let captured = lease.load().unwrap().unwrap();
        assert!(captured.previous().is_none());
        assert_eq!(captured.journal().unwrap().phase(), Phase::Captured);
        let manifest = lease.verify_snapshot(&point).unwrap();
        assert_eq!(manifest.database.source_path, source);
        assert_eq!(manifest.database.user_version, 19);
        let mut catalog = captured;
        catalog.advance(Phase::Installing).unwrap();
        catalog.advance(Phase::Verifying).unwrap();
        catalog.commit_upgrade(point.clone()).unwrap();
        lease.save(&catalog).unwrap();
        assert_eq!(lease.load().unwrap().unwrap().previous(), Some(&point));
        let path = lease.snapshot_manifest_path(point.id);
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        value["install_directory"] = temp
            .path()
            .join("foreign")
            .to_string_lossy()
            .into_owned()
            .into();
        fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(lease.verify_snapshot(&point).is_err());
    }
}
