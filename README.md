# smb-heat-spike

The correlation-engine spike: prove that raw `Microsoft-Windows-SMBServer` ETW
events can be joined into `(user, share, path)` on a live server, **open-centric**
(one signal per file open, below the read/write firehose). Nothing else — no
storage, no scoring, no network. This is the highest-risk unknown; everything
downstream is known engineering once this works.

## What's solid vs. what the server settles

**Solid (from the provider manifest):** the keyword mask, the channel, and the
event taxonomy.

- Mask `0xF0` = `Connection(0x10) | Session(0x20) | TreeConnect(0x40) | File(0x80)`.
  This selects the correlation backbone and **excludes** the high-volume
  `Request`/`Response` PDUs (read/write, `0x1`/`0x2`) by construction.
- Lifecycle events live on the **Analytic** channel (id 17), level Informational:

  | Event | Meaning | Carries |
  |------:|---------|---------|
  | 500 / 501 / 502 | connection accept / disconnect / terminate | ConnectionId → client address |
  | 550 / **552** / 554 / 555 | session alloc / **auth success** / term / close | **SessionId → user** |
  | 600 / 601 / 602 | tree alloc / disconnect / terminate | **TreeId → (SessionId, share)** |
  | **650** / 654 | **open established** / closed | **the access pulse** |
  | 108 | create response | fallback for path / access mask |

**The server settles two things** (centralized so each has one fix-site):

1. **ferrisetw API specifics** → `src/etw.rs`. Written against ferrisetw 1.1; the
   session start/process calls are the likeliest tweak. The pure logic
   (`correlation.rs`, `identity.rs`) is version-independent and unit-tested.
2. **ETW property names** → the `VERIFY` block in `src/events.rs`. The Analytic
   events are message-less in the manifest, so the field names are guesses until
   `discover` prints the real ones.

## Run order

```sh
# build on the Windows dev VM (ETW is Windows-only); run an elevated shell
cargo test          # exercises the pure correlation + identity logic
cargo build --release

# 1) DISCOVER — learn the real field names. Generate known traffic while this runs:
#    open a file from a client as a domain user, then as a machine/service account.
cargo run --release -- discover
#    -> for each event it prints the candidate name that parsed, e.g.
#       --- event 552 ---
#         session_id  <- SessionId = 81604... (u64)
#         user        <- UserName = CONTOSO\alice
#    Trim each array in events.rs to the name that showed up.

# 2) RESOLVE — the acceptance test.
cargo run --release -- resolve --rundown
```

### Acceptance test (this is "done")

From a client, open a file on a share as `DOMAIN\alice`. Within a moment the
resolve output shows a line like:

```
[Human] CONTOSO\alice @ 10.0.0.5 | DATA\projects\q3.xlsx  (sess ..., tree ...)
```

Then access something as a machine account and confirm it's tagged `[Machine]`.
Hit that, and the unknown is dead — the rest of the system is downstream of this
tuple.

## Notes

- **`--rundown`** ORs in `Rundown (0x10000)` so the engine learns about sessions
  and trees that existed *before* the trace started. On a busy server, long-lived
  sessions otherwise produce unresolved opens until clients reconnect. The
  rundown events (3004/3005/3006) are also discover targets; verify their fields
  too if you rely on them.
- **The one real contingency:** whether event **650 carries the file path and
  access mask directly, or only a FileId** that must be joined to the create
  response (108). `discover` answers this; if 650 has no usable name, add 108 to
  the resolve path and correlate on FileId. The templated events in the manifest
  (1020, 658, 1017) all surface User + Share + File together, so the linkage
  plainly exists — we're just confirming it's on 650.
- **Cleanup:** a real-time session can linger if the process is killed hard.
  `logman query -ets` to see it, `logman stop SmbHeatSpike -ets` to remove it.
- Must run **elevated**.

## Layout

```
src/
  main.rs          CLI: discover | resolve [--rundown]
  etw.rs           ferrisetw session + callback   (VERIFY: API specifics)
  events.rs        constants, SmbEvent, extractor  (VERIFY: property names)
  correlation.rs   three tables + open join        (pure, tested)
  identity.rs      human vs machine/service/system (pure, tested)
```
