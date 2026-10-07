use cc_switch_rollback_core::{
    Catalog, Digest, ForkVersion, InstallSource, Phase, Point, ProtocolError,
};
use uuid::Uuid;

fn version(number: u8) -> ForkVersion {
    ForkVersion::parse(&format!("3.20.4-fork.{number}")).unwrap()
}

fn complete_upgrade(catalog: &mut Catalog, number: u8, source: InstallSource) -> Point {
    let old_version = catalog.current_version().clone();
    assert!(catalog.begin_upgrade(version(number), source).unwrap());
    for phase in [
        Phase::Prepared,
        Phase::Quiescing,
        Phase::Captured,
        Phase::Installing,
        Phase::Verifying,
    ] {
        catalog.advance(phase).unwrap();
    }
    let journal = catalog.journal().unwrap();
    let point = Point {
        id: journal.point_id(),
        transaction_id: journal.id(),
        source_version: old_version,
        installed_version: version(number),
        captured_at_unix_ms: 1,
        snapshot_digest: Digest::parse(&"a".repeat(64)).unwrap(),
        source_setup_digest: source
            .uses_cached_setup()
            .then(|| Digest::parse(&"b".repeat(64)).unwrap()),
        source,
    };
    catalog.commit_upgrade(point.clone()).unwrap();
    catalog.validate().unwrap();
    catalog.finish_cleanup(catalog.cleanup_pending()).unwrap();
    point
}

#[test]
fn sequential_upgrades_only_restore_the_immediate_actual_source() {
    let mut catalog = Catalog::new(version(1));
    complete_upgrade(&mut catalog, 2, InstallSource::ProtocolInAppUpdate);
    complete_upgrade(&mut catalog, 3, InstallSource::ProtocolInAppUpdate);
    complete_upgrade(&mut catalog, 4, InstallSource::ProtocolInAppUpdate);
    assert_eq!(catalog.previous().unwrap().source_version, version(3));
    catalog.begin_rollback().unwrap();
    for phase in [
        Phase::Prepared,
        Phase::Quiescing,
        Phase::Captured,
        Phase::Installing,
        Phase::Restoring,
        Phase::Verifying,
    ] {
        catalog.advance(phase).unwrap();
    }
    catalog.commit_rollback().unwrap();
    assert_eq!(catalog.current_version(), &version(3));
    assert!(catalog.previous().is_none());
    assert!(matches!(catalog.begin_rollback(), Err(ProtocolError::Busy)));
    catalog.finish_cleanup(catalog.cleanup_pending()).unwrap();
    assert!(matches!(
        catalog.begin_rollback(),
        Err(ProtocolError::NoPoint)
    ));
    // Consuming a point does not permanently disable later upgrades.
    complete_upgrade(&mut catalog, 4, InstallSource::ManualSetup);
    assert_eq!(catalog.previous().unwrap().source_version, version(3));
}

#[test]
fn skipped_versions_and_legacy_updates_use_the_real_installed_source() {
    let mut catalog = Catalog::new(version(1));
    let point = complete_upgrade(&mut catalog, 4, InstallSource::LegacyInAppUpdate);
    assert_eq!(point.source_version, version(1));
    assert!(!point.source.uses_cached_setup());
    assert!(point.source_setup_digest.is_none());
}

#[test]
fn reinstall_preserves_the_point_and_fresh_install_has_none() {
    let mut catalog = Catalog::new(version(3));
    assert!(matches!(
        catalog.begin_rollback(),
        Err(ProtocolError::NoPoint)
    ));
    let point = complete_upgrade(&mut catalog, 4, InstallSource::ManualSetup);
    let unchanged = catalog.clone();
    assert!(!catalog
        .begin_upgrade(version(4), InstallSource::ManualSetup)
        .unwrap());
    assert_eq!(catalog, unchanged);
    assert_eq!(catalog.previous(), Some(&point));
    assert!(catalog
        .begin_upgrade(version(2), InstallSource::ManualSetup)
        .is_err());
    assert_eq!(catalog, unchanged);
}

#[test]
fn failed_next_upgrade_does_not_replace_the_working_old_point_after_restart() {
    let mut catalog = Catalog::new(version(1));
    let point = complete_upgrade(&mut catalog, 2, InstallSource::ProtocolInAppUpdate);
    catalog
        .begin_upgrade(version(4), InstallSource::ManualSetup)
        .unwrap();
    for phase in [
        Phase::Prepared,
        Phase::Quiescing,
        Phase::Captured,
        Phase::Installing,
        Phase::Failed,
        Phase::Recovering,
    ] {
        catalog.advance(phase).unwrap();
    }
    let bytes = serde_json::to_vec(&catalog).unwrap();
    let mut restarted: Catalog = serde_json::from_slice(&bytes).unwrap();
    restarted.validate().unwrap();
    assert!(restarted.confirm_recovered(&version(4)).is_err());
    restarted.confirm_recovered(&version(2)).unwrap();
    assert_eq!(restarted.previous(), Some(&point));
    assert_eq!(restarted.current_version(), &version(2));
    assert!(matches!(
        restarted.begin_upgrade(version(3), InstallSource::ManualSetup),
        Err(ProtocolError::Busy)
    ));
    let transaction_id = restarted.journal().unwrap().id();
    restarted.finish_recovery_cleanup(transaction_id).unwrap();
    assert!(restarted
        .begin_upgrade(version(3), InstallSource::ManualSetup)
        .unwrap());
}

#[test]
fn rollback_cannot_commit_until_old_data_has_been_restored_and_verified() {
    let mut catalog = Catalog::new(version(1));
    let point = complete_upgrade(&mut catalog, 4, InstallSource::ManualSetup);
    catalog.begin_rollback().unwrap();
    for phase in [
        Phase::Prepared,
        Phase::Quiescing,
        Phase::Captured,
        Phase::Installing,
    ] {
        catalog.advance(phase).unwrap();
    }
    assert!(catalog.advance(Phase::Verifying).is_err());
    assert!(catalog.commit_rollback().is_err());
    assert_eq!(catalog.previous(), Some(&point));
    catalog.advance(Phase::Failed).unwrap();
    catalog.advance(Phase::Recovering).unwrap();
    catalog.confirm_recovered(&version(4)).unwrap();
    assert_eq!(catalog.previous(), Some(&point));
}

#[test]
fn unbound_snapshot_or_missing_b_package_cannot_change_the_catalog() {
    let mut catalog = Catalog::new(version(1));
    catalog
        .begin_upgrade(version(2), InstallSource::ProtocolInAppUpdate)
        .unwrap();
    for phase in [
        Phase::Prepared,
        Phase::Quiescing,
        Phase::Captured,
        Phase::Installing,
        Phase::Verifying,
    ] {
        catalog.advance(phase).unwrap();
    }
    let before = catalog.clone();
    let journal = catalog.journal().unwrap();
    let mut point = Point {
        id: journal.point_id(),
        transaction_id: journal.id(),
        source_version: version(1),
        installed_version: version(2),
        captured_at_unix_ms: 1,
        snapshot_digest: Digest::parse(&"c".repeat(64)).unwrap(),
        source_setup_digest: None,
        source: InstallSource::ProtocolInAppUpdate,
    };
    assert!(catalog.commit_upgrade(point.clone()).is_err());
    assert_eq!(catalog, before);
    point.source_setup_digest = Some(Digest::parse(&"d".repeat(64)).unwrap());
    point.transaction_id = Uuid::new_v4();
    assert!(catalog.commit_upgrade(point).is_err());
    assert_eq!(catalog, before);
}

#[test]
fn interrupted_cleanup_keeps_only_the_new_point_visible_and_blocks_another_update() {
    let mut catalog = Catalog::new(version(1));
    let old = complete_upgrade(&mut catalog, 2, InstallSource::ManualSetup);
    catalog
        .begin_upgrade(version(3), InstallSource::ManualSetup)
        .unwrap();
    for phase in [
        Phase::Prepared,
        Phase::Quiescing,
        Phase::Captured,
        Phase::Installing,
        Phase::Verifying,
    ] {
        catalog.advance(phase).unwrap();
    }
    let journal = catalog.journal().unwrap();
    let next = Point {
        id: journal.point_id(),
        transaction_id: journal.id(),
        source_version: version(2),
        installed_version: version(3),
        captured_at_unix_ms: 2,
        snapshot_digest: Digest::parse(&"e".repeat(64)).unwrap(),
        source_setup_digest: None,
        source: InstallSource::ManualSetup,
    };
    catalog.commit_upgrade(next.clone()).unwrap();
    let mut restarted: Catalog =
        serde_json::from_slice(&serde_json::to_vec(&catalog).unwrap()).unwrap();
    restarted.validate().unwrap();
    assert_eq!(restarted.previous(), Some(&next));
    assert_eq!(restarted.cleanup_pending(), Some(old.id));
    assert!(matches!(
        restarted.begin_upgrade(version(4), InstallSource::ManualSetup),
        Err(ProtocolError::Busy)
    ));
    assert!(restarted.advance(Phase::Failed).is_err());
    assert!(restarted.finish_cleanup(Some(Uuid::new_v4())).is_err());
    restarted.finish_cleanup(Some(old.id)).unwrap();
    assert!(restarted
        .begin_upgrade(version(4), InstallSource::ManualSetup)
        .unwrap());
}

#[test]
fn invalid_serialized_versions_hashes_or_changed_rollback_target_are_rejected() {
    for input in [
        "4.0.0-preview.1",
        "3.20.4",
        "3.20.4-fork.4+different",
        "../../other",
    ] {
        assert!(ForkVersion::parse(input).is_err());
        assert!(serde_json::from_str::<ForkVersion>(&format!("{input:?}")).is_err());
    }
    assert!(Digest::parse(&"A".repeat(64)).is_err());
    let mut catalog = Catalog::new(version(1));
    complete_upgrade(&mut catalog, 4, InstallSource::ManualSetup);
    catalog.begin_rollback().unwrap();
    let mut value = serde_json::to_value(&catalog).unwrap();
    value["transaction"]["to"] = serde_json::json!("3.20.4-fork.2");
    let mut changed: Catalog = serde_json::from_value(value).unwrap();
    assert!(changed.validate().is_err());
    assert!(changed.advance(Phase::Prepared).is_err());
}
