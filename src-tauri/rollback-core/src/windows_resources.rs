use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use uuid::Uuid;
use windows_sys::Win32::Storage::FileSystem::{
    FileAttributeTagInfo, GetFileInformationByHandleEx, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT,
};
use windows_sys::Win32::System::SystemServices::IO_REPARSE_TAG_SYMLINK;

use crate::windows_database::{hash_reader, require_free_space};
use crate::windows_store::{invalid, lock_regular_file, validate_ntfs_path, validate_path_syntax};
use crate::{CaptureSlot, Digest, Direction, FileDacl, Phase, StoreError, StoreLease};

const MAX_RESOURCES: usize = 10_000;
const MAX_FILE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_INVENTORY_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceRole {
    Settings,
    AppPaths,
    Provider,
    Mcp,
    Prompt,
    Skill,
    ManagedAuthMarker,
    PortableMarker,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceKind {
    File,
    ManagedTree,
}

/// Produced only by the helper's audited resource resolver, never by a path
/// supplied over frontend IPC. ManagedTree is for a DB-owned skill deployment,
/// not an entire client home or the generic .agents/.codex/.claude directory.
#[derive(Debug, Clone)]
pub struct ResourceRequest {
    pub path: PathBuf,
    pub role: ResourceRole,
    pub kind: ResourceKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum ResourceState {
    Missing {
        kind: ResourceKind,
    },
    File {
        material_id: Uuid,
        bytes: u64,
        sha256: Digest,
        dacl: FileDacl,
        readonly: bool,
    },
    Directory {
        dacl: FileDacl,
    },
    Symlink {
        target: PathBuf,
        directory: bool,
        dacl: FileDacl,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotResource {
    pub path: PathBuf,
    pub role: ResourceRole,
    pub state: ResourceState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceInventory {
    format_version: u32,
    pub transaction_id: Uuid,
    pub point_id: Uuid,
    pub slot: CaptureSlot,
    pub resources: Vec<SnapshotResource>,
}

impl StoreLease {
    pub fn capture_resources(
        &mut self,
        requests: &[ResourceRequest],
        slot: CaptureSlot,
    ) -> Result<(ResourceInventory, Digest), StoreError> {
        let catalog = self
            .load()?
            .ok_or_else(|| invalid("resource capture has no catalog"))?;
        let journal = catalog
            .journal()
            .ok_or_else(|| invalid("resource capture has no transaction"))?;
        if journal.phase() != Phase::Quiescing
            || (slot == CaptureSlot::Previous && journal.direction() != Direction::Upgrade)
        {
            return Err(crate::ProtocolError::Phase.into());
        }
        if requests.is_empty() || requests.len() > MAX_RESOURCES {
            return Err(invalid("empty or oversized resource plan"));
        }
        let directory = self.resource_directory(journal.id(), journal.point_id(), slot);
        validate_ntfs_path(&directory)?;
        fs::create_dir_all(directory.parent().unwrap())?;
        validate_ntfs_path(directory.parent().unwrap())?;
        fs::create_dir(&directory)?;
        let mut cleanup = CaptureCleanup {
            path: directory.clone(),
            completed: false,
        };
        let mut capture = ResourceCapture {
            directory: &directory,
            private_root: &self.root,
            inventory: ResourceInventory {
                format_version: 1,
                transaction_id: journal.id(),
                point_id: journal.point_id(),
                slot,
                resources: Vec::new(),
            },
            paths: BTreeSet::new(),
            guards: Vec::new(),
            directories: Vec::new(),
        };
        // Reject overlaps between requests. Descendants of an explicitly
        // DB-owned tree are enumerated by the capture, not added independently.
        let normalized: Vec<_> = requests
            .iter()
            .map(|request| normalized_path(&request.path))
            .collect::<Result<_, _>>()?;
        for (index, path) in normalized.iter().enumerate() {
            if path_is_within(path, &normalized_path(&self.root)?) {
                return Err(invalid("resource plan overlaps private rollback storage"));
            }
            for (other_index, other) in normalized.iter().enumerate() {
                if index != other_index && path_is_within(path, other) {
                    return Err(invalid("resource plan has duplicate or overlapping roots"));
                }
            }
        }
        for request in requests {
            capture.capture_path(&request.path, request.role, request.kind, 0)?;
        }
        capture.check_stable()?;
        let bytes = serde_json::to_vec_pretty(&capture.inventory)?;
        if bytes.len() as u64 > MAX_INVENTORY_BYTES {
            return Err(invalid("resource inventory is too large"));
        }
        let digest = Digest::parse(&format!("{:x}", Sha256::digest(&bytes)))?;
        let mut output = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(directory.join("inventory.json"))?;
        output.write_all(&bytes)?;
        output.sync_all()?;
        cleanup.completed = true;
        Ok((capture.inventory, digest))
    }

    pub fn verify_resources(
        &self,
        inventory: &ResourceInventory,
        digest: &Digest,
    ) -> Result<(), StoreError> {
        inventory.validate()?;
        let directory =
            self.resource_directory(inventory.transaction_id, inventory.point_id, inventory.slot);
        validate_ntfs_path(&directory)?;
        let mut guard = lock_regular_file(&directory.join("inventory.json"))?;
        if guard.metadata()?.len() > MAX_INVENTORY_BYTES || hash_reader(&mut guard)? != *digest {
            return Err(invalid("resource inventory changed"));
        }
        // Compare canonical serialized metadata too: passing a modified in-memory
        // path or DACL alongside the old digest must not authorize a restore.
        let bytes = serde_json::to_vec_pretty(inventory)?;
        if Digest::parse(&format!("{:x}", Sha256::digest(&bytes)))? != *digest {
            return Err(invalid("resource selection differs from captured metadata"));
        }
        for resource in &inventory.resources {
            if let ResourceState::File {
                material_id,
                bytes,
                sha256,
                ..
            } = &resource.state
            {
                let mut file = lock_regular_file(&directory.join(format!("{material_id}.bin")))?;
                if file.metadata()?.len() != *bytes || hash_reader(&mut file)? != *sha256 {
                    return Err(invalid(
                        "resource snapshot content changed or was truncated",
                    ));
                }
            }
        }
        Ok(())
    }

    pub(crate) fn resource_directory(
        &self,
        transaction: Uuid,
        point: Uuid,
        slot: CaptureSlot,
    ) -> PathBuf {
        match slot {
            CaptureSlot::Previous => self
                .root
                .join("points")
                .join(point.to_string())
                .join("resources"),
            CaptureSlot::Rescue => self
                .root
                .join("transactions")
                .join(transaction.to_string())
                .join("rescue")
                .join("resources"),
        }
    }
}

impl ResourceInventory {
    pub(crate) fn validate(&self) -> Result<(), StoreError> {
        if self.format_version != 1
            || self.transaction_id.is_nil()
            || self.point_id.is_nil()
            || self.resources.is_empty()
            || self.resources.len() > MAX_RESOURCES
        {
            return Err(invalid(
                "invalid resource inventory identity, format or size",
            ));
        }
        let mut paths = BTreeSet::new();
        let mut materials = BTreeSet::new();
        for resource in &self.resources {
            let path = normalized_path(&resource.path)?;
            if !paths.insert(path) {
                return Err(invalid("resource inventory contains a duplicate path"));
            }
            match &resource.state {
                ResourceState::Missing { .. } => {}
                ResourceState::File {
                    material_id,
                    bytes,
                    dacl,
                    ..
                } => {
                    if material_id.is_nil()
                        || !materials.insert(*material_id)
                        || *bytes > MAX_FILE_BYTES
                    {
                        return Err(invalid("invalid resource material identity or size"));
                    }
                    dacl.validate()?;
                }
                ResourceState::Directory { dacl } => dacl.validate()?,
                ResourceState::Symlink { target, dacl, .. } => {
                    validate_path_syntax(target)?;
                    dacl.validate()?;
                }
            }
        }
        Ok(())
    }
}

struct ResourceCapture<'a> {
    directory: &'a Path,
    private_root: &'a Path,
    inventory: ResourceInventory,
    paths: BTreeSet<String>,
    guards: Vec<File>,
    directories: Vec<(PathBuf, BTreeSet<PathBuf>)>,
}

impl ResourceCapture<'_> {
    fn capture_path(
        &mut self,
        path: &Path,
        role: ResourceRole,
        kind: ResourceKind,
        depth: usize,
    ) -> Result<(), StoreError> {
        if depth > 64 {
            return Err(invalid("managed resource nesting limit exceeded"));
        }
        if self.inventory.resources.len() >= MAX_RESOURCES {
            return Err(invalid("resource count limit exceeded"));
        }
        validate_path_syntax(path)?;
        validate_ntfs_path(
            path.parent()
                .ok_or_else(|| invalid("resource must not be a volume root"))?,
        )?;
        let mut ancestor = path.parent().unwrap();
        while !ancestor.exists() {
            ancestor = ancestor
                .parent()
                .ok_or_else(|| invalid("resource parent is missing"))?;
        }
        self.guards.push(open_metadata_handle(ancestor)?);
        let normalized = normalized_path(path)?;
        if path_is_within(&normalized, &normalized_path(self.private_root)?)
            || !self.paths.insert(normalized)
        {
            return Err(invalid(
                "resource aliases another capture or private storage",
            ));
        }
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.inventory.resources.push(SnapshotResource {
                    path: path.into(),
                    role,
                    state: ResourceState::Missing { kind },
                });
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        };
        let state = if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            let file = open_metadata_handle(path)?;
            let mut tag = FILE_ATTRIBUTE_TAG_INFO::default();
            // SAFETY: a live metadata handle and a correctly sized output.
            if unsafe {
                GetFileInformationByHandleEx(
                    file.as_raw_handle().cast(),
                    FileAttributeTagInfo,
                    (&mut tag as *mut FILE_ATTRIBUTE_TAG_INFO).cast(),
                    std::mem::size_of_val(&tag) as u32,
                )
            } == 0
            {
                return Err(io::Error::last_os_error().into());
            }
            if tag.ReparseTag != IO_REPARSE_TAG_SYMLINK {
                return Err(invalid("managed resource has an unsupported reparse tag"));
            }
            let target = fs::read_link(path)?;
            let absolute = if target.is_absolute() {
                target
            } else {
                path.parent().unwrap().join(target)
            };
            // Relative '..' or a linked/network target is unsupported in v1.
            validate_ntfs_path(&absolute)?;
            if path_is_within(
                &normalized_path(&absolute)?,
                &normalized_path(self.private_root)?,
            ) {
                return Err(invalid(
                    "resource link points into private rollback storage",
                ));
            }
            if (tag.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0)
                != (kind == ResourceKind::ManagedTree)
            {
                return Err(invalid("resource link type does not match its resolver"));
            }
            let state = ResourceState::Symlink {
                target: absolute,
                directory: tag.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0,
                dacl: FileDacl::capture(&file)?,
            };
            self.guards.push(file);
            state
        } else if metadata.is_dir() {
            if kind != ResourceKind::ManagedTree || role != ResourceRole::Skill {
                return Err(invalid("directory capture requires a DB-owned Skill tree"));
            }
            let file = open_metadata_handle(path)?;
            let state = ResourceState::Directory {
                dacl: FileDacl::capture(&file)?,
            };
            self.guards.push(file);
            let children = read_children(path)?;
            self.directories.push((path.into(), children.clone()));
            self.inventory.resources.push(SnapshotResource {
                path: path.into(),
                role,
                state,
            });
            for child in children {
                let child_metadata = fs::symlink_metadata(&child)?;
                let child_kind = if child_metadata.is_dir() {
                    ResourceKind::ManagedTree
                } else {
                    ResourceKind::File
                };
                self.capture_path(&child, role, child_kind, depth + 1)?;
            }
            return Ok(());
        } else if metadata.is_file() {
            if kind == ResourceKind::ManagedTree {
                return Err(invalid("managed Skill tree became a file"));
            }
            let mut input = lock_regular_file(path)?;
            let before = input.metadata()?;
            if before.len() > MAX_FILE_BYTES {
                return Err(invalid("managed resource is too large"));
            }
            let dacl = FileDacl::capture(&input)?;
            require_free_space(
                self.directory,
                before
                    .len()
                    .checked_add(64 * 1024 * 1024)
                    .ok_or_else(|| invalid("resource size overflow"))?,
            )?;
            let material_id = Uuid::new_v4();
            let mut output = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(self.directory.join(format!("{material_id}.bin")))?;
            let mut hash = Sha256::new();
            let mut buffer = [0u8; 64 * 1024];
            let mut bytes = 0;
            loop {
                let count = input.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                bytes += count as u64;
                if bytes > before.len() {
                    return Err(invalid("resource grew during capture"));
                }
                hash.update(&buffer[..count]);
                output.write_all(&buffer[..count])?;
            }
            output.sync_all()?;
            let after = input.metadata()?;
            if before.len() != bytes
                || before.last_write_time() != after.last_write_time()
                || before.file_attributes() != after.file_attributes()
                || FileDacl::capture(&input)? != dacl
            {
                return Err(invalid(
                    "resource content or permissions changed during capture",
                ));
            }
            let state = ResourceState::File {
                material_id,
                bytes,
                sha256: Digest::parse(&format!("{:x}", hash.finalize()))?,
                dacl,
                readonly: before.permissions().readonly(),
            };
            self.guards.push(input);
            state
        } else {
            return Err(invalid("unsupported managed resource type"));
        };
        self.inventory.resources.push(SnapshotResource {
            path: path.into(),
            role,
            state,
        });
        Ok(())
    }

    fn check_stable(&self) -> Result<(), StoreError> {
        self.inventory.validate()?;
        for resource in &self.inventory.resources {
            match &resource.state {
                ResourceState::Missing { .. } => match fs::symlink_metadata(&resource.path) {
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                    Ok(_) => return Err(invalid("an absent resource appeared during capture")),
                },
                ResourceState::File {
                    bytes,
                    sha256,
                    dacl,
                    readonly,
                    ..
                } => {
                    let mut file = lock_regular_file(&resource.path)?;
                    if file.metadata()?.len() != *bytes
                        || hash_reader(&mut file)? != *sha256
                        || FileDacl::capture(&file)? != *dacl
                        || file.metadata()?.permissions().readonly() != *readonly
                    {
                        return Err(invalid("resource changed before snapshot completion"));
                    }
                }
                ResourceState::Directory { dacl } => {
                    if FileDacl::capture(&open_metadata_handle(&resource.path)?)? != *dacl {
                        return Err(invalid(
                            "resource directory permissions changed during capture",
                        ));
                    }
                }
                ResourceState::Symlink { target, dacl, .. } => {
                    let now = fs::read_link(&resource.path)?;
                    let now = if now.is_absolute() {
                        now
                    } else {
                        resource.path.parent().unwrap().join(now)
                    };
                    if normalized_path(&now)? != normalized_path(target)?
                        || FileDacl::capture(&open_metadata_handle(&resource.path)?)? != *dacl
                    {
                        return Err(invalid("resource link changed during capture"));
                    }
                }
            }
        }
        for (path, expected) in &self.directories {
            if &read_children(path)? != expected {
                return Err(invalid("managed Skill tree changed during capture"));
            }
        }
        Ok(())
    }
}

fn open_metadata_handle(path: &Path) -> Result<File, StoreError> {
    Ok(OpenOptions::new()
        .read(true)
        .access_mode(0x0002_0080) // READ_CONTROL | FILE_READ_ATTRIBUTES
        .share_mode(1 | 2)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?)
}

fn read_children(path: &Path) -> Result<BTreeSet<PathBuf>, StoreError> {
    let mut children = BTreeSet::new();
    for entry in fs::read_dir(path)? {
        children.insert(entry?.path());
        if children.len() > MAX_RESOURCES {
            return Err(invalid("managed directory exceeds resource count limit"));
        }
    }
    Ok(children)
}

pub(crate) fn normalized_path(path: &Path) -> Result<String, StoreError> {
    validate_path_syntax(path)?;
    // Canonicalize the nearest existing ancestor, resolving short-name aliases
    // without following the final resource (which may itself be a soft link).
    let mut ancestor = path
        .parent()
        .ok_or_else(|| invalid("resource has no parent"))?;
    let mut suffix = vec![path
        .file_name()
        .ok_or_else(|| invalid("resource has no filename"))?
        .to_owned()];
    while !ancestor.exists() {
        suffix.push(
            ancestor
                .file_name()
                .ok_or_else(|| invalid("resource ancestor is missing"))?
                .to_owned(),
        );
        ancestor = ancestor
            .parent()
            .ok_or_else(|| invalid("resource ancestor is missing"))?;
    }
    validate_ntfs_path(ancestor)?;
    let mut resolved = fs::canonicalize(ancestor)?;
    for part in suffix.into_iter().rev() {
        resolved.push(part);
    }
    Ok(resolved.to_string_lossy().replace('/', "\\").to_lowercase())
}

pub(crate) fn path_is_within(path: &str, root: &str) -> bool {
    path == root
        || path
            .strip_prefix(root)
            .is_some_and(|suffix| suffix.starts_with('\\'))
}

struct CaptureCleanup {
    path: PathBuf,
    completed: bool,
}
impl Drop for CaptureCleanup {
    fn drop(&mut self) {
        if !self.completed && validate_ntfs_path(&self.path).is_ok() {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Catalog, ForkVersion, InstallSource, PrivateRoot};

    fn lease(temp: &Path) -> StoreLease {
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
        lease
    }

    #[test]
    fn raw_auth_bytes_absence_and_readonly_state_are_preserved_without_scanning_client_home() {
        let temp = tempfile::tempdir().unwrap();
        let client = temp.path().join(".codex");
        fs::create_dir(&client).unwrap();
        let auth = client.join("auth.json");
        fs::write(&auth, b"{\"TOKEN\":\"fixture\"}\r\n").unwrap();
        let mut permissions = fs::metadata(&auth).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&auth, permissions).unwrap();
        fs::write(client.join("session-unmanaged.json"), b"user session").unwrap();
        let mut store = lease(temp.path());
        let requests = [
            ResourceRequest {
                path: auth.clone(),
                role: ResourceRole::Provider,
                kind: ResourceKind::File,
            },
            ResourceRequest {
                path: client.join("missing.toml"),
                role: ResourceRole::Provider,
                kind: ResourceKind::File,
            },
        ];
        let (inventory, digest) = store
            .capture_resources(&requests, CaptureSlot::Previous)
            .unwrap();
        assert_eq!(inventory.resources.len(), 2);
        store.verify_resources(&inventory, &digest).unwrap();
        assert!(matches!(
            inventory.resources[0].state,
            ResourceState::File { readonly: true, .. }
        ));
        assert!(matches!(
            inventory.resources[1].state,
            ResourceState::Missing { .. }
        ));
        let ResourceState::File { material_id, .. } = inventory.resources[0].state else {
            panic!("not a file")
        };
        assert_eq!(
            fs::read(
                store
                    .resource_directory(
                        inventory.transaction_id,
                        inventory.point_id,
                        inventory.slot
                    )
                    .join(format!("{material_id}.bin"))
            )
            .unwrap(),
            b"{\"TOKEN\":\"fixture\"}\r\n"
        );
        let mut permissions = fs::metadata(&auth).unwrap().permissions();
        permissions.set_readonly(false);
        fs::set_permissions(auth, permissions).unwrap();
    }

    #[test]
    fn skill_symlink_is_recorded_without_copying_or_modifying_its_target() {
        let temp = tempfile::tempdir().unwrap();
        let external = temp.path().join("skill-source");
        fs::create_dir(&external).unwrap();
        fs::write(external.join("SKILL.md"), b"source").unwrap();
        let link = temp.path().join("skill-deployment");
        std::os::windows::fs::symlink_dir(&external, &link).unwrap();
        let mut store = lease(temp.path());
        let (inventory, digest) = store
            .capture_resources(
                &[ResourceRequest {
                    path: link,
                    role: ResourceRole::Skill,
                    kind: ResourceKind::ManagedTree,
                }],
                CaptureSlot::Previous,
            )
            .unwrap();
        assert_eq!(inventory.resources.len(), 1);
        store.verify_resources(&inventory, &digest).unwrap();
        assert!(matches!(
            inventory.resources[0].state,
            ResourceState::Symlink {
                directory: true,
                ..
            }
        ));
        assert_eq!(fs::read(external.join("SKILL.md")).unwrap(), b"source");
    }

    #[test]
    fn overlapping_roots_and_private_storage_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = lease(temp.path());
        let tree = temp.path().join("managed");
        fs::create_dir(&tree).unwrap();
        let request = ResourceRequest {
            path: tree.clone(),
            role: ResourceRole::Skill,
            kind: ResourceKind::ManagedTree,
        };
        let duplicate = ResourceRequest {
            path: tree.join("SKILL.md"),
            role: ResourceRole::Skill,
            kind: ResourceKind::File,
        };
        assert!(store
            .capture_resources(&[request, duplicate], CaptureSlot::Previous)
            .is_err());
        let private = ResourceRequest {
            path: store.root.join("active.json"),
            role: ResourceRole::Provider,
            kind: ResourceKind::File,
        };
        assert!(store
            .capture_resources(&[private], CaptureSlot::Previous)
            .is_err());
    }

    #[test]
    fn inventory_path_substitution_and_truncated_material_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("settings.json");
        fs::write(&source, b"{}").unwrap();
        let mut store = lease(temp.path());
        let (mut inventory, digest) = store
            .capture_resources(
                &[ResourceRequest {
                    path: source,
                    role: ResourceRole::Settings,
                    kind: ResourceKind::File,
                }],
                CaptureSlot::Previous,
            )
            .unwrap();
        let original = inventory.clone();
        inventory.resources[0].path = temp.path().join("foreign.json");
        assert!(store.verify_resources(&inventory, &digest).is_err());
        let ResourceState::File { material_id, .. } = original.resources[0].state else {
            panic!("not a file")
        };
        fs::write(
            store
                .resource_directory(original.transaction_id, original.point_id, original.slot)
                .join(format!("{material_id}.bin")),
            b"x",
        )
        .unwrap();
        assert!(store.verify_resources(&original, &digest).is_err());
    }

    #[test]
    fn only_the_db_owned_skill_tree_is_captured_including_nested_contents() {
        let temp = tempfile::tempdir().unwrap();
        let managed = temp.path().join("managed-skill");
        fs::create_dir(&managed).unwrap();
        fs::create_dir(managed.join("references")).unwrap();
        fs::write(managed.join("SKILL.md"), b"fixture instructions").unwrap();
        fs::write(
            managed.join("references").join("notes.md"),
            b"fixture notes",
        )
        .unwrap();
        fs::create_dir(temp.path().join("unmanaged-sibling")).unwrap();
        fs::write(
            temp.path().join("unmanaged-sibling").join("keep.md"),
            b"user-owned",
        )
        .unwrap();
        let mut store = lease(temp.path());
        let (inventory, digest) = store
            .capture_resources(
                &[ResourceRequest {
                    path: managed.clone(),
                    role: ResourceRole::Skill,
                    kind: ResourceKind::ManagedTree,
                }],
                CaptureSlot::Previous,
            )
            .unwrap();
        assert_eq!(inventory.resources.len(), 4);
        assert!(inventory
            .resources
            .iter()
            .all(|resource| resource.path.starts_with(&managed)));
        store.verify_resources(&inventory, &digest).unwrap();
        assert_eq!(
            fs::read(temp.path().join("unmanaged-sibling").join("keep.md")).unwrap(),
            b"user-owned"
        );
    }
}
