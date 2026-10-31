//! `streamtap-host`: started by the browser for the extension (native
//! messaging). The browser passes the caller's origin as the first argument.
//! Nothing is printed: stdout is the protocol, and the host knows no secret.

use std::io;
use std::process::ExitCode;

use streamtap_host::relay::{connect_to_app, run};

fn main() -> ExitCode {
    let origin = std::env::args().nth(1);
    let code = run(
        origin.as_deref(),
        io::stdin().lock(),
        io::stdout(),
        connect_to_app,
        || std::process::exit(0),
    );
    ExitCode::from(code)
}
