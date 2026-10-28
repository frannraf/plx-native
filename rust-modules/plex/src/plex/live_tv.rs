//! Live TV and DVR on a Plex Media Server: the guide (EPG providers, lineups, the time grid),
//! recording rules and recording operations, and the live tune a channel plays from.
//! `docs/pms-api.md` §10 is the contract this file follows; the bundled OpenAPI spec documents
//! the paths but not the tune's real answer, which clients receive as a `MediaSubscription`.
//!
//! This file is the PROTOCOL half: one `impl Client` method per operation, each building its path
//! through [`QueryBuilder`] and reading its answer through `live_tv/parse.rs` into the plain values
//! of `live_tv/model.rs`. When to fetch, what to cache, and who owns a tune's release are the data
//! and media layers' business.
//!
//! **Every write is gated** ([`guard`]). Creating, editing, deleting or re-prioritising a rule,
//! cancelling a recording, and tuning a channel all change a household's server: a recording that
//! does not happen, or a tuner taken from one that should. A `devtriggers` (debug) build refuses
//! them unless the server is the synthetic mock (`tests/mock_pms.py --livetv`, which identifies as a
//! `-synthetic` build) or the owner armed the live switch (`plxnative-livetv-live`). A release build
//! always writes. Releasing a tune this process made ([`Client::live_release`]) is cleanup of a
//! permitted write and is never refused.

use super::client::{Client, QueryBuilder, ValueOutcome};
use crate::http::Method;
use serde_json::Value;

pub mod cloud;
pub(super) mod debug;
mod model;
mod parse;
#[cfg(test)]
mod fixtures;
#[cfg(test)]
mod tests;

pub use model::*;

/// The debug-build write gate. See the module doc.
pub mod guard {
    #[cfg(feature = "devtriggers")]
    plx_base::devtrig::latched_flag!(
        /// **The owner's live switch** (`plxnative-livetv-live` in the install's runtime root): a
        /// `devtriggers` build sends Live TV and DVR writes to a REAL server while it exists. Off by
        /// default, read once per process. **No agent creates this file unless the owner asks for it
        /// in that session** (AGENTS.md; `.claude/hooks/watchlist-live-guard.py` refuses a command that
        /// would).
        fn live_armed = "livetv-live";
    );
    #[cfg(not(feature = "devtriggers"))]
    fn live_armed() -> bool {
        false
    }

    /// **The owner's standing permission, compiled into one build**: a build made with
    /// `PLX_LIVETV_WRITES=1` in its environment writes Live TV and DVR changes to a real server
    /// with no switch, as if the switch were always armed. For the owner's own test builds: on
    /// 2026-10-22 they asked for a debug build that records and tunes without a switch. A plain
    /// `make` does not set it, so the default stays the gate; **no agent builds with it unless the
    /// owner asks for it in that session** (AGENTS.md).
    pub(super) const BUILT_WITH_WRITES: bool = match option_env!("PLX_LIVETV_WRITES") {
        Some(v) => is_one(v.as_bytes()),
        None => false,
    };

    /// `"1"`, compared in a const context (a `str` pattern is not allowed there).
    const fn is_one(b: &[u8]) -> bool {
        b.len() == 1 && b[0] == b'1'
    }

    /// The pure decision: a release build always writes; a debug build writes only to the mock or
    /// under the owner's switch (armed by the file, or compiled in).
    pub(super) fn permit(devtriggers_build: bool, mock_server: bool, live: bool) -> bool {
        !devtriggers_build || mock_server || live
    }

    /// Is `version` (a server's self-reported build) the synthetic mock's? `tests/mock_pms.py`
    /// answers `1.41.0.0000-synthetic`; no real PMS build carries the suffix.
    pub(super) fn is_mock_version(version: &str) -> bool {
        version.trim().ends_with("-synthetic")
    }

    /// Was the last Live TV write this process tried refused by the gate? Read by the debug cue.
    static LAST_REFUSED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    pub fn last_write_refused() -> bool {
        LAST_REFUSED.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// May this process write to the server that reports `version`? Logs a refusal by operation
    /// name only.
    pub(super) fn allow(op: &str, version: &str) -> bool {
        let live = live_armed() || BUILT_WITH_WRITES;
        let ok = permit(cfg!(feature = "devtriggers"), is_mock_version(version), live);
        LAST_REFUSED.store(!ok, std::sync::atomic::Ordering::Relaxed);
        if !ok {
            plx_base::eventlog::log(&format!(
                "livetv: {op} refused (debug build: the server is not the mock and the live switch is off)"
            ));
        }
        ok
    }

    /// BOOT: log once whether a debug build may write Live TV/DVR changes to a real server. Silent
    /// in a release build; the trigger's name rides its own gate (`ci/check-package.py`).
    pub fn announce_live_writes() {
        #[cfg(feature = "devtriggers")]
        {
            if BUILT_WITH_WRITES {
                plx_base::eventlog::log("livetv: LIVE writes enabled by the build (PLX_LIVETV_WRITES=1)");
            } else if live_armed() {
                plx_base::eventlog::log("livetv: LIVE writes enabled by trigger (plxnative-livetv-live)");
            } else {
                plx_base::eventlog::log("livetv: LIVE writes off (debug build; the mock is still written)");
            }
        }
    }
}

/// A path segment PMS hands out as an id (`12`, a uuid): letters, digits, `-`, `_`, `.`.
fn is_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 128 && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// A provider identifier as it prefixes a path (`tv.plex.providers.epg.cloud:12`).
fn is_provider(s: &str) -> bool {
    s.starts_with("tv.plex.providers.epg")
        && s.len() <= 128
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
}

/// `YYYY-MM-DD`.
fn is_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b.iter().enumerate().all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
}

/// The UTC calendar date of an epoch second, `YYYY-MM-DD` — the grid's `date` parameter, which
/// names a UTC day (`docs/pms-api.md` §10). Civil-from-days, Howard Hinnant's algorithm.
pub fn utc_date(epoch_s: i64) -> String {
    let days = epoch_s.div_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

// ---- pure request builders (tested without a server) ------------------------------------------

pub(super) fn lineup_path(provider: &str) -> Option<String> {
    is_provider(provider).then(|| format!("/{provider}/lineups/dvr/channels"))
}

pub(super) fn grid_path(provider: &str, grid_key: &str, date: &str) -> Option<String> {
    (is_provider(provider) && !grid_key.is_empty() && is_date(date)).then(|| {
        QueryBuilder::new(format!("/{provider}/grid"))
            .str("channelGridKey", grid_key)
            .str("date", date)
            .build()
    })
}

/// `GET /{provider}/hubs/discover?…` — the What's On page's hubs, `count` items each, with the
/// channels watched lately and the stations (the parameters the stock clients send).
pub(super) fn hubs_path(provider: &str, count: i64) -> Option<String> {
    is_provider(provider).then(|| {
        QueryBuilder::new(format!("/{provider}/hubs/discover"))
            .int("count", count)
            .int("includeStations", 1)
            .int("includeRecentChannels", 1)
            .int("includeMeta", 1)
            .int("includeExternalMetadata", 1)
            .build()
    })
}

/// Items asked of each What's On hub: a row of cards, as the stock page shows.
pub const HUB_ITEMS: i64 = 16;

pub(super) fn subscription_path(id: &str) -> Option<String> {
    is_id(id).then(|| format!("/media/subscriptions/{id}"))
}

pub(super) fn template_path(guid: &str) -> Option<String> {
    (!guid.is_empty()).then(|| QueryBuilder::new("/media/subscriptions/template").str("guid", guid).build())
}

/// `POST /media/subscriptions?…` — the official clients' parameter order: `includeGrabs`, the
/// target library and location, `type`, one `prefs[id]` per setting, then the template's
/// `parameters` verbatim.
pub(super) fn create_path(req: &CreateSubscription) -> Option<String> {
    if req.target_section.is_empty() || req.parameters.is_empty() {
        return None;
    }
    let mut q = QueryBuilder::new("/media/subscriptions")
        .int("includeGrabs", 1)
        .str("targetLibrarySectionID", &req.target_section)
        .opt_str("targetSectionLocationID", &req.target_location)
        .int("type", req.kind);
    for (id, value) in &req.prefs {
        q = q.encoded_key(&format!("prefs[{id}]"), value);
    }
    let path = q.verbatim(&req.parameters).build();
    // `verbatim` drops a fragment that is not a plain query string; a rule created without its
    // hints would record the wrong thing, so that is a refusal, not a smaller request.
    path.contains(req.parameters.trim_start_matches(['&', '?'])).then_some(path)
}

/// `PUT /media/subscriptions/{id}?…` — the create parameters minus `type` and `parameters`.
pub(super) fn edit_path(id: &str, req: &EditSubscription) -> Option<String> {
    let base = subscription_path(id)?;
    let mut q = QueryBuilder::new(base)
        .int("includeGrabs", 1)
        .opt_str("targetLibrarySectionID", &req.target_section)
        .opt_str("targetSectionLocationID", &req.target_location);
    for (sid, value) in &req.prefs {
        q = q.encoded_key(&format!("prefs[{sid}]"), value);
    }
    Some(q.build())
}

/// `PUT /media/subscriptions/{id}/move[?after={other}]` — no `after` moves it to the top.
pub(super) fn move_path(id: &str, after: Option<&str>) -> Option<String> {
    let base = format!("{}/move", subscription_path(id)?);
    match after {
        None => Some(base),
        Some(a) if is_id(a) => Some(QueryBuilder::new(base).str("after", a).build()),
        Some(_) => None,
    }
}

/// A grab's own key, which is what a cancel DELETEs. Only `/media/grabbers/operations/{id}` is
/// accepted: the key comes from the server and must not be able to name anything else.
pub(super) fn grab_path(grab_key: &str) -> Option<String> {
    let id = grab_key.strip_prefix("/media/grabbers/operations/")?;
    is_id(id).then(|| grab_key.to_string())
}

/// `POST /livetv/dvrs/{dvr}/channels/{channel}/tune?X-Plex-Session-Identifier={session}`. The
/// channel identifier is percent-encoded as one path segment (it can carry `-`, `.`, and in some
/// lineups `/`). The session names who holds the tuner — the keepalive and the transcode name the
/// same one — and a real server answered a tune without it HTTP 400.
pub(super) fn tune_path(dvr_key: &str, channel: &str, session: &str) -> Option<String> {
    (is_id(dvr_key) && !channel.is_empty() && !session.is_empty()).then(|| {
        QueryBuilder::new(format!("/livetv/dvrs/{dvr_key}/channels/{}/tune", super::client::urlenc_str(channel)))
            .str("X-Plex-Session-Identifier", session)
            .build()
    })
}

// ---- the operations ---------------------------------------------------------------------------

/// A completed answer → its JSON, or the error a non-2xx / unreadable / in-band failure is.
fn ok_body(outcome: ValueOutcome) -> Result<Value, LiveError> {
    match outcome {
        ValueOutcome::Transport => Err(LiveError::Transport),
        // A refusal with the server's own sentence ("Tuning failed") says more than its status;
        // the statuses callers act on (a retry by number, gone, forbidden, a duplicate) stay bare.
        ValueOutcome::Response { status, body } if !(200..300).contains(&status) => {
            let said = (!matches!(status, 400 | 403 | 404 | 409))
                .then(|| body.as_ref().and_then(parse::container).map(|c| parse::text(c, "message")))
                .flatten()
                .filter(|m| !m.is_empty());
            Err(match said {
                Some(message) => LiveError::Server { status: status as i64, message },
                None => LiveError::Status(status),
            })
        }
        ValueOutcome::Response { body: Some(v), .. } => match parse::container(&v).and_then(parse::server_error) {
            Some(e) => Err(e),
            None => Ok(v),
        },
        ValueOutcome::Response { body: None, .. } => Err(LiveError::Unreadable),
    }
}

/// A completed write whose body does not matter.
fn ok_status(outcome: ValueOutcome) -> LiveResult<()> {
    match outcome {
        ValueOutcome::Transport => Err(LiveError::Transport),
        ValueOutcome::Response { status, .. } if (200..300).contains(&status) => Ok(()),
        ValueOutcome::Response { status, .. } => Err(LiveError::Status(status)),
    }
}

impl Client {
    /// The write gate for this server ([`guard`]).
    fn live_write_allowed(&self, op: &str) -> bool {
        guard::allow(op, &super::serverinfo::version_of(self.id))
    }

    /// `GET /media/providers` → this server's EPG providers (one per DVR). Empty when the server
    /// has no DVR or the account may not use it.
    pub fn live_tv_providers(&self) -> LiveResult<Vec<EpgProvider>> {
        let body = ok_body(self.json_value(Method::Get, "/media/providers"))?;
        let list = parse::providers(&body);
        debug::parsed(&format!(
            "providers parsed: {} EPG provider(s), {} tunable, {} with a guide",
            list.len(),
            list.iter().filter(|p| p.tunable()).count(),
            list.iter().filter(|p| p.grid).count()
        ));
        Ok(list)
    }

    /// A provider's channel lineup.
    pub fn live_tv_channels(&self, provider: &str) -> LiveResult<Vec<Channel>> {
        let path = lineup_path(provider).ok_or(LiveError::Invalid)?;
        let body = ok_body(self.json_value(Method::Get, &path))?;
        let channels = parse::channels(&body);
        debug::parsed(&format!(
            "lineup parsed: {} channel(s), {} with a grid key, {} with a number",
            channels.len(),
            channels.iter().filter(|c| !c.grid_key.is_empty()).count(),
            channels.iter().filter(|c| !c.vcn.is_empty()).count()
        ));
        Ok(channels)
    }

    /// One channel's airings for one UTC day (`date` = [`utc_date`]).
    pub fn live_tv_grid(&self, provider: &str, channel: &Channel, date: &str) -> LiveResult<Vec<Airing>> {
        let path = grid_path(provider, &channel.grid_key, date).ok_or(LiveError::Invalid)?;
        let body = ok_body(self.json_value(Method::Get, &path))?;
        let airings = parse::grid(&body, Some(channel));
        // An answer that held entries but parsed to no airing is a shape this parser misses.
        let held = parse::container(&body).and_then(|c| c.get("size")).and_then(Value::as_i64).unwrap_or(0);
        if airings.is_empty() && held > 0 {
            debug::parsed(&format!("grid parsed: 0 airings from an answer of size {held}"));
        }
        Ok(airings)
    }

    /// **The What's On page's hubs** of the DVR's provider, [`HUB_ITEMS`] items each.
    pub fn live_tv_hubs(&self, provider: &str) -> LiveResult<Vec<LiveHub>> {
        let path = hubs_path(provider, HUB_ITEMS).ok_or(LiveError::Invalid)?;
        let body = ok_body(self.json_value(Method::Get, &path))?;
        let hubs = parse::hubs(&body);
        // A release build's event log too: the endpoint is undocumented, and its shape is what the
        // page is drawn from (hub types and counts, never a title or id).
        plx_base::eventlog::log(&format!(
            "livetv: What's On read: {} hub(s) {:?}",
            hubs.len(),
            hubs.iter()
                .map(|h| format!(
                    "{}:{} {} item(s), {} with times",
                    h.identifier.rsplit('.').next().unwrap_or(""),
                    h.kind,
                    h.items.len(),
                    h.items.iter().filter(|a| a.begins_at > 0).count()
                ))
                .collect::<Vec<_>>()
        ));
        Ok(hubs)
    }

    /// The four UltraBlur colours of an image (`url`: a PMS path or an absolute URL, as the guide's
    /// art is), corner-major: top-left, top-right, bottom-right, bottom-left, each RGB in 0..1. What
    /// the guide tints its page with; PMS extracts them, so the image never has to be decoded here.
    pub fn ultrablur_colors(&self, url: &str) -> LiveResult<[[f32; 3]; 4]> {
        if url.is_empty() {
            return Err(LiveError::Invalid);
        }
        let path = QueryBuilder::new("/services/ultrablur/colors").str("url", url).build();
        let body = ok_body(self.json_value(Method::Get, &path))?;
        parse::ultrablur(&body).ok_or(LiveError::Unreadable)
    }

    /// The recording rules, in priority order.
    pub fn dvr_subscriptions(&self) -> LiveResult<Vec<Subscription>> {
        let body = ok_body(self.json_value(Method::Get, "/media/subscriptions"))?;
        Ok(parse::subscriptions(&body))
    }

    /// One recording rule.
    pub fn dvr_subscription(&self, id: &str) -> LiveResult<Subscription> {
        let path = subscription_path(id).ok_or(LiveError::Invalid)?;
        let body = ok_body(self.json_value(Method::Get, &path))?;
        parse::subscriptions(&body).into_iter().next().ok_or(LiveError::Unreadable)
    }

    /// The recording operations (scheduled, recording now, finished, failed), soonest first.
    pub fn dvr_scheduled(&self) -> LiveResult<Vec<Grab>> {
        let body = ok_body(self.json_value(Method::Get, "/media/subscriptions/scheduled"))?;
        Ok(parse::scheduled(&body))
    }

    /// The record choices for a program (`guid` = [`Airing::guid`]).
    pub fn dvr_template(&self, guid: &str) -> LiveResult<Vec<TemplateChoice>> {
        let path = template_path(guid).ok_or(LiveError::Invalid)?;
        let body = ok_body(self.json_value(Method::Get, &path))?;
        let choices = parse::template(&body);
        debug::parsed(&format!(
            "template parsed: {} choice(s), kinds {:?}, {} with a library, {} with parameters",
            choices.len(),
            choices.iter().map(|c| c.kind).collect::<Vec<_>>(),
            choices.iter().filter(|c| !c.target_section.is_empty()).count(),
            choices.iter().filter(|c| !c.parameters.is_empty()).count()
        ));
        Ok(choices)
    }

    /// Create a recording rule. The rule exists on the server even when the answer carries
    /// conflicts; the caller then prefers it ([`Client::dvr_move`] with no `after`) or deletes it.
    pub fn dvr_create(&self, req: &CreateSubscription) -> LiveResult<CreateResult> {
        let path = create_path(req).ok_or(LiveError::Invalid)?;
        if !self.live_write_allowed("record") {
            return Err(LiveError::Refused);
        }
        let outcome = self.json_value(Method::Post, &path);
        log_write("record", &outcome);
        let made = parse::created(&ok_body(outcome)?);
        match &made {
            Ok(m) => debug::parsed(&format!(
                "record parsed: rule id {}, {} conflict(s)",
                if m.subscription_id.is_empty() { "missing" } else { "present" },
                m.conflicts.len()
            )),
            Err(e) => debug::parsed(&format!("record answer unreadable: {}", e.reason())),
        }
        made
    }

    /// Change a rule's library or preferences.
    pub fn dvr_edit(&self, id: &str, req: &EditSubscription) -> LiveResult<()> {
        let path = edit_path(id, req).ok_or(LiveError::Invalid)?;
        if !self.live_write_allowed("edit recording") {
            return Err(LiveError::Refused);
        }
        let outcome = self.json_value(Method::Put, &path);
        log_write("edit recording", &outcome);
        ok_status(outcome)
    }

    /// Delete a rule and every grab it scheduled ("Cancel recording"). A 404 means it was already
    /// gone, which is what the caller wanted.
    pub fn dvr_delete(&self, id: &str) -> LiveResult<()> {
        let path = subscription_path(id).ok_or(LiveError::Invalid)?;
        if !self.live_write_allowed("cancel recording") {
            return Err(LiveError::Refused);
        }
        let outcome = self.json_value(Method::Delete, &path);
        log_write("cancel recording", &outcome);
        match ok_status(outcome) {
            Err(LiveError::Status(404)) => Ok(()),
            other => other,
        }
    }

    /// Move a rule in the priority list: after `after`, or to the top when `None`. PMS allows it
    /// to the server's admin only (403 for anyone else).
    pub fn dvr_move(&self, id: &str, after: Option<&str>) -> LiveResult<()> {
        let path = move_path(id, after).ok_or(LiveError::Invalid)?;
        if !self.live_write_allowed("reprioritize recording") {
            return Err(LiveError::Refused);
        }
        let outcome = self.json_value(Method::Put, &path);
        log_write("reprioritize recording", &outcome);
        ok_status(outcome)
    }

    /// Cancel one recording operation — stop a recording in progress or skip one airing — by the
    /// grab's own key. Its rule stays.
    pub fn dvr_cancel_grab(&self, grab_key: &str) -> LiveResult<()> {
        let path = grab_path(grab_key).ok_or(LiveError::Invalid)?;
        if !self.live_write_allowed("stop recording") {
            return Err(LiveError::Refused);
        }
        let outcome = self.json_value(Method::Delete, &path);
        log_write("stop recording", &outcome);
        match ok_status(outcome) {
            Err(LiveError::Status(404)) => Ok(()),
            other => other,
        }
    }

    /// Tune a channel to watch it live. On success the server holds a tuner for this client until
    /// [`Client::live_release`] (or until it notices nobody is watching). A tune answered with
    /// conflicts holds nothing playable but may still carry a subscription id, which the caller
    /// releases too. `session` is the one the stream and the keepalive will name.
    pub fn live_tune(&self, dvr_key: &str, channel: &str, session: &str) -> LiveResult<TuneResult> {
        let path = tune_path(dvr_key, channel, session).ok_or(LiveError::Invalid)?;
        if !self.live_write_allowed("tune") {
            return Err(LiveError::Refused);
        }
        let outcome = self.json_value(Method::Post, &path);
        log_write("tune", &outcome);
        let tuned = parse::tune(&ok_body(outcome)?);
        match &tuned {
            Ok(t) => debug::parsed(&format!(
                "tune parsed: subscription {}, item key {:?}, ratingKey {}, playable {}, {} conflict(s), {} program(s), capture window {}, item {}x{} {}/{}, streams {:?}, captions {:?}",
                if t.subscription_id.is_empty() { "missing" } else { "present" },
                debug::path_shape(&t.item.key),
                if t.item.rating_key.is_empty() { "missing" } else { "present" },
                t.playable(),
                t.conflicts.len(),
                t.programs.len(),
                if t.capture.is_some() { "present" } else { "missing" },
                t.item.width,
                t.item.height,
                t.item.video_codec,
                t.item.audio_codec,
                t.item.streams,
                t.item.captions.iter().map(|c| (c.id > 0, c.codec.as_str(), c.language_code.as_str())).collect::<Vec<_>>()
            )),
            Err(e) => debug::parsed(&format!("tune answer unreadable: {}", e.reason())),
        }
        tuned
    }

    /// Re-read a live tune's subscription: the program list moves on as programs end.
    pub fn live_refresh(&self, subscription_id: &str) -> LiveResult<TuneResult> {
        let path = subscription_path(subscription_id).ok_or(LiveError::Invalid)?;
        parse::tune(&ok_body(self.json_value(Method::Get, &path))?)
    }

    /// Release a live tune this process made: delete its subscription so the tuner is free. Not
    /// gated (it undoes a permitted write) and idempotent (a 404 is done). `Err(Transport)` is
    /// the one answer a caller retries.
    pub fn live_release(&self, subscription_id: &str) -> LiveResult<()> {
        let path = subscription_path(subscription_id).ok_or(LiveError::Invalid)?;
        let outcome = self.json_value(Method::Delete, &path);
        log_write("release tune", &outcome);
        match ok_status(outcome) {
            Err(LiveError::Status(404)) => Ok(()),
            other => other,
        }
    }
}

/// A live tune turned into what the native player opens: the universal transcoder's `start.m3u8`
/// for the tuned key, encoded to the H.264/AAC HLS the player's segmented demuxer reads, with the
/// codecs and raster the decision promised (the Starfish Load is declared from these).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LiveStream {
    /// The playlist URL, token included — never logged.
    pub url: String,
    /// The transcoder session (`session=`), which a stop names.
    pub encoder_session: String,
    pub video_codec: String,
    pub audio_codec: String,
    pub width: i64,
    pub height: i64,
    /// The encode's frame rate as the decision states it (0 when it does not): what the Load
    /// declares, so a 59.94 broadcast is not presented on a 30 fps lattice.
    pub fps: f64,
    /// The caption stream burned into the encode (`TunedItem::captions`), 0 for none.
    pub subtitle_stream_id: i64,
}

/// The bound a live encode is asked for: 1080p at a rate a LAN carries comfortably. A tuner's
/// MPEG-2 or interlaced H.264 is re-encoded either way (the HLS profile copies nothing), so this
/// caps the server's work as much as the link's.
pub const LIVE_CEILING: super::Ceiling = super::Ceiling { max_kbps: 12_000, max_w: 1920, max_h: 1080 };

/// The transcode request for a live key: the same fixed-HLS contract the library's HLS rungs use.
/// `subtitle_stream_id` names a caption stream of the tuned part to burn in (0 for none): the
/// profile advertises no subtitle delivery, so the server's one way to show it is in the picture.
/// A tune is planned by no playback attempt, so its profile is the device's probed capabilities
/// (`devcaps::caps`), which `caps` carries.
pub(super) fn live_spec<'a>(
    key: &'a str,
    session: &'a str,
    caps: &'a plx_platform::devcaps::Caps,
    subtitle_stream_id: i64,
    offset: super::TranscodeOffset,
) -> super::TranscodeSpec<'a> {
    super::TranscodeSpec {
        rating_key: "",
        path_override: Some(key),
        media_index: 0,
        caps,
        session,
        encoder_session: session,
        contract: super::EncodeContract {
            remux: false,
            no_video_copy: true,
            ceiling: Some(LIVE_CEILING),
            delivery: super::TranscodeDelivery::FixedHls { seconds_per_segment: 2 },
            audio: super::AudioEnhancements::NONE,
        },
        audio_stream_id: 0,
        subtitle_stream_id: subtitle_stream_id.max(0),
        client_subtitles: false,
        offset,
    }
}

impl Client {
    /// Register a transcode of the tuned `key` (`TunedItem::key`) and answer the playlist the player
    /// opens. `session` is a fresh opaque id that names the encode from now until its stop.
    /// A refusal is `Server` with PMS's own sentence; nothing is left running (the decision of a
    /// refused encode registers nothing that needs a stop, and a transport failure is stopped by
    /// the caller with [`Client::live_stream_stop`] to be sure).
    pub fn live_stream(&self, off: &plx_base::task::OffFrame, key: &str, session: &str, subtitle_stream_id: i64) -> LiveResult<LiveStream> {
        if !key.starts_with('/') || session.is_empty() {
            return Err(LiveError::Invalid);
        }
        let caps = plx_platform::devcaps::caps();
        let spec = live_spec(key, session, &caps, subtitle_stream_id, super::TranscodeOffset::Fresh);
        let started = std::time::Instant::now();
        let decision = self.transcode_decision(off, &spec);
        let Some(decision) = decision else {
            debug::parsed(&format!("live decision: no answer after {} ms", started.elapsed().as_millis()));
            return Err(LiveError::Transport);
        };
        debug::parsed(&format!(
            "live decision in {} ms (captions {}): general {:?} {:?}, transcode {:?} {:?}, {} metadata, streams {:?}",
            started.elapsed().as_millis(),
            if subtitle_stream_id > 0 { "asked" } else { "none" },
            decision.general_decision_code,
            decision.general_decision_text,
            decision.transcode_decision_code,
            decision.transcode_decision_text,
            decision.metadata.len(),
            decision
                .metadata
                .iter()
                .flat_map(|m| m.media.iter())
                .flat_map(|m| m.part.iter())
                .flat_map(|p| p.stream.iter())
                .map(|s| format!("{}:{}@{}", s.stream_type, s.codec, s.frame_rate))
                .collect::<Vec<_>>()
        ));
        if let Some(code) = decision.general_decision_code {
            if !(1000..2000).contains(&code) {
                let text = if decision.transcode_decision_text.is_empty() { decision.general_decision_text.clone() } else { decision.transcode_decision_text.clone() };
                return Err(LiveError::Server { status: code, message: text });
            }
        }
        let media = decision.metadata.first().and_then(|m| m.media.first());
        let streams: Vec<&super::models::Stream> = media.map(|m| m.part.iter().flat_map(|p| p.stream.iter()).collect()).unwrap_or_default();
        let codec = |kind: i64, fallback: &str| {
            streams.iter().find(|s| s.stream_type == kind).map(|s| s.codec.clone()).filter(|c| !c.is_empty())
                .or_else(|| media.map(|m| if kind == 1 { m.video_codec.clone() } else { m.audio_codec.clone() }).filter(|c| !c.is_empty()))
                .unwrap_or_else(|| fallback.to_string())
        };
        Ok(LiveStream {
            url: self.transcode_start_url(&spec).to_url(),
            encoder_session: session.to_string(),
            subtitle_stream_id: subtitle_stream_id.max(0),
            video_codec: codec(1, "h264"),
            audio_codec: codec(2, "aac"),
            width: media.map_or(0, |m| m.width),
            height: media.map_or(0, |m| m.height),
            fps: streams.iter().find(|s| s.stream_type == 1).map(|s| s.frame_rate).filter(|f| f.is_finite() && *f > 0.0).unwrap_or(0.0),
        })
    }

    /// **The live stream again, from another moment of the tuner's capture** (time-shift): the
    /// same transcode `session` (the server keeps its capture buffer for it), started `offset_s`
    /// seconds after the capture window's origin ([`CaptureWindow::time_stamp`]), or at the live
    /// edge with `None`. No request goes out: `start.m3u8` makes its own decision when the player
    /// opens it, as the community clients rely on (`docs/pms-api.md` §10.6).
    pub fn live_stream_url(&self, key: &str, session: &str, subtitle_stream_id: i64, offset_s: Option<f64>) -> Option<String> {
        if !key.starts_with('/') || session.is_empty() {
            return None;
        }
        let offset = match offset_s {
            Some(s) if s.is_finite() && s >= 0.0 => super::TranscodeOffset::from_micros((s * 1_000_000.0) as i64),
            _ => super::TranscodeOffset::Fresh,
        };
        let caps = plx_platform::devcaps::caps();
        Some(self.transcode_start_url(&live_spec(key, session, &caps, subtitle_stream_id, offset)).to_url())
    }

    /// Stop a live encode (and free its streaming resource). `true` when the server took it.
    pub fn live_stream_stop(&self, session: &str) -> bool {
        self.transcode_stop(session)
    }

    /// One `/:/timeline` report for a live session: the tuned item's key and ratingKey with
    /// `time=0&duration=0`, so the server keeps the tune alive and records no resume point
    /// (`docs/pms-api.md` §10). `state` is `playing`, `paused` or `stopped`.
    pub fn live_timeline(&self, key: &str, rating_key: &str, state: &str, session: &str) -> bool {
        self.live_timeline_window(key, rating_key, state, session).0
    }

    /// [`Client::live_timeline`], and the tuner's capture window its answer carries
    /// (`CaptureBuffer > TranscodeSession`, else a lone top-level `TranscodeSession`): what can
    /// be played again of the channel, which moves on as the tuner records.
    pub fn live_timeline_window(&self, key: &str, rating_key: &str, state: &str, session: &str) -> (bool, Option<CaptureWindow>) {
        if key.is_empty() || session.is_empty() {
            return (false, None);
        }
        let q = QueryBuilder::new("/:/timeline")
            .opt_str("ratingKey", rating_key)
            .str("key", key)
            .str("state", state)
            .int("time", 0)
            .int("duration", 0)
            .str("X-Plex-Session-Identifier", session);
        let q = self.playback_identity(q);
        match self.body_2xx_post(&q.build()) {
            None => (false, None),
            Some(body) => {
                let answer = serde_json::from_slice::<Value>(&body).ok();
                let window = answer.as_ref().and_then(parse::timeline_capture).or_else(|| self.transcode_capture(session, answer.as_ref()));
                (true, window)
            }
        }
    }

    /// The capture window of transcode `session` from the server's list of transcodes, for a
    /// timeline answer (`timeline`) that carried none. Where a server keeps it is not yet
    /// measured: the first time for a session the event log says where it was found, or, when it
    /// was not, the shapes of both answers ([`debug::note_capture_source`]).
    fn transcode_capture(&self, session: &str, timeline: Option<&Value>) -> Option<CaptureWindow> {
        let sessions = match self.json_value(Method::Get, "/transcode/sessions") {
            ValueOutcome::Response { status, body } if (200..300).contains(&status) => body,
            _ => None,
        };
        let window = sessions.as_ref().and_then(|v| parse::transcode_capture(v, session));
        debug::note_capture_source(session, window.is_some(), timeline, sessions.as_ref());
        window
    }
}

/// One line per write that went out: the operation and its status, never a path, id or token.
fn log_write(op: &str, outcome: &ValueOutcome) {
    let what = match outcome {
        ValueOutcome::Response { status, .. } => format!("HTTP {status}"),
        ValueOutcome::Transport => "no answer".to_string(),
    };
    plx_base::eventlog::log(&format!("livetv: {op} -> {what}"));
}
