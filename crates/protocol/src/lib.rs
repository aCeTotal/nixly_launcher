use std::io::{self, Read, Write};
use std::path::PathBuf;

#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Toggle = 1,
    Show = 2,
    Hide = 3,
    Quit = 4,
}

impl Command {
    pub fn from_byte(b: u8) -> Option<Self> {
        match b {
            1 => Some(Self::Toggle),
            2 => Some(Self::Show),
            3 => Some(Self::Hide),
            4 => Some(Self::Quit),
            _ => None,
        }
    }

    pub fn write_to<W: Write>(self, w: &mut W) -> io::Result<()> {
        w.write_all(&[self as u8])
    }

    pub fn read_from<R: Read>(r: &mut R) -> io::Result<Option<Self>> {
        let mut buf = [0u8; 1];
        match r.read_exact(&mut buf) {
            Ok(()) => Ok(Self::from_byte(buf[0])),
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(None),
            Err(e) => Err(e),
        }
    }
}

fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

pub fn socket_path() -> PathBuf {
    runtime_dir().join("nixly_launcher.sock")
}

/* Eierskapslås for single-instance. Egen fil, ikke selve socketen: flock
 * må tas FØR socketen røres, ellers er vi tilbake til racet den skal
 * fjerne. Kjernen slipper låsen når prosessen dør, så en krasjet appd
 * sperrer ikke neste oppstart. */
pub fn lock_path() -> PathBuf {
    runtime_dir().join("nixly_launcher.lock")
}
