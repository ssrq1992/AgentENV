use aenv_volume_core::{
    posix::PosixVolumeCatalog, ManagedLayer, OverlaybdLayerRef, VolumeMode, VolumeRecord,
    VolumeStatus,
};
use tempfile::TempDir;

fn volume(id: &str) -> VolumeRecord {
    VolumeRecord {
        id: id.into(),
        name: format!("name_{id}"),
        mode: VolumeMode::Exclusive,
        size_mb: 16,
        status: VolumeStatus::Ready,
        reserved_by_sandbox_id: None,
        backing_image_config: None,
        backing_layers: vec![],
        read_only_mounts: vec![],
        deleting: false,
    }
}

#[test]
fn expansion_requires_exact_maintenance_owner_and_source() {
    let dir = TempDir::new().unwrap();
    let catalog = PosixVolumeCatalog::new(dir.path().into());
    let record = volume("vol_expand");
    catalog.create_volume(&record).unwrap();
    let layers = vec![OverlaybdLayerRef::Managed(ManagedLayer {
        digest: "sha256:expanded".into(),
        size: 4096,
        uuid: None,
    })];
    assert!(catalog
        .commit_expansion(&record.id, "maintenance_op", 16, &[], 32, &layers)
        .is_err());
    catalog
        .reserve_volume(&record.id, "maintenance_op")
        .unwrap();
    assert!(catalog
        .commit_expansion(&record.id, "maintenance_other", 16, &[], 32, &layers)
        .is_err());
    assert!(catalog
        .commit_expansion(&record.id, "maintenance_op", 8, &[], 32, &layers)
        .is_err());
    let mut ordinary = record.clone();
    ordinary.size_mb = 32;
    assert!(catalog.put_volume(&ordinary).is_err());
    catalog
        .commit_expansion(&record.id, "maintenance_op", 16, &[], 32, &layers)
        .unwrap();
    catalog
        .commit_expansion(&record.id, "maintenance_op", 16, &[], 32, &layers)
        .unwrap();
    assert!(catalog
        .commit_expansion(&record.id, "maintenance_op", 32, &layers, 16, &layers)
        .is_err());
    let result = catalog.get_volume(&record.id).unwrap().unwrap();
    assert_eq!(result.size_mb, 32);
    assert_eq!(result.backing_layers, layers);
    assert_eq!(
        result.reserved_by_sandbox_id.as_deref(),
        Some("maintenance_op")
    );
    catalog
        .replace_volume_owner_for(&record.id, "maintenance_op", None)
        .unwrap();
    assert!(catalog
        .commit_expansion(&record.id, "maintenance_op", 16, &[], 32, &layers)
        .is_err());
}

#[test]
fn competing_catalogs_have_one_writer_and_cannot_delete_it() {
    let dir = TempDir::new().unwrap();
    let catalog = PosixVolumeCatalog::new(dir.path().into());
    catalog.create_volume(&volume("vol_1")).unwrap();
    let owners: Vec<_> = std::thread::scope(|scope| {
        (0..12)
            .map(|n| {
                let root = dir.path().to_path_buf();
                scope.spawn(move || {
                    let owner = format!("run_{n}");
                    let result = PosixVolumeCatalog::new(root)
                        .reserve_volume("vol_1", &owner)
                        .unwrap();
                    (owner, result)
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|j| j.join().unwrap())
            .collect()
    });
    let winners: Vec<_> = owners
        .iter()
        .filter(|(_, conflict)| conflict.is_none())
        .collect();
    assert_eq!(winners.len(), 1);
    assert!(catalog.delete_volume("vol_1").is_err());
    let winner = &winners[0].0;
    catalog
        .replace_volume_owner_for("vol_1", "stale_owner", None)
        .unwrap();
    assert_eq!(
        catalog
            .get_volume("vol_1")
            .unwrap()
            .unwrap()
            .reserved_by_sandbox_id
            .as_deref(),
        Some(winner.as_str())
    );
    catalog
        .replace_volume_owner_for("vol_1", winner, None)
        .unwrap();
    catalog.delete_volume("vol_1").unwrap();
    assert!(catalog.get_volume("name_vol_1").unwrap().is_none());
}

#[test]
fn publication_does_not_restore_stale_reservation_or_bypass_uploading() {
    let dir = TempDir::new().unwrap();
    let catalog = PosixVolumeCatalog::new(dir.path().into());
    let mut record = volume("vol_1");
    catalog.create_volume(&record).unwrap();
    record
        .backing_layers
        .push(OverlaybdLayerRef::Managed(ManagedLayer {
            digest: "sha256:abc".into(),
            size: 16,
            uuid: None,
        }));
    assert!(catalog.put_volume(&record).is_err());
    let mut uploading = volume("vol_1");
    uploading.status = VolumeStatus::Uploading;
    catalog.put_volume(&uploading).unwrap();
    catalog.reserve_volume("vol_1", "new_owner").unwrap();
    record.reserved_by_sandbox_id = Some("stale_owner".into());
    record.backing_image_config = Some("/node/local/image.json".into());
    catalog.put_volume(&record).unwrap();
    let actual = catalog.get_volume("vol_1").unwrap().unwrap();
    assert_eq!(actual.reserved_by_sandbox_id.as_deref(), Some("new_owner"));
    assert_eq!(actual.backing_image_config, None);
    let serialized = serde_json::to_value(actual).unwrap();
    assert!(serialized.get("backingLayers").is_some());
    assert!(serialized.get("backingImageConfig").is_none());
}

#[test]
fn untrusted_paths_and_record_identity_mismatches_are_rejected() {
    let dir = TempDir::new().unwrap();
    let catalog = PosixVolumeCatalog::new(dir.path().into());
    assert!(catalog.get_volume("../other").is_err());
    catalog.create_volume(&volume("vol_1")).unwrap();
    let path = dir.path().join("volumes/records/vol_1.json");
    std::fs::write(path, serde_json::to_vec(&volume("vol_other")).unwrap()).unwrap();
    assert!(catalog.get_volume("vol_1").is_err());
}
