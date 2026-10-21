//! Reuse preparation only while the private input and selected executable remain
//! the same files. These records travel in the owner's private worker job, never
//! in client requests or the persistent segment cache.
use super::{hash_bytes, run, TIMEOUT};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::SystemTime,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FileStamp {
    bytes: u64,
    modified: SystemTime,
    readonly: bool,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    changed_seconds: i64,
    #[cfg(unix)]
    changed_nanos: i64,
    #[cfg(unix)]
    mode: u32,
    // Non-Unix filesystems do not expose an equivalent change clock here. Keep
    // content verification rather than silently weakening the reuse check.
    #[cfg(not(unix))]
    digest: String,
}

impl FileStamp {
    pub(crate) fn read(path: &Path) -> Result<Self> {
        let metadata = fs::metadata(path)?;
        ensure!(metadata.is_file(), "expected a regular file");
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Ok(Self {
            bytes: metadata.len(),
            modified: metadata.modified()?,
            readonly: metadata.permissions().readonly(),
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
            #[cfg(unix)]
            changed_seconds: metadata.ctime(),
            #[cfg(unix)]
            changed_nanos: metadata.ctime_nsec(),
            #[cfg(unix)]
            mode: metadata.mode(),
            #[cfg(not(unix))]
            digest: super::hash_file(path)?,
        })
    }

    pub(crate) fn check(&self, path: &Path) -> Result<()> {
        ensure!(
            *self == Self::read(path)?,
            "prepared file changed; prepare it again"
        );
        Ok(())
    }

    pub(crate) fn readonly(&self) -> bool {
        self.readonly
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PreparedEncoder {
    path: PathBuf,
    stamp: FileStamp,
    pub(crate) identity: String,
}

impl PreparedEncoder {
    pub(crate) fn capture() -> Result<Self> {
        let path = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|directory| directory.join(format!("ffmpeg{}", std::env::consts::EXE_SUFFIX)))
            .find(|candidate| {
                let Ok(metadata) = fs::metadata(candidate) else {
                    return false;
                };
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
                }
                #[cfg(not(unix))]
                {
                    metadata.is_file()
                }
            })
            .context("could not find FFmpeg; install it and ensure it is on PATH")?
            .canonicalize()?;
        let stamp = FileStamp::read(&path)?;
        let identity = hash_bytes(&run(
            Command::new(&path).arg("-version"),
            TIMEOUT,
            1_048_576,
        )?);
        stamp.check(&path)?;
        Ok(Self {
            path,
            stamp,
            identity,
        })
    }

    pub(crate) fn check(&self) -> Result<()> {
        self.stamp.check(&self.path)
    }
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
    pub(crate) fn command(&self) -> Command {
        crate::process::ffmpeg_at(&self.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_reuse_requires_the_unchanged_readonly_prepared_file() -> Result<()> {
        use crate::media::Source;
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("input.flac");
        crate::proof::fixture(&path, 4800, 2, 24)?;
        let writable = Source::open(&path)?;
        assert!(writable.preparation_stamp().is_err());
        let mut permissions = fs::metadata(&path)?.permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&path, permissions)?;
        let prepared = Source::open(&path)?;
        let stamp = prepared.preparation_stamp()?;
        let stamp = serde_json::from_slice(&serde_json::to_vec(&stamp)?)?;
        let resumed = Source::resume(&path, prepared.info.clone(), stamp)?;
        assert_eq!(resumed.info, prepared.info);
        let mut wrong_size = prepared.info.clone();
        wrong_size.bytes += 1;
        assert!(Source::resume(&path, wrong_size, prepared.preparation_stamp()?).is_err());
        fs::remove_file(&path)?;
        assert!(resumed.check_unchanged().is_err());
        Ok(())
    }

    #[test]
    fn stamp_survives_serialization_and_reads_but_rejects_replacement() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("file");
        fs::write(&path, b"original")?;
        let stamp = FileStamp::read(&path)?;
        let restored: FileStamp = serde_json::from_slice(&serde_json::to_vec(&stamp)?)?;
        assert_eq!(fs::read(&path)?, b"original");
        restored.check(&path)?;
        let replacement = dir.path().join("replacement");
        fs::write(&replacement, b"original")?;
        fs::File::options()
            .write(true)
            .open(&replacement)?
            .set_modified(stamp.modified)?;
        fs::rename(replacement, &path)?;
        #[cfg(unix)]
        assert!(restored.check(&path).is_err());
        Ok(())
    }

    #[test]
    fn stamp_rejects_same_length_edit_with_restored_mtime() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("file");
        fs::write(&path, b"original")?;
        let stamp = FileStamp::read(&path)?;
        std::thread::sleep(std::time::Duration::from_millis(2));
        fs::write(&path, b"modified")?;
        fs::File::options()
            .write(true)
            .open(&path)?
            .set_modified(stamp.modified)?;
        assert_eq!(fs::metadata(&path)?.len(), stamp.bytes);
        assert_eq!(fs::metadata(&path)?.modified()?, stamp.modified);
        assert!(stamp.check(&path).is_err());
        Ok(())
    }
}
