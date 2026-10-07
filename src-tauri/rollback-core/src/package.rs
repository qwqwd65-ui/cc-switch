use std::collections::BTreeMap;
use std::io::{self, Read};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use minisign_verify::{PublicKey, Signature};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};

use crate::{Digest, ForkVersion, ProtocolError};

/// Identical to the fork's updater configuration, checked again in GA. Do not
/// accept a public key supplied by a downloaded manifest or a frontend IPC.
pub const UPDATER_PUBLIC_KEY: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IDhCRTVERjhCNDdDODI0MEUKUldRT0pNaEhpOS9saXpvZmI4Q0xzWFpiV21ZNVc5YUxPMHNlQ1pkL0JFN3d1WlVER05rRFNJcW8K";
const RELEASE_ROOT: &str = "https://github.com/qwqwd65-ui/cc-switch/releases/download";
const MAX_MANIFEST: usize = 1024 * 1024;
const MAX_SETUP: u64 = 256 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum PackageError {
    #[error("Setup I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("Release manifest JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Setup signature verification failed: {0}")]
    Signature(#[from] minisign_verify::Error),
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
}

#[derive(Deserialize)]
struct UpdateManifest {
    version: ForkVersion,
    platforms: BTreeMap<String, PlatformSetup>,
}

#[derive(Deserialize)]
struct PlatformSetup {
    url: String,
    signature: String,
}

/// Fixed target selection. Both A downloads and B cache rechecks use this
/// exact target; corrupt B material must never fall back to 'latest'.
#[derive(Debug, Clone)]
pub struct FixedReleaseSetup {
    version: ForkVersion,
    url: String,
    signature: String,
}

impl FixedReleaseSetup {
    pub fn manifest_url(target: &ForkVersion) -> String {
        format!("{RELEASE_ROOT}/v{}/latest.json", target.as_string())
    }

    pub fn setup_url(target: &ForkVersion) -> String {
        let version = target.as_string();
        format!("{RELEASE_ROOT}/v{version}/CC-Switch-v{version}-Windows-Setup.exe")
    }

    pub fn from_manifest(target: &ForkVersion, bytes: &[u8]) -> Result<Self, PackageError> {
        if bytes.len() > MAX_MANIFEST {
            return Err(invalid("release manifest exceeds its size limit"));
        }
        let manifest: UpdateManifest = serde_json::from_slice(bytes)?;
        if &manifest.version != target {
            return Err(invalid(
                "fixed release manifest has the wrong target version",
            ));
        }
        let platform = manifest
            .platforms
            .get("windows-x86_64")
            .ok_or_else(|| invalid("fixed release has no Windows x64 Setup"))?;
        let url = Self::setup_url(target);
        if platform.url != url {
            return Err(invalid(
                "Setup URL does not belong to the exact fork Release target",
            ));
        }
        if platform.signature.len() > 16 * 1024 {
            return Err(invalid("Setup signature exceeds its size limit"));
        }
        // Validate the Tauri wrapper now; trust is established only by verify.
        decode_signature(&platform.signature)?;
        Ok(Self {
            version: target.clone(),
            url,
            signature: platform.signature.clone(),
        })
    }

    pub fn version(&self) -> &ForkVersion {
        &self.version
    }
    pub fn url(&self) -> &str {
        &self.url
    }
    pub fn signature(&self) -> &str {
        &self.signature
    }

    /// Stream every cached/downloaded byte through the standard minisign
    /// library AND SHA-256. Always call this again immediately before install.
    /// Returned evidence cannot be deserialized from unverified JSON.
    pub fn verify(
        &self,
        reader: &mut impl Read,
        expected_digest: Option<&Digest>,
    ) -> Result<VerifiedSetup, PackageError> {
        let key_text = decode_wrapper(UPDATER_PUBLIC_KEY)?;
        let key = PublicKey::decode(&key_text)?;
        let signature = decode_signature(&self.signature)?;
        let mut verifier = key.verify_stream(&signature)?;
        let mut hasher = Sha256::new();
        let mut bytes = 0u64;
        let mut header = Vec::with_capacity(2);
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let count = reader.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            bytes = bytes
                .checked_add(count as u64)
                .ok_or_else(|| invalid("Setup size overflow"))?;
            if bytes > MAX_SETUP {
                return Err(invalid("Setup exceeds its size limit"));
            }
            for byte in buffer[..count].iter().take(2 - header.len()) {
                header.push(*byte);
            }
            verifier.update(&buffer[..count]);
            hasher.update(&buffer[..count]);
        }
        verifier.finalize()?;
        if header != b"MZ" {
            return Err(invalid("signed asset is not a Windows executable"));
        }
        let digest = Digest::parse(&format!("{:x}", hasher.finalize()))?;
        if expected_digest.is_some_and(|expected| expected != &digest) {
            return Err(invalid("cached Setup differs from its bound SHA-256"));
        }
        // The global signature also authenticates this comment. Historical
        // fork.3 includes the exact application version and x64 Setup filename.
        let version = self.version.as_string();
        let filename = format!("CC Switch_{version}_x64-setup.exe");
        let versions: Vec<_> = signature
            .trusted_comment()
            .split('\t')
            .filter_map(|field| field.strip_prefix("version:"))
            .collect();
        let files: Vec<_> = signature
            .trusted_comment()
            .split('\t')
            .filter_map(|field| field.strip_prefix("file:"))
            .collect();
        if versions != [version.as_str()] || files != [filename.as_str()] {
            return Err(invalid(
                "authenticated Setup version or architecture does not match the target",
            ));
        }
        Ok(VerifiedSetup {
            version: self.version.clone(),
            sha256: digest,
            bytes,
            public_key_sha256: Digest::parse(&format!(
                "{:x}",
                Sha256::digest(key_text.as_bytes())
            ))?,
        })
    }
}

#[derive(Debug, Clone)]
pub struct VerifiedSetup {
    version: ForkVersion,
    sha256: Digest,
    bytes: u64,
    public_key_sha256: Digest,
}
impl VerifiedSetup {
    pub fn version(&self) -> &ForkVersion {
        &self.version
    }
    pub fn sha256(&self) -> &Digest {
        &self.sha256
    }
    pub fn bytes(&self) -> u64 {
        self.bytes
    }
    pub fn public_key_sha256(&self) -> &Digest {
        &self.public_key_sha256
    }
}

fn invalid(message: &str) -> PackageError {
    ProtocolError::Invalid(message.into()).into()
}
fn decode_wrapper(value: &str) -> Result<String, PackageError> {
    let bytes = STANDARD
        .decode(value.trim())
        .map_err(|_| invalid("invalid Tauri base64 wrapper"))?;
    String::from_utf8(bytes).map_err(|_| invalid("invalid minisign text encoding"))
}
fn decode_signature(value: &str) -> Result<Signature, PackageError> {
    Ok(Signature::decode(&decode_wrapper(value)?)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn historical_signature() -> &'static str {
        "dW50cnVzdGVkIGNvbW1lbnQ6IHNpZ25hdHVyZSBmcm9tIHRhdXJpIHNlY3JldCBrZXkKUlVRT0pNaEhpOS9saTFzSVgwdWlVbGZWdlRCMFFzODJZbTcvSjAvV0t4WVFPVUJrSXhPMWdmV3E5NkMyNUlDNWVsOVlwbUhLRmhXWEhTVGVzTTkwNGNkdEoxVkh4bVc2UHdZPQp0cnVzdGVkIGNvbW1lbnQ6IHRpbWVzdGFtcDoxNzkwMzMxODk3CWZpbGU6Q0MgU3dpdGNoXzMuMjAuNC1mb3JrLjNfeDY0LXNldHVwLmV4ZQl2ZXJzaW9uOjMuMjAuNC1mb3JrLjMKeEdMNUpqNnVUK01UVFZIS3FPOGkyQ3NPWUdmQ3JDZ0pjOE0rdFRUR3BnakZObVVhRVlMWHpMVkVsZnkzbWJJeFJqZ2JiVFoyenkvNmJsQUFvK1l0QWc9PQo="
    }

    fn manifest(version: &str, url: &str) -> Vec<u8> {
        serde_json::to_vec(&json!({"version":version, "platforms":{"windows-x86_64":{
            "url":url, "signature": historical_signature()
        }}}))
        .unwrap()
    }

    #[test]
    fn wrong_fork_version_platform_or_asset_is_rejected_before_download() {
        let target = ForkVersion::parse("3.20.4-fork.3").unwrap();
        let fixed = FixedReleaseSetup::setup_url(&target);
        assert!(
            FixedReleaseSetup::from_manifest(&target, &manifest("3.20.4-fork.4", &fixed)).is_err()
        );
        assert!(FixedReleaseSetup::from_manifest(
            &target,
            &manifest("3.20.4-fork.3", "https://example.com/setup.exe")
        )
        .is_err());
        assert!(FixedReleaseSetup::from_manifest(
            &target,
            &manifest("3.20.4-fork.3", &format!("{fixed}?download=1"))
        )
        .is_err());
        assert!(FixedReleaseSetup::from_manifest(
            &target,
            br#"{"version":"3.20.4-fork.3","platforms":{}}"#
        )
        .is_err());
    }

    #[test]
    fn a_valid_manifest_is_only_a_selection_and_does_not_authorize_an_unsigned_cache() {
        let target = ForkVersion::parse("3.20.4-fork.3").unwrap();
        let selection = FixedReleaseSetup::from_manifest(
            &target,
            &manifest("3.20.4-fork.3", &FixedReleaseSetup::setup_url(&target)),
        )
        .unwrap();
        assert!(selection
            .verify(&mut &b"MZunsigned cached installer"[..], None)
            .is_err());
    }

    #[test]
    fn manifest_and_signature_limits_are_enforced() {
        let target = ForkVersion::parse("3.20.4-fork.3").unwrap();
        assert!(FixedReleaseSetup::from_manifest(&target, &vec![b' '; MAX_MANIFEST + 1]).is_err());
        let mut value: serde_json::Value = serde_json::from_slice(&manifest(
            "3.20.4-fork.3",
            &FixedReleaseSetup::setup_url(&target),
        ))
        .unwrap();
        value["platforms"]["windows-x86_64"]["signature"] = "A".repeat(16 * 1024 + 1).into();
        assert!(
            FixedReleaseSetup::from_manifest(&target, &serde_json::to_vec(&value).unwrap())
                .is_err()
        );
    }
}
