//! Live TV / DVR answers → [`super::model`] values, read from a generic JSON value.
//!
//! **Why not the flat `MediaContainer` DTO.** PMS serialises one element as an OBJECT on one
//! endpoint and as an ARRAY of one on the next (`MediaSubscription`, `MediaGrabOperation`,
//! `TranscodeSession`, `Conflicts`), names the same child `Video` on one answer and `Metadata` on
//! another, and string-encodes numbers on some. A strict DTO fails the whole container on the first
//! surprise, and here a lost container is a lost subscription id — a tuner held until the server
//! notices. So every reader below takes `object | [object, …]`, every scalar takes `string | number
//! | bool`, and an element that cannot name what an action needs is skipped, never invented.

use super::model::*;
use serde_json::Value;

/// `MediaContainer` of a body, else the body itself when it is already a container-shaped object
/// (a PMS answer is always wrapped; a test fixture may hand the inner object).
pub(super) fn container(body: &Value) -> Option<&Value> {
    match body.get("MediaContainer") {
        Some(c) if c.is_object() => Some(c),
        Some(_) => None,
        None if body.is_object() => Some(body),
        None => None,
    }
}

/// The children named `key`: an array's objects, or a lone object as a list of one.
pub(super) fn many<'a>(v: &'a Value, key: &str) -> Vec<&'a Value> {
    match v.get(key) {
        Some(Value::Array(items)) => items.iter().filter(|i| i.is_object()).collect(),
        Some(o @ Value::Object(_)) => vec![o],
        _ => Vec::new(),
    }
}

/// The first child named `key`, or the first of several alternative names.
pub(super) fn first<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().find_map(|k| many(v, k).into_iter().next())
}

/// A scalar as text: a string as-is, a number in its JSON spelling, a bool as `1`/`0`.
pub(super) fn text(v: &Value, key: &str) -> String {
    match v.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => if *b { "1".into() } else { "0".into() },
        _ => String::new(),
    }
}

/// An integer from a number (a float truncates), a numeric string, or a bool; else 0.
pub(super) fn int(v: &Value, key: &str) -> i64 {
    match v.get(key) {
        Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)).unwrap_or(0),
        Some(Value::String(s)) => {
            let s = s.trim();
            s.parse::<i64>().ok().or_else(|| s.parse::<f64>().ok().map(|f| f as i64)).unwrap_or(0)
        }
        Some(Value::Bool(b)) => i64::from(*b),
        _ => 0,
    }
}

/// A float from a number or a numeric string; else 0.
pub(super) fn float(v: &Value, key: &str) -> f64 {
    match v.get(key) {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0.0),
        _ => 0.0,
    }
}

/// A flag from a bool, `1`/`0`, or `"1"`/`"true"`; else false.
pub(super) fn flag(v: &Value, key: &str) -> bool {
    match v.get(key) {
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_i64().is_some_and(|i| i != 0),
        Some(Value::String(s)) => matches!(s.trim(), "1" | "true" | "True"),
        _ => false,
    }
}

/// An id that PMS writes either bare (`12`) or as its resource path (`/media/subscriptions/12`).
fn bare_id(raw: &str, prefix: &str) -> String {
    raw.trim().strip_prefix(prefix).unwrap_or(raw.trim()).trim_matches('/').to_string()
}

/// PMS's in-band error on a 2xx: a container `status` other than 0/200 beside a `message`.
pub(super) fn server_error(c: &Value) -> Option<LiveError> {
    let status = int(c, "status");
    let message = text(c, "message");
    (status != 0 && status != 200 && (!message.is_empty() || c.get("status").is_some()))
        .then(|| LiveError::Server { status, message })
}

/// Percent-decode an `enumValues` label (`12%3A30 AM`), as `decodeURIComponent` would: `%XX` only,
/// a `+` stays a `+`. An invalid escape passes through.
pub(super) fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hex = |c: u8| (c as char).to_digit(16);
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---- providers, lineup, grid ----------------------------------------------------------------

/// `GET /media/providers` → the EPG providers. Providers are read from `MediaProvider[]`, and from
/// the container itself when it carries a provider's fields (the shape the spec's schema draws).
pub(super) fn providers(body: &Value) -> Vec<EpgProvider> {
    let Some(c) = container(body) else { return Vec::new() };
    let mut rows = many(c, "MediaProvider");
    if rows.is_empty() && c.get("identifier").is_some() {
        rows.push(c);
    }
    rows.into_iter()
        .filter_map(|p| {
            let identifier = text(p, "identifier");
            if !identifier.contains("tv.plex.providers.epg") {
                return None;
            }
            let features = many(p, "Feature");
            let grid = features.iter().any(|f| text(f, "type") == "grid");
            let recordable = features.iter().any(|f| text(f, "type") == "subscribe" && text(f, "flavor") == "record");
            Some(EpgProvider {
                dvr_key: text(p, "parentID"),
                title: text(p, "title"),
                server_name: text(p, "friendlyName"),
                identifier,
                recordable,
                grid,
            })
        })
        .collect()
}

/// `GET /{provider}/lineups/dvr/channels` → the lineup. A row without an `id` cannot be tuned or
/// matched and is skipped.
pub(super) fn channels(body: &Value) -> Vec<Channel> {
    let Some(c) = container(body) else { return Vec::new() };
    many(c, "Channel")
        .into_iter()
        .filter_map(|ch| {
            let identifier = text(ch, "id");
            if identifier.is_empty() {
                return None;
            }
            let grid_key = text(ch, "gridKey");
            Some(Channel {
                grid_key: if grid_key.is_empty() { identifier.clone() } else { grid_key },
                identifier,
                vcn: text(ch, "vcn"),
                title: text(ch, "title"),
                call_sign: { let cs = text(ch, "callSign"); if cs.is_empty() { text(ch, "shortTitle") } else { cs } },
                thumb: text(ch, "thumb"),
                hd: flag(ch, "isHd") || flag(ch, "hd"),
                part_key: many(ch, "Media").into_iter().flat_map(|m| many(m, "Part")).map(|p| text(p, "key")).find(|k| !k.is_empty()).unwrap_or_default(),
                drm: flag(ch, "drm") || many(ch, "Media").into_iter().any(|m| flag(m, "drm") || many(m, "Part").into_iter().any(|p| flag(p, "drm"))),
            })
        })
        .collect()
}

/// The metadata half of an airing (titles, ids, subscription marks) from a `Metadata`/`Video`.
fn airing_meta(m: &Value) -> Airing {
    Airing {
        rating_key: text(m, "ratingKey"),
        key: text(m, "key"),
        guid: text(m, "guid"),
        grandparent_guid: text(m, "grandparentGuid"),
        kind: text(m, "type"),
        title: text(m, "title"),
        show_title: text(m, "grandparentTitle"),
        season: int(m, "parentIndex"),
        episode: int(m, "index"),
        year: int(m, "year"),
        summary: text(m, "summary"),
        thumb: { let t = text(m, "thumb"); if t.is_empty() { text(m, "grandparentThumb") } else { t } },
        poster: {
            let cover = many(m, "Image").into_iter().find(|i| text(i, "type") == "coverPoster").map(|i| text(i, "url"));
            [cover.unwrap_or_default(), text(m, "grandparentThumb"), text(m, "thumb")].into_iter().find(|s| !s.is_empty()).unwrap_or_default()
        },
        art: {
            // a server's grid says art/grandparentArt; Plex's own channels say Image[type=background]
            let image = |kind: &str| many(m, "Image").into_iter().find(|i| text(i, "type") == kind).map(|i| text(i, "url"));
            [text(m, "art"), text(m, "grandparentArt"), image("background").unwrap_or_default(), image("coverArt").unwrap_or_default()]
                .into_iter()
                .find(|s| !s.is_empty())
                .unwrap_or_default()
        },
        content_rating: text(m, "contentRating"),
        genres: many(m, "Genre").into_iter().map(|g| text(g, "tag")).filter(|t| !t.is_empty()).collect(),
        subscription_id: nonzero(text(m, "subscriptionID")),
        show_subscription_id: nonzero(text(m, "grandparentSubscriptionID")),
        ..Airing::default()
    }
}

/// The cloud provider's root → its genre tabs (`Feature type="grid"` → `GridChannelFilter[]`).
pub(super) fn cloud_tabs(body: &Value) -> Vec<CloudTab> {
    let Some(c) = container(body) else { return Vec::new() };
    let mut providers = many(c, "MediaProvider");
    if providers.is_empty() {
        providers.push(c);
    }
    providers
        .into_iter()
        .flat_map(|p| many(p, "Feature"))
        .filter(|f| text(f, "type") == "grid")
        .flat_map(|f| many(f, "GridChannelFilter"))
        .filter_map(|g| {
            let (identifier, title) = (text(g, "identifier"), text(g, "title"));
            (!identifier.is_empty() && !title.is_empty()).then_some(CloudTab { identifier, title })
        })
        .collect()
}

/// `GET /services/ultrablur/colors` → the four corners (object or array of one), corner-major:
/// top-left, top-right, bottom-right, bottom-left. `None` unless all four read as hex RGB.
pub(super) fn ultrablur(body: &Value) -> Option<[[f32; 3]; 4]> {
    let c = container(body)?;
    let colors = many(c, "UltraBlurColors").into_iter().next()?;
    let corner = |key: &str| hex_rgb(&text(colors, key));
    Some([corner("topLeft")?, corner("topRight")?, corner("bottomRight")?, corner("bottomLeft")?])
}

/// `"44181b"` (or `"#44181b"`) → RGB in 0..1.
fn hex_rgb(s: &str) -> Option<[f32; 3]> {
    let s = s.trim().trim_start_matches('#');
    if s.len() != 6 || !s.is_ascii() {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(&s[i..i + 2], 16).ok().map(|b| f32::from(b) / 255.0);
    Some([byte(0)?, byte(2)?, byte(4)?])
}

/// `"0"` means "no rule" on the wire as much as an absent field does.
fn nonzero(s: String) -> String {
    if s.trim() == "0" { String::new() } else { s }
}

/// Fill the airing half (`Media`): times, channel, flags.
fn with_media(mut a: Airing, media: &Value) -> Airing {
    a.begins_at = int(media, "beginsAt");
    a.ends_at = int(media, "endsAt");
    a.channel_identifier = text(media, "channelIdentifier");
    a.channel_vcn = text(media, "channelVcn");
    a.channel_title = text(media, "channelTitle");
    a.channel_thumb = text(media, "channelThumb");
    a.on_air = flag(media, "onAir");
    a.premiere = flag(media, "premiere");
    a.media_id = text(media, "id");
    a
}

/// Every airing (a `Media` with a real span) of a list of metadata elements.
fn airings_of(items: Vec<&Value>) -> Vec<Airing> {
    let mut out = Vec::new();
    for m in items {
        let meta = airing_meta(m);
        for media in many(m, "Media") {
            let a = with_media(meta.clone(), media);
            if a.begins_at > 0 && a.ends_at > a.begins_at {
                out.push(a);
            }
        }
    }
    out
}

/// `GET /{provider}/grid?channelGridKey=&date=` → one channel's airings for one UTC day, sorted by
/// start, one per start instant (PMS repeats an airing on two days' answers when it spans midnight;
/// a merged cache dedups the same way). `channel` fills the channel identity when PMS leaves it out.
pub(super) fn grid(body: &Value, channel: Option<&Channel>) -> Vec<Airing> {
    let Some(c) = container(body) else { return Vec::new() };
    let mut items = many(c, "Metadata");
    items.extend(many(c, "Video"));
    let mut out = airings_of(items);
    if let Some(ch) = channel {
        for a in &mut out {
            if a.channel_identifier.is_empty() {
                a.channel_identifier = ch.identifier.clone();
            }
            if a.channel_vcn.is_empty() {
                a.channel_vcn = ch.vcn.clone();
            }
            if a.channel_title.is_empty() {
                a.channel_title = ch.title.clone();
            }
        }
    }
    out.sort_by_key(|a| (a.begins_at, a.ends_at));
    out.dedup_by_key(|a| a.begins_at);
    out
}

// ---- subscriptions, templates, grabs --------------------------------------------------------

fn setting(s: &Value) -> Option<Setting> {
    let id = text(s, "id");
    if id.is_empty() {
        return None;
    }
    let kind = match text(s, "type").as_str() {
        "bool" => SettingKind::Bool,
        "int" => SettingKind::Int,
        _ => SettingKind::Text,
    };
    let options = text(s, "enumValues")
        .split('|')
        .filter(|pair| !pair.is_empty())
        .filter_map(|pair| {
            let (value, label) = pair.split_once(':')?;
            Some(SettingOption { value: value.to_string(), label: pct_decode(label) })
        })
        .collect();
    Some(Setting {
        label: text(s, "label"),
        summary: text(s, "summary"),
        kind,
        default: scalar_setting(s, "default", kind),
        value: scalar_setting(s, "value", kind),
        hidden: flag(s, "hidden"),
        advanced: flag(s, "advanced"),
        group: text(s, "group"),
        options,
        id,
    })
}

/// A setting value as the wire wants it back: bools as `1`/`0`, whatever PMS sent them as.
fn scalar_setting(s: &Value, key: &str, kind: SettingKind) -> String {
    match (kind, s.get(key)) {
        (SettingKind::Bool, Some(Value::Bool(b))) => if *b { "1".into() } else { "0".into() },
        (SettingKind::Bool, Some(Value::String(v))) if v == "true" || v == "false" => {
            if v == "true" { "1".into() } else { "0".into() }
        }
        _ => text(s, key),
    }
}

fn hint(sub: &Value) -> SubscriptionHint {
    let Some(h) = first(sub, &["Video", "Directory", "Metadata", "Playlist"]) else {
        return SubscriptionHint::default();
    };
    SubscriptionHint {
        kind: text(h, "type"),
        title: text(h, "title"),
        show_title: text(h, "grandparentTitle"),
        year: int(h, "year"),
        thumb: { let t = text(h, "thumb"); if t.is_empty() { text(h, "grandparentThumb") } else { t } },
        guid: text(h, "guid"),
    }
}

fn grab(g: &Value) -> Grab {
    let item = first(g, &["Metadata", "Video", "Directory"]);
    let airing = item.map_or_else(Airing::default, |m| {
        let meta = airing_meta(m);
        match many(m, "Media").into_iter().next() {
            Some(media) => with_media(meta, media),
            None => meta,
        }
    });
    let id = text(g, "id");
    let key = text(g, "key");
    Grab {
        key: if key.is_empty() && !id.is_empty() { format!("/media/grabbers/operations/{id}") } else { key },
        id,
        subscription_id: bare_id(&text(g, "mediaSubscriptionID"), "/media/subscriptions/"),
        status: GrabStatus::of(&text(g, "status")),
        percent: float(g, "percent"),
        airing,
    }
}

fn subscription(s: &Value) -> Option<Subscription> {
    let id = bare_id(&text(s, "key"), "/media/subscriptions/");
    if id.is_empty() {
        return None;
    }
    Some(Subscription {
        id,
        kind: int(s, "type"),
        title: text(s, "title"),
        target_section: text(s, "targetLibrarySectionID"),
        target_location: nonzero(text(s, "targetSectionLocationID")),
        section_title: text(s, "librarySectionTitle"),
        location_path: text(s, "locationPath"),
        airings_type: text(s, "airingsType"),
        settings: many(s, "Setting").into_iter().filter_map(setting).collect(),
        hint: hint(s),
        grabs: many(s, "MediaGrabOperation").into_iter().map(grab).collect(),
    })
}

/// `GET /media/subscriptions` (or one rule's GET) → the rules, in priority order.
pub(super) fn subscriptions(body: &Value) -> Vec<Subscription> {
    let Some(c) = container(body) else { return Vec::new() };
    many(c, "MediaSubscription").into_iter().filter_map(subscription).collect()
}

/// `GET /media/subscriptions/template?guid=` → the choices, flattened across `SubscriptionTemplate`.
pub(super) fn template(body: &Value) -> Vec<TemplateChoice> {
    let Some(c) = container(body) else { return Vec::new() };
    let mut templates = many(c, "SubscriptionTemplate");
    if templates.is_empty() {
        templates.push(c);
    }
    templates
        .into_iter()
        .flat_map(|t| many(t, "MediaSubscription"))
        .map(|s| TemplateChoice {
            title: text(s, "title"),
            selected: flag(s, "selected"),
            kind: int(s, "type"),
            parameters: text(s, "parameters"),
            target_section: text(s, "targetLibrarySectionID"),
            target_location: nonzero(text(s, "targetSectionLocationID")),
            section_title: text(s, "librarySectionTitle"),
            location_path: text(s, "locationPath"),
            airings_type: text(s, "airingsType"),
            settings: many(s, "Setting").into_iter().filter_map(setting).collect(),
            hint: hint(s),
        })
        .collect()
}

/// `GET /media/subscriptions/scheduled` → the recording operations, soonest first.
pub(super) fn scheduled(body: &Value) -> Vec<Grab> {
    let Some(c) = container(body) else { return Vec::new() };
    let mut grabs: Vec<Grab> = many(c, "MediaGrabOperation").into_iter().map(grab).collect();
    grabs.sort_by_key(|g| (g.airing.begins_at == 0, g.airing.begins_at));
    grabs
}

/// The conflicts a grab carries (`Conflicts > MediaGrabOperation`, `Conflicts` itself an object or
/// an array).
fn conflicts_of(g: &Value) -> Vec<Conflict> {
    many(g, "Conflicts")
        .into_iter()
        .flat_map(|c| many(c, "MediaGrabOperation"))
        .map(|cg| {
            let parsed = grab(cg);
            Conflict {
                grab_key: parsed.key,
                subscription_id: parsed.subscription_id,
                rolling: flag(cg, "rolling"),
                airing: parsed.airing,
            }
        })
        .collect()
}

/// `POST /media/subscriptions` → the new rule's id and its conflicts.
pub(super) fn created(body: &Value) -> LiveResult<CreateResult> {
    let c = container(body).ok_or(LiveError::Unreadable)?;
    let Some(sub) = many(c, "MediaSubscription").into_iter().next() else {
        return Err(server_error(c).unwrap_or(LiveError::Unreadable));
    };
    Ok(CreateResult {
        subscription_id: bare_id(&text(sub, "key"), "/media/subscriptions/"),
        conflicts: many(sub, "MediaGrabOperation").into_iter().flat_map(conflicts_of).collect(),
    })
}

// ---- tune ------------------------------------------------------------------------------------

fn tuned_item(m: &Value) -> TunedItem {
    let media = many(m, "Media").into_iter().next();
    let streams: Vec<&Value> = media
        .map(|md| many(md, "Part").into_iter().flat_map(|p| many(p, "Stream")).collect())
        .unwrap_or_default();
    let video = streams.iter().find(|s| int(s, "streamType") == 1);
    let audio = streams.iter().find(|s| int(s, "streamType") == 2);
    TunedItem {
        key: text(m, "key"),
        rating_key: text(m, "ratingKey"),
        kind: text(m, "type"),
        title: text(m, "title"),
        show_title: text(m, "grandparentTitle"),
        session: media.map(|md| text(md, "uuid")).unwrap_or_default(),
        begins_at: media.map_or(0, |md| int(md, "beginsAt")),
        ends_at: media.map_or(0, |md| int(md, "endsAt")),
        channel_identifier: media.map(|md| text(md, "channelIdentifier")).unwrap_or_default(),
        channel_title: media.map(|md| text(md, "channelTitle")).unwrap_or_default(),
        channel_vcn: media.map(|md| text(md, "channelVcn")).unwrap_or_default(),
        width: video.map_or(0, |v| int(v, "width")),
        height: video.map_or(0, |v| int(v, "height")),
        video_codec: video.map(|v| text(v, "codec")).unwrap_or_default(),
        audio_codec: audio.map(|a| text(a, "codec")).unwrap_or_default(),
        streams: streams.iter().map(|s| (int(s, "streamType"), text(s, "codec"))).collect(),
        captions: streams
            .iter()
            .filter(|s| int(s, "streamType") == 3)
            .map(|s| LiveCaption {
                id: int(s, "id"),
                codec: text(s, "codec"),
                language: text(s, "language"),
                language_code: text(s, "languageCode"),
                title: text(s, "title"),
            })
            .collect(),
    }
}

/// A `TranscodeSession`'s capture window, if it carries one.
pub(super) fn capture_of(v: &Value) -> Option<CaptureWindow> {
    let ts = first(v, &["TranscodeSession"])?;
    if ts.get("timeStamp").is_none() && ts.get("maxOffsetAvailable").is_none() {
        return None;
    }
    Some(CaptureWindow {
        time_stamp: float(ts, "timeStamp"),
        min_offset: float(ts, "minOffsetAvailable"),
        max_offset: float(ts, "maxOffsetAvailable"),
    })
}

/// A tune's answer (`POST …/tune`) and a refresh of it (`GET /media/subscriptions/{id}`).
///
/// The documented-by-clients shape is `MediaContainer > MediaSubscription(key) >
/// MediaGrabOperation > Video|Metadata (+ TranscodeSession)`, with `Conflicts` on the grab and the
/// programs around now under `Timeline`. The spec's own example is the bare `MediaContainer >
/// Metadata[]` with a `/livetv/sessions/{uuid}` key; that shape is read too, with no subscription
/// id. A conflicted tune may carry no playable item and still carry its subscription's key, which
/// the caller must release.
pub(super) fn tune(body: &Value) -> LiveResult<TuneResult> {
    let c = container(body).ok_or(LiveError::Unreadable)?;
    if let Some(sub) = many(c, "MediaSubscription").into_iter().next() {
        let grabs = many(sub, "MediaGrabOperation");
        let conflicts: Vec<Conflict> = grabs.iter().flat_map(|g| conflicts_of(g)).collect();
        // The grab whose item is the tuned stream: the first that names one.
        let tuned = grabs.iter().find_map(|g| first(g, &["Video", "Metadata"]))
            .or_else(|| first(sub, &["Video", "Metadata"]));
        let item = tuned.map(tuned_item).unwrap_or_default();
        let capture = tuned.and_then(capture_of);
        let programs = first(sub, &["Timeline"])
            .map(|t| {
                let mut items = many(t, "Video");
                items.extend(many(t, "Metadata"));
                let mut a = airings_of(items);
                a.sort_by_key(|x| x.begins_at);
                a
            })
            .unwrap_or_default();
        let subscription_id = bare_id(&text(sub, "key"), "/media/subscriptions/");
        if subscription_id.is_empty() && item.key.is_empty() {
            return Err(LiveError::Unreadable);
        }
        return Ok(TuneResult { subscription_id, item, capture, conflicts, programs });
    }
    if let Some(m) = first(c, &["Metadata", "Video"]) {
        let item = tuned_item(m);
        if item.key.is_empty() {
            return Err(LiveError::Unreadable);
        }
        return Ok(TuneResult { capture: capture_of(m).or_else(|| capture_of(c)), item, ..TuneResult::default() });
    }
    Err(server_error(c).unwrap_or(LiveError::Unreadable))
}
