use std::path::PathBuf;
use std::process::Command;

pub fn load_default() -> std::io::Result<Vec<u8>> {
    let path = find()?;
    log::info!("font: {}", path.display());
    std::fs::read(path)
}

fn find() -> std::io::Result<PathBuf> {
    let out = Command::new("fc-match")
        .args(["-f", "%{file}", "sans-serif:weight=normal:slant=roman"])
        .output()?;
    if !out.status.success() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "fc-match failed",
        ));
    }
    let s = std::str::from_utf8(&out.stdout)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?
        .trim();
    if s.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "fc-match returned empty path",
        ));
    }
    Ok(PathBuf::from(s))
}
