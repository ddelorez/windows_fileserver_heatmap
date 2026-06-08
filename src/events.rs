//! SMBServer provider constants, the normalized event enum, and the property
//! extraction adapter.
//!
//! Everything in the "FACTS" section is taken straight from the provider
//! manifest (`wevtutil gp Microsoft-Windows-SMBServer /ge /gm:true`) on a live
//! server. The "SCHEMA" section property names + types were extracted from the
//! provider event templates (`Get-WinEvent ... | %{ $_.Template }`) and
//! confirmed against a Stage-1 live capture. The headline finding: the
//! correlation keys are per-object **GUIDs** (ConnectionGUID / SessionGUID /
//! TreeConnectGUID), not integer ids — which is why the earlier integer probes
//! resolved nothing.

use ferrisetw::parser::Parser;
use ferrisetw::GUID;

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
// Correlation key
// ---------------------------------------------------------------------------

/// A normalized, hashable correlation key.
///
/// SMB lifecycle objects are identified by 128-bit GUIDs (`win:GUID` template
/// fields). We fold each GUID into a `u128` so the engine's tables key on a
/// cheap `Copy` type and the unit tests can use plain integer literals.
pub type GuidKey = u128;

/// Fold a windows `GUID` into our canonical `u128` key. Field order is
/// big-endian (data1 in the high bits) so `fmt_guid_key` can render it back in
/// the usual textual form.
pub fn guid_key(g: GUID) -> GuidKey {
    ((g.data1 as u128) << 96)
        | ((g.data2 as u128) << 80)
        | ((g.data3 as u128) << 64)
        | (u64::from_be_bytes(g.data4) as u128)
}

/// Render a `GuidKey` back to the canonical
/// `XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX` textual form.
pub fn fmt_guid_key(k: GuidKey) -> String {
    let d1 = (k >> 96) as u32;
    let d2 = (k >> 80) as u16;
    let d3 = (k >> 64) as u16;
    let d4 = (k as u64).to_be_bytes();
    format!(
        "{d1:08X}-{d2:04X}-{d3:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
        d4[0], d4[1], d4[2], d4[3], d4[4], d4[5], d4[6], d4[7]
    )
}

// ---------------------------------------------------------------------------
// SCHEMA (property names + types — from the provider templates, confirmed live)
//
//   500 ConnAccept : ConnectionGUID(GUID), Address(SocketAddress), TransportName(str)
//   550 SessAlloc  : SessionGUID(GUID), ConnectionGUID(GUID)
//   552 SessAuth   : SessionGUID(GUID), ConnectionGUID(GUID), UserName(str), DomainName(str)
//   600 TreeConn   : TreeConnectGUID(GUID), SessionGUID(GUID), ConnectionGUID(GUID),
//                    ShareGUID(GUID), ShareName(str), ScopeName(str)
//   650 Open       : OpenGUID(GUID), TreeConnectGUID(GUID), SessionGUID(GUID),
//                    ConnectionGUID(GUID), ShareGUID(GUID), Name(str), DesiredAccess(u32)
//
// UserName / ShareName / Name are counted strings that already parse by name as
// String — we deliberately do NOT read the matching *Length fields by hand.
// ---------------------------------------------------------------------------
pub const P_CONN_GUID: &[&str] = &["ConnectionGUID"];
pub const P_SESSION_GUID: &[&str] = &["SessionGUID"];
pub const P_TREE_GUID: &[&str] = &["TreeConnectGUID"];
pub const P_SHARE_GUID: &[&str] = &["ShareGUID"];
pub const P_OPEN_GUID: &[&str] = &["OpenGUID"];
pub const P_USER_NAME: &[&str] = &["UserName"];
pub const P_DOMAIN_NAME: &[&str] = &["DomainName"];
pub const P_SHARE_NAME: &[&str] = &["ShareName"];
pub const P_SCOPE_NAME: &[&str] = &["ScopeName"];
pub const P_FILE_NAME: &[&str] = &["Name"];
pub const P_ADDRESS: &[&str] = &["Address"];
pub const P_TRANSPORT: &[&str] = &["TransportName"];
pub const P_DESIRED_ACCESS: &[&str] = &["DesiredAccess"];

// ---------------------------------------------------------------------------
// Normalized event — decouples the engine from ETW parsing.
//
// The resolve path is now share-name resolution + per-file heat counting, so
// only the two events that drive it are modeled. Identity is intentionally out
// of scope (no 500/552 user/client rejoin), hence no Conn*/Session* variants.
// ---------------------------------------------------------------------------
#[derive(Debug, Clone)]
pub enum SmbEvent {
    /// 600 Smb2TreeConnectAllocate — supplies the ShareGUID -> ShareName binding
    /// the engine keys share resolution on.
    TreeConnect { share_guid: GuidKey, share: String },
    /// 650 Smb2FileOpen — the access pulse. Carries its own ShareGUID (the share
    /// key), OpenGUID (the heat counter), and TreeConnectGUID (kept for
    /// reference, no longer the share key).
    Open { open: GuidKey, tree: GuidKey, share_guid: Option<GuidKey>, path: String, access: u32 },
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

/// Try each candidate name as a `win:GUID` property and normalize it to a
/// `GuidKey`. This is the parse type ferrisetw 1.2.0 exposes for GUID fields
/// (`Parser::try_parse::<ferrisetw::GUID>`); it enforces `InTypeGuid`.
pub fn first_guid(parser: &Parser, names: &[&str]) -> Option<GuidKey> {
    for n in names {
        if let Ok(g) = parser.try_parse::<GUID>(n) {
            return Some(guid_key(g));
        }
    }
    None
}

/// Try each candidate name as an unsigned integer; widen across widths because
/// some masks are 32-bit and we normalize to u64 at the call site.
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

/// Try each candidate name as a `win:SocketAddress`.
///
/// ferrisetw 1.2.0 has no `SocketAddress` parser (and its `IpAddr` impl rejects
/// the SocketAddress out-type), so we pull the raw `SOCKADDR` bytes via the
/// `Vec<u8>` parse and decode them ourselves. NOTE: the SOCKADDR layout decode
/// below is the one part of this change that still needs confirmation against a
/// live 500 event.
pub fn first_socket_addr(parser: &Parser, names: &[&str]) -> Option<String> {
    for n in names {
        if let Ok(bytes) = parser.try_parse::<Vec<u8>>(n) {
            return Some(decode_socket_address(&bytes));
        }
    }
    None
}

/// Decode a Windows `SOCKADDR` blob to a bare IP string (port discarded — we
/// only want the client address for attribution). Falls back to hex so nothing
/// is dropped silently if the family is unrecognized.
pub fn decode_socket_address(b: &[u8]) -> String {
    const AF_INET: u16 = 2;
    const AF_INET6: u16 = 23;
    match b.get(0..2).map(|f| u16::from_le_bytes([f[0], f[1]])) {
        // sockaddr_in:  family(2) port(2) addr(4)
        Some(AF_INET) if b.len() >= 8 => {
            std::net::Ipv4Addr::new(b[4], b[5], b[6], b[7]).to_string()
        }
        // sockaddr_in6: family(2) port(2) flowinfo(4) addr(16) scope(4)
        Some(AF_INET6) if b.len() >= 24 => {
            let mut a = [0u8; 16];
            a.copy_from_slice(&b[8..24]);
            std::net::Ipv6Addr::from(a).to_string()
        }
        _ => b.iter().map(|x| format!("{x:02x}")).collect(),
    }
}

/// Map a raw event to a normalized `SmbEvent`. Returns `None` for ids we don't
/// model (everything outside 600/650 now) or when a required key is missing.
pub fn parse_event(event_id: u16, parser: &Parser) -> Option<SmbEvent> {
    match event_id {
        // 600 carries both the ShareGUID and the ShareName — the binding the
        // engine resolves shares on.
        E_TREE_ALLOC => Some(SmbEvent::TreeConnect {
            share_guid: first_guid(parser, P_SHARE_GUID)?,
            share: first_str(parser, P_SHARE_NAME).unwrap_or_default(),
        }),
        // 650 resolves its share via its OWN ShareGUID; OpenGUID is the heat
        // counter. An open with no OpenGUID can't be counted, so it's skipped;
        // a missing ShareGUID is allowed (resolves to UNKNOWN downstream).
        E_OPEN => Some(SmbEvent::Open {
            open: first_guid(parser, P_OPEN_GUID)?,
            tree: first_guid(parser, P_TREE_GUID).unwrap_or(0),
            share_guid: first_guid(parser, P_SHARE_GUID),
            path: first_str(parser, P_FILE_NAME).unwrap_or_default(),
            access: first_u64(parser, P_DESIRED_ACCESS).unwrap_or(0) as u32,
        }),
        _ => None,
    }
}
