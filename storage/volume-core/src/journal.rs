//! Durable native-side operation identity. This is a side-effect journal, not
//! a writer lease: authorization must already have been checked by its caller.
use crate::{
    catalog::{CatalogError, CatalogResult},
    is_valid_volume_component,
};
use nix::fcntl::{Flock, FlockArg};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeIntent {
    pub operation_id: String,
    pub input_digest: String,
    pub volume_id: String,
    pub repository_id: String,
    pub kind: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NativeOperationState {
    Intent,
    Started,
    NeedsRecovery,
    Completed,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NativeOperation {
    pub version: u32,
    pub intent: NativeIntent,
    pub state: NativeOperationState,
    pub receipt: Option<Value>,
    #[serde(default)]
    pub checkpoints: std::collections::BTreeMap<String, Value>,
}

/// The filesystem lock is held across the native operation. Never use a lease
/// timeout to steal it from a still-running formatter or daemon operation.
pub struct LockedOperation {
    path: PathBuf,
    record: NativeOperation,
    _lock: Flock<File>,
}

fn invalid(reason: impl Into<String>) -> CatalogError {
    CatalogError::InvalidRequest {
        reason: reason.into(),
    }
}
fn io(error: std::io::Error) -> CatalogError {
    CatalogError::backend("native operation journal", error)
}

impl LockedOperation {
    pub fn open(root: &Path, intent: NativeIntent) -> CatalogResult<Self> {
        if !root.is_absolute()
            || !is_valid_volume_component(&intent.operation_id)
            || !is_valid_volume_component(&intent.volume_id)
            || intent.repository_id.is_empty()
            || intent.kind.is_empty()
            || intent.input_digest.len() != 64
            || !intent.input_digest.bytes().all(|v| v.is_ascii_hexdigit())
        {
            return Err(invalid("invalid native operation identity"));
        }
        fs::create_dir_all(root).map_err(io)?;
        let root = root.canonicalize().map_err(io)?;
        let lock_path = root.join(format!("{}.lock", intent.operation_id));
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(lock_path)
            .map_err(io)?;
        if !file.metadata().map_err(io)?.is_file() {
            return Err(invalid("native operation lock is not a regular file"));
        }
        let lock = Flock::lock(file, FlockArg::LockExclusiveNonblock).map_err(|(_, error)| {
            CatalogError::backend("native operation is already executing", error)
        })?;
        let path = root.join(format!("{}.json", intent.operation_id));
        let existing = match OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
            .open(&path)
        {
            Ok(file) => {
                if !file.metadata().map_err(io)?.is_file() {
                    return Err(invalid("native operation record is not a regular file"));
                }
                let mut bytes = Vec::new();
                file.take(4 * 1024 * 1024 + 1)
                    .read_to_end(&mut bytes)
                    .map_err(io)?;
                if bytes.len() > 4 * 1024 * 1024 {
                    return Err(invalid("oversized native operation record"));
                }
                let existing: NativeOperation = serde_json::from_slice(&bytes)
                    .map_err(|e| CatalogError::backend("decode native operation", e))?;
                if existing.version != 1 || existing.intent != intent {
                    return Err(invalid("native operation identity conflict"));
                }
                if existing.state == NativeOperationState::Completed && existing.receipt.is_none() {
                    return Err(invalid("completed native operation lacks receipt"));
                }
                if existing.receipt.as_ref().is_some_and(Value::is_null)
                    || (existing.state != NativeOperationState::Completed
                        && existing.receipt.is_some())
                {
                    return Err(invalid("native operation receipt does not match state"));
                }
                existing
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => NativeOperation {
                version: 1,
                intent,
                state: NativeOperationState::Intent,
                receipt: None,
                checkpoints: Default::default(),
            },
            Err(error) => return Err(io(error)),
        };
        let op = Self {
            path,
            record: existing,
            _lock: lock,
        };
        op.persist()?;
        Ok(op)
    }
    pub fn record(&self) -> &NativeOperation {
        &self.record
    }
    /// Persist immutable evidence before/after a native side effect. Replaying
    /// a checkpoint cannot replace its original input or captured artifact.
    pub fn checkpoint(&mut self, name: &str, value: Value) -> CatalogResult<()> {
        if !is_valid_volume_component(name) || value.is_null() {
            return Err(invalid("invalid operation checkpoint"));
        }
        if let Some(prior) = self.record.checkpoints.get(name) {
            return if prior == &value {
                Ok(())
            } else {
                Err(invalid("native checkpoint cannot change"))
            };
        }
        if self.record.state == NativeOperationState::Completed {
            return Err(invalid("completed operation is immutable"));
        }
        self.record.checkpoints.insert(name.to_owned(), value);
        if let Err(error) = self.persist() {
            self.record.checkpoints.remove(name);
            return Err(error);
        }
        Ok(())
    }
    pub fn begin(&mut self) -> CatalogResult<()> {
        if self.record.state != NativeOperationState::Intent {
            return Err(invalid(
                "native side effect already started; inspect the same operation",
            ));
        }
        self.set(NativeOperationState::Started, None)
    }
    pub fn mark_unknown(&mut self) -> CatalogResult<()> {
        if self.record.state != NativeOperationState::Started {
            return Err(invalid("only a started operation can become unknown"));
        }
        self.set(NativeOperationState::NeedsRecovery, None)
    }
    pub fn complete(&mut self, receipt: Value) -> CatalogResult<()> {
        if receipt.is_null() {
            return Err(invalid("completion requires a native receipt"));
        }
        if self.record.state == NativeOperationState::Completed {
            return if self.record.receipt.as_ref() == Some(&receipt) {
                Ok(())
            } else {
                Err(invalid("completed operation receipt cannot change"))
            };
        }
        if !matches!(
            self.record.state,
            NativeOperationState::Started | NativeOperationState::NeedsRecovery
        ) {
            return Err(invalid("operation was never dispatched"));
        }
        self.set(NativeOperationState::Completed, Some(receipt))
    }
    fn set(&mut self, state: NativeOperationState, receipt: Option<Value>) -> CatalogResult<()> {
        let old = self.record.clone();
        self.record.state = state;
        self.record.receipt = receipt;
        if let Err(error) = self.persist() {
            self.record = old;
            return Err(error);
        }
        Ok(())
    }
    fn persist(&self) -> CatalogResult<()> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| invalid("journal parent missing"))?;
        let bytes = serde_json::to_vec(&self.record)
            .map_err(|e| CatalogError::backend("serialize native operation", e))?;
        if bytes.len() > 4 * 1024 * 1024 {
            return Err(invalid("oversized native operation record"));
        }
        let mut temp = tempfile::NamedTempFile::new_in(parent).map_err(io)?;
        temp.write_all(&bytes).map_err(io)?;
        temp.as_file().sync_all().map_err(io)?;
        temp.persist(&self.path).map_err(|e| io(e.error))?;
        File::open(parent).and_then(|f| f.sync_all()).map_err(io)
    }
}
