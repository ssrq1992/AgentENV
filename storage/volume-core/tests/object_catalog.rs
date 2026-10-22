//! Deterministic CAS transport fixtures; these are not real OSS validation.
use aenv_volume_core::{
    object::{ObjectVolumeCatalog, VolumeObjectStore},
    VolumeMode, VolumeRecord, VolumeStatus,
};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

#[derive(Default)]
struct MemoryStore {
    objects: Mutex<BTreeMap<String, (Vec<u8>, u64)>>,
}
#[async_trait::async_trait]
impl VolumeObjectStore for MemoryStore {
    fn is_not_found_error(error: &anyhow::Error) -> bool {
        error.to_string() == "not found"
    }
    async fn get_bytes(&self, key: &str) -> anyhow::Result<Vec<u8>> {
        Ok(self.get_bytes_with_etag(key).await?.0)
    }
    async fn get_bytes_with_etag(&self, key: &str) -> anyhow::Result<(Vec<u8>, Option<String>)> {
        self.objects
            .lock()
            .unwrap()
            .get(key)
            .map(|(v, n)| (v.clone(), Some(n.to_string())))
            .ok_or_else(|| anyhow::anyhow!("not found"))
    }
    async fn exists(&self, key: &str) -> anyhow::Result<bool> {
        Ok(self.objects.lock().unwrap().contains_key(key))
    }
    async fn list_keys_page(
        &self,
        prefix: &str,
        after: Option<&str>,
        limit: usize,
    ) -> anyhow::Result<Vec<String>> {
        Ok(self
            .objects
            .lock()
            .unwrap()
            .keys()
            .filter(|k| k.starts_with(prefix) && after.is_none_or(|a| k.as_str() > a))
            .take(limit)
            .cloned()
            .collect())
    }
    async fn put_bytes_conditionally(
        &self,
        key: &str,
        bytes: Vec<u8>,
        etag: Option<&str>,
    ) -> anyhow::Result<bool> {
        let mut objects = self.objects.lock().unwrap();
        let current = objects.get(key).map(|(_, n)| n.to_string());
        if current.as_deref() != etag {
            return Ok(false);
        }
        let version = objects.get(key).map(|(_, n)| n + 1).unwrap_or(1);
        objects.insert(key.into(), (bytes, version));
        Ok(true)
    }
    async fn delete(&self, key: &str) -> anyhow::Result<()> {
        self.objects.lock().unwrap().remove(key);
        Ok(())
    }
}

fn volume(id: &str) -> VolumeRecord {
    VolumeRecord {
        id: id.into(),
        name: "workspace".into(),
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

#[tokio::test]
async fn expansion_cas_preserves_owner_and_rejects_stale_content() {
    let catalog = ObjectVolumeCatalog::new(Arc::new(MemoryStore::default()));
    let record = volume("vol_expand");
    catalog.create_volume(record.clone()).await.unwrap();
    let layers = vec![aenv_volume_core::OverlaybdLayerRef::Managed(
        aenv_volume_core::ManagedLayer {
            digest: "sha256:expanded".into(),
            size: 4096,
            uuid: None,
        },
    )];
    catalog
        .reserve_volume(&record.id, "maintenance_op")
        .await
        .unwrap();
    assert!(catalog
        .commit_expansion(&record.id, "maintenance_other", 16, &[], 32, &layers)
        .await
        .is_err());
    assert!(catalog
        .commit_expansion(&record.id, "maintenance_op", 8, &[], 32, &layers)
        .await
        .is_err());
    catalog
        .commit_expansion(&record.id, "maintenance_op", 16, &[], 32, &layers)
        .await
        .unwrap();
    catalog
        .commit_expansion(&record.id, "maintenance_op", 16, &[], 32, &layers)
        .await
        .unwrap();
    assert!(catalog.put_volume(record.clone()).await.is_err());
    let result = catalog.get_volume(&record.id).await.unwrap().unwrap();
    assert_eq!(result.size_mb, 32);
    assert_eq!(result.backing_layers, layers);
    assert_eq!(
        result.reserved_by_sandbox_id.as_deref(),
        Some("maintenance_op")
    );
    catalog
        .replace_volume_owner_for(&record.id, "maintenance_op", None)
        .await
        .unwrap();
    assert!(catalog
        .commit_expansion(&record.id, "maintenance_op", 16, &[], 32, &layers)
        .await
        .is_err());
}

#[tokio::test]
async fn unknown_create_cannot_steal_a_reserved_alias() {
    let store = Arc::new(MemoryStore::default());
    // First creator claimed the alias; its record write has an unknown result.
    assert!(store
        .put_bytes_conditionally(
            "volumes/aliases/workspace.json",
            b"\"vol_a\"".to_vec(),
            None
        )
        .await
        .unwrap());
    let catalog = ObjectVolumeCatalog::new(store.clone());
    assert!(catalog.create_volume(volume("vol_b")).await.is_err());
    catalog.create_volume(volume("vol_a")).await.unwrap();
    assert_eq!(
        catalog.get_volume("workspace").await.unwrap().unwrap().id,
        "vol_a"
    );
    assert!(!store.exists("volumes/records/vol_b.json").await.unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_owners_and_stale_releases_preserve_one_writer() {
    let store = Arc::new(MemoryStore::default());
    let catalog = ObjectVolumeCatalog::new(store.clone());
    catalog.create_volume(volume("vol_a")).await.unwrap();
    let mut jobs = Vec::new();
    for n in 0..16 {
        let store = store.clone();
        jobs.push(tokio::spawn(async move {
            let owner = format!("run_{n}");
            let result = ObjectVolumeCatalog::new(store)
                .reserve_volume("vol_a", &owner)
                .await;
            (owner, result)
        }));
    }
    let mut winners = Vec::new();
    for job in jobs {
        let (owner, result) = job.await.unwrap();
        if matches!(result, Ok(None)) {
            winners.push(owner);
        }
    }
    assert_eq!(winners.len(), 1);
    assert!(catalog.delete_volume("vol_a").await.is_err());
    catalog
        .replace_volume_owner_for("vol_a", "stale", None)
        .await
        .unwrap();
    assert_eq!(
        catalog
            .get_volume("vol_a")
            .await
            .unwrap()
            .unwrap()
            .reserved_by_sandbox_id,
        Some(winners[0].clone())
    );
}

#[tokio::test]
async fn missing_etag_fails_closed() {
    struct Unversioned(MemoryStore);
    #[async_trait::async_trait]
    impl VolumeObjectStore for Unversioned {
        fn is_not_found_error(e: &anyhow::Error) -> bool {
            MemoryStore::is_not_found_error(e)
        }
        async fn get_bytes(&self, k: &str) -> anyhow::Result<Vec<u8>> {
            self.0.get_bytes(k).await
        }
        async fn get_bytes_with_etag(&self, k: &str) -> anyhow::Result<(Vec<u8>, Option<String>)> {
            Ok((self.0.get_bytes(k).await?, None))
        }
        async fn exists(&self, k: &str) -> anyhow::Result<bool> {
            self.0.exists(k).await
        }
        async fn list_keys_page(
            &self,
            p: &str,
            a: Option<&str>,
            l: usize,
        ) -> anyhow::Result<Vec<String>> {
            self.0.list_keys_page(p, a, l).await
        }
        async fn put_bytes_conditionally(
            &self,
            k: &str,
            b: Vec<u8>,
            e: Option<&str>,
        ) -> anyhow::Result<bool> {
            self.0.put_bytes_conditionally(k, b, e).await
        }
        async fn delete(&self, k: &str) -> anyhow::Result<()> {
            self.0.delete(k).await
        }
    }
    let store = Arc::new(Unversioned(MemoryStore::default()));
    let catalog = ObjectVolumeCatalog::new(store);
    catalog.create_volume(volume("vol_a")).await.unwrap();
    assert!(catalog
        .reserve_volume("vol_a", "run_a")
        .await
        .unwrap_err()
        .to_string()
        .contains("ETags"));
}

#[cfg(feature = "metadata")]
#[tokio::test]
async fn object_heads_cas_preserves_snapshot_heads_and_capacity_generation() {
    use aenv_volume_core::{
        head_repository::{HeadRepository, ObjectHeadStore},
        heads::PublishedHead,
        ManagedLayer, OverlaybdLayerRef,
    };
    let client = Arc::new(MemoryStore::default());
    let first = PublishedHead {
        version: 1,
        volume_id: "vol_head".into(),
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
    .unwrap();
    let store = ObjectHeadStore::new(client.clone());
    store.publish(None, &first).await.unwrap();
    let mut next = first.clone();
    next.parent = Some(first.revision.clone());
    next.operation_id = "op_b".into();
    next = next.seal().unwrap();
    let mut competing = next.clone();
    competing.operation_id = "op_c".into();
    competing = competing.seal().unwrap();
    let other = ObjectHeadStore::new(client);
    let (a, b) = tokio::join!(
        store.publish(Some(&first.revision), &next),
        other.publish(Some(&first.revision), &competing)
    );
    assert_eq!([a.is_ok(), b.is_ok()].into_iter().filter(|v| *v).count(), 1);
    assert_eq!(
        store.read(&first.volume_id, &first.revision).await.unwrap(),
        Some(first.clone())
    );
    let mut expansion = store.current(&first.volume_id).await.unwrap().unwrap();
    let expected = expansion.revision.clone();
    expansion.parent = Some(expected.clone());
    expansion.capacity_bytes *= 2;
    expansion = expansion.seal().unwrap();
    assert!(store.publish(Some(&expected), &expansion).await.is_err());
    expansion.capacity_generation += 1;
    expansion = expansion.seal().unwrap();
    store.publish(Some(&expected), &expansion).await.unwrap();
    store.publish(Some(&expected), &expansion).await.unwrap();
}
