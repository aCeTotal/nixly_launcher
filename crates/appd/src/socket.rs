use std::io;
use std::os::unix::net::UnixListener;
use std::thread;

use calloop::channel::Sender;
use protocol::Command;

pub fn spawn(tx: Sender<Command>) -> io::Result<()> {
    let path = protocol::socket_path();
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
