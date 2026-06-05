//! SMBServer provider constants, the normalized event enum, and the property
//! extraction adapter.
//!
//! Everything in the "facts" section below is taken straight from the provider
//! manifest (`wevtutil gp Microsoft-Windows-SMBServer /ge /gm:true`) on a live
//! server, so it's solid. Everything in the "VERIFY" section is a *guess* at the
//! ETW property names, because the message-less Analytic events (552/600/650)
//! don't render a template in the manifest. Run `smb-heat-spike discover` first;
//! it prints the field names that actually parse, and you fix the arrays here.

use ferrisetw::parser::Parser;

// ---------------------------------------------------------------------------
// FACTS (from the manifest — high confidence)
// ---------------------------------------------------------------------------

// Keyword bits. Each lifecycle family is tagged with exactly one of these, so a
// single MatchAnyKeyword selects just the correlation backbone and excludes the
// per-I/O Request/Response (0x1/0x2) torrent by construction.
pub const KW_CONNECTION: u64 = 0x0000_0010;
pub const KW_SESSION: u64 = 0x0000_0020;
pub const KW_TREECONNECT: u64 = 0x0000_0040;
pub const KW_FILE: u64 = 0x0000_0080;
pub const KW_RUNDOWN: u64 = 0x0001_0000;

/// The open-centric correlation mask: Connection | Session | TreeConnect | File.
pub const MASK_CORRELATION: u64 = KW_CONNECTION | KW_SESSION | KW_TREECONNECT | KW_FILE; // 0xF0

// Event IDs on the Analytic channel (id 17), Informational level.
pub const E_CONN_ACCEPT: u16 = 500;
pub const E_CONN_DISC: u16 = 501;
pub const E_CONN_TERM: u16 = 502;
pub const E_SESS_ALLOC: u16 = 550;
pub const E_SESS_AUTH: u16 = 552; // authenticated user lives here
pub const E_SESS_TERM: u16 = 554;
pub const E_SESS_CLOSE: u16 = 555;
pub const E_TREE_ALLOC: u16 = 600; // tree -> (session, share)
pub const E_TREE_DISC: u16 = 601;
pub const E_TREE_TERM: u16 = 602;
pub const E_OPEN: u16 = 650; // "Open established" — the access pulse
pub const E_OPEN_CLOSE: u16 = 654;
pub const E_CREATE_RESP: u16 = 108; // fallback if 650 lacks path / access mask
// Rundown enumeration of pre-existing objects (channel 0, KW_RUNDOWN).
pub const E_FILE_RUNDOWN: u16 = 3004;
pub const E_SESSION_RUNDOWN: u16 = 3005;
pub const E_SHARE_RUNDOWN: u16 = 3006;

/// Every event id the discover harness probes.
pub const DISCOVER_TARGETS: &[u16] = &[
    E_CONN_ACCEPT, E_CONN_DISC, E_CONN_TERM,
    E_SESS_ALLOC, E_SESS_AUTH, E_SESS_TERM, E_SESS_CLOSE,
    E_TREE_ALLOC, E_TREE_DISC, E_TREE_TERM,
    E_OPEN, E_OPEN_CLOSE, E_CREATE_RESP,
    E_FILE_RUNDOWN, E_SESSION_RUNDOWN, E_SHARE_RUNDOWN,
];

// ---------------------------------------------------------------------------
// VERIFY (property names — confirm with `discover`, then trim each list to the
// single name that actually parses)
// ---------------------------------------------------------------------------
pub const P_CONN_ID: &[&str] = &["ConnectionId", "ConnId", "Connection"];
pub const P_SESSION_ID: &[&str] = &["SessionId", "SmbSessionId", "SessId"];
pub const P_TREE_ID: &[&str] = &["TreeConnectId", "TreeId", "TreeConnect"];
pub const P_USER_NAME: &[&str] = &["UserName", "User", "Account", "AccountName"];
pub const P_USER_SID: &[&str] = &["UserSid", "Sid", "SidString", "SecurityId"];
pub const P_SHARE_NAME: &[&str] = &["ShareName", "Share"];
pub const P_FILE_NAME: &[&str] = &["FileName", "Name", "RelativeTargetName", "FilePath"];
pub const P_CLIENT_ADDR: &[&str] = &["ClientAddress", "Address", "ClientName"];

// ---------------------------------------------------------------------------
// Normalized event — decouples the engine from ETW parsing.
// ---------------------------------------------------------------------------
#[derive(Debug, Clone)]
pub enum SmbEvent {
    ConnAccept { conn_id: u64, client: String },
    ConnEnd { conn_id: u64 },
    SessionAuth { session_id: u64, conn_id: Option<u64>, user: String },
    SessionEnd { session_id: u64 },
    TreeConnect { tree_id: u64, session_id: u64, share: String },
    TreeEnd { tree_id: u64 },
    Open { session_id: Option<u64>, tree_id: Option<u64>, path: String },
}

/// Try each candidate name as a string property; return the first that parses.
pub fn first_str(parser: &Parser, names: &[&str]) -> Option<String> {
    for n in names {
        if let Ok(v) = parser.try_parse::<String>(n) {
            return Some(v);
        }
    }
    None
}

/// Try each candidate name as an integer property; widen across widths because
/// SessionId is 64-bit, TreeId 32-bit, etc., and we normalize to u64.
pub fn first_u64(parser: &Parser, names: &[&str]) -> Option<u64> {
    for n in names {
        if let Ok(v) = parser.try_parse::<u64>(n) {
            return Some(v);
        }
        if let Ok(v) = parser.try_parse::<u32>(n) {
            return Some(v as u64);
        }
        if let Ok(v) = parser.try_parse::<u16>(n) {
            return Some(v as u64);
        }
    }
    None
}

/// Map a raw event to a normalized `SmbEvent`. Returns `None` for ids we don't
/// model or when a required key is missing (the spike just skips those).
pub fn parse_event(event_id: u16, parser: &Parser) -> Option<SmbEvent> {
    match event_id {
        E_CONN_ACCEPT => Some(SmbEvent::ConnAccept {
            conn_id: first_u64(parser, P_CONN_ID)?,
            client: first_str(parser, P_CLIENT_ADDR).unwrap_or_default(),
        }),
        E_CONN_DISC | E_CONN_TERM => Some(SmbEvent::ConnEnd {
            conn_id: first_u64(parser, P_CONN_ID)?,
        }),
        E_SESS_AUTH => Some(SmbEvent::SessionAuth {
            session_id: first_u64(parser, P_SESSION_ID)?,
            conn_id: first_u64(parser, P_CONN_ID),
            user: first_str(parser, P_USER_NAME)
                .or_else(|| first_str(parser, P_USER_SID))
                .unwrap_or_else(|| "<unknown>".into()),
        }),
        E_SESS_TERM | E_SESS_CLOSE => Some(SmbEvent::SessionEnd {
            session_id: first_u64(parser, P_SESSION_ID)?,
        }),
        E_TREE_ALLOC => Some(SmbEvent::TreeConnect {
            tree_id: first_u64(parser, P_TREE_ID)?,
            session_id: first_u64(parser, P_SESSION_ID).unwrap_or(0),
            share: first_str(parser, P_SHARE_NAME).unwrap_or_default(),
        }),
        E_TREE_DISC | E_TREE_TERM => Some(SmbEvent::TreeEnd {
            tree_id: first_u64(parser, P_TREE_ID)?,
        }),
        E_OPEN => Some(SmbEvent::Open {
            session_id: first_u64(parser, P_SESSION_ID),
            tree_id: first_u64(parser, P_TREE_ID),
            path: first_str(parser, P_FILE_NAME).unwrap_or_default(),
        }),
        _ => None,
    }
}
