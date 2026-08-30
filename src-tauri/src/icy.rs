//! ICY metadata decoding — `StreamTitle` / `StreamUrl` and the compact schedule.
//!
//! The station encodes a complete sync + schedule document into the ICY
//! `StreamUrl` query string (contract:
//! `music-db/docs/player-stream-metadata-api.md`). Because Icecast attaches a
//! metadata block to the *source's byte position*, the block arrives in the
//! byte stream exactly where the audio it describes does — which is what makes
//! both accurate now-playing text and drift measurement possible without any
//! extra network request.
//!
//! This module is the decode half of the codec whose canonical implementation
//! is `music-db/src/lib/stream-metadata/icy-payload.ts`; the encoder lives in
//! `raas/src-tauri/src/streaming/icy_metadata.rs`. The three are pinned against
//! the same fixture corpus (`tests/fixtures/icy-payload.json`), so a change on
//! any side that breaks the wire format fails a test here.
//!
//! Everything returns `Option` on purpose. Titles are whitespace-collapsed but
//! NOT escaped by the emitter, so a track title containing `';` can break the
//! block framing; a truncated or malformed block must leave the previous
//! metadata standing rather than panic or blank the display.

const TITLE_TAG: &str = "StreamTitle='";
const URL_TAG: &str = "';StreamUrl='";
const TERM: &str = "';";

/// The contract version this build understands. Anything else is ignored
/// outright rather than best-effort parsed — a future v2 may reuse key names
/// with different meanings.
pub const PAYLOAD_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// Wire types
//
// Deserialise from the compact single-letter keys the wire uses, but serialise
// with long camelCase names for the frontend, which never sees the short form.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct Segment {
    #[serde(rename(deserialize = "k", serialize = "kind"), default)]
    pub kind: String,
    #[serde(rename(deserialize = "i", serialize = "id"), default)]
    pub id: String,
    #[serde(
        rename(deserialize = "a", serialize = "artist"),
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub artist: Option<String>,
    #[serde(
        rename(deserialize = "ti", serialize = "title"),
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub title: Option<String>,
    #[serde(rename(deserialize = "st", serialize = "startMs"), default, deserialize_with = "de_i64")]
    pub start_ms: i64,
    #[serde(rename(deserialize = "du", serialize = "durationMs"), default, deserialize_with = "de_i64")]
    pub duration_ms: i64,
}

#[derive(Debug, Clone, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct Talk {
    #[serde(rename(deserialize = "i", serialize = "id"), default)]
    pub id: String,
    #[serde(rename(deserialize = "st", serialize = "startMs"), default, deserialize_with = "de_i64")]
    pub start_ms: i64,
    /// 0 means the line is still being generated upstream.
    #[serde(rename(deserialize = "du", serialize = "durationMs"), default, deserialize_with = "de_i64")]
    pub duration_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dj: Option<String>,
    #[serde(
        rename(deserialize = "c", serialize = "fromChat"),
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub from_chat: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct Jingle {
    #[serde(rename(deserialize = "i", serialize = "id"), default)]
    pub id: String,
    #[serde(rename(deserialize = "st", serialize = "startMs"), default, deserialize_with = "de_i64")]
    pub start_ms: i64,
    #[serde(rename(deserialize = "du", serialize = "durationMs"), default, deserialize_with = "de_i64")]
    pub duration_ms: i64,
    /// opener | hour | between
    #[serde(rename(deserialize = "r", serialize = "role"), default)]
    pub role: String,
    #[serde(
        rename(deserialize = "n", serialize = "name"),
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct Daypart {
    #[serde(default)]
    pub id: String,
    #[serde(rename(deserialize = "n", serialize = "name"), default)]
    pub name: String,
    #[serde(
        rename(deserialize = "sh", serialize = "showTitle"),
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub show_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dj: Option<String>,
}

/// The compact schedule carried in the `d` query parameter.
///
/// `q` / `tk` / `j` default to empty: the encoder drops them from the tail
/// (`q` first, then `j`, then `tk`, then `d` wholesale) whenever the block
/// would exceed its byte cap, so a short or absent list is routine, not an
/// error condition.
#[derive(Debug, Clone, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct Payload {
    pub v: u32,
    #[serde(rename(deserialize = "s", serialize = "seq"), default, deserialize_with = "de_u64")]
    pub seq: u64,
    #[serde(rename(deserialize = "p", serialize = "playheadMs"), default, deserialize_with = "de_i64")]
    pub playhead_ms: i64,
    #[serde(rename(deserialize = "t", serialize = "wallMs"), default, deserialize_with = "de_i64")]
    pub wall_ms: i64,
    #[serde(
        rename(deserialize = "dp", serialize = "programme"),
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub daypart: Option<Daypart>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub now: Option<Segment>,
    #[serde(rename(deserialize = "q", serialize = "next"), default)]
    pub next: Vec<Segment>,
    #[serde(rename(deserialize = "tk", serialize = "talk"), default)]
    pub talk: Vec<Talk>,
    #[serde(rename(deserialize = "j", serialize = "jingles"), default)]
    pub jingles: Vec<Jingle>,
}

/// The parsed `StreamUrl`. `payload` is absent whenever `d` was dropped.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamUrl {
    pub seq: u64,
    /// Station timeline position (ms) of the audio this block is attached to.
    pub playhead_ms: i64,
    /// Station wall clock (epoch ms) for that same audio instant. The
    /// difference between this and our clock, measured when the block reaches
    /// playback, is the listener's true end-to-end lag.
    pub wall_ms: i64,
    /// track | jingle | talk | off | hb
    pub kind: String,
    pub item_id: String,
    pub payload: Option<Payload>,
}

/// One complete ICY metadata block.
#[derive(Debug, Clone, PartialEq)]
pub struct Block {
    pub title: String,
    pub url: Option<StreamUrl>,
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Split a raw ICY block into its title and (optional) stream URL.
///
/// Uses the literal `';StreamUrl='` as the delimiter rather than stopping at
/// the first `';`, so a title that itself contains `';` still yields the whole
/// title and a usable URL.
pub fn parse_block(raw: &str) -> Option<Block> {
    let start = raw.find(TITLE_TAG)? + TITLE_TAG.len();
    let rest = &raw[start..];

    let (title, url) = match rest.find(URL_TAG) {
        Some(i) => {
            let after = &rest[i + URL_TAG.len()..];
            let url = after.find(TERM).map_or(after, |e| &after[..e]);
            (&rest[..i], (!url.is_empty()).then(|| parse_stream_url(url)).flatten())
        }
        None => (rest.find(TERM).map_or(rest, |e| &rest[..e]), None),
    };

    let title = title.trim().to_string();
    if title.is_empty() && url.is_none() {
        return None;
    }
    Some(Block { title, url })
}

/// Parse the `StreamUrl` query string. Returns `None` for any version other
/// than [`PAYLOAD_VERSION`], or when the URL carries no query at all.
pub fn parse_stream_url(url: &str) -> Option<StreamUrl> {
    let query = url.split_once('?')?.1;

    let mut version: Option<u32> = None;
    let mut seq = 0u64;
    let mut playhead_ms = 0i64;
    let mut wall_ms = 0i64;
    let mut kind = String::new();
    let mut item_id = String::new();
    let mut d: Option<&str> = None;

    // The base may already carry its own query (`…/now?src=icy&v=1&…`), so
    // walk every pair and pick out the ones we know rather than assuming
    // position or that `v` comes first.
    for pair in query.split('&') {
        let (key, val) = pair.split_once('=').unwrap_or((pair, ""));
        match key {
            "v" => version = val.parse().ok(),
            "s" => seq = val.parse().unwrap_or(0),
            "p" => playhead_ms = val.parse().unwrap_or(0),
            "t" => wall_ms = val.parse().unwrap_or(0),
            "k" => kind = percent_decode(val),
            "i" => item_id = percent_decode(val),
            "d" => d = Some(val),
            _ => {}
        }
    }

    if version != Some(PAYLOAD_VERSION) {
        return None;
    }

    Some(StreamUrl {
        seq,
        playhead_ms,
        wall_ms,
        kind,
        item_id,
        payload: d.and_then(decode_payload),
    })
}

/// base64url (no padding) -> JSON -> [`Payload`].
pub fn decode_payload(d: &str) -> Option<Payload> {
    let bytes = base64url_decode(d)?;
    let payload: Payload = serde_json::from_slice(&bytes).ok()?;
    (payload.v == PAYLOAD_VERSION).then_some(payload)
}

// ---------------------------------------------------------------------------
// Small codecs
//
// Hand-rolled rather than pulled in as crates: between them this is under 50
// lines, both are pinned by the shared fixtures, and the engine's dependency
// list is deliberately short.
// ---------------------------------------------------------------------------

fn base64url_decode(s: &str) -> Option<Vec<u8>> {
    fn sextet(c: u8) -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => u32::from(c - b'A'),
            b'a'..=b'z' => u32::from(c - b'a') + 26,
            b'0'..=b'9' => u32::from(c - b'0') + 52,
            b'-' => 62,
            b'_' => 63,
            _ => return None,
        })
    }

    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc = 0u32;
    let mut bits = 0u32;
    for &c in s.as_bytes() {
        if c == b'=' {
            break; // tolerate padding even though the encoder never emits it
        }
        acc = (acc << 6) | sextet(c)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// Decode `%XX` escapes. Note `+` is *not* treated as a space: the emitter uses
/// `encodeURIComponent` semantics, which percent-encodes spaces as `%20`.
fn percent_decode(s: &str) -> String {
    fn hex(c: u8) -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    }

    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---------------------------------------------------------------------------
// Lenient number decoding
//
// The emitter rounds every millisecond field to an integer before encoding, so
// in practice these are always JSON integers. They are decoded leniently
// anyway: serde aborts the *whole* payload on one type mismatch, so a producer
// that ever emitted `"p": 8123456.4` would not merely lose that field — it
// would silently cost us the entire schedule, and with it the sync anchor.
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
#[serde(untagged)]
enum LooseNum {
    Int(i64),
    Float(f64),
    Str(String),
}

impl LooseNum {
    fn to_i64(&self) -> i64 {
        match self {
            LooseNum::Int(i) => *i,
            LooseNum::Float(f) => *f as i64,
            LooseNum::Str(s) => s.trim().parse().unwrap_or(0),
        }
    }
}

fn de_i64<'de, D: serde::Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
    use serde::Deserialize;
    Ok(LooseNum::deserialize(d)?.to_i64())
}

fn de_u64<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    use serde::Deserialize;
    Ok(LooseNum::deserialize(d)?.to_i64().max(0) as u64)
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// The shared cross-language corpus. The TS reference codec produced the
    /// `expected` values; raas' encoder is pinned against the same file. If a
    /// change here diverges from the contract, these fail rather than a
    /// listener seeing the wrong track.
    const FIXTURES: &str = include_str!("../tests/fixtures/icy-payload.json");

    fn cases() -> Vec<serde_json::Value> {
        let v: serde_json::Value = serde_json::from_str(FIXTURES).unwrap();
        v["cases"].as_array().unwrap().clone()
    }

    #[test]
    fn decodes_every_shared_fixture() {
        for case in cases() {
            let name = case["name"].as_str().unwrap();
            let url = case["expected"]["url"].as_str().unwrap();
            let dropped: Vec<&str> = case["expected"]["dropped"]
                .as_array()
                .unwrap()
                .iter()
                .map(|d| d.as_str().unwrap())
                .collect();

            if url.is_empty() {
                // "no base means no StreamUrl" — a title-only block is valid.
                assert!(parse_stream_url(url).is_none(), "{name}");
                continue;
            }

            let parsed = parse_stream_url(url).unwrap_or_else(|| panic!("{name}: no parse"));
            let input = &case["input"]["url"];
            assert_eq!(parsed.seq, input["seq"].as_u64().unwrap(), "{name}: seq");
            assert_eq!(parsed.kind, input["kind"].as_str().unwrap(), "{name}: kind");
            assert_eq!(
                parsed.item_id,
                input["itemId"].as_str().unwrap(),
                "{name}: itemId (percent-decoded)"
            );
            // playheadMs is rounded to an integer by the encoder.
            let want_p = input["playheadMs"].as_f64().unwrap() as i64;
            assert_eq!(parsed.playhead_ms, want_p, "{name}: playheadMs");

            if dropped.contains(&"d") {
                // Truncation dropped the schedule wholesale — still a usable
                // sync anchor, which is the point of the minimum contract.
                assert!(parsed.payload.is_none(), "{name}: expected no payload");
                continue;
            }

            let payload = parsed.payload.unwrap_or_else(|| panic!("{name}: no payload"));
            assert_eq!(payload.v, PAYLOAD_VERSION, "{name}");

            // With nothing dropped, the decoded document must match the exact
            // JSON the encoder base64'd.
            if dropped.is_empty() {
                let want: serde_json::Value =
                    serde_json::from_str(case["expected"]["json"].as_str().unwrap()).unwrap();
                assert_eq!(payload.seq, want["s"].as_u64().unwrap(), "{name}");
                assert_eq!(
                    payload.next.len(),
                    want["q"].as_array().map_or(0, |a| a.len()),
                    "{name}: q length"
                );
                assert_eq!(
                    payload.talk.len(),
                    want["tk"].as_array().map_or(0, |a| a.len()),
                    "{name}: tk length"
                );
                assert_eq!(
                    payload.jingles.len(),
                    want["j"].as_array().map_or(0, |a| a.len()),
                    "{name}: j length"
                );
            }

            // Truncation only ever removes from the tail, so whatever survives
            // is a prefix of what went in.
            let sent_q = case["input"]["payload"]["q"].as_array().map_or(0, |a| a.len());
            assert!(payload.next.len() <= sent_q, "{name}: q grew");
        }
    }

    #[test]
    fn decodes_a_real_track_block() {
        let case = &cases()[0];
        let raw = format!(
            "StreamTitle='{}';StreamUrl='{}';",
            case["expected"]["title"].as_str().unwrap(),
            case["expected"]["url"].as_str().unwrap()
        );
        let block = parse_block(&raw).unwrap();
        assert_eq!(block.title, "The Tower Block Collective - Concrete Sunrise");

        let p = block.url.unwrap().payload.unwrap();
        let now = p.now.unwrap();
        assert_eq!(now.artist.as_deref(), Some("The Tower Block Collective"));
        assert_eq!(now.title.as_deref(), Some("Concrete Sunrise"));
        assert_eq!(p.next[0].title.as_deref(), Some("Second Song"));
        assert_eq!(p.daypart.unwrap().show_title.as_deref(), Some("The Drive Home"));
        assert_eq!(p.talk[0].from_chat, Some(true));
        assert_eq!(p.jingles[0].role, "between");
    }

    /// The emitter does not escape titles, so a title carrying the block's own
    /// terminator must not swallow the URL that follows it.
    #[test]
    fn title_containing_the_terminator_keeps_the_url() {
        let raw = "StreamTitle='Oops'; nope';StreamUrl='https://x.test/n?v=1&s=5&p=1&t=2&k=track&i=z';";
        let block = parse_block(raw).unwrap();
        assert_eq!(block.title, "Oops'; nope");
        assert_eq!(block.url.unwrap().seq, 5);
    }

    #[test]
    fn title_only_block_still_parses() {
        let block = parse_block("StreamTitle='Artist - Track';").unwrap();
        assert_eq!(block.title, "Artist - Track");
        assert!(block.url.is_none());
    }

    #[test]
    fn rejects_other_contract_versions() {
        assert!(parse_stream_url("https://x.test/n?v=2&s=1&p=0&t=0&k=track&i=a").is_none());
        assert!(parse_stream_url("https://x.test/n?s=1&k=track").is_none());
    }

    #[test]
    fn percent_encoded_id_and_base_with_existing_query() {
        let u =
            parse_stream_url("https://x.test/now?src=icy&v=1&s=3&p=50&t=60&k=jingle&i=a%20b%26c")
                .unwrap();
        assert_eq!(u.item_id, "a b&c");
        assert_eq!(u.kind, "jingle");
        assert_eq!(u.seq, 3);
    }

    #[test]
    fn malformed_input_never_panics() {
        for raw in [
            "",
            "\0\0\0\0",
            "StreamTitle=",
            "StreamTitle='unterminated",
            "StreamUrl='https://x.test/n?v=1';",
            "StreamTitle='a';StreamUrl='not a url';",
            "StreamTitle='a';StreamUrl='https://x.test/n?v=1&s=1&p=0&t=0&k=track&i=a&d=!!!not-base64';",
            "StreamTitle='a';StreamUrl='https://x.test/n?v=1&s=1&p=0&t=0&k=track&i=a&d=eyJ2Ijox';",
        ] {
            let _ = parse_block(raw); // must not panic
        }
        assert!(decode_payload("!!!").is_none());
        assert!(decode_payload("").is_none());
    }

    #[test]
    fn base64url_round_trips_all_lengths() {
        // "any", "any1", "any12" cover len % 3 == 0, 1, 2.
        assert_eq!(base64url_decode("YW55").unwrap(), b"any");
        assert_eq!(base64url_decode("YW55MQ").unwrap(), b"any1");
        assert_eq!(base64url_decode("YW55MTI").unwrap(), b"any12");
        // Padding is tolerated even though the encoder never emits it.
        assert_eq!(base64url_decode("YW55MQ==").unwrap(), b"any1");
        // The URL-safe alphabet: '-' and '_' rather than '+' and '/'.
        assert_eq!(base64url_decode("--__").unwrap(), vec![0xfb, 0xef, 0xff]);
        assert!(base64url_decode("a+b/").is_none());
    }

    #[test]
    fn percent_decode_handles_utf8_and_bad_escapes() {
        assert_eq!(percent_decode("Caf%C3%A9"), "Café");
        assert_eq!(percent_decode("a%ZZb"), "a%ZZb");
        assert_eq!(percent_decode("trailing%"), "trailing%");
        // '+' is a literal plus, not a space: the emitter uses
        // encodeURIComponent, which writes a space as %20.
        assert_eq!(percent_decode("a+b"), "a+b");
    }

    #[test]
    fn accepts_non_integer_millis() {
        // {"v":1,"s":1,"p":100.4,"t":2,"q":[],"tk":[],"j":[]}
        let json = br#"{"v":1,"s":1,"p":100.4,"t":2,"q":[],"tk":[],"j":[]}"#;
        let p: Payload = serde_json::from_slice(json).unwrap();
        assert_eq!(p.playhead_ms, 100);
    }
}
