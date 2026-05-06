use std::path::PathBuf;

// Network filesystems (NFS/SMB/CIFS/autofs) are deliberately excluded from
// scan roots: walking them eats RAM proportional to remote file count and
// stalls on network latency. Only HOME is indexed.
pub fn walk_roots() -> Vec<PathBuf> {
    match std::env::var_os("HOME") {
        Some(home) => vec![PathBuf::from(home)],
        None => Vec::new(),
    }
}
