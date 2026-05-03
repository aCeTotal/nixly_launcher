use std::collections::HashSet;
use std::path::PathBuf;

// Returns the roots that file/git scans should cover: home dir plus every
// detected network filesystem mount point.
pub fn walk_roots() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        roots.push(PathBuf::from(home));
    }
    roots.extend(network_mounts());
    let mut seen: HashSet<PathBuf> = HashSet::new();
    roots.retain(|p| seen.insert(p.clone()));
    roots
}

// Discovers NFS and SMB/CIFS mounts from two sources, then dedupes:
//
//   - /proc/mounts: currently active mounts, including autofs entries that
//     trigger on access. autofs is included because direct-mount autofs maps
//     show up here even before the underlying NFS is brought online — the
//     act of opendir() on the mountpoint triggers the actual mount.
//
//   - /etc/fstab: declared mounts (NixOS's `fileSystems."/mnt/x"` option
//     generates entries here via systemd-fstab-generator). Catches mounts
//     declared but not currently active, e.g. noauto or just unmounted at
//     daemon startup.
//
// Filters to existing mountpoints so stale fstab declarations pointing at
// deleted directories don't poison the walk roots.
pub fn network_mounts() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    out.extend(parse_mount_file("/proc/mounts"));
    out.extend(parse_mount_file("/etc/fstab"));

    out.retain(|p| std::fs::metadata(p).is_ok());

    let mut seen: HashSet<PathBuf> = HashSet::new();
    out.retain(|p| seen.insert(p.clone()));

    if !out.is_empty() {
        log::info!("network mounts: {out:?}");
    }
    out
}

fn parse_mount_file(path: &str) -> Vec<PathBuf> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some(mp) = parse_mount_line(trimmed) {
            out.push(mp);
        }
    }
    out
}

fn parse_mount_line(line: &str) -> Option<PathBuf> {
    let mut parts = line.split_whitespace();
    let _device = parts.next()?;
    let mountpoint = parts.next()?;
    let fstype = parts.next()?;
    if is_network_fs(fstype) {
        Some(unescape(mountpoint))
    } else {
        None
    }
}

fn is_network_fs(fs: &str) -> bool {
    matches!(
        fs,
        "nfs"
            | "nfs3"
            | "nfs4"
            | "nfs4.0"
            | "nfs4.1"
            | "nfs4.2"
            | "smbfs"
            | "cifs"
            | "smb"
            | "smb3"
            | "autofs"
    )
}

// /proc/mounts and /etc/fstab encode whitespace/special chars in mount paths
// as \NNN (octal). Most paths don't need this but we decode anyway so spaces
// in share names round-trip correctly.
fn unescape(s: &str) -> PathBuf {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() {
            if let Ok(seq) = std::str::from_utf8(&bytes[i + 1..i + 4]) {
                if let Ok(byte) = u8::from_str_radix(seq, 8) {
                    out.push(byte as char);
                    i += 4;
                    continue;
                }
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    PathBuf::from(out)
}
