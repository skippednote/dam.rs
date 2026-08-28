//! Where a migration's bytes come from.
//!
//! A transfer needs two things per record: the metadata, which the crosswalk already maps out of the JSON
//! lines the operator supplies, and the *bytes*, which have to be fetched from wherever the old system keeps
//! them. This is the second half.
//!
//! **The filesystem first, and not a vendor API.** §G7 names a comparator's public API as the obvious first
//! connector. It is the wrong first one: it cannot be reached from here, so it would be verified only against
//! its own fakes, and a migration verified against a fake is the kind of thing that is discovered to be wrong
//! while somebody's library is half-moved. A filesystem source is the shape most DAM exports actually take —
//! a metadata file next to a folder of assets — needs no credentials, and can be driven end to end against
//! real files. The trait is here so the second connector is an implementation rather than a refactor.
//!
//! **A reader, not a buffer.** `fetch` hands back something to read rather than the bytes themselves. A DAM
//! holds video; buffering an asset to move it would put a cap on the file size a migration can carry, decided
//! by whatever RAM the box running `damctl` happens to have. The transfer loop reads in chunks and streams
//! them into an upload session, so a 40 GB master costs the same memory as a thumbnail.

use std::path::{Component, Path, PathBuf};

use async_trait::async_trait;
use serde_json::{Map, Value};
use tokio::io::AsyncRead;

use crate::{Error, Result};

/// One asset's bytes, on their way in.
pub struct Fetched {
    /// What to call it. Becomes the upload's declared filename, so the existing path derives the extension
    /// and the MIME type from it exactly as it would for a browser upload.
    pub filename: String,
    /// How many bytes to expect, when the source knows.
    ///
    /// Passed to the session as the declared length, which is what lets the resumable engine refuse a
    /// stream that turns out longer than promised instead of writing it.
    pub len: Option<u64>,
    pub reader: Box<dyn AsyncRead + Send + Unpin>,
}

impl std::fmt::Debug for Fetched {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fetched")
            .field("filename", &self.filename)
            .field("len", &self.len)
            .finish_non_exhaustive()
    }
}

/// Somewhere a migration's bytes can be read from.
#[async_trait]
pub trait Source: Send + Sync {
    /// A short, stable name for the log line and the report.
    fn name(&self) -> &'static str;

    /// Opens the asset this record describes.
    ///
    /// Takes the whole record rather than a resolved path, because which field names the file is a property
    /// of the export and not of the connector — and a source that reaches an API will want a different field
    /// entirely.
    ///
    /// An `Err` here fails *this record*, not the run: one unreadable file in a 400k-asset extraction is a
    /// line in the report, and aborting on it would mean an operator discovers the problem by finding the
    /// migration stopped somewhere in the middle. Anything that would fail every record — a root that does
    /// not exist — is refused when the source is built instead, where it costs nothing to say so.
    async fn fetch(&self, record: &Map<String, Value>) -> Result<Fetched>;
}

/// A folder of files, with the records naming them by relative path.
#[derive(Debug, Clone)]
pub struct Filesystem {
    root: PathBuf,
    field: String,
}

impl Filesystem {
    /// Anchors a source at `root`, reading each record's path from `field`.
    ///
    /// The root is resolved and checked here rather than per record. A mistyped root is the single most
    /// likely operator error and it would otherwise present as every record failing for its own reason,
    /// four hundred thousand times.
    pub fn rooted(root: impl AsRef<Path>, field: impl Into<String>) -> Result<Self> {
        let root = root.as_ref();
        let resolved = std::fs::canonicalize(root).map_err(|error| {
            Error::Permanent(format!("source root {}: {error}", root.display()))
        })?;
        if !resolved.is_dir() {
            return Err(Error::Permanent(format!(
                "source root {} is not a directory",
                resolved.display()
            )));
        }
        Ok(Self {
            root: resolved,
            field: field.into(),
        })
    }

    /// Turns a record's relative path into a real one inside the root, or refuses.
    ///
    /// Two checks, and they catch different things. The component walk refuses `..` and absolute paths
    /// *before* touching the disk, which is the only check that can be made about a file that does not
    /// exist. Comparing the canonical path against the canonical root afterwards catches the case the walk
    /// cannot see: a symlink inside the export pointing out of it.
    ///
    /// Worth being strict about. The paths come from a file somebody exported from another system, which is
    /// to say from outside this one, and a transfer runs with whatever rights `damctl` was given. A path of
    /// `../../../etc/passwd` should read as a failed record and not as an asset.
    fn resolve(&self, relative: &str) -> Result<PathBuf> {
        let candidate = Path::new(relative);
        for component in candidate.components() {
            match component {
                Component::Normal(_) | Component::CurDir => {}
                Component::ParentDir => {
                    return Err(Error::Permanent(format!(
                        "{relative} leaves the source root"
                    )));
                }
                Component::RootDir | Component::Prefix(_) => {
                    return Err(Error::Permanent(format!(
                        "{relative} is not a relative path"
                    )));
                }
            }
        }

        let joined = self.root.join(candidate);
        let resolved = std::fs::canonicalize(&joined)
            .map_err(|error| Error::Permanent(format!("{relative}: {error}")))?;
        if !resolved.starts_with(&self.root) {
            return Err(Error::Permanent(format!(
                "{relative} resolves outside the source root"
            )));
        }
        Ok(resolved)
    }
}

#[async_trait]
impl Source for Filesystem {
    fn name(&self) -> &'static str {
        "filesystem"
    }

    async fn fetch(&self, record: &Map<String, Value>) -> Result<Fetched> {
        let relative = record
            .get(&self.field)
            .and_then(Value::as_str)
            .ok_or_else(|| {
                Error::Permanent(format!("no {} naming the file to transfer", self.field))
            })?;

        let path = self.resolve(relative)?;
        let file = tokio::fs::File::open(&path)
            .await
            .map_err(|error| Error::Permanent(format!("{relative}: {error}")))?;
        let len = file.metadata().await.ok().map(|meta| meta.len());

        // The name from the path rather than from a separate field: the export named this file, and a
        // migration that renamed assets on the way in would be answering a question nobody asked.
        let filename = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(relative)
            .to_owned();

        Ok(Fetched {
            filename,
            len,
            reader: Box::new(file),
        })
    }
}
