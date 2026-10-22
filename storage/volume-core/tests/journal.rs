use aenv_volume_core::journal::{LockedOperation, NativeIntent, NativeOperationState};
use serde_json::json;

fn intent() -> NativeIntent {
    NativeIntent {
        operation_id: "op_a".into(),
        volume_id: "vol_a".into(),
        repository_id: "repo_a".into(),
        kind: "create".into(),
        input_digest: "a".repeat(64),
    }
}

#[test]
fn crash_after_dispatch_never_restarts_formatter() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut op = LockedOperation::open(dir.path(), intent()).unwrap();
    op.begin().unwrap();
    assert!(LockedOperation::open(dir.path(), intent()).is_err());
    drop(op);
    let mut recovered = LockedOperation::open(dir.path(), intent()).unwrap();
    assert_eq!(recovered.record().state, NativeOperationState::Started);
    assert!(recovered.begin().is_err());
    recovered.mark_unknown().unwrap();
    assert!(recovered.begin().is_err());
    recovered
        .complete(json!({"head":"h1","fs_uuid":"u1"}))
        .unwrap();
    drop(recovered);
    let mut complete = LockedOperation::open(dir.path(), intent()).unwrap();
    assert!(complete.begin().is_err());
    complete
        .complete(json!({"head":"h1","fs_uuid":"u1"}))
        .unwrap();
    assert!(complete.complete(json!({"head":"h2"})).is_err());
}

#[test]
fn mismatched_request_and_corrupt_state_fail_closed() {
    let dir = tempfile::TempDir::new().unwrap();
    drop(LockedOperation::open(dir.path(), intent()).unwrap());
    let mut changed = intent();
    changed.input_digest = "b".repeat(64);
    assert!(LockedOperation::open(dir.path(), changed).is_err());
    std::fs::write(dir.path().join("op_a.json"), b"truncated").unwrap();
    assert!(LockedOperation::open(dir.path(), intent()).is_err());
}

#[test]
fn journal_rejects_symlinks_and_invalid_completion() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::TempDir::new().unwrap();
    let target = dir.path().join("unrelated");
    std::fs::write(&target, b"do not replace").unwrap();
    symlink(&target, dir.path().join("op_a.lock")).unwrap();
    assert!(LockedOperation::open(dir.path(), intent()).is_err());
    std::fs::remove_file(dir.path().join("op_a.lock")).unwrap();
    symlink(&target, dir.path().join("op_a.json")).unwrap();
    assert!(LockedOperation::open(dir.path(), intent()).is_err());
    assert_eq!(std::fs::read(&target).unwrap(), b"do not replace");
    std::fs::remove_file(dir.path().join("op_a.json")).unwrap();
    let record =
        json!({"version":1,"intent":intent(),"state":"Started","receipt":{"head":"not-committed"}});
    std::fs::write(
        dir.path().join("op_a.json"),
        serde_json::to_vec(&record).unwrap(),
    )
    .unwrap();
    assert!(LockedOperation::open(dir.path(), intent()).is_err());
}

#[test]
fn checkpoints_survive_crash_and_cannot_change() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut op = LockedOperation::open(dir.path(), intent()).unwrap();
    op.checkpoint("request", json!({"capacity":4096})).unwrap();
    op.begin().unwrap();
    op.checkpoint("sealed_head", json!({"revision":"h1"}))
        .unwrap();
    drop(op);
    let mut recovered = LockedOperation::open(dir.path(), intent()).unwrap();
    assert_eq!(
        recovered.record().checkpoints["request"],
        json!({"capacity":4096})
    );
    assert!(recovered
        .checkpoint("request", json!({"capacity":8192}))
        .is_err());
    recovered
        .checkpoint("sealed_head", json!({"revision":"h1"}))
        .unwrap();
    recovered.complete(json!({"head":"h1"})).unwrap();
    assert!(recovered.checkpoint("extra", json!(1)).is_err());
}
