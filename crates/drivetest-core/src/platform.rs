//! The boundary between the engine and the operating system.
//!
//! `drivetest-win` implements [`Platform`] with real Windows calls;
//! [`crate::demo::DemoPlatform`] implements it with simulated drives.
//!
//! Note the shape of the write path: [`Platform::open_write`] and
//! [`Platform::lock_volumes`] take a [`WriteAuthorization`], which only
//! [`crate::policy::Policy`] can create. Front ends therefore cannot obtain a
//! writable raw handle without going through the policy.

use std::any::Any;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::health::HealthSnapshot;
use crate::model::{DriveInfo, VolumeInfo};
use crate::policy::WriteAuthorization;

/// Errors from the platform layer.
#[derive(Debug, thiserror::Error)]
pub enum PlatformError {
    #[error("not supported: {0}")]
    Unsupported(String),
    #[error("access denied: {0}")]
    AccessDenied(String),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("{0}")]
    Other(String),
}

/// Kind of drive self-test to start.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SelfTestKind {
    Short,
    Extended,
}

impl SelfTestKind {
    pub fn label(self) -> &'static str {
        match self {
            SelfTestKind::Short => "short",
            SelfTestKind::Extended => "extended",
        }
    }
}

/// A bad sector traced to the file that occupies it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SectorFile {
    pub lba: u64,
    /// Mount point plus file path, or a description such as "free space".
    pub location: String,
}

/// A backend-confirmed sector read error. Generic I/O failures carry their original error.
#[derive(Debug, thiserror::Error)]
#[error("unreadable sector: {0}")]
pub struct MediaReadError(pub io::Error);

pub fn is_media_read_error(error: &io::Error) -> bool {
    error.get_ref().is_some_and(|cause| cause.is::<MediaReadError>())
}

/// Read access to a raw block device.
pub trait RawReader: Send {
    fn size_bytes(&self) -> u64;
    fn logical_sector_size(&self) -> u32;
    /// Reads exactly `buf.len()` bytes at `offset`. `buf` is sector aligned.
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()>;
    /// Simulated devices report the latency they model; real devices return
    /// `None` and the engine measures wall-clock time instead.
    fn simulated_latency(&self) -> Option<Duration> {
        None
    }
}

/// Read and write access to a raw block device. Only reachable through the policy.
pub trait RawWriter: RawReader {
    /// Writes exactly `buf.len()` bytes at `offset`. `buf` is sector aligned.
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> io::Result<()>;
    fn flush(&mut self) -> io::Result<()>;
}

/// Operating-system services the engine needs.
pub trait Platform: Send + Sync {
    /// Short name shown in the UI, e.g. "Windows" or "Demo".
    fn name(&self) -> &'static str;

    /// Lists physical drives with their system-disk role already decided.
    fn enumerate(&self) -> Result<Vec<DriveInfo>, PlatformError>;

    /// Opens a drive read-only.
    fn open_read(&self, disk: u32) -> io::Result<Box<dyn RawReader>>;

    /// Opens a drive for raw writes. Requires a policy-issued authorization.
    fn open_write(&self, auth: &WriteAuthorization) -> io::Result<Box<dyn RawWriter>>;

    /// Locks and dismounts every volume on the authorized drive. Dropping the
    /// returned guard unlocks them.
    fn lock_volumes(&self, auth: &WriteAuthorization) -> io::Result<Box<dyn Any + Send>>;

    /// Reads SMART or NVMe health. Never writes user data.
    fn read_health(&self, disk: u32) -> Result<HealthSnapshot, PlatformError>;

    /// Starts the drive's built-in self-test. The drive does the work.
    fn start_self_test(&self, disk: u32, kind: SelfTestKind) -> Result<(), PlatformError>;

    /// Keeps the computer awake while the returned guard lives. Must be
    /// created and dropped on the same (worker) thread.
    fn keep_awake(&self) -> Box<dyn Any> {
        Box::new(())
    }

    /// Free bytes on a mounted volume.
    fn volume_free_bytes(&self, root: &Path) -> io::Result<u64>;

    /// Root directory of a mounted volume for file-level work. Windows uses
    /// the volume GUID path, which cannot be redirected to a different volume.
    fn volume_root(&self, volume: &VolumeInfo) -> PathBuf {
        PathBuf::from(volume.mount_points.first().cloned().unwrap_or_else(|| volume.id.clone()))
    }

    /// Confirms that `dir` really lives on the volume with id `volume_id`.
    fn path_is_on_volume(&self, _dir: &Path, _volume_id: &str) -> io::Result<bool> {
        Ok(true)
    }

    /// Opens a test file for unbuffered I/O (bypassing the OS cache).
    fn open_test_file(&self, path: &Path, write: bool) -> io::Result<File> {
        let mut options = std::fs::OpenOptions::new();
        if write {
            options.write(true).create_new(true);
        } else {
            options.read(true);
        }
        options.open(path)
    }

    /// Opens an existing file for reading without the OS cache, so timings and
    /// read-back checks reflect the drive.
    fn open_unbuffered_read(&self, path: &Path) -> io::Result<File> {
        File::open(path)
    }

    /// Opens an existing file for an in-place refresh: read + write, other
    /// writers blocked, writes going straight through to the drive.
    fn open_for_refresh(&self, path: &Path) -> io::Result<File> {
        std::fs::OpenOptions::new().read(true).write(true).open(path)
    }

    /// Reads up to `max_bytes` from the start of a file, bypassing the cache,
    /// and returns (bytes read, time taken).
    fn time_file_read(&self, path: &Path, max_bytes: u64) -> io::Result<(u64, Duration)> {
        let mut file = self.open_unbuffered_read(path)?;
        let mut buf = crate::aligned::AlignedBuf::new(1 << 20);
        let start = std::time::Instant::now();
        let mut total = 0u64;
        while total < max_bytes {
            let n = crate::aligned::read_until_eof(&mut file, &mut buf)?;
            total += n as u64;
            if n < buf.len() {
                break;
            }
        }
        Ok((total, start.elapsed()))
    }

    /// Where an opened file really is, as a path relative to the volume root,
    /// after resolving links, junctions and short (8.3) names. Errors when the
    /// file is not on the volume `volume_id` rooted at `root`.
    fn resolve_on_volume(&self, _file: &File, path: &Path, root: &Path, _volume_id: &str) -> io::Result<PathBuf> {
        let real = std::fs::canonicalize(path)?;
        let base = std::fs::canonicalize(root)?;
        real.strip_prefix(&base)
            .map(Path::to_path_buf)
            .map_err(|_| io::Error::new(io::ErrorKind::PermissionDenied, "file is outside the selected volume"))
    }

    /// Restores a file's attributes (e.g. the Archive bit) after a refresh.
    fn restore_attributes(&self, _path: &Path, _attributes: u32) -> io::Result<()> {
        Ok(())
    }

    /// Traces bad sectors to the files that occupy them (NTFS only).
    fn map_sectors_to_files(&self, _disk: u32, _lbas: &[u64]) -> Result<Vec<SectorFile>, PlatformError> {
        Err(PlatformError::Unsupported("file mapping is not available on this platform".into()))
    }

    /// True when the process has the rights raw disk access needs.
    fn is_elevated(&self) -> bool {
        true
    }
}
