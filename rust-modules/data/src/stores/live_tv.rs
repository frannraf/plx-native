//! **The Live TV / DVR store**: which server's DVR the app reads, its providers and lineup, the
//! guide's day cache, the recording rules and operations, and the writes a page asks for.
//! The protocol is `plx_plex::plex::live_tv`; the snapshot and cache are `crate::live_tv`.
//!
//! Like `stores::notify`, it has no `StoreId` and no `StoreCmd`: the application opens it for the
//! current server, pumps it once a frame (`Bridge::live_tv_feed`), forwards a page's requests to it
//! and hands the page a [`Snapshot`] when [`LiveTvStore::generation`] moved. Nothing is recorded or
//! replayed through the landing gate: a guide is a picture of wall-clock time, not of a session.
//!
//! **Work runs off the frame** through a [`Runner`] (`spawn_off_frame` in production), at most
//! [`IN_FLIGHT`] reads at a time; the server's client is resolved at the spawn site and every
//! landing carries the store's epoch, so a server switch or a reset turns late answers into no-ops.
//! **Writes** go through the plex layer's gate (a debug build writes only to the mock unless the
//! owner armed `plxnative-livetv-live`) one at a time in press order, and each ends in a
//! [`WriteNotice`]. A successful record or cancel overlays the guide at once and re-reads the DVR
//! lists; it never waits for the grid to be read again.
//!
//! **The live tune is not here.** Watching a channel is playback, and the tuner it takes is owned
//! and released by the media route (`plx_media::route`), which outlives any page.

use crate::live_tv::{self as lt, Availability, GuideCache, Overlay, Phase, RecordKind, Snapshot, WriteNotice, WriteOp};
use plx_plex::plex::live_tv::{Airing, Channel, CreateSubscription, EpgProvider, Grab, LiveError, Subscription};
use plx_plex::plex::ServerId;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// Reads in flight at once (a guide window asks for a dozen channel-days).
pub const IN_FLIGHT: usize = 4;

/// A unit of server work.
#[derive(Clone, Debug, PartialEq)]
pub enum Job {
    Providers,
    Channels { provider: String },
    Grid { provider: String, ch: usize, channel: Channel, date: String },
    Dvr,
    Write(Write),
}

/// A write a page asked for.
#[derive(Clone, Debug, PartialEq)]
pub enum Write {
    Record { airing: Airing, kind: RecordKind },
    CancelRule { id: String },
    StopGrab { key: String },
    MoveRule { id: String, after: Option<String> },
}

/// A job's answer.
#[derive(Debug)]
pub enum Landing {
    Providers(Result<Vec<EpgProvider>, LiveError>),
    Channels(Result<Vec<Channel>, LiveError>),
    Grid { ch: usize, date: String, result: Result<Vec<Airing>, LiveError> },
    Dvr(Result<(Vec<Subscription>, Vec<Grab>), LiveError>),
    Write { write: Write, result: Result<WriteDone, LiveError> },
}

/// What a successful write did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct WriteDone {
    /// A record: the new rule's id.
    pub rule: String,
    /// A record that answered conflicts: what it collides with (titles).
    pub conflicts: Vec<String>,
}

/// Where a job's worker posts its landing; stamped with the store epoch it was issued under.
#[derive(Clone)]
pub struct Post {
    epoch: u64,
    inbox: Arc<Mutex<Vec<(u64, Landing)>>>,
}

impl Post {
    pub fn post(&self, landing: Landing) {
        self.inbox.lock().unwrap_or_else(|e| e.into_inner()).push((self.epoch, landing));
        plx_machine::idle::invalidate();
        plx_machine::present::wake_from_worker();
    }
}

/// Runs a job for a server. `false` = it could not start (the OS refused the thread).
pub trait Runner {
    fn run(&mut self, sid: ServerId, job: Job, post: Post) -> bool;
}

/// The production runner: one small worker per job, the server's client resolved at the spawn site.
#[cfg(not(any(test, feature = "test-support")))]
struct Spawn;

#[cfg(not(any(test, feature = "test-support")))]
impl Runner for Spawn {
    fn run(&mut self, sid: ServerId, job: Job, post: Post) -> bool {
        let Some(client) = plx_plex::plex::client_for(sid) else { return false };
        plx_base::task::spawn_off_frame("livetv", move |_off| post.post(execute(client, job)))
    }
}

/// Under test only an injected runner runs: a default-built store never reaches a server.
#[cfg(any(test, feature = "test-support"))]
struct Refuse;

#[cfg(any(test, feature = "test-support"))]
impl Runner for Refuse {
    fn run(&mut self, _sid: ServerId, _job: Job, _post: Post) -> bool {
        false
    }
}

fn default_runner() -> Box<dyn Runner> {
    #[cfg(not(any(test, feature = "test-support")))]
    return Box::new(Spawn);
    #[cfg(any(test, feature = "test-support"))]
    return Box::new(Refuse);
}

/// Do `job` against `client` (on a worker). Public so a test runner can execute jobs against a
/// loopback client too.
pub fn execute(client: &plx_plex::plex::Client, job: Job) -> Landing {
    match job {
        Job::Providers => Landing::Providers(client.live_tv_providers()),
        Job::Channels { provider } => Landing::Channels(client.live_tv_channels(&provider)),
        Job::Grid { provider, ch, channel, date } => {
            let result = client.live_tv_grid(&provider, &channel, &date);
            Landing::Grid { ch, date, result }
        }
        Job::Dvr => Landing::Dvr(client.dvr_subscriptions().and_then(|rules| client.dvr_scheduled().map(|s| (rules, s)))),
        Job::Write(write) => {
            let result = match &write {
                Write::Record { airing, kind } => record(client, airing, *kind),
                Write::CancelRule { id } => client.dvr_delete(id).map(|_| WriteDone::default()),
                Write::StopGrab { key } => client.dvr_cancel_grab(key).map(|_| WriteDone::default()),
                Write::MoveRule { id, after } => client.dvr_move(id, after.as_deref()).map(|_| WriteDone::default()),
            };
            Landing::Write { write, result }
        }
    }
}

/// A record: read the template for the airing's program, take the choice that matches `kind`
/// (episode/movie rule, or series rule), and create it with the template's own defaults.
fn record(client: &plx_plex::plex::Client, airing: &Airing, kind: RecordKind) -> Result<WriteDone, LiveError> {
    let choices = client.dvr_template(&airing.guid)?;
    let want = |t: &plx_plex::plex::live_tv::TemplateChoice| match kind {
        RecordKind::Series => t.kind == 2,
        RecordKind::This => t.kind == 1 || t.kind == 4,
    };
    let choice = choices.iter().find(|t| want(t)).ok_or(LiveError::Server { status: 0, message: "The server offers no such recording for this program.".into() })?;
    if choice.target_section.is_empty() {
        return Err(LiveError::Server { status: 0, message: "There is no library to record into.".into() });
    }
    let made = client.dvr_create(&CreateSubscription::from_choice(choice))?;
    Ok(WriteDone {
        rule: made.subscription_id,
        conflicts: made.conflicts.iter().map(|c| if c.airing.title.is_empty() { "another recording".to_string() } else { c.airing.title.clone() }).collect(),
    })
}

/// The store.
pub struct LiveTvStore {
    sid: Option<ServerId>,
    epoch: u64,
    inbox: Arc<Mutex<Vec<(u64, Landing)>>>,
    runner: Box<dyn Runner>,
    queue: VecDeque<Job>,
    in_flight: usize,
    write_queue: VecDeque<Write>,
    writing: bool,
    availability: Availability,
    providers_phase: Phase,
    provider: Option<EpgProvider>,
    channels_phase: Phase,
    channels: Vec<Channel>,
    guide: GuideCache,
    dvr_phase: Phase,
    rules: Vec<Subscription>,
    scheduled: Vec<Grab>,
    notice: Option<WriteNotice>,
    notice_serial: u32,
    now: i64,
    utc_offset: i64,
    generation: u32,
}

impl Default for LiveTvStore {
    fn default() -> Self {
        Self::with_runner(default_runner())
    }
}

impl LiveTvStore {
    pub fn with_runner(runner: Box<dyn Runner>) -> Self {
        Self {
            sid: None,
            epoch: 0,
            inbox: Arc::default(),
            runner,
            queue: VecDeque::new(),
            in_flight: 0,
            write_queue: VecDeque::new(),
            writing: false,
            availability: Availability::Unknown,
            providers_phase: Phase::Idle,
            provider: None,
            channels_phase: Phase::Idle,
            channels: Vec::new(),
            guide: GuideCache::default(),
            dvr_phase: Phase::Idle,
            rules: Vec::new(),
            scheduled: Vec::new(),
            notice: None,
            notice_serial: 0,
            now: 0,
            utc_offset: 0,
            generation: 0,
        }
    }

    fn bump(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }

    /// Moves whenever anything the snapshot holds moved.
    pub fn generation(&self) -> u32 {
        self.generation
    }

    pub fn server(&self) -> Option<ServerId> {
        self.sid
    }

    pub fn availability(&self) -> &Availability {
        &self.availability
    }

    /// Read `sid`'s Live TV. A different server (or none) resets everything; the same server
    /// re-asks only what failed.
    pub fn open(&mut self, sid: ServerId) {
        if self.sid != Some(sid) {
            self.reset();
            self.sid = Some(sid);
        }
        if matches!(self.providers_phase, Phase::Idle | Phase::Failed(_)) {
            self.providers_phase = Phase::Loading;
            self.queue.push_back(Job::Providers);
            self.bump();
        }
    }

    /// Forget everything (a profile or server switch, a sign-out). Late answers are dropped.
    pub fn reset(&mut self) {
        let keep = (self.now, self.utc_offset, self.generation, self.notice_serial);
        let runner = std::mem::replace(&mut self.runner, Box::new(NoRun));
        let epoch = self.epoch + 1;
        *self = Self::with_runner(runner);
        self.epoch = epoch;
        // the generation and notice serial never go back: a page compares them for "moved"
        (self.now, self.utc_offset, self.generation, self.notice_serial) = keep;
        self.bump();
    }

    /// The guide window a page shows: channel rows `rows` between `from` and `to` (epoch seconds).
    /// Asks for every channel-day it does not hold, nearest rows first.
    pub fn want_window(&mut self, rows: std::ops::Range<usize>, from: i64, to: i64) {
        let Some(provider) = self.provider.as_ref().map(|p| p.identifier.clone()) else { return };
        if !self.channels_phase.is_ready() || to <= from {
            return;
        }
        let rows = rows.start.min(self.channels.len())..rows.end.min(self.channels.len());
        let dates = lt::days_covering(from, to);
        let need = self.guide.want(rows, &dates);
        if need.is_empty() {
            return;
        }
        // The window the page shows NOW goes first: its days jump the queue, in row order. A day
        // still queued from an earlier window stays queued (it is cheap, and the page may scroll
        // back); a day only ever asks once, because `want` returns unasked days alone.
        for (ch, date) in need.into_iter().rev() {
            self.guide.set_loading(ch, &date);
            let channel = self.channels[ch].clone();
            self.queue.push_front(Job::Grid { provider: provider.clone(), ch, channel, date });
        }
        self.guide.evict();
        self.bump();
    }

    /// Ask again for every guide day that failed (the page's Retry).
    pub fn retry_failed(&mut self) {
        self.guide.reset_failed();
        if matches!(self.channels_phase, Phase::Failed(_)) {
            if let Some(p) = &self.provider {
                self.channels_phase = Phase::Loading;
                self.queue.push_front(Job::Channels { provider: p.identifier.clone() });
            }
        }
        if matches!(self.providers_phase, Phase::Failed(_)) {
            self.providers_phase = Phase::Loading;
            self.queue.push_front(Job::Providers);
        }
        self.bump();
    }

    /// The airings of channel row `ch` in `[from, to)`, from the cache.
    pub fn airings(&self, ch: usize, from: i64, to: i64) -> Vec<Airing> {
        self.guide.airings(ch, from, to)
    }

    /// Is channel row `ch` still loading for `[from, to)`?
    pub fn row_pending(&self, ch: usize, from: i64, to: i64) -> bool {
        self.guide.pending(ch, &lt::days_covering(from, to))
    }

    /// Did every day of channel row `ch` for `[from, to)` fail?
    pub fn row_failed(&self, ch: usize, from: i64, to: i64) -> bool {
        self.guide.failed(ch, &lt::days_covering(from, to))
    }

    /// Re-read the recording rules and operations.
    pub fn refresh_dvr(&mut self) {
        if self.sid.is_none() || self.queue.iter().any(|j| matches!(j, Job::Dvr)) {
            return;
        }
        if !self.dvr_phase.is_loading() {
            self.dvr_phase = Phase::Loading;
        }
        self.queue.push_back(Job::Dvr);
        self.bump();
    }

    /// Queue a write. Writes go one at a time, in press order.
    pub fn write(&mut self, w: Write) {
        if self.sid.is_none() {
            return;
        }
        self.write_queue.push_back(w);
        self.bump();
    }

    /// Once a frame: the clock, then every landing, then whatever can start. `true` = the
    /// snapshot moved.
    pub fn pump(&mut self) -> bool {
        let before = self.generation;
        let now = lt::wall_now();
        if lt::plausible_now(now) {
            let minute = now - now.rem_euclid(60);
            if minute != self.now {
                self.now = minute;
                self.utc_offset = plx_platform::i18n::utc_offset_s(minute);
                self.bump();
            }
        }
        let landed: Vec<(u64, Landing)> = std::mem::take(&mut *self.inbox.lock().unwrap_or_else(|e| e.into_inner()));
        for (epoch, landing) in landed {
            if epoch != self.epoch {
                continue;
            }
            self.land(landing);
        }
        self.start();
        self.generation != before
    }

    fn land(&mut self, landing: Landing) {
        match landing {
            Landing::Providers(r) => {
                self.in_flight = self.in_flight.saturating_sub(1);
                match r {
                    Ok(list) => {
                        self.providers_phase = Phase::Ready;
                        // the first provider that can tune; else the first with a guide
                        self.provider = list.iter().find(|p| p.tunable() && p.grid).or_else(|| list.iter().find(|p| p.grid)).cloned();
                        self.availability = if self.provider.is_some() { Availability::Available } else { Availability::None };
                        if let Some(p) = &self.provider {
                            self.channels_phase = Phase::Loading;
                            self.queue.push_front(Job::Channels { provider: p.identifier.clone() });
                            self.dvr_phase = Phase::Loading;
                            self.queue.push_back(Job::Dvr);
                        }
                    }
                    Err(LiveError::Status(403)) => {
                        self.providers_phase = Phase::Ready;
                        self.availability = Availability::None;
                    }
                    Err(e) => self.providers_phase = Phase::Failed(e.reason()),
                }
            }
            Landing::Channels(r) => {
                self.in_flight = self.in_flight.saturating_sub(1);
                match r {
                    Ok(ch) => {
                        self.channels = ch;
                        self.channels_phase = Phase::Ready;
                        self.guide.clear();
                    }
                    Err(e) => self.channels_phase = Phase::Failed(e.reason()),
                }
            }
            Landing::Grid { ch, date, result } => {
                self.in_flight = self.in_flight.saturating_sub(1);
                self.guide.land(ch, &date, result.map_err(|e| e.reason()));
            }
            Landing::Dvr(r) => {
                self.in_flight = self.in_flight.saturating_sub(1);
                match r {
                    Ok((rules, scheduled)) => {
                        self.rules = rules;
                        self.scheduled = scheduled;
                        self.dvr_phase = Phase::Ready;
                    }
                    Err(LiveError::Status(403)) => {
                        self.rules.clear();
                        self.scheduled.clear();
                        self.dvr_phase = Phase::Failed(LiveError::Status(403).reason());
                    }
                    Err(e) => self.dvr_phase = Phase::Failed(e.reason()),
                }
            }
            Landing::Write { write, result } => {
                self.writing = false;
                self.notice_serial = self.notice_serial.wrapping_add(1);
                let op = match &write {
                    Write::Record { kind: RecordKind::This, .. } => WriteOp::Record,
                    Write::Record { kind: RecordKind::Series, .. } => WriteOp::RecordSeries,
                    Write::CancelRule { .. } => WriteOp::CancelRule,
                    Write::StopGrab { .. } => WriteOp::StopRecording,
                    Write::MoveRule { .. } => WriteOp::MoveRule,
                };
                match result {
                    Ok(done) => {
                        match &write {
                            Write::Record { airing, kind: RecordKind::This } => {
                                self.guide.overlay(&Overlay::Episode { guid: airing.guid.clone(), rule: done.rule.clone() })
                            }
                            Write::Record { airing, kind: RecordKind::Series } => self.guide.overlay(&Overlay::Series {
                                show_guid: airing.grandparent_guid.clone(),
                                rule: done.rule.clone(),
                            }),
                            Write::CancelRule { id } => {
                                self.guide.overlay(&Overlay::Removed { rule: id.clone() });
                                self.rules.retain(|r| &r.id != id);
                            }
                            Write::StopGrab { .. } | Write::MoveRule { .. } => {}
                        }
                        self.notice = Some(WriteNotice { serial: self.notice_serial, ok: true, op, reason: String::new(), conflicts: done.conflicts });
                    }
                    Err(e) => {
                        self.notice = Some(WriteNotice { serial: self.notice_serial, ok: false, op, reason: e.reason(), conflicts: Vec::new() });
                    }
                }
                // what the server now records changed: read the lists again
                if !self.queue.iter().any(|j| matches!(j, Job::Dvr)) {
                    self.queue.push_back(Job::Dvr);
                }
            }
        }
        self.bump();
    }

    /// Start queued work up to the read budget, and the next write if none is running.
    fn start(&mut self) {
        let Some(sid) = self.sid else { return };
        if !self.writing {
            if let Some(w) = self.write_queue.pop_front() {
                let post = Post { epoch: self.epoch, inbox: Arc::clone(&self.inbox) };
                if self.runner.run(sid, Job::Write(w.clone()), post) {
                    self.writing = true;
                } else {
                    self.write_queue.push_front(w);
                }
                self.bump();
            }
        }
        while self.in_flight < IN_FLIGHT {
            let Some(job) = self.queue.pop_front() else { break };
            let post = Post { epoch: self.epoch, inbox: Arc::clone(&self.inbox) };
            if self.runner.run(sid, job.clone(), post) {
                self.in_flight += 1;
            } else {
                // the OS refused a thread: try again next frame
                self.queue.push_front(job);
                break;
            }
        }
    }

    /// The page's picture.
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            generation: self.generation,
            availability: self.availability.clone(),
            providers_phase: self.providers_phase.clone(),
            provider: self.provider.clone(),
            channels_phase: self.channels_phase.clone(),
            channels: self.channels.clone(),
            now: self.now,
            utc_offset: self.utc_offset,
            dvr_phase: self.dvr_phase.clone(),
            rules: self.rules.clone(),
            scheduled: self.scheduled.clone(),
            notice: self.notice.clone(),
            writes_in_flight: u32::from(self.writing) + self.write_queue.len() as u32,
        }
    }

    /// The guide cache, for a page that draws rows straight from it.
    pub fn guide(&self) -> &GuideCache {
        &self.guide
    }
}

/// The placeholder runner a reset holds for an instant.
struct NoRun;
impl Runner for NoRun {
    fn run(&mut self, _sid: ServerId, _job: Job, _post: Post) -> bool {
        false
    }
}

#[cfg(test)]
#[path = "live_tv_tests.rs"]
mod tests;
