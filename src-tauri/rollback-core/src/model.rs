use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const FORMAT_VERSION: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("Invalid rollback metadata: {0}")]
    Invalid(String),
    #[error("An installation transaction or pending cleanup must finish first")]
    Busy,
    #[error("No matching previous-version rollback point")]
    NoPoint,
    #[error("Invalid transaction phase transition")]
    Phase,
    #[error("Rollback confirmation expired, was cancelled, or was already consumed")]
    TicketUnavailable,
    #[error("Rollback snapshot or installer changed; verify and confirm again")]
    SelectionChanged,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ForkVersion(semver::Version);

impl ForkVersion {
    pub fn parse(value: &str) -> Result<Self, ProtocolError> {
        value.to_owned().try_into()
    }

    pub fn is_newer_than(&self, other: &Self) -> bool {
        self.0 > other.0
    }
}

impl TryFrom<String> for ForkVersion {
    type Error = ProtocolError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        let parsed = semver::Version::parse(&value)
            .map_err(|_| ProtocolError::Invalid("malformed application version".into()))?;
        if parsed.major != 3
            || !parsed.pre.as_str().starts_with("fork.")
            || !parsed.build.is_empty()
        {
            return Err(ProtocolError::Invalid("expected a 3.x fork version".into()));
        }
        Ok(Self(parsed))
    }
}

impl From<ForkVersion> for String {
    fn from(value: ForkVersion) -> Self {
        value.0.to_string()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Digest(String);

impl Digest {
    pub fn parse(value: &str) -> Result<Self, ProtocolError> {
        value.to_owned().try_into()
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for Digest {
    type Error = ProtocolError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.len() != 64
            || !value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(ProtocolError::Invalid(
                "expected lowercase SHA-256 digest".into(),
            ));
        }
        Ok(Self(value))
    }
}

impl From<Digest> for String {
    fn from(value: Digest) -> Self {
        value.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallSource {
    ManualSetup,
    LegacyInAppUpdate,
    ProtocolInAppUpdate,
}

impl InstallSource {
    pub fn uses_cached_setup(self) -> bool {
        self == Self::ProtocolInAppUpdate
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Point {
    pub id: Uuid,
    pub transaction_id: Uuid,
    pub source_version: ForkVersion,
    pub installed_version: ForkVersion,
    pub captured_at_unix_ms: u64,
    pub snapshot_digest: Digest,
    pub source_setup_digest: Option<Digest>,
    pub source: InstallSource,
}

impl Point {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.id.is_nil() || self.transaction_id.is_nil() || self.captured_at_unix_ms == 0 {
            return Err(ProtocolError::Invalid(
                "missing snapshot identity or capture time".into(),
            ));
        }
        if !self.installed_version.is_newer_than(&self.source_version) {
            return Err(ProtocolError::Invalid(
                "rollback source is not strictly older".into(),
            ));
        }
        if self.source.uses_cached_setup() && self.source_setup_digest.is_none() {
            return Err(ProtocolError::Invalid(
                "protocol update requires a verified cached source Setup".into(),
            ));
        }
        Ok(())
    }
}
