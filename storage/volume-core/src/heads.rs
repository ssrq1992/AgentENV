//! Durable native head publication. SSF authorizes use/maintenance; this store
//! only enforces repository generation CAS and immutable content descriptors.
use crate::{
    catalog::{CatalogError, CatalogResult},
    is_valid_volume_component, OverlaybdLayerRef,
};
use nix::fcntl::{Flock, FlockArg};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishedHead {
    pub version: u32,
    pub volume_id: String,
    pub revision: String,
    pub parent: Option<String>,
    pub capacity_bytes: u64,
    pub capacity_generation: u64,
    pub filesystem_uuid: String,
    pub layers: Vec<OverlaybdLayerRef>,
    pub operation_id: String,
    pub input_digest: String,
}
impl PublishedHead {
    pub fn seal(mut self) -> CatalogResult<Self> {
        self.revision.clear();
        self.revision = crate::digest::sha256_digest(&serde_json::to_vec(&self).map_err(error)?)
            .trim_start_matches("sha256:")
            .to_owned();
        self.validate()?;
        Ok(self)
    }
    pub fn validate_transition(
        &self,
        prior: Option<&Self>,
        expected: Option<&str>,
    ) -> CatalogResult<()> {
        if prior.map(|h| h.revision.as_str()) != expected || self.parent.as_deref() != expected {
            return Err(invalid("native head publication CAS conflict"));
        }
        if let Some(prior) = prior {
            if self.capacity_bytes < prior.capacity_bytes
                || (self.capacity_bytes == prior.capacity_bytes
                    && self.capacity_generation != prior.capacity_generation)
                || (self.capacity_bytes > prior.capacity_bytes
                    && self.capacity_generation
                        != prior
                            .capacity_generation
                            .checked_add(1)
                            .ok_or_else(|| invalid("capacity generation overflow"))?)
                || self.filesystem_uuid != prior.filesystem_uuid
            {
                return Err(invalid(
                    "native capacity generation or filesystem identity conflict",
                ));
            }
        } else if self.capacity_generation != 1 {
            return Err(invalid("initial capacity generation must be one"));
        }
        Ok(())
    }
    pub fn validate(&self) -> CatalogResult<()> {
        if self.version != 1
            || !is_valid_volume_component(&self.volume_id)
            || !is_valid_volume_component(&self.operation_id)
            || !hash(&self.input_digest)
            || !hash(&self.revision)
            || self.parent.as_ref().is_some_and(|v| !hash(v))
            || self.capacity_bytes == 0
            || self.capacity_bytes % 512 != 0
            || self.capacity_generation == 0
            || self.layers.is_empty()
            || uuid::Uuid::parse_str(&self.filesystem_uuid).map_or(true, |u| {
                u.is_nil() || u.to_string() != self.filesystem_uuid
            })
        {
            return Err(invalid("invalid native published head"));
        }
        let mut body = self.clone();
        body.revision.clear();
        let computed = crate::digest::sha256_digest(&serde_json::to_vec(&body).map_err(error)?);
        if computed.trim_start_matches("sha256:") != self.revision {
            return Err(invalid("native head digest mismatch"));
        }
        Ok(())
    }
}
#[derive(Clone)]
pub struct PosixHeadStore {
    root: PathBuf,
}
impl PosixHeadStore {
    pub fn new(root: PathBuf) -> CatalogResult<Self> {
        if !root.is_absolute() {
            return Err(invalid("absolute managed head root required"));
        }
        Ok(Self { root })
    }
    fn directory(&self, id: &str) -> CatalogResult<PathBuf> {
        if !is_valid_volume_component(id) {
            return Err(invalid("invalid managed volume identity"));
        }
        Ok(self.root.join(id))
    }
    pub fn read(&self, id: &str, revision: &str) -> CatalogResult<Option<PublishedHead>> {
        if !hash(revision) {
            return Err(invalid("invalid head revision"));
        }
        let head = read_head(
            &self
                .directory(id)?
                .join("heads")
                .join(format!("{revision}.json")),
        )?;
        if head
            .as_ref()
            .is_some_and(|h| h.volume_id != id || h.revision != revision)
        {
            return Err(invalid("head identity mismatch"));
        }
        Ok(head)
    }
    pub fn current(&self, id: &str) -> CatalogResult<Option<PublishedHead>> {
        let head = read_head(&self.directory(id)?.join("current.json"))?;
        if head.as_ref().is_some_and(|h| h.volume_id != id) {
            return Err(invalid("current head identity mismatch"));
        }
        Ok(head)
    }
    pub fn publish(&self, expected: Option<&str>, head: &PublishedHead) -> CatalogResult<()> {
        head.validate()?;
        let directory = self.directory(&head.volume_id)?;
        fs::create_dir_all(directory.join("heads")).map_err(error)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(directory.join("publish.lock"))
            .map_err(error)?;
        let _lock =
            Flock::lock(file, FlockArg::LockExclusiveNonblock).map_err(|(_, e)| error(e))?;
        let prior = self.current(&head.volume_id)?;
        if prior.as_ref() == Some(head) {
            return Ok(());
        }
        head.validate_transition(prior.as_ref(), expected)?;
        let path = directory
            .join("heads")
            .join(format!("{}.json", head.revision));
        match read_head(&path)? {
            Some(existing) if existing != *head => {
                return Err(invalid("immutable native head conflict"))
            }
            Some(_) => (),
            None => write_head(&path, head)?,
        }
        // Pin the immutable head before making it current. A crash leaves a retained
        // head or the old current pointer, never a pointer to an absent descriptor.
        write_head(&directory.join("current.json"), head)?;
        File::open(&self.root)
            .and_then(|f| f.sync_all())
            .map_err(error)
    }
}
fn hash(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn invalid(reason: &str) -> CatalogError {
    CatalogError::InvalidRequest {
        reason: reason.into(),
    }
}
fn error(e: impl Into<anyhow::Error>) -> CatalogError {
    CatalogError::backend("native head repository", e)
}
fn read_head(path: &Path) -> CatalogResult<Option<PublishedHead>> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(error(e)),
    };
    if !file.metadata().map_err(error)?.is_file() {
        return Err(invalid("head is not a regular file"));
    }
    let mut bytes = Vec::new();
    file.take(4 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(error)?;
    if bytes.len() > 4 * 1024 * 1024 {
        return Err(invalid("native head too large"));
    }
    let head: PublishedHead = serde_json::from_slice(&bytes).map_err(error)?;
    head.validate()?;
    Ok(Some(head))
}
fn write_head(path: &Path, head: &PublishedHead) -> CatalogResult<()> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid("missing head parent"))?;
    let mut temp = tempfile::NamedTempFile::new_in(parent).map_err(error)?;
    temp.write_all(&serde_json::to_vec(head).map_err(error)?)
        .map_err(error)?;
    temp.as_file().sync_all().map_err(error)?;
    temp.persist(path).map_err(|e| error(e.error))?;
    File::open(parent).and_then(|f| f.sync_all()).map_err(error)
}
