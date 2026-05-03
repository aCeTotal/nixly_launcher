use std::os::unix::net::UnixStream;
use std::process::ExitCode;

use protocol::Command;

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
        Err(e) => {
            eprintln!("apptoggle: connect {}: {e}", path.display());
            return ExitCode::from(1);
        }
    };

    if let Err(e) = cmd.write_to(&mut sock) {
        eprintln!("apptoggle: send: {e}");
        return ExitCode::from(1);
    }

    ExitCode::SUCCESS
}
