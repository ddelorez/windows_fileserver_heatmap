//! smb-heat-spike — correlation-engine spike.
//!
//! Usage:
//!   smb-heat-spike discover            # dump real ETW field names for target events
//!   smb-heat-spike resolve             # print resolved (user, share, path) per open
//!   smb-heat-spike resolve --rundown   # also enumerate pre-existing sessions/trees
//!
//! Must run elevated (a real-time ETW session requires admin).

mod correlation;
mod etw;
mod events;
mod identity;

use etw::Mode;

fn main() {
    let args: Vec<String> = std::env::args().collect();

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
