use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Component, Path, PathBuf, Prefix};
use std::ptr;
use uuid::Uuid;
use windows_sys::Win32::{
    Foundation::{LocalFree, ERROR_SHARING_VIOLATION},
    Security::{
        Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
            SDDL_REVISION_1,
        },
        GetTokenInformation, SetFileSecurityW, TokenUser, DACL_SECURITY_INFORMATION,
        PROTECTED_DACL_SECURITY_INFORMATION, TOKEN_QUERY, TOKEN_USER,
    },
    Storage::FileSystem::{
        GetDriveTypeW, GetFileInformationByHandle, GetVolumeInformationW, MoveFileExW,
        BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT,
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    },
    System::Threading::{GetCurrentProcess, OpenProcessToken},
};

use crate::{Catalog, ProtocolError};

const MAX_CATALOG_BYTES: u64 = 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("Another installation transaction holds the rollback lease")]
    Busy,
    #[error("Rollback storage I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("Rollback storage JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Rollback SQLite operation failed: {0}")]
    Database(#[from] rusqlite::Error),
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
}

/// Fixed per-user rollback storage, independent of the business DB override.
/// No plaintext snapshot or package is written until its DACL is protected.
pub struct PrivateRoot {
    path: PathBuf,
    account_sid: String,
}

impl PrivateRoot {
    pub fn open() -> Result<Self, StoreError> {
        let local = dirs::data_local_dir().ok_or_else(|| invalid("LocalAppData is unavailable"))?;
        Self::create_at(local.join("com.ccswitch.desktop").join("rollback"))
    }

    pub(crate) fn create_at(path: PathBuf) -> Result<Self, StoreError> {
        validate_ntfs_path(&path)?;
        fs::create_dir_all(&path)?;
        validate_ntfs_path(&path)?;
        if !fs::symlink_metadata(&path)?.is_dir() {
            return Err(invalid("rollback root is not a directory"));
        }
        let sid = current_account_sid()?;
        protect_directory(&path, &sid)?;
        Ok(Self {
            path,
            account_sid: sid,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn account_sid(&self) -> &str {
        &self.account_sid
    }

    pub fn try_lease(&self) -> Result<StoreLease, StoreError> {
        validate_local_path(&self.path)?;
        let lock_path = self.path.join("installation.lease");
        validate_regular_file_if_present(&lock_path)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .share_mode(0)
            .open(lock_path)
            .map_err(|error| {
                if error.raw_os_error() == Some(ERROR_SHARING_VIOLATION as i32) {
                    StoreError::Busy
                } else {
                    StoreError::Io(error)
                }
            })?;
        Ok(StoreLease {
            root: self.path.clone(),
            _lock: lock,
        })
    }
}

/// An OS handle, not a PID file: a crashed process automatically releases it.
pub struct StoreLease {
    pub(crate) root: PathBuf,
    _lock: File,
}

impl StoreLease {
    pub fn load(&self) -> Result<Option<Catalog>, StoreError> {
        let path = self.root.join("active.json");
        validate_local_path(&self.root)?;
        validate_regular_file_if_present(&path)?;
        let file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let mut bytes = Vec::new();
        file.take(MAX_CATALOG_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_CATALOG_BYTES {
            return Err(invalid("catalog exceeds its size limit"));
        }
        let catalog: Catalog = serde_json::from_slice(&bytes)?;
        catalog.validate()?;
        Ok(Some(catalog))
    }

    /// Persist intent before each external action. A commit updates the only
    /// active pointer and its journal in the same file, avoiding torn pairs.
    pub fn save(&mut self, catalog: &Catalog) -> Result<(), StoreError> {
        catalog.validate()?;
        validate_local_path(&self.root)?;
        let destination = self.root.join("active.json");
        validate_regular_file_if_present(&destination)?;
        let bytes = serde_json::to_vec_pretty(catalog)?;
        if bytes.len() as u64 > MAX_CATALOG_BYTES {
            return Err(invalid("catalog exceeds its size limit"));
        }
        let temporary = self.root.join(format!("catalog-{}.tmp", Uuid::new_v4()));
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        // Only clean up a file that this writer actually created.
        let cleanup = TemporaryCatalog(temporary.clone());
        file.write_all(&bytes)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        drop(file);
        let source = wide_path(&temporary);
        let target = wide_path(&destination);
        // SAFETY: paths are NUL-terminated and remain alive. Both files are on
        // the same local volume, inside the private root protected by the lease.
        let moved = unsafe {
            MoveFileExW(
                source.as_ptr(),
                target.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        if moved == 0 {
            return Err(io::Error::last_os_error().into());
        }
        drop(cleanup);
        Ok(())
    }
}

struct TemporaryCatalog(PathBuf);
impl Drop for TemporaryCatalog {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

pub(crate) fn invalid(message: &str) -> StoreError {
    ProtocolError::Invalid(message.into()).into()
}
pub(crate) fn wide_path(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

pub(crate) fn validate_local_path(path: &Path) -> Result<(), StoreError> {
    validate_path_syntax(path)?;
    for ancestor in path.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 => {
                return Err(invalid("rollback storage cannot traverse a reparse point"))
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

pub(crate) fn validate_path_syntax(path: &Path) -> Result<(), StoreError> {
    let disk = matches!(path.components().next(), Some(Component::Prefix(prefix))
        if matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_)));
    if !path.is_absolute()
        || !disk
        || path.as_os_str().encode_wide().any(|unit| unit == 0)
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(invalid(
            "rollback storage requires an absolute local disk path",
        ));
    }
    for component in path.components() {
        if let Component::Normal(part) = component {
            let text = part
                .to_str()
                .ok_or_else(|| invalid("path has invalid Unicode"))?;
            let stem = text.split('.').next().unwrap_or("").to_ascii_uppercase();
            let device = matches!(
                stem.as_str(),
                "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$" | "CONIN$" | "CONOUT$"
            ) || ((stem.starts_with("COM") || stem.starts_with("LPT"))
                && stem.len() == 4
                && matches!(stem.as_bytes()[3], b'1'..=b'9'));
            if text.contains(':') || text.ends_with('.') || text.ends_with(' ') || device {
                return Err(invalid(
                    "path uses a device, alternate stream or ambiguous filename",
                ));
            }
        }
    }
    Ok(())
}

/// First release only supports local NTFS volumes. A mapped drive can have a
/// disk-shaped prefix too, so syntax validation alone does not prove locality.
pub(crate) fn validate_ntfs_path(path: &Path) -> Result<(), StoreError> {
    validate_local_path(path)?;
    let root: PathBuf = path.components().take(2).collect();
    let root = wide_path(&root);
    // SAFETY: the drive root is an absolute NUL-terminated path.
    let drive = unsafe { GetDriveTypeW(root.as_ptr()) };
    if drive != 3 {
        return Err(invalid("rollback requires a fixed local disk"));
    }
    let mut filesystem = [0u16; 32];
    // SAFETY: only the filesystem output is requested; its buffer is sized.
    if unsafe {
        GetVolumeInformationW(
            root.as_ptr(),
            ptr::null_mut(),
            0,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            filesystem.as_mut_ptr(),
            filesystem.len() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error().into());
    }
    let length = filesystem.iter().position(|unit| *unit == 0).unwrap_or(32);
    if String::from_utf16_lossy(&filesystem[..length]) != "NTFS" {
        return Err(invalid(
            "rollback requires NTFS until other filesystems are tested",
        ));
    }
    Ok(())
}

/// Deny both writes and deletion for the lifetime of this handle. If another
/// writer is already open Windows rejects the capture instead of racing it.
pub(crate) fn lock_regular_file(path: &Path) -> Result<File, StoreError> {
    validate_ntfs_path(path)?;
    validate_regular_file_if_present(path)?;
    let file = OpenOptions::new()
        .read(true)
        .share_mode(1)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: file owns a live handle and information is a writable structure.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle().cast(), &mut information) } == 0 {
        return Err(io::Error::last_os_error().into());
    }
    if information.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || information.nNumberOfLinks != 1
        || !file.metadata()?.is_file()
    {
        return Err(invalid(
            "capture requires an ordinary file with a single hard link",
        ));
    }
    Ok(file)
}

pub(crate) fn validate_regular_file_if_present(path: &Path) -> Result<(), StoreError> {
    match fs::symlink_metadata(path) {
        Ok(metadata)
            if !metadata.is_file()
                || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 =>
        {
            Err(invalid("rollback metadata must be a regular file"))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn current_account_sid() -> Result<String, StoreError> {
    let mut token = ptr::null_mut();
    // SAFETY: a valid process pseudo-handle and writable handle output pointer.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error().into());
    }
    // SAFETY: OpenProcessToken returned an owned handle, closed on every path.
    let _token = unsafe { OwnedHandle::from_raw_handle(token.cast()) };
    let mut needed = 0;
    // SAFETY: the first call asks for the required buffer size without writing.
    unsafe { GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut needed) };
    if needed == 0 || needed > 8192 {
        return Err(invalid("invalid account identity buffer"));
    }
    // usize storage provides the alignment required by TOKEN_USER and SID.
    let mut storage = vec![0usize; (needed as usize).div_ceil(std::mem::size_of::<usize>())];
    // SAFETY: the aligned buffer is at least `needed` bytes and remains alive.
    if unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            storage.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    } == 0
    {
        return Err(io::Error::last_os_error().into());
    }
    // SAFETY: GetTokenInformation returned a TOKEN_USER in the aligned buffer.
    let user = unsafe { &*storage.as_ptr().cast::<TOKEN_USER>() };
    let mut sid = ptr::null_mut();
    // SAFETY: User.Sid points inside the live token buffer; sid is an output.
    if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut sid) } == 0 {
        return Err(io::Error::last_os_error().into());
    }
    let mut length = 0;
    // SAFETY: Windows returned a NUL-terminated SID string. Valid SID strings
    // are short; the bound rejects unexpected results instead of scanning forever.
    while length < 256 && unsafe { *sid.add(length) } != 0 {
        length += 1;
    }
    let result = if length < 256 {
        // SAFETY: the preceding scan found the terminator within the allocation.
        String::from_utf16(unsafe { std::slice::from_raw_parts(sid, length) })
            .map_err(|_| invalid("invalid account SID encoding"))
    } else {
        Err(invalid("account SID exceeds its size limit"))
    };
    // SAFETY: ConvertSidToStringSidW allocates with LocalAlloc.
    unsafe { LocalFree(sid.cast()) };
    result
}

fn protect_directory(path: &Path, sid: &str) -> Result<(), StoreError> {
    let sddl: Vec<u16> = format!("D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;{sid})")
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let mut descriptor = ptr::null_mut();
    // SAFETY: fixed SDDL syntax with a SID returned by Windows, terminated text,
    // and writable descriptor pointer. Windows validates the descriptor.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            ptr::null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error().into());
    }
    let path = wide_path(path);
    // SAFETY: the descriptor and NUL-terminated local path are live. Only the
    // app-owned root DACL changes; owner and audit ACL are left intact.
    let changed = unsafe {
        SetFileSecurityW(
            path.as_ptr(),
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            descriptor,
        )
    };
    let result = if changed == 0 {
        Err(io::Error::last_os_error().into())
    } else {
        Ok(())
    };
    // SAFETY: the converted descriptor was allocated by LocalAlloc.
    unsafe { LocalFree(descriptor.cast()) };
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ForkVersion;

    fn test_root(temp: &tempfile::TempDir) -> PrivateRoot {
        PrivateRoot::create_at(temp.path().join("中文 单旧版 快照")).unwrap()
    }

    #[test]
    fn private_catalog_roundtrips_without_a_business_database_or_gui() {
        let temp = tempfile::tempdir().unwrap();
        let root = test_root(&temp);
        assert!(root.account_sid().starts_with("S-1-"));
        let catalog = Catalog::new(ForkVersion::parse("3.20.4-fork.3").unwrap());
        let mut lease = root.try_lease().unwrap();
        assert!(lease.load().unwrap().is_none());
        lease.save(&catalog).unwrap();
        drop(lease);
        assert_eq!(root.try_lease().unwrap().load().unwrap(), Some(catalog));
    }

    #[test]
    fn another_writer_is_blocked_until_the_os_handle_is_released() {
        let temp = tempfile::tempdir().unwrap();
        let root = test_root(&temp);
        let lease = root.try_lease().unwrap();
        assert!(matches!(root.try_lease(), Err(StoreError::Busy)));
        drop(lease);
        assert!(root.try_lease().is_ok());
    }

    #[test]
    fn blocked_atomic_replace_keeps_the_old_catalog_and_removes_staging() {
        let temp = tempfile::tempdir().unwrap();
        let root = test_root(&temp);
        let original = Catalog::new(ForkVersion::parse("3.20.4-fork.3").unwrap());
        let changed = Catalog::new(ForkVersion::parse("3.20.4-fork.4").unwrap());
        let mut lease = root.try_lease().unwrap();
        lease.save(&original).unwrap();
        let blocker = OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(root.path().join("active.json"))
            .unwrap();
        assert!(lease.save(&changed).is_err());
        drop(blocker);
        assert_eq!(lease.load().unwrap(), Some(original));
        assert!(!fs::read_dir(root.path()).unwrap().any(|entry| entry
            .unwrap()
            .path()
            .extension()
            .is_some_and(|ext| ext == "tmp")));
    }

    #[test]
    fn a_linked_root_is_rejected_without_changing_its_target() {
        let temp = tempfile::tempdir().unwrap();
        let foreign = temp.path().join("external");
        fs::create_dir(&foreign).unwrap();
        fs::write(foreign.join("keep.txt"), b"unmanaged").unwrap();
        let link = temp.path().join("linked-rollback");
        std::os::windows::fs::symlink_dir(&foreign, &link).unwrap();
        assert!(PrivateRoot::create_at(link).is_err());
        assert_eq!(fs::read(foreign.join("keep.txt")).unwrap(), b"unmanaged");
        assert_eq!(fs::read_dir(&foreign).unwrap().count(), 1);
    }

    #[test]
    fn network_devices_streams_and_ambiguous_names_are_rejected() {
        for path in [
            r"\\server\share\rollback",
            r"C:\data\auth.json:secret",
            r"C:\data\NUL.json",
            r"C:\data\settings.json.",
            r"C:\data\settings.json ",
            r"C:\data\..\elsewhere",
        ] {
            assert!(validate_path_syntax(Path::new(path)).is_err(), "{path}");
        }
    }
}
