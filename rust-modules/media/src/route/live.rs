//! **Live TV playback's share of the route**: the tune lease a live playback holds, and the one
//! release every way out of it goes through. `docs/pms-api.md` §10 is the protocol.
//!
//! A live playback is a stream with no library item behind it: the tuned channel's transcode
//! (`plx_plex::plex::live_tv::LiveStream`), installed with [`install_live`] the way the pipeline
//! tier installs a `plxnative-playurl` stream (a URL and its Load declaration, `cur_rk` empty), so
//! none of the library playback's writes can run for it: no PlayQueue, no `/:/timeline` progress
//! from the VOD reporter, no resume point, no track write-back, no Up Next.
//!
//! **What a live playback owes the server, and when.** The tune took a tuner (a rolling
//! subscription) and the transcode an encoder. While it plays, a keepalive reports the tuned key
//! with `time=0&duration=0` every [`KEEPALIVE_S`] seconds, so PMS does not reclaim an unreported
//! grab. When it ends, ONE release runs, on a worker: a `stopped` report, the transcode stop, then
//! `DELETE /media/subscriptions/{id}` (retried while nothing answers; a 404 is done).
//!
//! **Exactly once, whatever ends it.** The lease is an owned value in [`PlaybackSession`] and its
//! `Drop` IS the release: a real stop (`engine::teardown` → [`release_live`]), a reset of the
//! session, a new plan installed over it (`apply_plan` assigns the whole session), an app-switch
//! park (the application stops live playback rather than suspending it), and the process's own
//! shutdown ([`drain_live_releases`] waits for the workers) all drop it once. What it does not
//! cover is a crash or a kill: the server reclaims an unreported grab on its own (Rivulet's report
//! of ~300 s; unmeasured here).

use super::decision::PlaybackSession;
use plx_plex::plex::live_tv::CaptureWindow;
use plx_plex::plex::ServerId;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// Seconds between keepalive reports: the cadence clients use for live sessions.
pub const KEEPALIVE_S: u64 = 10;

/// What the player shows about a live playback. Published to screens (it holds no resource).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LiveInfo {
    pub title: String,
    pub show_title: String,
    pub channel: String,
    /// The program on air when tuned, epoch seconds (0 when unknown).
    pub begins_at: i64,
    pub ends_at: i64,
    /// One of Plex's own channels (a provider's stream): nothing the server's DVR could record.
    pub provider: bool,
}

/// Everything [`install_live`] needs: what was tuned, and the stream it was turned into.
#[derive(Clone, Debug, Default)]
pub struct LiveRoute {
    pub sid: ServerId,
    /// The rolling subscription the tune created (empty when the server answered without one).
    pub subscription_id: String,
    /// The tuned item's key (`/livetv/sessions/{uuid}`) and ratingKey, which the keepalive names.
    pub key: String,
    pub rating_key: String,
    /// The transcoder session the stream is encoded under.
    pub encoder_session: String,
    /// The playlist the player opens (token included; never logged).
    pub url: String,
    pub video_codec: String,
    pub audio_codec: String,
    pub width: i64,
    pub height: i64,
    /// The encode's frame rate (0 = unknown: the Load's default envelope).
    pub fps: f64,
    /// The caption stream the server burns into the encode (0 for none): what the Subtitles
    /// menu shows as chosen.
    pub subtitle_stream_id: i64,
    pub info: LiveInfo,
    /// A cloud provider's stream (Plex's own channels): no server session to keep alive or
    /// release, and its HLS is read in the provider's shape (`ff::PROVIDER_STREAM`).
    pub provider: bool,
    /// The tuner's capture window the tune answered (pause and rewind play from it).
    pub capture: Option<CaptureWindow>,
}

/// The tuner and encoder a live playback holds. Dropping it releases both, once.
pub struct LiveLease {
    sid: ServerId,
    subscription_id: String,
    key: String,
    rating_key: String,
    encoder_session: String,
    keepalive_stop: Arc<AtomicBool>,
    released: bool,
    /// A provider's stream: nothing on the server to release.
    provider: bool,
    /// The caption stream burned into the encode: a time-shift asks the same.
    subtitle_stream_id: i64,
}

impl std::fmt::Debug for LiveLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // ids only: no URL, no token
        f.debug_struct("LiveLease").field("subscription", &self.subscription_id).field("released", &self.released).finish()
    }
}

/// Release workers still running (waited for at shutdown).
static RELEASING: AtomicUsize = AtomicUsize::new(0);

/// Has the viewer asked, from the player's record disc, to record the program on now on the
/// channel being watched? Cleared with the tune (a new channel, a release).
/// The end (epoch seconds) of the program the record disc was pressed for; 0 when it was not.
/// Per program, not per tune: the next program on the same channel can be recorded in its turn.
static RECORD_ASKED: AtomicI64 = AtomicI64::new(0);

/// The record disc was pressed (the application has queued the recording).
pub fn set_record_asked(until: i64) {
    RECORD_ASKED.store(until, Ordering::Relaxed);
}

/// Did the viewer ask to record what is on now (the record disc's ring)?
pub fn record_asked() -> bool {
    let until = RECORD_ASKED.load(Ordering::Relaxed);
    until > 0 && std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64) < until
}

impl LiveLease {
    fn release(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        self.keepalive_stop.store(true, Ordering::Release);
        forget_shift(&self.encoder_session);
        // Whatever drops a lease (a stop, a reset, a plan assigned over the session), the next
        // stream the demuxer opens is not this channel's: never a library stream read as live.
        crate::ff::set_live_stream(false);
        crate::ff::set_provider_stream(false);
        RECORD_ASKED.store(0, Ordering::Relaxed);
        if self.provider {
            crate::player::log("livetv: a provider's channel ends (nothing to release)");
            return;
        }
        let (sid, sub, key, rk, enc) = (
            self.sid,
            std::mem::take(&mut self.subscription_id),
            std::mem::take(&mut self.key),
            std::mem::take(&mut self.rating_key),
            std::mem::take(&mut self.encoder_session),
        );
        crate::player::log("livetv: releasing the tune");
        RELEASING.fetch_add(1, Ordering::AcqRel);
        plx_base::task::spawn_small_or_inline(
            "livetv-release",
            const { &plx_base::task::BlockingLabel::new("live tune release (worker thread refused)") },
            move || {
                release_now(sid, &sub, &key, &rk, &enc);
                RELEASING.fetch_sub(1, Ordering::AcqRel);
            },
        );
    }
}

impl Drop for LiveLease {
    fn drop(&mut self) {
        self.release();
    }
}

// ---- pause and rewind (time-shift) -------------------------------------------------------------

/// How far behind the capture's edge a live stream plays at its edge (the encode's segments and
/// the player's buffer): the **live position**, where the shift clock puts a playback started at
/// the live edge. An estimate; every "behind" is measured from this same point, so it cancels out.
const EDGE_LAG_S: f64 = 8.0;

/// A target this close to the live position (or past it) plays the live edge itself, and a
/// picture this close to it reads LIVE. Under the shortest Skip interval (5 s), so one press back
/// from LIVE always lands behind it, and one on from there always comes back.
pub const LIVE_SNAP_S: f64 = 3.0;

/// The least capture a channel needs before it offers rewinding: room for one skip back from the
/// live position past the snap (a tune of a few seconds has nothing to rewind into). The capture
/// grows one second a second from the tune (measured 2026-10-26), so this is a few seconds in.
const SHIFT_MIN_DEPTH_S: f64 = EDGE_LAG_S + LIVE_SNAP_S + 1.0;

/// **Where the picture of the live channel playing is**, as a moment of the channel in the
/// server's capture time (epoch seconds, the clock its `timeStamp` counts): `base` at wall time
/// `at`, advancing with the wall clock while playing and standing still while paused. Reset at
/// every tune ([`install_live`], the live edge) and every shift ([`note_live_shift`], its target).
/// A clock, not the demuxer's timestamps: a live encode's presentation times say nothing about the
/// broadcast's time. Never the TV's own clock either: a server whose clock differs would read as
/// behind (or ahead of) its own edge.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ShiftClock {
    pub base: f64,
    pub at: f64,
    pub paused_total: f64,
    pub paused_since: Option<f64>,
    /// Started at the live edge (no shift since, or a shift back to it).
    pub live: bool,
    /// `base` is in the capture's time: a clock started at the edge before any capture window was
    /// reported counts from the TV's clock until one is ([`ShiftClock::anchor`]).
    pub anchored: bool,
}

impl ShiftClock {
    /// Started at the live edge at wall time `now`, the capture's edge then at `edge` (`None`: no
    /// window reported yet, the TV's clock stands in for it until one is).
    pub fn at_edge(now: f64, edge: Option<f64>) -> Self {
        let base = edge.unwrap_or(now) - EDGE_LAG_S;
        ShiftClock { base, at: now, paused_total: 0.0, paused_since: None, live: true, anchored: edge.is_some() }
    }
    pub fn at(target: f64, now: f64) -> Self {
        ShiftClock { base: target, at: now, paused_total: 0.0, paused_since: None, live: false, anchored: true }
    }
    /// The capture's edge is `edge` at wall time `now`: a clock started at the edge before it was
    /// known counts from it now. It started at the live position of its own `at`, and the edge
    /// moves on with the clock, so pauses since stay counted.
    pub fn anchor(&mut self, edge: f64, now: f64) {
        if !self.anchored {
            self.base = edge - (now - self.at).max(0.0) - EDGE_LAG_S;
            self.anchored = true;
        }
    }
    /// Playing at the live edge, never paused since: the picture is the live position itself.
    pub fn at_live(&self) -> bool {
        self.live && self.paused_total <= 0.0 && self.paused_since.is_none()
    }
    /// The moment of the channel on screen at wall time `now`.
    pub fn position(&self, now: f64) -> f64 {
        let paused_now = self.paused_since.map_or(0.0, |p| (now - p).max(0.0));
        self.base + (now - self.at).max(0.0) - self.paused_total - paused_now
    }
    pub fn pause(&mut self, now: f64) {
        if self.paused_since.is_none() {
            self.paused_since = Some(now);
        }
    }
    pub fn resume(&mut self, now: f64) {
        if let Some(p) = self.paused_since.take() {
            self.paused_total += (now - p).max(0.0);
        }
    }
}

/// The capture window as seen at wall time `seen`, at wall time `now`: `(earliest, edge)` in epoch
/// seconds. The tuner keeps recording, so the edge moves on with the clock between reports.
pub fn capture_span(w: &CaptureWindow, seen: f64, now: f64) -> (f64, f64) {
    (w.time_stamp + w.min_offset, w.live_edge() + (now - seen).max(0.0))
}

/// **Where a skip of `delta` seconds from `position` lands** within the capture (its `earliest`
/// moment, and `live`: the live position, its edge less [`EDGE_LAG_S`]): a moment to play from, or
/// `None` for the live edge (a target within [`LIVE_SNAP_S`] of the live position, or past it).
pub fn shift_target(position: f64, delta: f64, earliest: f64, live: f64) -> Option<f64> {
    let t = (position + delta).max(earliest + 1.0);
    (t < live - LIVE_SNAP_S).then_some(t)
}

/// The live playback's time-shift: its transcode session, the capture window last reported for it
/// and the wall time that was seen, and the clock of its picture. One lock, so a window and the
/// clock it anchors never belong to two playbacks.
#[derive(Clone, Debug)]
struct Shift {
    session: String,
    window: Option<(CaptureWindow, f64)>,
    clock: ShiftClock,
}

/// The time-shift of the live playback: installed with its tune, gone with its lease. A late
/// keepalive of a channel left finds another session here (or none) and writes nothing.
static SHIFT: Mutex<Option<Shift>> = Mutex::new(None);

fn wall_s() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64())
}

fn shift_lock() -> std::sync::MutexGuard<'static, Option<Shift>> {
    SHIFT.lock().unwrap_or_else(|e| e.into_inner())
}

/// A keepalive's report of the capture window, for the playback of `session` only.
fn note_window(session: &str, w: CaptureWindow) {
    let now = wall_s();
    if let Some(s) = shift_lock().as_mut().filter(|s| s.session == session) {
        if s.window.is_none() {
            crate::player::log(&format!(
                "livetv: capture window: {:.0} s deep, its edge {:+.0} s from this TV's clock",
                w.depth(),
                w.live_edge() - now
            ));
        }
        s.window = Some((w, now));
        s.clock.anchor(capture_span(&w, now, now).1, now);
    }
}

/// A lease released: its time-shift goes with it (a later tune's stays).
fn forget_shift(session: &str) {
    let mut g = shift_lock();
    if g.as_ref().is_some_and(|s| s.session == session) {
        *g = None;
    }
}

/// The live playback's position now: `(position, behind, earliest, live)` in seconds of the
/// capture's time (`live`: the live position, where a playback at the edge is; `behind`: how far the
/// picture is from it). `None` when no live playback has a capture window.
pub fn live_shift_state() -> Option<(f64, f64, f64, f64)> {
    let now = wall_s();
    let s = shift_lock().clone()?;
    let (w, seen) = s.window?;
    let (earliest, edge) = capture_span(&w, seen, now);
    let live = edge - EDGE_LAG_S;
    // at the edge, never paused: the live position, however the capture's reports step
    let position = if s.clock.at_live() { live } else { s.clock.position(now).min(live) };
    Some((position, live - position, earliest, live))
}

/// Can the live channel playing be paused and rewound: a DVR channel whose tuner reported a capture
/// window of some depth, with its lease (its tuner) still held?
pub fn live_can_shift(ps: &PlaybackSession) -> bool {
    let Some(lease) = super::decision::live_lease_ref(ps) else { return false };
    if lease.provider {
        return false;
    }
    // the capture as it stands now: the tuner has recorded on since the window was last reported
    let now = wall_s();
    shift_lock().as_ref().filter(|s| s.session == lease.encoder_session).and_then(|s| s.window).is_some_and(|(w, seen)| {
        let (earliest, edge) = capture_span(&w, seen, now);
        edge - earliest >= SHIFT_MIN_DEPTH_S
    })
}

/// The stream of the live channel playing from `target` (epoch seconds; `None`: the live edge), on
/// the same transcode session: `None` when it cannot be shifted.
pub fn live_shift_url(ps: &PlaybackSession, target: Option<f64>) -> Option<String> {
    let lease = super::decision::live_lease_ref(ps)?;
    if lease.provider {
        return None;
    }
    let client = plx_plex::plex::client_for(lease.sid)?;
    let offset = match target {
        Some(t) => {
            let (w, _) = shift_lock().as_ref().filter(|s| s.session == lease.encoder_session)?.window?;
            // never before the capture's start; the edge moved on since the window was reported,
            // and a target is kept short of it by `shift_target`
            Some((t - w.time_stamp).max(w.min_offset))
        }
        None => None,
    };
    client.live_stream_url(&lease.key, &lease.encoder_session, lease.subtitle_stream_id, offset)
}

/// The picture now plays from `target` (`None`: the live edge): the shift clock starts there.
pub fn note_live_shift(ps: &PlaybackSession, target: Option<f64>) {
    let Some(lease) = super::decision::live_lease_ref(ps) else { return };
    let now = wall_s();
    if let Some(s) = shift_lock().as_mut().filter(|s| s.session == lease.encoder_session) {
        s.clock = match target {
            Some(t) => ShiftClock::at(t, now),
            None => ShiftClock::at_edge(now, s.window.map(|(w, seen)| capture_span(&w, seen, now).1)),
        };
    }
}

/// The clock of the live playback's picture (a shift that did not start puts it back).
pub fn live_shift_clock() -> Option<ShiftClock> {
    shift_lock().as_ref().map(|s| s.clock)
}

/// Put back a clock [`live_shift_clock`] read, on the same playback.
pub fn restore_live_shift_clock(ps: &PlaybackSession, clock: ShiftClock) {
    let Some(lease) = super::decision::live_lease_ref(ps) else { return };
    if let Some(s) = shift_lock().as_mut().filter(|s| s.session == lease.encoder_session) {
        s.clock = clock;
    }
}

/// The transport paused or played on: a live playback's clock stands still while paused.
pub fn note_live_paused(paused: bool) {
    let now = wall_s();
    if let Some(s) = shift_lock().as_mut() {
        if paused {
            s.clock.pause(now);
        } else {
            s.clock.resume(now);
        }
    }
}

/// The release itself (on a worker): stopped report, transcode stop, subscription delete. The
/// delete is retried while nothing answers, three times a second apart.
fn release_now(sid: ServerId, sub: &str, key: &str, rk: &str, enc: &str) {
    let Some(client) = plx_plex::plex::client_for(sid) else {
        crate::player::log("livetv: release skipped (the server is gone)");
        return;
    };
    let _ = client.live_timeline(key, rk, "stopped", enc);
    let stopped = client.live_stream_stop(enc);
    let mut deleted = sub.is_empty();
    for attempt in 0..3 {
        if deleted {
            break;
        }
        match client.live_release(sub) {
            Ok(()) => deleted = true,
            Err(plx_plex::plex::live_tv::LiveError::Transport) if attempt < 2 => {
                std::thread::sleep(std::time::Duration::from_secs(1))
            }
            Err(e) => {
                crate::player::log(&format!("livetv: release delete failed ({})", e.reason()));
                break;
            }
        }
    }
    crate::player::log(&format!("livetv: released (transcode stop {}, subscription {})",
        if stopped { "ok" } else { "unanswered" }, if deleted { "deleted" } else { "NOT deleted" }));
}

/// Install a tuned channel as the session's route and start its keepalive. The session's earlier
/// lease (a live playback replaced by another) is released by the assignment. From this call on
/// the tune is the route's: `false` (the declaration was refused, nothing installed) has already
/// released it.
pub fn install_live(ps: &mut PlaybackSession, route: LiveRoute) -> bool {
    RECORD_ASKED.store(0, Ordering::Relaxed);
    let stop = Arc::new(AtomicBool::new(false));
    // Made first, so every way out of this function below releases the tune it holds.
    let lease = LiveLease {
        sid: route.sid,
        subscription_id: route.subscription_id.clone(),
        key: route.key.clone(),
        rating_key: route.rating_key.clone(),
        encoder_session: route.encoder_session.clone(),
        keepalive_stop: Arc::clone(&stop),
        released: false,
        provider: route.provider,
        subtitle_stream_id: route.subtitle_stream_id.max(0),
    };
    super::decision::reset_for_live(ps);
    if !super::decision::set_stream_declaration(
        ps,
        &route.video_codec,
        &route.audio_codec,
        route.fps,
        plx_data::metadata::Dovi::NONE,
        false,
    ) {
        crate::player::log("livetv: the stream's declaration was refused");
        drop(lease);
        return false;
    }
    if route.width > 0 && route.height > 0 {
        super::decision::set_stream_source_raster(ps, route.width.min(i64::from(u16::MAX)) as u16, route.height.min(i64::from(u16::MAX)) as u16);
    }
    super::decision::install_live_lease(ps, lease, route.info.clone(), &route.url);
    // the caption stream burned in is the subtitle "on": the menu checks its row
    super::decision::set_subtitle(ps, route.subtitle_stream_id.max(0));
    crate::ff::set_live_stream(true);
    crate::ff::set_provider_stream(route.provider);
    // pause and rewind: the tuner's capture as the tune answered it, the picture at its edge (a
    // Plex channel has none: nothing of an earlier playback stays)
    *shift_lock() = (!route.provider).then(|| {
        let now = wall_s();
        let window = route.capture.map(|w| (w, now));
        let edge = window.map(|(w, seen)| capture_span(&w, seen, now).1);
        Shift { session: route.encoder_session.clone(), window, clock: ShiftClock::at_edge(now, edge) }
    });
    crate::player::log(&format!(
        "livetv: installed live stream {}/{} {}x{} @{} fps, subscription {}, captions {}, capture {}, keepalive every {}s",
        route.video_codec,
        route.audio_codec,
        route.width,
        route.height,
        route.fps,
        if route.subscription_id.is_empty() { "none" } else { "held" },
        if route.subtitle_stream_id > 0 { "burned in" } else { "off" },
        route.capture.map_or_else(|| "not in the tune's answer".to_string(), |w| format!("{:.0} s deep", w.depth())),
        KEEPALIVE_S
    ));
    if !route.provider {
        start_keepalive(route.sid, route.key, route.rating_key, route.encoder_session, stop);
    }
    true
}

fn start_keepalive(sid: ServerId, key: String, rk: String, enc: String, stop: Arc<AtomicBool>) {
    let ok = plx_base::task::spawn_small("livetv-keepalive", move || {
        let Some(client) = plx_plex::plex::client_for(sid) else { return };
        // the first report at once: the session is playing from now on
        let mut since = KEEPALIVE_S;
        let (mut sent, mut failed) = (0u32, 0u32);
        while !stop.load(Ordering::Acquire) {
            if since >= KEEPALIVE_S {
                since = 0;
                let (ok, window) = client.live_timeline_window(&key, &rk, "playing", &enc);
                // not once the lease is released (the answer of a report sent before it)
                if let Some(w) = window.filter(|_| !stop.load(Ordering::Acquire)) {
                    note_window(&enc, w);
                }
                sent += 1;
                // The first answer, every failure, and a tally once in a while: a server that
                // stops taking the report is a tune about to be reclaimed.
                if !ok {
                    failed += 1;
                    crate::player::log(&format!("livetv: keepalive #{sent} not taken ({failed} failed so far)"));
                } else if sent == 1 || sent % 30 == 0 {
                    crate::player::log(&format!("livetv: keepalive #{sent} taken ({failed} failed so far)"));
                }
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
            since += 1;
        }
    });
    if !ok {
        crate::player::log("livetv: keepalive thread refused (the server may reclaim the tune)");
    }
}

/// Release the session's live lease now, if it holds one (a real stop). Idempotent.
pub fn release_live(ps: &mut PlaybackSession) {
    drop(super::decision::take_live_lease(ps));
}

/// Give back the tuner a DVR channel holds while its picture plays on from what is buffered: the
/// live player's CH▲/CH▼, whose next tune may need it (seen on the TV: "every tuner is busy" with
/// the channel being left still held). The playback stays a live one until the next channel
/// replaces it. A provider's channel holds no tuner and keeps its lease: `false`.
pub fn release_live_tuner(ps: &mut PlaybackSession) -> bool {
    if super::decision::live_info(ps).is_none_or(|i| i.provider) {
        return false;
    }
    let lease = super::decision::take_live_lease_only(ps);
    let held = lease.is_some();
    drop(lease);
    held
}

/// The program on the live channel moved on: the HUD's title and the bar's span follow it.
/// `true` when something changed.
pub fn roll_live_program(ps: &mut PlaybackSession, title: &str, show_title: &str, begins_at: i64, ends_at: i64) -> bool {
    super::decision::roll_live_program(ps, title.to_owned(), show_title.to_owned(), begins_at, ends_at)
}

/// The HUD's channel line says `line` for a while (a CH▲/CH▼'s "Tuning…"); `None` puts the
/// channel's own words back.
pub fn set_live_context(ps: &mut PlaybackSession, line: Option<&str>) {
    super::decision::set_live_context(ps, line)
}

/// Does the session still hold its live lease (a DVR channel's tuner, or a Plex channel's
/// stream)? `false` once a CH▲/CH▼ handed a DVR channel's tuner back.
pub fn live_lease_held(ps: &PlaybackSession) -> bool {
    super::decision::live_lease_held(ps)
}

/// Is a released tune still being handed back (the next tune on that DVR waits for it)?
pub fn live_releasing() -> bool {
    RELEASING.load(Ordering::Acquire) > 0
}

/// Is the session playing a live channel?
pub fn is_live(ps: &PlaybackSession) -> bool {
    super::decision::live_info(ps).is_some()
}

/// Is it a channel of the server's DVR (one its record disc can record)? Not one of Plex's own.
pub fn is_recordable_live(ps: &PlaybackSession) -> bool {
    super::decision::live_info(ps).is_some_and(|i| !i.provider)
}

/// The span of the program on air when the channel was tuned, `(begins, ends)` in epoch seconds:
/// what the player's bar fills by. `None` off a live channel or when the guide gave no span.
pub fn live_airing(ps: &PlaybackSession) -> Option<(i64, i64)> {
    super::decision::live_info(ps).filter(|i| i.begins_at > 0 && i.ends_at > i.begins_at).map(|i| (i.begins_at, i.ends_at))
}

/// At process exit: wait (bounded) for release workers already running, so a shutdown right after
/// leaving a channel still frees the tuner.
pub fn drain_live_releases() {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while RELEASING.load(Ordering::Acquire) > 0 && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lease_releases_once_however_often_it_is_dropped_or_released() {
        let stop = Arc::new(AtomicBool::new(false));
        let mut lease = LiveLease {
            sid: ServerId::UNSET,
            subscription_id: "77".into(),
            key: "/livetv/sessions/x".into(),
            rating_key: String::new(),
            encoder_session: "enc".into(),
            keepalive_stop: Arc::clone(&stop),
            released: false,
            provider: false,
            subtitle_stream_id: 0,
        };
        lease.release();
        assert!(lease.released && stop.load(Ordering::Acquire), "the keepalive is told to stop");
        assert!(lease.subscription_id.is_empty(), "the ids moved into the one release");
        lease.release();
        drop(lease);
        drain_live_releases();
        assert_eq!(RELEASING.load(Ordering::Acquire), 0);
    }

    #[test]
    fn installing_live_declares_the_stream_and_a_reset_releases_it() {
        let mut ps = PlaybackSession::IDLE;
        let route = LiveRoute {
            sid: ServerId::UNSET,
            subscription_id: "77".into(),
            key: "/livetv/sessions/x".into(),
            encoder_session: "enc".into(),
            url: "http://127.0.0.1:9/video/:/transcode/universal/start.m3u8?x=1".into(),
            video_codec: "h264".into(),
            audio_codec: "aac".into(),
            width: 1280,
            height: 720,
            info: LiveInfo { title: "Program".into(), channel: "Channel".into(), ..LiveInfo::default() },
            ..LiveRoute::default()
        };
        assert!(install_live(&mut ps, route));
        assert!(is_live(&ps));
        assert!(super::super::decision::has_url(&ps));
        assert_eq!(super::super::decision::live_info(&ps).unwrap().title, "Program");
        assert!(super::super::decision::transcode_session(&ps).is_empty(), "no VOD transcode identity: the lease stops it");
        let published = ps.publication();
        assert!(is_live(&published), "screens see that it is live");
        assert!(super::super::decision::take_live_lease(&mut ps.publication()).is_none(), "the screen copy holds no lease");
        release_live(&mut ps);
        assert!(super::super::decision::take_live_lease(&mut ps).is_none());
        drain_live_releases();
    }

    /// A CH▲/CH▼ hands a DVR channel's tuner back with the picture still a live one; a Plex channel
    /// holds no tuner and keeps its lease.
    #[test]
    fn a_channel_step_hands_the_tuner_back_and_the_playback_stays_live() {
        let mut ps = PlaybackSession::IDLE;
        let dvr = LiveRoute { subscription_id: "77".into(), url: "http://127.0.0.1:9/x.m3u8".into(), ..LiveRoute::default() };
        assert!(install_live(&mut ps, dvr));
        assert!(release_live_tuner(&mut ps), "the DVR's tuner goes back");
        assert!(is_live(&ps), "the picture is still a live channel's until the next replaces it");
        assert!(super::super::decision::take_live_lease_only(&mut ps).is_none(), "no lease left");
        assert!(!release_live_tuner(&mut ps), "nothing more to hand back");
        release_live(&mut ps);
        drain_live_releases();

        let plex = LiveRoute {
            url: "https://127.0.0.1:9/x.m3u8".into(),
            provider: true,
            info: LiveInfo { provider: true, ..LiveInfo::default() },
            ..LiveRoute::default()
        };
        assert!(install_live(&mut ps, plex));
        assert!(!release_live_tuner(&mut ps), "a Plex channel holds no tuner");
        release_live(&mut ps);
        drain_live_releases();
    }

    /// **Pause and rewind's clock**: the picture advances with the wall clock while playing and
    /// stands while paused; a skip lands inside the capture, never before its start, and one near
    /// the live edge (or past it) plays the edge itself.
    #[test]
    fn the_shift_clock_stands_while_paused_and_a_skip_lands_inside_the_capture() {
        let mut c = ShiftClock::at(1_000.0, 50.0);
        assert_eq!(c.position(60.0), 1_010.0);
        c.pause(60.0);
        assert_eq!(c.position(90.0), 1_010.0, "paused: still");
        c.resume(90.0);
        assert_eq!(c.position(100.0), 1_020.0, "played on from where it paused");
        let mut edge = ShiftClock::at_edge(500.0, Some(2_000.0));
        assert!(edge.at_live() && edge.position(500.0) < 2_000.0, "a playback at the edge is a little behind the capture's edge");
        edge.pause(510.0);
        edge.resume(512.0);
        assert!(edge.live && !edge.at_live(), "paused once: behind the edge since");
        let (earliest, live) = (100.0, 2_000.0);
        assert_eq!(shift_target(1_000.0, -30.0, earliest, live), Some(970.0));
        assert_eq!(shift_target(120.0, -300.0, earliest, live), Some(101.0), "never before the capture's start");
        assert_eq!(shift_target(1_999.0, -2.0, earliest, live), None, "near the live position: the edge itself");
        assert_eq!(shift_target(1_980.0, -10.0, earliest, live), Some(1_970.0), "thirty seconds behind is behind");
        assert_eq!(shift_target(1_900.0, 600.0, earliest, live), None, "past it: the edge");
        let w = CaptureWindow { time_stamp: 1_000.0, min_offset: 10.0, max_offset: 600.0 };
        assert_eq!(capture_span(&w, 50.0, 60.0), (1_010.0, 1_610.0), "the edge moves on with the clock");
    }

    /// **One reference for LIVE** (review 2026-10-26): from the live position, the shortest skip
    /// back lands behind it and reads so, and one skip on from there comes back to the edge, whatever
    /// the server's clock says against the TV's.
    #[test]
    fn a_skip_back_from_live_lands_behind_it_and_a_skip_on_comes_back() {
        for (server_ahead, step) in [(0.0, 5.0), (0.0, 10.0), (40.0, 10.0), (-25.0, 15.0)] {
            let tv_now = 10_000.0;
            let capture_edge = tv_now + server_ahead - 3.0; // the capture trails a little
            let clock = ShiftClock::at_edge(tv_now, Some(capture_edge));
            let (earliest, live) = (capture_edge - 600.0, capture_edge - EDGE_LAG_S);
            let at = clock.position(tv_now);
            assert!((live - at).abs() < 1e-9, "at the edge: behind 0 (server {server_ahead:+})");
            let back = shift_target(at, -step, earliest, live).expect("a skip back from LIVE lands");
            assert!(live - back >= LIVE_SNAP_S, "and reads as behind ({step} s)");
            assert_eq!(shift_target(back, step, earliest, live), None, "a skip on comes back to the edge");
        }
    }

    /// A clock started at the edge before the capture was known counts from it once it is, its
    /// pauses kept.
    #[test]
    fn a_clock_started_before_the_window_anchors_to_the_capture_when_it_comes() {
        let mut c = ShiftClock::at_edge(100.0, None);
        c.pause(102.0);
        c.resume(106.0);
        c.anchor(5_000.0, 110.0);
        assert!(c.anchored);
        // the edge was 5_000 - 10 at the start: the live position then, ten seconds on, less the pause
        assert_eq!(c.position(110.0), 5_000.0 - 10.0 - EDGE_LAG_S + 10.0 - 4.0);
        let before = c;
        c.anchor(9_999.0, 120.0);
        assert_eq!(c, before, "anchored once");
    }

    /// The HUD names the show and its episode, and a daily show whose episode title is its own name
    /// once (seen on the TV: "Show · Show").
    #[test]
    fn the_hud_title_names_a_show_once_when_its_episode_is_named_the_same() {
        let shown = |show: &str, title: &str| {
            let mut ps = PlaybackSession::IDLE;
            let info = LiveInfo { title: title.into(), show_title: show.into(), ..LiveInfo::default() };
            assert!(install_live(&mut ps, LiveRoute { url: "http://127.0.0.1:9/x.m3u8".into(), info, ..LiveRoute::default() }));
            // SAFETY: the session's own NUL-terminated title buffer, read while `ps` lives.
            let text = unsafe { std::ffi::CStr::from_ptr(super::super::decision::title_cptr(&ps)) }.to_string_lossy().into_owned();
            release_live(&mut ps);
            drain_live_releases();
            text
        };
        assert_eq!(shown("Morning Show", "Morning Show"), "Morning Show");
        assert_eq!(shown("A Series", "An Episode"), "A Series \u{b7} An Episode");
        assert_eq!(shown("", "A Movie"), "A Movie");
        assert_eq!(shown("A Show", ""), "A Show");
    }
}
