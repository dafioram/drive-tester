//! Windows implementation of the Drive Test platform layer.
//!
//! All `unsafe` Windows calls live in this crate. It implements
//! [`drivetest_core::platform::Platform`]:
//!
//! * enumeration of `\\.\PhysicalDriveN` with bus, serial, sector sizes and
//!   media type, and **system-disk detection** (Windows folder, boot / EFI
//!   partition, pagefile, hibernation and swap files);
//! * unbuffered raw reads, and raw writes only with a policy-issued
//!   [`drivetest_core::policy::WriteAuthorization`] plus a fresh re-check here;
//! * SMART over ATA pass-through, SMART over SAT (USB enclosures) and the
//!   NVMe health log;
//! * volume lock / dismount for destructive tests;
//! * bad-sector-to-file mapping on NTFS.
//!
//! On other operating systems the crate is empty.

#![cfg(windows)]

mod devtree;
mod enumerate;
mod filemap;
mod handle;
mod health;
mod platform;
mod rawdisk;
mod sdcard;
mod system;
mod volumes;

pub use platform::WinPlatform;
