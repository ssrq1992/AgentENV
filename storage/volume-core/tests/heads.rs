#![cfg(feature = "metadata")]
use aenv_volume_core::{
    heads::{PosixHeadStore, PublishedHead},
    ManagedLayer, OverlaybdLayerRef,
};
fn head() -> PublishedHead {
    PublishedHead {
        version: 1,
        volume_id: "vol_a".into(),
        revision: String::new(),
        parent: None,
        capacity_bytes: 1 << 20,
        capacity_generation: 1,
        filesystem_uuid: "9f0558e1-18cc-4a4a-adf9-51c6eb0efdb3".into(),
        layers: vec![OverlaybdLayerRef::Managed(ManagedLayer {
            digest: format!("sha256:{}", "a".repeat(64)),
            size: 4096,
            uuid: None,
        })],
        operation_id: "op_a".into(),
        input_digest: "a".repeat(64),
    }
    .seal()
    .unwrap()
}
#[test]
fn concurrent_publications_have_one_winner_and_retain_original_head() {
    let dir = tempfile::TempDir::new().unwrap();
    let store = PosixHeadStore::new(dir.path().to_owned()).unwrap();
    let first = head();
    store.publish(None, &first).unwrap();
    let mut next = first.clone();
    next.parent = Some(first.revision.clone());
    next.operation_id = "op_b".into();
    next = next.seal().unwrap();
    let mut other = next.clone();
    other.operation_id = "op_c".into();
    other = other.seal().unwrap();
    let a = store.clone();
    let b = store.clone();
    let expected = first.revision.clone();
    let e = expected.clone();
    let x = std::thread::spawn(move || a.publish(Some(&e), &next));
    let y = std::thread::spawn(move || b.publish(Some(&expected), &other));
    let wins = [x.join().unwrap().is_ok(), y.join().unwrap().is_ok()]
        .into_iter()
        .filter(|v| *v)
        .count();
    assert_eq!(wins, 1);
    assert_eq!(store.read("vol_a", &first.revision).unwrap(), Some(first));
}
#[test]
fn publication_replay_is_idempotent_and_stale_capacity_is_rejected() {
    let dir = tempfile::TempDir::new().unwrap();
    let store = PosixHeadStore::new(dir.path().to_owned()).unwrap();
    let first = head();
    store.publish(None, &first).unwrap();
    store.publish(None, &first).unwrap();
    let mut larger = first.clone();
    larger.parent = Some(first.revision.clone());
    larger.capacity_bytes *= 2;
    larger.operation_id = "expand_a".into();
    larger = larger.seal().unwrap();
    assert!(store.publish(Some(&first.revision), &larger).is_err());
    larger.capacity_generation += 1;
    larger = larger.seal().unwrap();
    store.publish(Some(&first.revision), &larger).unwrap();
    assert!(store.publish(None, &first).is_err());
    assert_eq!(store.current("vol_a").unwrap(), Some(larger));
}
#[test]
fn corruption_and_foreign_identity_fail_closed() {
    let dir = tempfile::TempDir::new().unwrap();
    let store = PosixHeadStore::new(dir.path().to_owned()).unwrap();
    let first = head();
    store.publish(None, &first).unwrap();
    let mut changed = first.clone();
    changed.capacity_bytes *= 2;
    assert!(store.publish(None, &changed).is_err());
    std::fs::write(dir.path().join("vol_a/current.json"), b"truncated").unwrap();
    assert!(store.current("vol_a").is_err());
    assert!(store.read("../vol_a", &first.revision).is_err());
}
