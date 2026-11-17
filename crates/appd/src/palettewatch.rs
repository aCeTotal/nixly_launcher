use std::cell::Cell;
use std::error::Error;
use std::ffi::CString;
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::FromRawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::rc::Rc;
use std::time::Duration;

use calloop::generic::Generic;
use calloop::timer::{TimeoutAction, Timer};
use calloop::{Interest, LoopHandle, Mode, PostAction};

use crate::palette;

// matugen rewrites in place.
const DEBOUNCE: Duration = Duration::from_millis(60);
const WATCH_MASK: u32 = libc::IN_CLOSE_WRITE | libc::IN_MOVED_TO | libc::IN_CREATE;
const EVENT_HEADER: usize = std::mem::size_of::<libc::inotify_event>();
const NAME_LEN_AT: usize = std::mem::offset_of!(libc::inotify_event, len);
const READ_BUF: usize = 4096;

pub fn insert<D: 'static>(
    handle: &LoopHandle<'static, D>,
    reload: fn(&mut D),
) -> Result<(), Box<dyn Error>> {
    let dir = palette::dir().ok_or("HOME unset")?;
    std::fs::create_dir_all(&dir)?;
    let inotify = watch_dir(&dir)?;
    let armed = Rc::new(Cell::new(false));
    let timers = handle.clone();
    handle
        .insert_source(
            Generic::new(inotify, Interest::READ, Mode::Level),
            move |_, inotify, _| {
                if drain(inotify) && !armed.replace(true) {
                    arm(&timers, &armed, reload);
                }
                Ok(PostAction::Continue)
            },
        )
        .map_err(|e| format!("insert palette watch: {e}"))?;
    Ok(())
}

fn watch_dir(dir: &Path) -> io::Result<File> {
    let dir = CString::new(dir.as_os_str().as_bytes())?;
    let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let inotify = unsafe { File::from_raw_fd(fd) };
    if unsafe { libc::inotify_add_watch(fd, dir.as_ptr(), WATCH_MASK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(inotify)
}

fn arm<D: 'static>(handle: &LoopHandle<'static, D>, armed: &Rc<Cell<bool>>, reload: fn(&mut D)) {
    let pending = Rc::clone(armed);
    let timer = handle.insert_source(Timer::from_duration(DEBOUNCE), move |_, _, data| {
        pending.set(false);
        reload(data);
        TimeoutAction::Drop
    });
    if let Err(e) = timer {
        armed.set(false);
        log::error!("palette debounce timer: {e}");
    }
}

// True if palette file changed.
fn drain(mut inotify: &File) -> bool {
    let mut buf = [0u8; READ_BUF];
    let mut hit = false;
    while let Ok(n @ 1..) = inotify.read(&mut buf) {
        hit |= names(&buf[..n]).any(|name| name == palette::FILE.as_bytes());
    }
    hit
}

fn names(mut events: &[u8]) -> impl Iterator<Item = &[u8]> {
    std::iter::from_fn(move || {
        let header = events.get(..EVENT_HEADER)?;
        let len = u32::from_ne_bytes(header[NAME_LEN_AT..].try_into().ok()?) as usize;
        let name = events.get(EVENT_HEADER..EVENT_HEADER + len)?;
        events = &events[EVENT_HEADER + len..];
        let end = name.iter().position(|&b| b == 0).unwrap_or(name.len());
        Some(&name[..end])
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(name: &str, padded: u32) -> Vec<u8> {
        let mut raw = vec![0; EVENT_HEADER];
        raw[NAME_LEN_AT..].copy_from_slice(&padded.to_ne_bytes());
        raw.extend_from_slice(name.as_bytes());
        raw.resize(EVENT_HEADER + padded as usize, 0);
        raw
    }

    #[test]
    fn names_strip_padding() {
        let mut raw = event("foot.ini", 16);
        raw.extend(event(palette::FILE, 32));
        let got: Vec<&[u8]> = names(&raw).collect();
        assert_eq!(got, [b"foot.ini".as_slice(), palette::FILE.as_bytes()]);
    }

    #[test]
    fn drain_sees_in_place_rewrite() {
        let dir = std::env::temp_dir().join(format!("appd-palettewatch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let inotify = watch_dir(&dir).unwrap();
        std::fs::write(dir.join("foot.ini"), "x").unwrap();
        assert!(!drain(&inotify));
        std::fs::write(dir.join(palette::FILE), "old").unwrap();
        assert!(drain(&inotify));
        std::fs::write(dir.join(palette::FILE), "new").unwrap();
        assert!(drain(&inotify));
        assert!(!drain(&inotify));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
