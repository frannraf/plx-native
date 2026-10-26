//! **Live TV debug output** (a `devtriggers` build's event log): what each Live TV exchange with the
//! server came back as — the operation, its HTTP status, how long it took, and the STRUCTURE of the
//! answer: its keys, its array lengths and its value types, never its content. No title, summary,
//! guid, id, name or URL field reaches the log; a `key` path is printed with every id in it replaced
//! by `#`, and only protocol words and numbers (decision codes, codecs, dimensions, `beginsAt`) are
//! printed as they are. The one exception is a server SENTENCE ([`TEXTS`]: a decision's reason, an
//! error's `message`), printed as the server wrote it because it is the answer to "why": it is
//! generic protocol text, but nothing stops a server from quoting a title in one, so a line carrying
//! one is not for pasting anywhere. The structure is what this module's parsers are designed from, so a server
//! that answers differently from the protocol evidence (`docs/pms-api.md` §10) shows up as a shape
//! to compare against the fixtures (`live_tv/fixtures.rs`).
//!
//! A read logs its shape the first [`SHAPES_PER_OP`] times per process and its failures always; a
//! write and a tune log every time. A release build logs none of this.

use super::super::client::ValueOutcome;
use crate::http::Method;
use serde_json::Value;
use std::sync::atomic::{AtomicU32, Ordering};

/// Answers of one read operation whose shape is logged (the rest log only when they fail).
pub(crate) const SHAPES_PER_OP: u32 = 2;
/// A shape longer than this is cut (an event-log line stays readable).
const MAX_LEN: usize = 1_800;
const MAX_DEPTH: usize = 10;
/// Array elements whose keys are merged into the one element shape printed for the array.
const MERGED: usize = 40;

/// Values printed as they are: protocol words, codes and numbers, none of them personal.
const SHOWN: &[&str] = &[
    "status", "generalDecisionCode", "transcodeDecisionCode", "directPlayDecisionCode", "mdeDecisionCode",
    "protocol", "container", "videoCodec", "audioCodec", "codec", "width", "height", "frameRate",
    "videoResolution", "videoFrameRate", "type", "streamType", "decision", "state", "size", "totalSize",
    "offset", "airingsType", "onAir", "premiere", "beginsAt", "endsAt", "duration", "selected", "hdr",
    "scanType", "channels", "bitrate", "audioChannels", "videoProfile", "level", "live", "timeStamp",
    "minOffsetAvailable", "maxOffsetAvailable", "throttled", "complete", "progress", "speed", "context",
    "sourceVideoCodec", "sourceAudioCodec", "transcodeHwRequested", "transcodeHwDecoding",
    "transcodeHwEncoding", "percent", "rolling", "identifier", "protocolCapabilities", "flavor",
    "subscriptionType", "lineupType", "lineup", "allowSync", "mediaProviderID", "librarySectionID",
];

/// Server sentences printed as they are (a decision's reason): generic protocol text.
const TEXTS: &[&str] = &["generalDecisionText", "transcodeDecisionText", "directPlayDecisionText", "mdeDecisionText", "message"];

/// Keys whose string value is a path: printed with every id in it replaced by `#`.
const PATHS: &[&str] = &["key", "parentKey", "grandparentKey", "sessionKey"];

/// The operation a Live TV request is, from its method and path (never the path itself).
pub(crate) fn op_of(method: Method, path: &str) -> &'static str {
    let p = path.split('?').next().unwrap_or(path);
    match method {
        Method::Get if p == "/media/providers" => "providers",
        Method::Get if p.ends_with("/lineups/dvr/channels") => "lineup",
        Method::Get if p.ends_with("/grid") => "grid",
        Method::Get if p == "/media/subscriptions/template" => "template",
        Method::Get if p == "/media/subscriptions/scheduled" => "scheduled",
        Method::Get if p == "/media/subscriptions" => "rules",
        Method::Get if p.starts_with("/media/subscriptions/") => "subscription",
        Method::Post if p.ends_with("/tune") => "tune",
        Method::Post if p == "/media/subscriptions" => "create",
        Method::Put if p.ends_with("/move") => "move",
        Method::Put => "edit",
        Method::Delete if p.starts_with("/media/grabbers/") => "cancel-grab",
        Method::Delete => "delete-subscription",
        _ => "other",
    }
}

/// Log one exchange (`ms` from the request to its answer). See the module doc for what is logged.
pub(crate) fn exchange(method: Method, path: &str, outcome: &ValueOutcome, ms: u128) {
    if !cfg!(feature = "devtriggers") {
        return;
    }
    let op = op_of(method, path);
    let every = !matches!(method, Method::Get);
    let nth = count(op);
    let line = match outcome {
        ValueOutcome::Transport => format!("livetv dbg: {op} -> no answer after {ms} ms"),
        ValueOutcome::Response { status, body } => {
            let ok = (200..300).contains(status);
            if !every && ok && nth > SHAPES_PER_OP {
                return;
            }
            let shape = body.as_ref().map_or_else(|| "(no JSON body)".to_string(), shape);
            format!("livetv dbg: {op} #{nth} -> HTTP {status} in {ms} ms: {shape}")
        }
    };
    plx_base::eventlog::log(&line);
}

/// Log a parser's verdict on an answer (what it found, by count): the other half of a shape, when
/// an answer reads differently from what the screen then shows.
pub(crate) fn parsed(what: &str) {
    if cfg!(feature = "devtriggers") {
        plx_base::eventlog::log(&format!("livetv dbg: {what}"));
    }
}

/// Where a live session's capture window came from, once per transcode session and in a release
/// build's log too (pause and rewind cannot work without it, and where a server keeps it is not yet
/// measured): the transcode list, or nowhere — then with the [`shape`]s of the timeline answer and
/// of the transcode list, keys and protocol values only.
pub(crate) fn note_capture_source(session: &str, found: bool, timeline: Option<&Value>, sessions: Option<&Value>) {
    static NOTED: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());
    {
        let mut g = NOTED.lock().unwrap_or_else(|e| e.into_inner());
        if *g == session {
            return;
        }
        session.clone_into(&mut g);
    }
    if found {
        plx_base::eventlog::log("livetv: the capture window comes from the transcode list (not the timeline answer)");
    } else {
        plx_base::eventlog::log(&format!(
            "livetv: no capture window; timeline answer {}; transcode list {}",
            timeline.map_or_else(|| "unreadable".to_string(), shape),
            sessions.map_or_else(|| "unread".to_string(), shape)
        ));
    }
}

/// How many answers `op` has had in this process (1 for the first).
fn count(op: &'static str) -> u32 {
    static COUNTS: [AtomicU32; 16] = [const { AtomicU32::new(0) }; 16];
    const OPS: [&str; 16] = [
        "providers", "lineup", "grid", "template", "scheduled", "rules", "subscription", "tune", "create", "move",
        "edit", "cancel-grab", "delete-subscription", "other", "", "",
    ];
    let i = OPS.iter().position(|o| *o == op).unwrap_or(13);
    COUNTS[i].fetch_add(1, Ordering::Relaxed).saturating_add(1)
}

/// The structure of `v`: keys, array lengths (`[N×…]`, the element shape merged over the first
/// elements) and value types (`s` string, `n` number, `b` bool), with the values of [`SHOWN`],
/// [`TEXTS`] and [`PATHS`] keys printed (paths with their ids replaced by `#`).
pub(crate) fn shape(v: &Value) -> String {
    let mut out = String::new();
    write_shape(&mut out, None, v, 0);
    if out.len() > MAX_LEN {
        let mut cut = MAX_LEN;
        while !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out.truncate(cut);
        out.push('…');
    }
    out
}

fn write_shape(out: &mut String, key: Option<&str>, v: &Value, depth: usize) {
    if out.len() > MAX_LEN {
        return;
    }
    let shown = key.is_some_and(|k| SHOWN.contains(&k));
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) if shown => out.push_str(if *b { "true" } else { "false" }),
        Value::Bool(_) => out.push('b'),
        Value::Number(n) if shown => out.push_str(&n.to_string()),
        Value::Number(_) => out.push('n'),
        Value::String(s) => match key {
            Some(k) if SHOWN.contains(&k) && s.len() <= 40 && is_word(s) => {
                out.push('"');
                out.push_str(s);
                out.push('"');
            }
            Some(k) if TEXTS.contains(&k) && s.len() <= 200 && s.chars().all(|c| c.is_ascii_graphic() || c == ' ') => {
                out.push('"');
                out.push_str(s);
                out.push('"');
            }
            Some(k) if PATHS.contains(&k) => {
                out.push('"');
                out.push_str(&path_shape(s));
                out.push('"');
            }
            _ => out.push('s'),
        },
        Value::Array(a) => {
            out.push('[');
            out.push_str(&a.len().to_string());
            if let Some(element) = merged(a) {
                out.push('×');
                if depth >= MAX_DEPTH {
                    out.push('…');
                } else {
                    write_shape(out, key, &element, depth + 1);
                }
            }
            out.push(']');
        }
        Value::Object(m) => {
            if depth >= MAX_DEPTH {
                out.push_str("{…}");
                return;
            }
            out.push('{');
            for (i, (k, v)) in m.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(k);
                out.push(':');
                write_shape(out, Some(k), v, depth + 1);
            }
            out.push('}');
        }
    }
}

/// One element standing for an array: the first element, or for objects the union of the keys
/// of the first [`MERGED`] elements (each key's first value), so a key only some elements carry
/// still shows.
fn merged(a: &[Value]) -> Option<Value> {
    let first = a.first()?;
    if !first.is_object() {
        return Some(first.clone());
    }
    let mut union = serde_json::Map::new();
    for element in a.iter().take(MERGED) {
        if let Value::Object(m) = element {
            for (k, v) in m {
                union.entry(k.clone()).or_insert_with(|| v.clone());
            }
        }
    }
    Some(Value::Object(union))
}

/// A protocol word: short ASCII letters, digits and `._-:/x` (never a sentence or a name).
fn is_word(s: &str) -> bool {
    s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b':' | b'/'))
}

/// A path with every segment that is not a plain lowercase word replaced by `#`, and its query cut.
/// An id's shape, never its content: its length and the kinds of character in it ("24 hex",
/// "36 hex-dash", "12 digit-dot", "40 alnum-dash"), what tells a channel's lineup id from its grid
/// key or its number when a list names one of them.
pub(crate) fn id_shape(s: &str) -> String {
    let b = s.as_bytes();
    let has = |f: fn(&u8) -> bool| b.iter().any(f);
    let kind = if b.is_empty() {
        "empty"
    } else if b.iter().all(u8::is_ascii_digit) {
        "digit"
    } else if b.iter().all(|c| c.is_ascii_digit() || *c == b'.') {
        "digit-dot"
    } else if b.iter().all(u8::is_ascii_hexdigit) {
        "hex"
    } else if b.iter().all(|c| c.is_ascii_hexdigit() || *c == b'-') {
        "hex-dash"
    } else if b.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'-' || *c == b'_') {
        if has(|c| *c == b'-' || *c == b'_') { "alnum-dash" } else { "alnum" }
    } else {
        "other"
    };
    format!("{} {kind}", b.len())
}

pub(crate) fn path_shape(s: &str) -> String {
    let (path, query) = match s.split_once('?') {
        Some((p, _)) => (p, "?…"),
        None => (s, ""),
    };
    let segs: Vec<&str> = path
        .split('/')
        .map(|seg| if !seg.is_empty() && seg.bytes().all(|b| b.is_ascii_lowercase()) && seg.len() <= 24 { seg } else if seg.is_empty() { "" } else { "#" })
        .collect();
    format!("{}{query}", segs.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_id_shape_names_its_length_and_its_kind_only() {
        assert_eq!(id_shape("5e20b730f2f8d5003d739db7"), "24 hex");
        assert_eq!(id_shape("4.1"), "3 digit-dot");
        assert_eq!(id_shape("ab-12_cd"), "8 alnum-dash");
        assert_eq!(id_shape(""), "0 empty");
    }

    #[test]
    fn a_shape_keeps_the_structure_and_none_of_the_content() {
        let v: Value = serde_json::from_str(
            r#"{"MediaContainer":{"size":2,"MediaSubscription":[{"key":"/media/subscriptions/1234","title":"A Private Show",
            "MediaGrabOperation":[{"status":"inprogress","Metadata":{"key":"/livetv/sessions/0e0e-77","guid":"plex://episode/5d9","summary":"Secret",
            "Media":[{"beginsAt":1760011200,"videoCodec":"mpeg2video","channelTitle":"Home Channel"}]}}]},{"extra":true}]}}"#,
        )
        .unwrap();
        let s = shape(&v);
        for leaked in ["Private", "Secret", "5d9", "1234", "0e0e", "Home Channel"] {
            assert!(!s.contains(leaked), "{leaked} leaked into {s}");
        }
        assert!(s.contains("size:2"), "{s}");
        assert!(s.contains("MediaSubscription:[2×{"), "{s}");
        assert!(s.contains("key:\"/media/subscriptions/#\""), "{s}");
        assert!(s.contains("key:\"/livetv/sessions/#\""), "{s}");
        assert!(s.contains("status:\"inprogress\""), "{s}");
        assert!(s.contains("beginsAt:1760011200"), "{s}");
        assert!(s.contains("videoCodec:\"mpeg2video\""), "{s}");
        assert!(s.contains("title:s") && s.contains("guid:s") && s.contains("channelTitle:s"), "{s}");
        assert!(s.contains("extra:b"), "a key only a later element carries still shows: {s}");
    }

    #[test]
    fn a_sentence_under_a_word_key_is_not_printed() {
        let v: Value = serde_json::from_str(r#"{"type":"My Show Name","generalDecisionText":"Direct play not available; Conversion OK."}"#).unwrap();
        let s = shape(&v);
        assert!(s.contains("type:s"), "{s}");
        assert!(s.contains("Conversion OK"), "{s}");
    }

    #[test]
    fn requests_are_named_by_operation_never_by_path() {
        assert_eq!(op_of(Method::Post, "/livetv/dvrs/1/channels/abc/tune"), "tune");
        assert_eq!(op_of(Method::Get, "/tv.plex.providers.epg.cloud:1/grid?channelGridKey=x&date=2025-10-09"), "grid");
        assert_eq!(op_of(Method::Get, "/media/subscriptions/template?guid=plex%3A%2F%2Fx"), "template");
        assert_eq!(op_of(Method::Delete, "/media/grabbers/operations/9"), "cancel-grab");
        assert_eq!(op_of(Method::Delete, "/media/subscriptions/9"), "delete-subscription");
        assert_eq!(op_of(Method::Put, "/media/subscriptions/9/move?after=3"), "move");
        assert_eq!(path_shape("/library/metadata/123/children?x=1"), "/library/metadata/#/children?…");
    }
}
