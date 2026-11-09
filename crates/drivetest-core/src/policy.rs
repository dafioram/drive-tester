//! The safety policy.
//!
//! Guarantees enforced here, independent of any front end:
//!
//! 1. Read-only tests are allowed on every drive.
//! 2. Any test that writes needs [`Mode::ReadWrite`].
//! 3. Nothing is ever written to a drive whose [`SystemRole`] is not `Data`.
//!    `Undetermined` counts as a system disk, so detection failures fail closed.
//!    There is no override.
//! 4. Destructive tests additionally need the drive's serial number typed by
//!    the user, a fresh re-check that the same drive is still at that position,
//!    and every volume on the drive locked and dismounted before the first write.
//!
//! The only way to obtain a writable raw handle is [`Policy::begin_destructive`],
//! because [`WriteAuthorization`] cannot be constructed outside this module.

use std::any::Any;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::model::{DriveInfo, SystemRole, VolumeInfo, normalize_serial};
use crate::platform::{Platform, RawReader, RawWriter};

/// Global application mode. The app always starts in `ReadOnly`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    ReadOnly,
    ReadWrite,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Mode::ReadOnly => "Read-only",
            Mode::ReadWrite => "Read-write",
        }
    }
}

/// How invasive a test is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// Raw device opened read-only, or commands that only read state.
    ReadOnly,
    /// Writes through the file system into free space; user data untouched.
    NonDestructive,
    /// Raw writes that destroy the drive's contents.
    Destructive,
}

impl Tier {
    pub fn label(self) -> &'static str {
        match self {
            Tier::ReadOnly => "Read-only",
            Tier::NonDestructive => "Non-destructive write",
            Tier::Destructive => "Destructive",
        }
    }
}

/// Why the policy refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PolicyError {
    #[error("Blocked: this is the system disk ({0}). Only read-only tests are allowed.")]
    SystemDisk(String),
    #[error("Blocked: could not confirm this is not the system disk ({0}). Writes are refused when in doubt.")]
    Undetermined(String),
    #[error("Requires Read-write mode.")]
    ReadOnlyMode,
    #[error("Blocked: the drive is write-protected.")]
    WriteProtected,
    #[error("The drive reports no serial number, so it cannot be confirmed for a destructive test.")]
    NoSerial,
    #[error("The serial number typed does not match this drive.")]
    SerialMismatch,
    #[error("The drive changed since it was selected: {0}")]
    DriveChanged(String),
    #[error("Volume {0} is not on this drive.")]
    ForeignVolume(String),
    #[error(
        "On the system disk ({0}), file refresh needs the explicit opt-in and runs only on selected files, such as the slow files found by the stale-data check."
    )]
    SystemRefreshRestricted(String),
    #[error("Volume {0} spans several disks; write tests are not supported on spanned or striped volumes.")]
    SpannedVolume(String),
    #[error("Could not lock the drive's volumes: {0}. Close programs using the drive and try again.")]
    LockFailed(String),
    #[error("Could not open the drive: {0}")]
    Open(String),
    #[error("Could not re-check the drive: {0}")]
    Platform(String),
}

/// Private marker so the authorizations below cannot be built elsewhere.
#[derive(Debug)]
struct Sealed;

/// Proof that the policy approved raw writes to one specific drive.
///
/// Not `Clone`, not constructible outside this module.
#[derive(Debug)]
pub struct WriteAuthorization {
    disk: u32,
    serial: String,
    size_bytes: u64,
    _sealed: Sealed,
}

impl WriteAuthorization {
    pub fn disk_number(&self) -> u32 {
        self.disk
    }

    /// Serial the user confirmed; platforms re-check it on the opened handle.
    pub fn serial(&self) -> &str {
        &self.serial
    }

    /// Capacity at confirmation time; platforms re-check it on the opened handle.
    pub fn size_bytes(&self) -> u64 {
        self.size_bytes
    }
}

/// Proof that the policy approved a free-space test on one mounted volume.
#[derive(Debug)]
pub struct FreeSpaceAuthorization {
    disk: u32,
    mount: String,
    volume: VolumeInfo,
    _sealed: Sealed,
}

impl FreeSpaceAuthorization {
    pub fn disk_number(&self) -> u32 {
        self.disk
    }

    /// Mount point the user chose (for display).
    pub fn mount(&self) -> &str {
        &self.mount
    }

    /// The volume as re-read at authorization time. Its `id` (the volume GUID
    /// path on Windows) is what the test writes through.
    pub fn volume(&self) -> &VolumeInfo {
        &self.volume
    }
}

/// Upper limit on files in a targeted refresh of the system disk.
pub const MAX_SYSTEM_REFRESH_FILES: usize = 2000;

/// Proof that the policy approved an in-place file refresh on one volume.
#[derive(Debug)]
pub struct FileRefreshAuthorization {
    disk: u32,
    mount: String,
    volume: VolumeInfo,
    on_system_disk: bool,
    _sealed: Sealed,
}

impl FileRefreshAuthorization {
    pub fn disk_number(&self) -> u32 {
        self.disk
    }

    pub fn mount(&self) -> &str {
        &self.mount
    }

    pub fn volume(&self) -> &VolumeInfo {
        &self.volume
    }

    /// The system-disk guardrails (excluded folders) apply.
    pub fn on_system_disk(&self) -> bool {
        self.on_system_disk
    }
}

/// An open destructive session: volumes locked, raw writer open.
///
/// Field order matters: the writer is dropped (handle closed) before the
/// volume locks are released.
pub struct DestructiveSession {
    writer: Box<dyn RawWriter>,
    _locks: Box<dyn Any + Send>,
    drive: DriveInfo,
}

impl DestructiveSession {
    /// The writer, borrowed: it cannot be moved out and outlive the locks.
    pub fn writer(&mut self) -> &mut dyn RawWriter {
        &mut *self.writer
    }

    /// The drive as re-read at the moment the session started.
    pub fn drive(&self) -> &DriveInfo {
        &self.drive
    }
}

/// The safety policy for one run.
#[derive(Debug, Clone, Copy, Default)]
pub struct Policy {
    mode: Mode,
}

impl Policy {
    pub fn new(mode: Mode) -> Self {
        Self { mode }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Decides whether a tier is allowed on a drive. Front ends call this to
    /// enable or explain buttons; the engines call it again before acting.
    pub fn check(&self, drive: &DriveInfo, tier: Tier) -> Result<(), PolicyError> {
        if tier == Tier::ReadOnly {
            return Ok(());
        }
        // The system-disk rule is checked first so its message wins: it is
        // the one rule no mode change can lift.
        match &drive.role {
            SystemRole::Data => {}
            SystemRole::System { reasons } => return Err(PolicyError::SystemDisk(reasons.join("; "))),
            SystemRole::Undetermined { reason } => return Err(PolicyError::Undetermined(reason.clone())),
        }
        if self.mode != Mode::ReadWrite {
            return Err(PolicyError::ReadOnlyMode);
        }
        if drive.write_protected {
            return Err(PolicyError::WriteProtected);
        }
        Ok(())
    }

    /// Opens a drive read-only. Allowed on every drive.
    pub fn open_read(&self, platform: &dyn Platform, drive: &DriveInfo) -> Result<Box<dyn RawReader>, PolicyError> {
        platform.open_read(drive.number).map_err(|e| PolicyError::Open(e.to_string()))
    }

    /// Approves a free-space test on one of the drive's mounted volumes.
    ///
    /// Re-reads the drive list first, so a stale selection (drive swapped, drive
    /// letter reassigned, volume removed) is refused rather than written to.
    pub fn authorize_free_space(
        &self,
        platform: &dyn Platform,
        drive: &DriveInfo,
        mount: &str,
    ) -> Result<FreeSpaceAuthorization, PolicyError> {
        self.check(drive, Tier::NonDestructive)?;
        let volume = drive.volume_by_mount(mount).ok_or_else(|| PolicyError::ForeignVolume(mount.to_string()))?;
        if !volume.other_disks.is_empty() {
            return Err(PolicyError::SpannedVolume(volume.display_name()));
        }
        let fresh = self.fresh_drive(platform, drive)?;
        self.check(&fresh, Tier::NonDestructive)?;
        let fresh_volume = fresh
            .volume_by_mount(mount)
            .filter(|v| v.id == volume.id)
            .ok_or_else(|| PolicyError::DriveChanged(format!("{mount} no longer points to the same volume")))?
            .clone();
        if !fresh_volume.other_disks.is_empty() {
            return Err(PolicyError::SpannedVolume(fresh_volume.display_name()));
        }
        Ok(FreeSpaceAuthorization {
            disk: fresh.number,
            mount: mount.to_string(),
            volume: fresh_volume,
            _sealed: Sealed,
        })
    }

    /// Approves an in-place file refresh (rewriting files with their own bytes,
    /// through the file system).
    ///
    /// Data drives: needs Read-write mode. The system disk is the one opt-in
    /// exception to "never write to the system disk": it needs Read-write mode,
    /// `system_opt_in`, and a targeted list of at most [`MAX_SYSTEM_REFRESH_FILES`]
    /// files (`targeted_files`); the refresh
    /// itself then skips Windows, program and system files.
    pub fn authorize_file_refresh(
        &self,
        platform: &dyn Platform,
        drive: &DriveInfo,
        mount: &str,
        system_opt_in: bool,
        targeted_files: Option<usize>,
    ) -> Result<FileRefreshAuthorization, PolicyError> {
        let targeted = targeted_files.is_some_and(|n| (1..=MAX_SYSTEM_REFRESH_FILES).contains(&n));
        if self.mode != Mode::ReadWrite {
            return Err(PolicyError::ReadOnlyMode);
        }
        let check_role = |d: &DriveInfo| -> Result<bool, PolicyError> {
            if d.write_protected {
                return Err(PolicyError::WriteProtected);
            }
            match &d.role {
                SystemRole::Data => Ok(false),
                SystemRole::System { .. } | SystemRole::Undetermined { .. } if system_opt_in && targeted => Ok(true),
                SystemRole::System { reasons } => Err(PolicyError::SystemRefreshRestricted(reasons.join("; "))),
                SystemRole::Undetermined { reason } => Err(PolicyError::SystemRefreshRestricted(reason.clone())),
            }
        };
        check_role(drive)?;
        let volume = drive.volume_by_mount(mount).ok_or_else(|| PolicyError::ForeignVolume(mount.to_string()))?;
        if !volume.other_disks.is_empty() {
            return Err(PolicyError::SpannedVolume(volume.display_name()));
        }
        let fresh = self.fresh_drive(platform, drive)?;
        let on_system_disk = check_role(&fresh)?;
        let fresh_volume = fresh
            .volume_by_mount(mount)
            .filter(|v| v.id == volume.id)
            .ok_or_else(|| PolicyError::DriveChanged(format!("{mount} no longer points to the same volume")))?
            .clone();
        if !fresh_volume.other_disks.is_empty() {
            return Err(PolicyError::SpannedVolume(fresh_volume.display_name()));
        }
        Ok(FileRefreshAuthorization {
            disk: fresh.number,
            mount: mount.to_string(),
            volume: fresh_volume,
            on_system_disk,
            _sealed: Sealed,
        })
    }

    /// Re-reads one drive and confirms it is still the same physical drive.
    fn fresh_drive(&self, platform: &dyn Platform, drive: &DriveInfo) -> Result<DriveInfo, PolicyError> {
        let fresh = platform
            .enumerate()
            .map_err(|e| PolicyError::Platform(e.to_string()))?
            .into_iter()
            .find(|d| d.number == drive.number)
            .ok_or_else(|| PolicyError::DriveChanged("the drive is no longer present".into()))?;
        if normalize_serial(&fresh.serial) != normalize_serial(&drive.serial) {
            return Err(PolicyError::DriveChanged("a different drive is now at this position".into()));
        }
        if fresh.size_bytes != drive.size_bytes {
            return Err(PolicyError::DriveChanged("the reported capacity changed".into()));
        }
        Ok(fresh)
    }

    /// Starts a destructive session after every check passes:
    /// mode, system-disk role, write protection, typed serial, a fresh
    /// re-enumeration confirming the same drive, then volume lock and dismount.
    pub fn begin_destructive(
        &self,
        platform: &dyn Platform,
        drive: &DriveInfo,
        typed_serial: &str,
    ) -> Result<DestructiveSession, PolicyError> {
        self.check(drive, Tier::Destructive)?;
        if normalize_serial(&drive.serial).is_empty() {
            return Err(PolicyError::NoSerial);
        }
        if !drive.serial_matches(typed_serial) {
            return Err(PolicyError::SerialMismatch);
        }
        if let Some(v) = drive.volumes.iter().find(|v| !v.other_disks.is_empty()) {
            return Err(PolicyError::SpannedVolume(v.display_name()));
        }

        // Re-read the drive list: the selection could be stale (drive swapped,
        // re-numbered, or a volume was added since the UI last looked).
        let fresh = self.fresh_drive(platform, drive)?;
        self.check(&fresh, Tier::Destructive)?;
        if let Some(v) = fresh.volumes.iter().find(|v| !v.other_disks.is_empty()) {
            return Err(PolicyError::SpannedVolume(v.display_name()));
        }

        let auth = WriteAuthorization {
            disk: fresh.number,
            serial: fresh.serial.clone(),
            size_bytes: fresh.size_bytes,
            _sealed: Sealed,
        };
        let locks = platform.lock_volumes(&auth).map_err(|e| PolicyError::LockFailed(e.to_string()))?;
        let writer = platform.open_write(&auth).map_err(|e| PolicyError::Open(e.to_string()))?;
        Ok(DestructiveSession { writer, _locks: locks, drive: fresh })
    }
}

/// Resolves the directory a free-space test may write into.
pub fn free_space_root(platform: &dyn Platform, auth: &FreeSpaceAuthorization) -> PathBuf {
    platform.volume_root(auth.volume())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demo::{DemoPlatform, sample_drive};

    fn data_drive() -> DriveInfo {
        sample_drive()
    }

    #[test]
    fn read_only_is_always_allowed() {
        let policy = Policy::new(Mode::ReadOnly);
        let mut d = data_drive();
        d.role = SystemRole::System { reasons: vec!["holds Windows".into()] };
        assert!(policy.check(&d, Tier::ReadOnly).is_ok());
    }

    #[test]
    fn writes_need_read_write_mode() {
        let d = data_drive();
        assert_eq!(Policy::new(Mode::ReadOnly).check(&d, Tier::NonDestructive), Err(PolicyError::ReadOnlyMode));
        assert!(Policy::new(Mode::ReadWrite).check(&d, Tier::Destructive).is_ok());
    }

    #[test]
    fn system_and_undetermined_disks_are_never_writable() {
        let policy = Policy::new(Mode::ReadWrite);
        let mut d = data_drive();
        d.role = SystemRole::System { reasons: vec!["holds Windows".into()] };
        for tier in [Tier::NonDestructive, Tier::Destructive] {
            assert!(matches!(policy.check(&d, tier), Err(PolicyError::SystemDisk(_))));
        }
        d.role = SystemRole::Undetermined { reason: "boot partition not found".into() };
        for tier in [Tier::NonDestructive, Tier::Destructive] {
            assert!(matches!(policy.check(&d, tier), Err(PolicyError::Undetermined(_))));
        }
    }

    #[test]
    fn write_protected_drives_are_refused() {
        let mut d = data_drive();
        d.write_protected = true;
        assert_eq!(Policy::new(Mode::ReadWrite).check(&d, Tier::Destructive), Err(PolicyError::WriteProtected));
    }

    #[test]
    fn destructive_needs_matching_serial() {
        let platform = DemoPlatform::new_fast();
        let drives = platform.enumerate().unwrap();
        let usb = drives.iter().find(|d| d.bus == crate::model::BusType::Usb).unwrap();
        let policy = Policy::new(Mode::ReadWrite);
        assert!(matches!(policy.begin_destructive(&platform, usb, "WRONG"), Err(PolicyError::SerialMismatch)));
        assert!(policy.begin_destructive(&platform, usb, &usb.serial).is_ok());
    }

    #[test]
    fn destructive_refuses_system_disk_even_with_serial() {
        let platform = DemoPlatform::new_fast();
        let drives = platform.enumerate().unwrap();
        let system = drives.iter().find(|d| d.is_protected()).unwrap();
        let policy = Policy::new(Mode::ReadWrite);
        assert!(matches!(policy.begin_destructive(&platform, system, &system.serial), Err(PolicyError::SystemDisk(_))));
    }

    #[test]
    fn destructive_detects_swapped_drive() {
        let platform = DemoPlatform::new_fast();
        let drives = platform.enumerate().unwrap();
        let mut stale = drives.iter().find(|d| d.bus == crate::model::BusType::Usb).unwrap().clone();
        stale.serial = "OLD-SERIAL".into();
        let policy = Policy::new(Mode::ReadWrite);
        assert!(matches!(policy.begin_destructive(&platform, &stale, "OLD-SERIAL"), Err(PolicyError::DriveChanged(_))));
    }

    #[test]
    fn spanned_volumes_are_refused() {
        let mut d = data_drive();
        d.volumes[0].other_disks = vec![0];
        let mount = d.volumes[0].mount_points[0].clone();
        let policy = Policy::new(Mode::ReadWrite);
        let platform = DemoPlatform::new_fast();
        assert!(matches!(policy.authorize_free_space(&platform, &d, &mount), Err(PolicyError::SpannedVolume(_))));
        assert!(matches!(policy.begin_destructive(&platform, &d, &d.serial), Err(PolicyError::SpannedVolume(_))));
    }

    #[test]
    fn free_space_needs_a_volume_on_this_drive() {
        let d = data_drive();
        let platform = DemoPlatform::new_fast();
        let policy = Policy::new(Mode::ReadWrite);
        assert!(matches!(policy.authorize_free_space(&platform, &d, "Q:\\"), Err(PolicyError::ForeignVolume(_))));
        let mount = d.volumes[0].mount_points[0].clone();
        assert!(policy.authorize_free_space(&platform, &d, &mount.to_lowercase()).is_ok());
    }

    #[test]
    fn free_space_refuses_a_reassigned_drive_letter() {
        let mut d = data_drive();
        d.volumes[0].id = "\\\\?\\Volume{some-other-volume}\\".into();
        let mount = d.volumes[0].mount_points[0].clone();
        let platform = DemoPlatform::new_fast();
        let policy = Policy::new(Mode::ReadWrite);
        assert!(matches!(policy.authorize_free_space(&platform, &d, &mount), Err(PolicyError::DriveChanged(_))));
    }
}
