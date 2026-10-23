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
    /// The channel playing (full screen or in the guide's corner), as its tab and row: the player's
    /// record disc records what is on it, OK on it brings the corner's picture back, CH▲/CH▼ step
    /// from it. Set when its tune lands and plays, not when it is asked.
    watching: Option<(plx_data::live_tv::Source, usize)>,
    /// The tune asked of the store and not yet landed.
    asked: Option<Asked>,
    /// A tune waiting for a tuner to come back first: a DVR has none to spare (seen on the TV:
    /// "every tuner is busy" while the channel left was still being released).
    pending: Option<(Asked, Option<plx_plex::plex::live_tv::Airing>)>,
    /// The store generation the player's program was last read at ([`Self::live_tv_program_now`]).
    rolled_at: Option<u32>,
    /// The wall-clock minute the guide and the recordings lists were last kept fresh at.
    freshened: Option<i64>,
}

/// A Watch asked for: its tab and row, and whether the live player's CH▲/CH▼ asked it (its landing
/// plays over the player, where a guide press's plays only over the guide).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Asked {
    pub source: plx_data::live_tv::Source,
    pub row: usize,
    pub step: bool,
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
        // While the guide is in use (its page up, or a channel playing from it) the guide and the
        // recordings lists are kept fresh: stale channel-days are read again, the lists every five
        // minutes. Idle on Home, nothing is asked.
        let minute = plx_data::live_tv::wall_now() / 60;
        let in_use = d.nav.tabs.stack.entries.iter().any(|e| e.arg == AppArg::LiveTv) || self.live_tv.watching.is_some();
        if in_use && self.live_tv.freshened != Some(minute) {
            self.live_tv.freshened = Some(minute);
            self.stores.live_tv.refresh_stale();
            if minute % 5 == 0 {
                self.stores.live_tv.refresh_dvr_quietly();
            }
        }
        let Some((instance, page)) = Self::live_tv_page(d) else {
            if self.live_tv.instance.take().is_some() {
                self.live_tv.generation = None;
                self.live_tv.want = None;
            }
            // a Watch press whose page is gone is nobody's
            self.live_tv_cancel_tune();
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
            LiveTvOut::Watch { row, airing } => self.live_tv_ask(row, airing, false),
            LiveTvOut::Source(source) => store.select_source(source),
            LiveTvOut::CancelTune => self.live_tv_cancel_tune(),
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
        // Not while a CH▲/CH▼ is under way: the picture is the channel being left, and the row it
        // would record is the one being tuned.
        if plx_media::route::record_asked() || self.live_tv_step_busy() {
            return;
        }
        let Some((source, row)) = self.live_tv.watching.clone() else { return };
        if &source != self.stores.live_tv.source() {
            plx_base::eventlog::log("livetv: record from the player: the guide shows another tab");
            return;
        }
        let Some(airing) = self.stores.live_tv.on_now(row) else {
            plx_base::eventlog::log("livetv: record from the player: nothing on now on that row");
            return;
        };
        plx_base::eventlog::log(&format!("livetv: record from the player, row {row}"));
        let ends_at = airing.ends_at;
        self.stores.live_tv.write(Write::Record { airing, kind: RecordKind::This });
        plx_media::route::set_record_asked(ends_at);
    }

    /// Ask the store for a Watch of row `row`: now, or, on a DVR whose tuners are still coming
    /// back (a channel just left, a superseded tune), once they are ([`Self::live_tv_pump`]). One of
    /// Plex's channels takes no tuner and goes at once.
    fn live_tv_ask(&mut self, row: usize, airing: Option<plx_plex::plex::live_tv::Airing>, step: bool) {
        let store = &mut self.stores.live_tv;
        let asked = Asked { source: store.source().clone(), row, step };
        if !asked.source.is_cloud() && (plx_media::route::live_releasing() || store.tuners_settling()) {
            plx_base::eventlog::log(&format!("livetv: row {row} waits for a tuner to come back"));
            store.cancel_tune();
            // …shown as tuning meanwhile, so the press is answered and BACK can take it back
            if let Some(words) = store.channel_words(row) {
                store.show_tuning(words);
            }
            self.live_tv.asked = None;
            self.live_tv.pending = Some((asked, airing));
            return;
        }
        self.live_tv.pending = None;
        if store.tune(row, airing) {
            self.live_tv.asked = Some(asked);
        } else {
            self.live_tv.asked = None;
            plx_base::eventlog::log("livetv: nothing to tune on that row");
        }
    }

    /// Once a frame: a Watch waiting on a tuner goes when none is still coming back, while the
    /// screen that asked for it is up (a CH▲/CH▼'s the player, a guide press's the guide).
    pub(crate) fn live_tv_pump(&mut self, in_player: bool, in_guide: bool) {
        let Some((asked, _)) = &self.live_tv.pending else { return };
        // …while the screen that asked is up and the guide still shows the tab it asked under (a
        // row of another tab is another channel)
        if (asked.step && !in_player) || (!asked.step && !in_guide) || &asked.source != self.stores.live_tv.source() {
            self.live_tv.pending = None;
            self.stores.live_tv.cancel_tune();
            return;
        }
        if plx_media::route::live_releasing() || self.stores.live_tv.tuners_settling() {
            return;
        }
        let Some((asked, airing)) = self.live_tv.pending.take() else { return };
        self.live_tv_ask(asked.row, airing, asked.step);
    }

    /// CH▲/CH▼ in the live player: the row `step` on from the channel playing, or from the one a
    /// step already under way is going to, wrapping round the lineup. Only within the lineup the
    /// guide shows (a channel watched from another tab has no neighbours here).
    pub(crate) fn live_tv_step_target(&self, step: i32) -> Option<usize> {
        let store = &self.stores.live_tv;
        let from = match (&self.live_tv.pending, &self.live_tv.asked, &self.live_tv.watching) {
            (Some((a, _)), _, _) if a.step => (a.source.clone(), a.row),
            (_, Some(a), _) if a.step => (a.source.clone(), a.row),
            (_, _, Some(w)) => w.clone(),
            _ => return None,
        };
        let n = store.channel_count();
        // a step of 0 is the same channel tuned again (closed captions on or off)
        if n == 0 || (step != 0 && n < 2) || &from.0 != store.source() {
            return None;
        }
        let next = (from.1 as i64 + i64::from(step)).rem_euclid(n as i64) as usize;
        plx_base::eventlog::log(&format!("livetv: channel step {step:+} from row {} to row {next}", from.1));
        Some(next)
    }

    /// Tune row `row` for a CH▲/CH▼, through the guide's own Watch flow.
    pub(crate) fn live_tv_step_to(&mut self, row: usize) {
        let airing = self.stores.live_tv.on_now(row);
        self.live_tv_ask(row, airing, true);
    }

    /// The program on now on the channel being watched, when the guide moved since it was last
    /// asked (a landing, the minute): the player's lines and bar follow it. `None` while nothing
    /// moved, while a CH▲/CH▼ is under way, or when the guide shows another tab.
    pub(crate) fn live_tv_program_now(&mut self) -> Option<plx_plex::plex::live_tv::Airing> {
        let generation = self.stores.live_tv.generation();
        if self.live_tv.rolled_at == Some(generation) || self.live_tv_step_busy() {
            return None;
        }
        self.live_tv.rolled_at = Some(generation);
        let (source, row) = self.live_tv.watching.as_ref()?;
        if source != self.stores.live_tv.source() {
            return None;
        }
        self.stores.live_tv.on_now(*row)
    }

    /// How the channel row `row` of the tab shown is named ("4.1 KXYZ"), for the player's lines.
    pub(crate) fn live_tv_channel_words(&self, row: usize) -> Option<String> {
        self.stores.live_tv.channel_words(row)
    }

    /// Are the broadcast's closed captions wanted on the channels tuned?
    pub(crate) fn live_tv_captions_wanted(&self) -> bool {
        self.stores.live_tv.captions_wanted()
    }

    /// Closed captions on or off for the channels tuned from now on (the player's Subtitles menu).
    pub(crate) fn live_tv_set_captions(&mut self, on: bool) {
        self.stores.live_tv.set_captions(on);
    }

    /// Is a CH▲/CH▼ under way (waiting on a tuner, or tuning)?
    pub(crate) fn live_tv_step_busy(&self) -> bool {
        self.live_tv.pending.as_ref().is_some_and(|(a, _)| a.step) || self.live_tv.asked.as_ref().is_some_and(|a| a.step)
    }

    /// The player was left (STOP, BACK, the background): a CH▲/CH▼ under way is dropped, its tune
    /// released if it lands.
    pub(crate) fn live_tv_abandon_step(&mut self) {
        if self.live_tv_step_busy() {
            plx_base::eventlog::log("livetv: a channel step is dropped (the player was left)");
            self.live_tv.pending = None;
            self.live_tv.asked = None;
            self.stores.live_tv.cancel_tune();
        }
    }

    /// The Watch the landed tune answers (taken: one landing per ask).
    pub(crate) fn live_tv_take_asked(&mut self) -> Option<Asked> {
        self.live_tv.asked.take()
    }

    /// The landed tune plays: it is the channel watched now.
    pub(crate) fn live_tv_now_watching(&mut self, asked: &Asked) {
        self.live_tv.watching = Some((asked.source.clone(), asked.row));
    }

    /// No channel plays any more: nothing is watched from the guide (which is kept fresh only while
    /// its page is up or a channel plays from it).
    pub(crate) fn live_tv_forget_watching(&mut self) {
        self.live_tv.watching = None;
    }

    /// The Watch asked failed (the store says so): forget it. `Some(step)` once per failure.
    pub(crate) fn live_tv_take_failure(&mut self) -> Option<bool> {
        if self.live_tv.asked.is_some() && self.stores.live_tv.tune_failed() {
            return self.live_tv.asked.take().map(|a| a.step);
        }
        None
    }

    /// A failed step's notice is the player's to act on, not the guide's to show later.
    pub(crate) fn live_tv_clear_tune_failure(&mut self) {
        self.stores.live_tv.clear_tune_failure();
    }

    /// Is `row` of the tab shown the channel last asked to be watched (the corner's, while one
    /// plays there)?
    pub(crate) fn live_tv_is_watching(&self, row: usize) -> bool {
        self.live_tv.watching.as_ref().is_some_and(|(source, r)| *r == row && source == self.stores.live_tv.source())
    }

    /// Forget a Watch press still on its way (the app is going to the background).
    pub(crate) fn live_tv_cancel_tune(&mut self) {
        self.live_tv.pending = None;
        self.live_tv.asked = None;
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
    // The row's own words ("4.1 KXYZ"; a Plex channel's title alone), else what the tuned item says.
    let channel = if !t.channel.is_empty() { t.channel.clone() } else {
        let station = plx_plex::plex::live_tv::station_id(&t.item.channel_title);
        format!("{} {station}", t.item.channel_vcn).trim().to_string()
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
        subtitle_stream_id: t.stream.subtitle_stream_id,
        info: plx_media::route::LiveInfo { title, show_title, channel, begins_at, ends_at, provider: t.cloud },
        provider: t.cloud,
    }
}

/// What the player's track menus read of a live channel: no audio to choose (the encode carries
/// the broadcast's), and the broadcast's caption streams as subtitles — picking one has the server
/// burn it in (`playback::live_captions_commit`). No library item: `rk` is empty.
pub(crate) fn live_playing_item(t: &Tuned) -> plx_data::metadata::PlayingItem {
    plx_data::metadata::PlayingItem {
        sid: t.sid,
        rk: String::new(),
        show_rk: String::new(),
        kind: "live".into(),
        audio: Vec::new(),
        subs: t
            .item
            .captions
            .iter()
            .filter(|c| c.id > 0)
            .map(|c| {
                // A tuner's closed captions come with no language (seen on the set: CEA-608 on a
                // US broadcast): they are the broadcast's, English on this continent's CC1, and
                // a row that says so beats "Unknown" under Other languages.
                let (lang, code) = if c.language.is_empty() && c.language_code.is_empty() {
                    ("English".to_string(), "eng".to_string())
                } else {
                    (c.language.clone(), c.language_code.clone())
                };
                plx_data::metadata::Stream {
                    id: c.id,
                    lang,
                    lang_code: code,
                    codec: c.codec.clone(),
                    title: c.title.clone(),
                    ..Default::default()
                }
            })
            .collect(),
        video_fps: t.stream.fps,
        width: t.stream.width,
        height: t.stream.height,
        bitrate: 0,
        dovi: Default::default(),
        markers: Vec::new(),
        chapters: Vec::new(),
        hdr: false,
    }
}
