//! Windows single-previous-version rollback protocol, independent of the GUI.
//! Filesystem capture and installer execution are separate from this catalog.

mod catalog;
mod journal;
mod model;
mod ticket;
#[cfg(windows)]
mod windows_database;
#[cfg(windows)]
mod windows_store;

pub use catalog::Catalog;
pub use journal::{Direction, Journal, Phase};
pub use model::{Digest, ForkVersion, InstallSource, Point, ProtocolError};
pub use ticket::{PreparedSelection, TicketGate};
#[cfg(windows)]
pub use windows_database::{CaptureSlot, DatabaseImage};
#[cfg(windows)]
pub use windows_store::{PrivateRoot, StoreError, StoreLease};
