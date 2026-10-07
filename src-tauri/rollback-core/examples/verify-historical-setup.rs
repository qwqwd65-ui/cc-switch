//! Read-only GA contract for the actual signed production fork.3 Setup.
//! Does not execute an installer or access any user database.
use cc_switch_rollback_core::{Digest, FixedReleaseSetup, ForkVersion, UPDATER_PUBLIC_KEY};
use std::fs::{self, File};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arguments: Vec<_> = std::env::args_os().collect();
    if arguments.len() != 4 {
        return Err("usage: verify-historical-setup SETUP MANIFEST TAURI_CONFIG".into());
    }
    let config: serde_json::Value = serde_json::from_slice(&fs::read(&arguments[3])?)?;
    if config["plugins"]["updater"]["pubkey"].as_str() != Some(UPDATER_PUBLIC_KEY) {
        return Err("Rollback verifier and application updater public keys differ".into());
    }
    let target = ForkVersion::parse("3.20.4-fork.3")?;
    let manifest = fs::read(&arguments[2])?;
    let selection = FixedReleaseSetup::from_manifest(&target, &manifest)?;
    let mut file = File::open(&arguments[1])?;
    let pinned = Digest::parse("27329df67ca6d6b783c76444dad0d99f2c27701ccdac37afebab619a98295a8b")?;
    let verified = selection.verify(&mut file, Some(&pinned))?;
    assert_eq!(verified.version(), &target);
    assert_eq!(verified.bytes(), 10_185_687);
    // A damaged B cache must fail, and a different target manifest must fail.
    let mut damaged = fs::read(&arguments[1])?;
    damaged[4096] ^= 1;
    assert!(selection
        .verify(&mut damaged.as_slice(), Some(&pinned))
        .is_err());
    let wrong_target = ForkVersion::parse("3.20.4-fork.4")?;
    assert!(FixedReleaseSetup::from_manifest(&wrong_target, &manifest).is_err());
    let wrong_digest = Digest::parse(&"0".repeat(64))?;
    assert!(selection
        .verify(&mut File::open(&arguments[1])?, Some(&wrong_digest))
        .is_err());
    println!("::notice title=Historical Setup signature contract::Production fork.3 passed standard minisign streaming verification, authenticated version/architecture binding and pinned SHA-256. A tampered cache, wrong target and wrong bound digest were rejected. No installer was executed by this test.");
    Ok(())
}
