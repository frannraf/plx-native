//! **Live TV, application side** (`docs/pms-api.md` §10): which server's DVR the store reads, the
//! strip pill's availability, what feeds the page from the store, what the page's requests do to
//! it, and the start of a live playback when a tune lands.
//!
//! A child of `bridge`, like `watchlist`, because the feed reads the rig's private fields.
//!
//! * **Aim.** Once a frame the store is pointed at the current server; a server that answered
//!   "no Live TV" is remembered and the next registered one is asked, so the pill appears for the
//!   household's DVR whichever server is browsed. A roster change (a profile switch, a sign-out)
//!   resets it.
//! * **Pump and feed.** The store lands its answers ahead of the page's tick; the page is handed a
//!   [`LiveTvView`] when the store's generation or the window the page wants moved. While a menu
//!   veils the page nothing is sent (the Watchlist's rule).
//! * **Requests.** `content::live_tv_requests` drains the page's [`LiveTvOut`]s: the strip and BACK
//!   navigate, the rest go to the store through [`Bridge::live_tv_request`].
//! * **Watch.** The store tunes off the frame; a landed stream is taken once
//!   ([`Bridge::live_tv_take_tuned`]) and installed into the media route, whose lease releases the
//!   tuner when the playback ends however it ends.
use plx_data::live_tv::{Availability, RecordKind, Tuned};
use plx_data::stores::live_tv::Write;
use plx_screens::live_tv::model::{LiveTvView, RowAirings, Want};
use plx_screens::live_tv::page::{LiveTvOut, LiveTvScreen};
use plx_screens::registry::AppArg;

use super::*;

/// What the feed last handed the page, and the servers already found without Live TV.
#[derive(Default)]
pub(crate) struct Feed {
    instance: Option<InstanceId>,
    generation: Option<u32>,
    want: Option<Want>,
    tried: Vec<plx_plex::plex::ServerId>,
    /// When a failed providers read is asked again (the app's logical clock, ms).
    retry_at: Option<u32>,
    /// The channel row last asked to be watched: the player's record disc records what is on it.
    watching: Option<usize>,
}

/// How long after a failed providers read it is asked again: a set that booted before its network
/// came up still finds its DVR, and a server that is down is not hammered.
const PROVIDERS_RETRY_MS: u32 = 30_000;

impl Bridge {
    /// Is the Live TV pill on the strip (the store found a DVR)?
    pub(crate) fn live_tv_on_strip(&self) -> bool {
        matches!(self.stores.live_tv.availability(), Availability::Available)
    }

    fn live_tv_page(d: &mut Dispatcher<AppHost>) -> Option<(InstanceId, &mut LiveTvScreen)> {
        let id = d.nav.tabs.stack.entries.iter().find(|e| e.arg == AppArg::LiveTv)?.id;
        let inst = d.nav.entry_mut(id)?.inst.as_mut()?;
        let instance = inst.id;
        let screen = inst.screen.as_any_mut()?.downcast_mut::<LiveTvScreen>()?;
        Some((instance, screen))
    }

    /// Point the store at a server: the current one, else (it has no Live TV) the next registered
    /// one not yet found without it. A server that left the roster (a profile switch, a sign-out:
    /// slot numbers are never reused) starts the search again.
    fn live_tv_aim(&mut self) {
        let store = &mut self.stores.live_tv;
        if store.providers_failed() {
            let now = crate::app::clock::now();
            match self.live_tv.retry_at {
                None => self.live_tv.retry_at = Some(now.wrapping_add(PROVIDERS_RETRY_MS)),
                // the logical clock wraps: due once `now` passes `at`
                Some(at) if now.wrapping_sub(at) as i32 >= 0 => {
                    self.live_tv.retry_at = None;
                    if let Some(sid) = store.server() {
                        store.open(sid);
                    }
                }
                Some(_) => {}
            }
        } else {
            self.live_tv.retry_at = None;
        }
        match store.server() {
            Some(sid) if plx_plex::plex::client_for(sid).is_none() => {
                store.reset();
                self.live_tv.tried.clear();
            }
            Some(sid) if *store.availability() == Availability::None => {
                if !self.live_tv.tried.contains(&sid) {
                    self.live_tv.tried.push(sid);
                }
                let tried = &self.live_tv.tried;
                if let Some(next) = plx_plex::plex::server_ids().find(|id| !tried.contains(id)) {
                    store.open(next);
                }
            }
            Some(_) => {}
            None => {
                let current = plx_plex::plex::current_server();
                if current != plx_plex::plex::ServerId::UNSET && plx_plex::plex::client_for(current).is_some() {
                    store.open(current);
                }
            }
        }
    }

    /// Once a frame, route-unconditional: aim and pump the store, then hand the mounted page what
    /// moved and ask the store for the window the page shows.
    pub(crate) fn live_tv_feed(&mut self, d: &mut Dispatcher<AppHost>) {
        self.live_tv_aim();
        self.stores.live_tv.pump();
        let Some((instance, page)) = Self::live_tv_page(d) else {
            if self.live_tv.instance.take().is_some() {
                self.live_tv.generation = None;
                self.live_tv.want = None;
            }
            // a Watch press whose page is gone is nobody's
            self.stores.live_tv.cancel_tune();
            return;
        };
        if self.live_tv.instance != Some(instance) {
            self.live_tv.instance = Some(instance);
            self.live_tv.generation = None;
            self.live_tv.want = None;
            self.stores.live_tv.refresh_dvr();
        }
        if let Some(art) = page.hero_art() {
            self.stores.live_tv.want_blur(&art);
        }
        let want = page.want();
        if want != self.live_tv.want {
            if let Some(w) = &want {
                self.stores.live_tv.want_window(w.rows.clone(), w.from, w.to);
            }
            self.live_tv.want = want.clone();
            self.live_tv.generation = None;
        }
        if plx_ui::containers::modal::page_veiled() {
            return;
        }
        let generation = self.stores.live_tv.generation();
        if self.live_tv.generation == Some(generation) {
            return;
        }
        self.live_tv.generation = Some(generation);
        page.input(view_of(&self.stores.live_tv, want.as_ref()));
    }

    /// The page's requests, drained by `content::live_tv_requests`.
    pub(crate) fn take_live_tv_reqs(&mut self) -> Vec<(MachineId, LiveTvOut, ReturnState<u32, PageMemory>)> {
        std::mem::take(&mut self.live_tv_reqs)
    }

    /// A page request that is the store's (the strip, BACK and the chip are navigation).
    pub(crate) fn live_tv_request(&mut self, out: LiveTvOut) {
        let store = &mut self.stores.live_tv;
        match out {
            LiveTvOut::Watch { row, airing } => {
                self.live_tv.watching = Some(row);
                if !store.tune(row, airing) {
                    plx_base::eventlog::log("livetv: nothing to tune on that row");
                }
            }
            LiveTvOut::CancelTune => store.cancel_tune(),
            LiveTvOut::DismissTuneFailure => store.clear_tune_failure(),
            LiveTvOut::Write(w) => store.write(w),
            LiveTvOut::Retry => store.retry_failed(),
            LiveTvOut::RefreshDvr => store.refresh_dvr(),
            LiveTvOut::Tab(_) | LiveTvOut::Account | LiveTvOut::Back => {}
        }
    }

    /// The live player's record disc: record the program on now on the channel being watched,
    /// once (the disc rings to say it was asked; the write's answer is the guide's notice).
    pub(crate) fn live_tv_record_playing(&mut self) {
        if plx_media::route::record_asked() {
            return;
        }
        let Some(row) = self.live_tv.watching else { return };
        let Some(airing) = self.stores.live_tv.on_now(row) else {
            plx_base::eventlog::log("livetv: record from the player: nothing on now on that row");
            return;
        };
        plx_base::eventlog::log(&format!("livetv: record from the player, row {row}"));
        self.stores.live_tv.write(Write::Record { airing, kind: RecordKind::This });
        plx_media::route::set_record_asked(true);
    }

    /// Forget a Watch press still on its way (the app is going to the background).
    pub(crate) fn live_tv_cancel_tune(&mut self) {
        self.stores.live_tv.cancel_tune();
    }

    /// At exit: release whatever tune the store still owns and wait (bounded) for its releases.
    pub(crate) fn live_tv_release_for_exit(&mut self) {
        self.stores.live_tv.release_for_exit();
        plx_data::stores::live_tv::drain_releases();
    }

    /// A tune that landed for the page, once: the caller installs it into the media route.
    pub(crate) fn live_tv_take_tuned(&mut self) -> Option<Tuned> {
        self.stores.live_tv.take_tuned()
    }

    /// Hand a stream back to the store to release (it could not be played).
    pub(crate) fn live_tv_drop_tuned(&mut self, t: Tuned) {
        self.stores.live_tv.release_tuned(t);
    }
}

/// The page's view: the store's snapshot, and the airings of the rows the page wants.
fn view_of(store: &plx_data::stores::live_tv::LiveTvStore, want: Option<&Want>) -> LiveTvView {
    let snap = store.snapshot();
    let n = snap.channels.len();
    let mut rows = vec![RowAirings { airings: Vec::new(), pending: true, failed: false }; n];
    if let Some(w) = want {
        for r in w.rows.clone().filter(|r| *r < n) {
            rows[r] = RowAirings {
                airings: store.airings(r, w.from, w.to),
                pending: store.row_pending(r, w.from, w.to),
                failed: store.row_failed(r, w.from, w.to),
            };
        }
    }
    LiveTvView { snap, rows }
}

/// The media route's description of a landed tune: the stream, the tuned item the keepalive
/// names, and the lines the player shows.
pub(crate) fn live_route(t: Tuned) -> plx_media::route::LiveRoute {
    let (title, show_title, begins_at, ends_at) = match &t.airing {
        Some(a) => (a.title.clone(), a.show_title.clone(), a.begins_at, a.ends_at),
        None => (t.item.title.clone(), t.item.show_title.clone(), t.item.begins_at, t.item.ends_at),
    };
    let channel = if t.item.channel_title.is_empty() { t.channel.clone() } else {
        format!("{} {}", t.item.channel_vcn, t.item.channel_title).trim().to_string()
    };
    plx_media::route::LiveRoute {
        sid: t.sid,
        subscription_id: t.subscription_id,
        key: t.item.key,
        rating_key: t.item.rating_key,
        encoder_session: t.stream.encoder_session,
        url: t.stream.url,
        video_codec: t.stream.video_codec,
        audio_codec: t.stream.audio_codec,
        width: t.stream.width,
        height: t.stream.height,
        fps: t.stream.fps,
        info: plx_media::route::LiveInfo { title, show_title, channel, begins_at, ends_at },
    }
}
