use std::fs::File;
use std::io;
use std::os::windows::io::AsRawHandle;
use std::ptr;

use crate::windows_store::invalid;
use crate::StoreError;
use serde::{Deserialize, Serialize};
use windows_sys::Win32::{
    Foundation::LocalFree,
    Security::{
        Authorization::{
            ConvertSecurityDescriptorToStringSecurityDescriptorW,
            ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo, SetSecurityInfo,
            SDDL_REVISION_1, SE_FILE_OBJECT,
        },
        GetSecurityDescriptorControl, GetSecurityDescriptorDacl, DACL_SECURITY_INFORMATION,
        PROTECTED_DACL_SECURITY_INFORMATION, SE_DACL_PROTECTED,
        UNPROTECTED_DACL_SECURITY_INFORMATION,
    },
};

/// Preserve only the original DACL, including its protection bit. Owners and
/// audit ACLs are not changed. Handles refer to the actual file/link, not a
/// symlink target resolved later by a pathname-based security API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileDacl {
    sddl: String,
}

impl FileDacl {
    pub(crate) fn capture(file: &File) -> Result<Self, StoreError> {
        let mut descriptor = ptr::null_mut();
        // SAFETY: file owns a valid handle, only the descriptor output is used.
        let status = unsafe {
            GetSecurityInfo(
                file.as_raw_handle().cast(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                &mut descriptor,
            )
        };
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status as i32).into());
        }
        let descriptor = LocalAllocation(descriptor);
        let mut text = ptr::null_mut();
        let mut count = 0;
        // SAFETY: Windows produced the descriptor. text is a LocalAlloc output.
        if unsafe {
            ConvertSecurityDescriptorToStringSecurityDescriptorW(
                descriptor.0,
                SDDL_REVISION_1,
                DACL_SECURITY_INFORMATION,
                &mut text,
                &mut count,
            )
        } == 0
        {
            return Err(io::Error::last_os_error().into());
        }
        let allocation = LocalAllocation(text.cast());
        if count == 0 || count > 64 * 1024 {
            return Err(invalid("file DACL exceeds its size limit"));
        }
        // SAFETY: Windows returned the allocation's character count. Determine
        // the text length from its NUL terminator inside that bounded buffer.
        let buffer = unsafe { std::slice::from_raw_parts(text, count as usize) };
        let length = buffer
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(buffer.len());
        let units = &buffer[..length];
        let sddl = String::from_utf16(units).map_err(|_| invalid("invalid file DACL text"))?;
        // SetSecurityInfo can add AI (auto-inherited) even to a protected DACL.
        // AI/AR are propagation bookkeeping; retain P and every ACE/ACE flag.
        // Comparing these canonical forms verifies actual ACL and inheritance
        // protection without rejecting a harmless Windows control-bit update.
        let sddl = canonical_dacl(&sddl);
        drop(allocation);
        let result = Self { sddl };
        result.validate()?;
        Ok(result)
    }

    pub(crate) fn validate(&self) -> Result<(), StoreError> {
        self.descriptor().map(|_| ())
    }

    pub(crate) fn apply(&self, file: &File) -> Result<(), StoreError> {
        let descriptor = self.descriptor()?;
        let mut control = 0;
        let mut revision = 0;
        let mut present = 0;
        let mut defaulted = 0;
        let mut dacl = ptr::null_mut();
        // SAFETY: the validated descriptor remains live; outputs are sized.
        if unsafe { GetSecurityDescriptorControl(descriptor.0, &mut control, &mut revision) } == 0
            || unsafe {
                GetSecurityDescriptorDacl(descriptor.0, &mut present, &mut dacl, &mut defaulted)
            } == 0
        {
            return Err(io::Error::last_os_error().into());
        }
        if present == 0 {
            return Err(invalid("file snapshot lacks its DACL"));
        }
        let protection = if control & SE_DACL_PROTECTED != 0 {
            PROTECTED_DACL_SECURITY_INFORMATION
        } else {
            UNPROTECTED_DACL_SECURITY_INFORMATION
        };
        // SAFETY: callers open this exact file with WRITE_DAC, and the DACL
        // belongs to the validated live descriptor. No owner or SACL changes.
        let status = unsafe {
            SetSecurityInfo(
                file.as_raw_handle().cast(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | protection,
                ptr::null_mut(),
                ptr::null_mut(),
                dacl,
                ptr::null_mut(),
            )
        };
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status as i32).into());
        }
        if Self::capture(file)? != *self {
            return Err(invalid("restored file DACL differs from its snapshot"));
        }
        Ok(())
    }

    fn descriptor(&self) -> Result<LocalAllocation, StoreError> {
        if !self.sddl.starts_with("D:") {
            return Err(invalid("snapshot DACL has no D: component"));
        }
        if self.sddl.len() > 64 * 1024 {
            return Err(invalid("snapshot DACL text is too large"));
        }
        if self.sddl.contains('\0') {
            return Err(invalid("snapshot DACL text contains an embedded NUL"));
        }
        if ["O:", "G:", "S:"]
            .iter()
            .any(|prefix| self.sddl.contains(prefix))
        {
            return Err(invalid(
                "snapshot DACL includes an unexpected owner/group/audit component",
            ));
        }
        let text: Vec<u16> = self.sddl.encode_utf16().chain(Some(0)).collect();
        let mut descriptor = ptr::null_mut();
        // SAFETY: terminated SDDL and writable output; Windows validates it.
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                text.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                ptr::null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error().into());
        }
        Ok(LocalAllocation(descriptor))
    }
}

fn canonical_dacl(sddl: &str) -> String {
    let prefix_end = sddl.find('(').unwrap_or(sddl.len());
    let (prefix, aces) = sddl.split_at(prefix_end);
    format!("{}{}", prefix.replace("AI", "").replace("AR", ""), aces)
}

struct LocalAllocation(*mut std::ffi::c_void);
impl Drop for LocalAllocation {
    fn drop(&mut self) {
        // SAFETY: instances exclusively own Windows LocalAlloc outputs.
        unsafe { LocalFree(self.0) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, OpenOptions};
    use std::os::windows::fs::OpenOptionsExt;

    #[test]
    fn protected_and_inherited_dacls_roundtrip_without_changing_owner() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("private-file");
        fs::write(&path, b"private").unwrap();
        let file = OpenOptions::new()
            .read(true)
            .access_mode(0x0002_0000 | 0x0004_0000)
            .open(&path)
            .unwrap(); // READ_CONTROL | WRITE_DAC
        let inherited = FileDacl::capture(&file).unwrap();
        let protected = FileDacl {
            sddl: "D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;WD)".into(),
        };
        protected.apply(&file).unwrap();
        assert_eq!(FileDacl::capture(&file).unwrap(), protected);
        inherited.apply(&file).unwrap();
        assert_eq!(FileDacl::capture(&file).unwrap(), inherited);
    }

    #[test]
    fn missing_owner_or_audit_components_cannot_be_smuggled_into_restore() {
        assert!(FileDacl {
            sddl: "O:WDD:(A;;FA;;;WD)".into()
        }
        .validate()
        .is_err());
        assert!(FileDacl {
            sddl: "S:(AU;SA;FA;;;WD)".into()
        }
        .validate()
        .is_err());
        assert!(FileDacl {
            sddl: "D:malformed".into()
        }
        .validate()
        .is_err());
    }
}
