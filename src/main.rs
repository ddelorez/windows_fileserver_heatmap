//! smb-heat-spike — correlation-engine spike.
//!
//! Usage:
//!   smb-heat-spike discover            # dump real ETW field names for target events
//!   smb-heat-spike resolve             # print resolved (user, share, path) per open
//!   smb-heat-spike resolve --rundown   # also enumerate pre-existing sessions/trees
//!   smb-heat-spike walk <Share> <dir>  # standalone file-size inventory of <dir>
//!
//! The ETW modes must run elevated (a real-time ETW session requires admin); the
//! `walk` mode does not.

mod correlation;
mod etw;
mod events;
mod identity;
mod inventory;

use std::path::Path;

use etw::Mode;

fn main() {
    let args: Vec<String> = std::env::args().collect();

    // Standalone inventory walker — deliberately NOT wired into the ETW/resolve
    // path. Builds an in-memory inventory of a local tree and prints a summary.
    if args.get(1).map(String::as_str) == Some("walk") {
        let (Some(share), Some(root)) = (args.get(2), args.get(3)) else {
            eprintln!("usage: smb-heat-spike walk <ShareName> <local_root>");
            std::process::exit(2);
        };
        match inventory::walk(share, Path::new(root)) {
            Ok(w) => print!("{}", w.summary()),
            Err(e) => {
                eprintln!("walk failed: {e}");
                std::process::exit(1);
            }
        }
        return;
    }

    let mode = match args.get(1).map(String::as_str) {
        Some("discover") => Mode::Discover,
        Some("resolve") | None => Mode::Resolve,
        Some(other) => {
            eprintln!("unknown mode '{other}'. use: discover | resolve [--rundown]");
            std::process::exit(2);
        }
    };

    let mut mask = events::MASK_CORRELATION;
    if args.iter().any(|a| a == "--rundown") {
        // Pull in Rundown enumeration so the engine isn't blind to sessions and
        // trees that existed before the trace started. Recommended on a busy
        // server like SGIFS01 where long-lived sessions dominate.
        mask |= events::KW_RUNDOWN;
    }

    if let Err(e) = etw::run(mode, mask) {
        eprintln!("error: {e}");
        eprintln!();
        eprintln!("if this is a session/ETW error, a previous run's session may be lingering:");
        eprintln!("  logman query -ets | findstr SmbHeatSpike");
        eprintln!("  logman stop SmbHeatSpike -ets");
        std::process::exit(1);
    }
}
