use std::fs::OpenOptions;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixListener;
use std::thread;

use calloop::channel::Sender;
use protocol::Command;

/* Feilen som sier "en annen appd eier socketen alt". main() avslutter da
 * stille istf å stjele socketen fra en instans som virker. */
pub fn already_running(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::AddrInUse
}

/* Ta eierskapet atomisk.
 *
 * Den gamle vakten var connect → unlink → bind, og den er ikke atomisk: to
 * appd som starter samtidig — sesjonsmålet OG autostart i config.kdl gjør
 * nøyaktig det, målt 2 s fra hverandre — rekker begge å se "ingen svarer"
 * før noen har rukket å binde. Da unlinker begge og begge binder. Den som
 * binder sist eier stien; den første blir liggende og lytte på en unlinket
 * inode og hører aldri fra apptoggle igjen. Halvparten av modkey+p havnet
 * dermed hos en daemon uten synlig vindu, og det så ut som at launcheren
 * ikke poppet opp.
 *
 * flock avgjør eierskapet før socketen røres. Nøyaktig én vinner, uansett
 * hvor tett oppstartene ligger. */
fn acquire_ownership() -> io::Result<std::fs::File> {
    let path = protocol::lock_path();
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)?;
    /* SAFETY: gyldig fd fra File som lever ut kallet. */
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let e = io::Error::last_os_error();
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!("another appd holds {}: {e}", path.display()),
        ));
    }
    Ok(file)
}

pub fn spawn(tx: Sender<Command>) -> io::Result<()> {
    let path = protocol::socket_path();
    let lock = acquire_ownership()?;
    /* Låsen må leve like lenge som prosessen — droppes File'n lukkes fd-en
     * og flock slippes, og da kan en ny appd binde oppå oss. */
    std::mem::forget(lock);

    /* Vi eier låsen, så en socket-fil som ligger igjen her er per
     * definisjon foreldreløs (forrige appd døde uten å rydde). */
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    log::info!("listening on {}", path.display());

    thread::Builder::new()
        .name("apptoggle-socket".into())
        .spawn(move || {
            for conn in listener.incoming() {
                match conn {
                    Ok(mut stream) => match Command::read_from(&mut stream) {
                        Ok(Some(cmd)) => {
                            if tx.send(cmd).is_err() {
                                return;
                            }
                        }
                        Ok(None) => log::warn!("empty message"),
                        Err(e) => log::warn!("read: {e}"),
                    },
                    Err(e) => log::warn!("accept: {e}"),
                }
            }
        })?;
    Ok(())
}
