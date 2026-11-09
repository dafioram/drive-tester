//! File attributes and the refresh / scan exclusion rules.

use std::fs::Metadata;
use std::path::Path;

pub const ATTR_READONLY: u32 = 0x1;
pub const ATTR_HIDDEN: u32 = 0x2;
pub const ATTR_SYSTEM: u32 = 0x4;
pub const ATTR_ARCHIVE: u32 = 0x20;
pub const ATTR_SPARSE: u32 = 0x200;
pub const ATTR_REPARSE_POINT: u32 = 0x400;
pub const ATTR_COMPRESSED: u32 = 0x800;
pub const ATTR_OFFLINE: u32 = 0x1000;
pub const ATTR_ENCRYPTED: u32 = 0x4000;
pub const ATTR_RECALL_ON_OPEN: u32 = 0x40000;
pub const ATTR_RECALL_ON_DATA_ACCESS: u32 = 0x400000;

/// Windows file attributes (0 elsewhere).
pub fn attributes(meta: &Metadata) -> u32 {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        meta.file_attributes()
    }
    #[cfg(not(windows))]
    {
        let _ = meta;
        0
    }
}

/// Cloud placeholders (OneDrive and similar): reading them triggers a download.
pub fn is_cloud_placeholder(attrs: u32) -> bool {
    attrs & (ATTR_OFFLINE | ATTR_RECALL_ON_OPEN | ATTR_RECALL_ON_DATA_ACCESS) != 0
}

/// Folders never walked or touched on any volume.
const ALWAYS_SKIPPED_DIRS: [&str; 3] = ["system volume information", "$recycle.bin", "$windows.~bt"];

/// Top-level folders never refreshed on the system disk.
const SYSTEM_DISK_SKIPPED_DIRS: [&str; 5] =
    ["windows", "program files", "program files (x86)", "programdata", "recovery"];

const SYSTEM_FILES: [&str; 3] = ["pagefile.sys", "hiberfil.sys", "swapfile.sys"];

/// Accepts only a plain relative path (file and folder names). Rejects
/// drive letters, roots, `.` and `..`, so a target can never leave the volume.
pub fn safe_relative(p: &str) -> Option<std::path::PathBuf> {
    use std::path::Component;
    let normalized = p.replace(['\\', '/'], std::path::MAIN_SEPARATOR_STR);
    let path = std::path::PathBuf::from(normalized.trim_matches(std::path::MAIN_SEPARATOR));
    let ok = path.components().count() > 0 && path.components().all(|c| matches!(c, Component::Normal(_)));
    // Reject Windows drive prefixes even when parsed on another OS ("C:x").
    let has_drive = p.len() >= 2 && p.as_bytes()[1] == b':';
    (ok && !has_drive).then_some(path)
}

/// Lower-cased path components relative to the volume root.
fn components(rel: &Path) -> Vec<String> {
    rel.components().map(|c| c.as_os_str().to_string_lossy().to_lowercase()).collect()
}

/// True when a directory (relative to the volume root) should not be walked.
pub fn skip_dir(rel: &Path) -> bool {
    components(rel).iter().any(|c| ALWAYS_SKIPPED_DIRS.contains(&c.as_str()))
}

/// Why a file must not be refreshed, if it must not.
pub fn refresh_exclusion(rel: &Path, attrs: u32, on_system_disk: bool) -> Option<&'static str> {
    let parts = components(rel);
    if parts.iter().any(|c| ALWAYS_SKIPPED_DIRS.contains(&c.as_str())) {
        return Some("system folder");
    }
    if parts.len() == 1 && SYSTEM_FILES.contains(&parts[0].as_str()) {
        return Some("pagefile / hibernation / swap file");
    }
    if on_system_disk && parts.first().is_some_and(|c| SYSTEM_DISK_SKIPPED_DIRS.contains(&c.as_str())) {
        return Some("Windows or program folder on the system disk");
    }
    if is_cloud_placeholder(attrs) {
        return Some("cloud placeholder (would trigger a download)");
    }
    if attrs & ATTR_REPARSE_POINT != 0 {
        return Some("link or reparse point");
    }
    if attrs & ATTR_ENCRYPTED != 0 {
        return Some("EFS-encrypted");
    }
    if attrs & ATTR_COMPRESSED != 0 {
        return Some("compressed");
    }
    if attrs & ATTR_SPARSE != 0 {
        return Some("sparse file");
    }
    if attrs & ATTR_SYSTEM != 0 {
        return Some("system file");
    }
    if attrs & ATTR_READONLY != 0 {
        return Some("read-only file");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn exclusions() {
        let p = |s: &str| PathBuf::from(s.replace('\\', std::path::MAIN_SEPARATOR_STR));
        assert_eq!(
            refresh_exclusion(&p("Windows\\System32\\x.dll"), 0, true),
            Some("Windows or program folder on the system disk")
        );
        assert_eq!(refresh_exclusion(&p("Windows\\notes.txt"), 0, false), None);
        assert!(refresh_exclusion(&p("pagefile.sys"), 0, false).is_some());
        assert!(refresh_exclusion(&p("Users\\a\\OneDrive\\f.docx"), ATTR_RECALL_ON_DATA_ACCESS, false).is_some());
        assert_eq!(refresh_exclusion(&p("Users\\a\\Videos\\clip.mp4"), 0, true), None);
        assert!(skip_dir(&p("System Volume Information")));
    }

    #[test]
    fn only_plain_relative_paths_are_accepted() {
        assert!(safe_relative("Users\\a\\x.bin").is_some());
        assert!(safe_relative("folder").is_some());
        // A leading separator is dropped, so the path stays inside the volume root.
        assert_eq!(safe_relative("\\Windows\\x.dll"), Some(Path::new("Windows").join("x.dll")));
        for bad in ["C:\\Windows\\x", "C:x", "..\\..\\x", "a\\..\\..\\b", ".\\a", ""] {
            assert!(safe_relative(bad).is_none(), "{bad} accepted");
        }
    }
}
