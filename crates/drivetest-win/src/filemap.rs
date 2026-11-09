//! Maps bad sectors to the files that occupy them, using NTFS's
//! FSCTL_LOOKUP_STREAM_FROM_CLUSTER. Read-only.

use std::collections::BTreeMap;
use std::mem::{offset_of, size_of};

use drivetest_core::platform::SectorFile;
use windows_sys::Win32::Foundation::GENERIC_READ;
use windows_sys::Win32::System::Ioctl::{
    FSCTL_LOOKUP_STREAM_FROM_CLUSTER, LOOKUP_STREAM_FROM_CLUSTER_ENTRY,
    LOOKUP_STREAM_FROM_CLUSTER_ENTRY_FLAG_FS_SYSTEM_FILE, LOOKUP_STREAM_FROM_CLUSTER_ENTRY_FLAG_PAGE_FILE,
    LOOKUP_STREAM_FROM_CLUSTER_INPUT, LOOKUP_STREAM_FROM_CLUSTER_OUTPUT,
};

use crate::handle::{i64_at, ioctl_bytes, open, u32_at};
use crate::volumes::{VolumeRecord, cluster_size};

const INPUT_CLUSTERS_AT: usize = offset_of!(LOOKUP_STREAM_FROM_CLUSTER_INPUT, Cluster);
const OUTPUT_HEADER: usize = size_of::<LOOKUP_STREAM_FROM_CLUSTER_OUTPUT>();
const ENTRY_FLAGS_AT: usize = offset_of!(LOOKUP_STREAM_FROM_CLUSTER_ENTRY, Flags);
const ENTRY_CLUSTER_AT: usize = offset_of!(LOOKUP_STREAM_FROM_CLUSTER_ENTRY, Cluster);
const ENTRY_NAME_AT: usize = offset_of!(LOOKUP_STREAM_FROM_CLUSTER_ENTRY, FileName);

/// Finds which volume (and cluster) a byte offset on `disk` falls in.
fn locate(volumes: &[VolumeRecord], disk: u32, byte: u64) -> Option<(usize, u64)> {
    for (i, v) in volumes.iter().enumerate() {
        let mut volume_offset = 0u64;
        for e in &v.extents {
            if e.disk == disk && byte >= e.offset && byte < e.offset + e.length {
                return Some((i, volume_offset + (byte - e.offset)));
            }
            volume_offset += e.length;
        }
    }
    None
}

fn lookup(volume: &VolumeRecord, clusters: &[i64]) -> Result<Vec<(i64, u32, String)>, String> {
    let h = open(volume.device_path(), GENERIC_READ, 0).map_err(|e| format!("cannot open volume: {e}"))?;
    let mut input = vec![0u8; INPUT_CLUSTERS_AT + clusters.len() * 8];
    input[4..8].copy_from_slice(&(clusters.len() as u32).to_le_bytes());
    for (i, c) in clusters.iter().enumerate() {
        let at = INPUT_CLUSTERS_AT + i * 8;
        input[at..at + 8].copy_from_slice(&c.to_le_bytes());
    }
    let mut out = vec![0u8; 256 * 1024];
    ioctl_bytes(&h, FSCTL_LOOKUP_STREAM_FROM_CLUSTER, &input, &mut out).map_err(|e| e.to_string())?;
    let first = u32_at(&out, 0) as usize;
    let matches = u32_at(&out, 4) as usize;
    let mut results = Vec::new();
    let mut pos = if first >= OUTPUT_HEADER { first } else { OUTPUT_HEADER };
    for _ in 0..matches {
        if pos + ENTRY_NAME_AT > out.len() {
            break;
        }
        let next = u32_at(&out, pos) as usize;
        if next != 0 && next < ENTRY_NAME_AT {
            break; // malformed entry; stop rather than misread
        }
        let flags = u32_at(&out, pos + ENTRY_FLAGS_AT);
        let cluster = i64_at(&out, pos + ENTRY_CLUSTER_AT);
        let end = if next == 0 { out.len() } else { (pos + next).min(out.len()) };
        let name_bytes = &out[pos + ENTRY_NAME_AT..end];
        let name: Vec<u16> =
            name_bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&c| c != 0).collect();
        let name = String::from_utf16_lossy(&name);
        results.push((cluster, flags, name.trim_end_matches("::$DATA").to_string()));
        if next == 0 {
            break;
        }
        pos += next;
    }
    Ok(results)
}

pub fn map(disk: u32, lbas: &[u64], sector: u64, volumes: &[VolumeRecord]) -> Vec<SectorFile> {
    let mut out = Vec::with_capacity(lbas.len());
    // volume index -> [(lba, cluster)]
    let mut per_volume: BTreeMap<usize, Vec<(u64, i64)>> = BTreeMap::new();
    for &lba in lbas {
        match locate(volumes, disk, lba * sector) {
            None => out
                .push(SectorFile { lba, location: "Outside any volume (partition table or unallocated space)".into() }),
            Some((vi, voff)) => {
                let v = &volumes[vi];
                let Some(mount) = v.mount_points.first() else {
                    out.push(SectorFile { lba, location: format!("Unmounted volume {}", v.guid_path) });
                    continue;
                };
                if !v.file_system.eq_ignore_ascii_case("NTFS") {
                    out.push(SectorFile {
                        lba,
                        location: format!("{mount} ({}; file lookup needs NTFS)", v.file_system),
                    });
                    continue;
                }
                match cluster_size(mount) {
                    Some(cs) => per_volume.entry(vi).or_default().push((lba, (voff / cs) as i64)),
                    None => out.push(SectorFile { lba, location: format!("{mount} (cluster size unknown)") }),
                }
            }
        }
    }
    for (vi, items) in per_volume {
        let v = &volumes[vi];
        let mount = v.mount_points.first().cloned().unwrap_or_default();
        for chunk in items.chunks(32) {
            let clusters: Vec<i64> = chunk.iter().map(|(_, c)| *c).collect();
            match lookup(v, &clusters) {
                Err(e) => {
                    for (lba, _) in chunk {
                        out.push(SectorFile { lba: *lba, location: format!("{mount} (lookup failed: {e})") });
                    }
                }
                Ok(found) => {
                    for (lba, cluster) in chunk {
                        let hits: Vec<&(i64, u32, String)> = found.iter().filter(|f| f.0 == *cluster).collect();
                        let location = match hits.first() {
                            None => format!("{mount} free space (no file)"),
                            Some((_, flags, name)) => {
                                let path = format!("{}{}", mount.trim_end_matches('\\'), name);
                                if flags & LOOKUP_STREAM_FROM_CLUSTER_ENTRY_FLAG_PAGE_FILE != 0 {
                                    format!("{path} (page file)")
                                } else if flags & LOOKUP_STREAM_FROM_CLUSTER_ENTRY_FLAG_FS_SYSTEM_FILE != 0 {
                                    format!("{path} (NTFS metadata)")
                                } else {
                                    path
                                }
                            }
                        };
                        out.push(SectorFile { lba: *lba, location });
                    }
                }
            }
        }
    }
    out.sort_by_key(|s| s.lba);
    out
}
