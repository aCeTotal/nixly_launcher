use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Command as Proc, ExitCode, Stdio};
use std::time::{Duration, Instant};

use protocol::Command;

/* Hvor lenge vi venter på at en nystartet appd skal binde socketen. */
const DAEMON_START_TIMEOUT: Duration = Duration::from_secs(3);
const RETRY_INTERVAL: Duration = Duration::from_millis(50);

fn main() -> ExitCode {
    let arg = std::env::args().nth(1).unwrap_or_else(|| "toggle".to_string());
    let cmd = match arg.as_str() {
        "toggle" => Command::Toggle,
        "show" => Command::Show,
        "hide" => Command::Hide,
        "quit" => Command::Quit,
        other => {
            eprintln!("apptoggle: unknown command {other:?}");
            eprintln!("usage: apptoggle [toggle|show|hide|quit]");
            return ExitCode::from(2);
        }
    };

    let path = protocol::socket_path();
    let mut sock = match UnixStream::connect(&path) {
        Ok(s) => s,
        Err(e) if !is_dead_socket(&e) => {
            /* Daemonen lever, men connect feilet av en forbigående grunn
             * (full accept-kø, avbrutt kall). Å starte en ny appd her ville
             * bare gitt to som slåss om modkey+p. */
            eprintln!("apptoggle: connect {}: {e} — daemon lever, gir opp", path.display());
            return ExitCode::from(1);
        }
        Err(e) => {
            /* Ingen daemon svarer (krasjet, eller sesjonen startet den
             * aldri). Tastetrykket skal likevel åpne launcheren, så start
             * daemonen og vent på socketen. Hide/Quit har ingenting å
             * gjøre uten daemon. */
            if matches!(cmd, Command::Hide | Command::Quit) {
                eprintln!("apptoggle: connect {}: {e}", path.display());
                return ExitCode::from(1);
            }
            eprintln!("apptoggle: no daemon ({e}) — starting appd");
            match start_daemon_and_wait(&path) {
                Some(s) => s,
                None => {
                    eprintln!("apptoggle: appd did not come up");
                    return ExitCode::from(1);
                }
            }
        }
    };

    if let Err(e) = cmd.write_to(&mut sock) {
        eprintln!("apptoggle: send: {e}");
        return ExitCode::from(1);
    }
    let _ = sock.flush();

    ExitCode::SUCCESS
}

/* Skiller "ingen daemon" fra "daemon finnes, men connect gikk galt".
 * NotFound = ingen socket-fil. ConnectionRefused = fil uten lytter, altså
 * rester etter en appd som døde. Alt annet betyr at noen er der. */
fn is_dead_socket(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
    )
}

fn start_daemon_and_wait(path: &Path) -> Option<UnixStream> {
    /* IKKE unlink her. Den gamle koden slettet socket-stien ubetinget, og
     * traff den en levende appd ble den foreldreløs: prosessen lytter
     * videre på en inode uten navn, mens neste appd binder en ny fil. Da
     * har du to daemoner og modkey+p treffer feil halvpart av gangene.
     * appd rydder selv opp i en foreldreløs fil — den holder flock'en som
     * beviser at ingen andre eier socketen. */
    Proc::new("appd")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    let deadline = Instant::now() + DAEMON_START_TIMEOUT;
    loop {
        if let Ok(s) = UnixStream::connect(path) {
            return Some(s);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(RETRY_INTERVAL);
    }
}
