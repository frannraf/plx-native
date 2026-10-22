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
use plx_plex::plex::ServerId;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

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
    pub info: LiveInfo,
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
}

impl std::fmt::Debug for LiveLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // ids only: no URL, no token
        f.debug_struct("LiveLease").field("subscription", &self.subscription_id).field("released", &self.released).finish()
    }
}

/// Release workers still running (waited for at shutdown).
static RELEASING: AtomicUsize = AtomicUsize::new(0);

impl LiveLease {
    fn release(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        self.keepalive_stop.store(true, Ordering::Release);
        // Whatever drops a lease (a stop, a reset, a plan assigned over the session), the next
        // stream the demuxer opens is not this channel's: never a library stream read as live.
        crate::ff::set_live_stream(false);
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
    };
    super::decision::reset_for_live(ps);
    if !super::decision::set_stream_declaration(
        ps,
        &route.video_codec,
        &route.audio_codec,
        0.0,
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
    crate::ff::set_live_stream(true);
    start_keepalive(route.sid, route.key, route.rating_key, route.encoder_session, stop);
    true
}

fn start_keepalive(sid: ServerId, key: String, rk: String, enc: String, stop: Arc<AtomicBool>) {
    let ok = plx_base::task::spawn_small("livetv-keepalive", move || {
        let Some(client) = plx_plex::plex::client_for(sid) else { return };
        // the first report at once: the session is playing from now on
        let mut since = KEEPALIVE_S;
        while !stop.load(Ordering::Acquire) {
            if since >= KEEPALIVE_S {
                since = 0;
                let _ = client.live_timeline(&key, &rk, "playing", &enc);
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

/// Is the session playing a live channel?
pub fn is_live(ps: &PlaybackSession) -> bool {
    super::decision::live_info(ps).is_some()
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
}
