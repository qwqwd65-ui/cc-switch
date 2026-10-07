//! Windows single-previous-version rollback protocol, independent of the GUI.
//! Filesystem capture and installer execution are separate from this catalog.

mod catalog;
mod journal;
mod model;
mod package;
mod ticket;
#[cfg(windows)]
mod windows_database;
#[cfg(windows)]
mod windows_resource_restore;
#[cfg(windows)]
mod windows_resources;
#[cfg(windows)]
mod windows_restore;
#[cfg(windows)]
mod windows_security;
#[cfg(windows)]
mod windows_snapshot;
#[cfg(windows)]
mod windows_store;

pub use catalog::Catalog;
pub use journal::{Direction, Journal, Phase};
pub use model::{Digest, ForkVersion, InstallSource, Point, ProtocolError};
pub use package::{FixedReleaseSetup, PackageError, VerifiedSetup, UPDATER_PUBLIC_KEY};
pub use ticket::{PreparedSelection, TicketGate};
#[cfg(windows)]
pub use windows_database::{CaptureSlot, DatabaseImage};
#[cfg(windows)]
pub use windows_resources::{
    ResourceInventory, ResourceKind, ResourceRequest, ResourceRole, ResourceState, SnapshotResource,
};
#[cfg(windows)]
pub use windows_security::FileDacl;
#[cfg(windows)]
pub use windows_snapshot::{CachedSourceSetup, SnapshotManifest};
#[cfg(windows)]
pub use windows_store::{PrivateRoot, StoreError, StoreLease};
