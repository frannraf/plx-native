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
//! **The live tune is only begun here.** A Watch press tunes the channel and opens its encode off
//! the frame ([`LiveTvStore::tune`]); the application takes the [`Tuned`] stream
//! ([`LiveTvStore::take_tuned`]) and hands it to the media route, whose lease owns the tuner from
//! then on and outlives any page. Until it is taken the store owns it: a tune that lands after the
//! page stopped wanting it (cancelled, superseded, a reset or a server switch) is released here,
//! against the server that tuned it, and never played.

use crate::live_tv::{
    self as lt, Availability, CloudAuth, GuideCache, Overlay, Phase, RecordKind, Snapshot, Source, TunePhase, Tuned, WriteNotice, WriteOp,
};
use plx_plex::plex::live_tv::{Airing, Channel, CloudTab, CreateSubscription, EpgProvider, Grab, LiveError, Subscription};
use plx_plex::plex::ServerId;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// Reads in flight at once (a guide window asks for a dozen channel-days).
pub const IN_FLIGHT: usize = 4;

/// Guide art whose UltraBlur colours are remembered; the oldest is forgotten first and asked
/// again if its program is shown again.
pub const MAX_BLURS: usize = 48;

/// Release workers still running (waited for at exit, [`drain_releases`]).
static RELEASING: AtomicUsize = AtomicUsize::new(0);

/// At exit: wait, at most about three seconds, for release workers already running, so a quit
/// right after leaving a tune still frees the tuner. (A counted wait: no clock is read here.)
pub fn drain_releases() {
    for _ in 0..150 {
        if RELEASING.load(Ordering::Acquire) == 0 {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// A unit of server work.
#[derive(Clone, Debug, PartialEq)]
pub enum Job {
    Providers,
    Channels { provider: String },
    Grid { provider: String, ch: usize, channel: Channel, date: String },
    Dvr,
    Write(Write),
    /// Tune `channel` on DVR `dvr_key` and open its encode under `session`.
    Tune { serial: u32, dvr_key: String, channel: Channel, label: String, airing: Option<Airing>, session: String },
    /// Release a tune nobody will play: stop its encode, delete its subscription.
    Release { subscription_id: String, session: String },
    /// The UltraBlur colours of a piece of guide art (the page's tint).
    Blur { art: String },
    /// Plex's own channels: their genre tabs; a lineup (all of them, or one tab); one channel-day.
    CloudTabs { auth: CloudAuth },
    CloudChannels { source: Source, auth: CloudAuth },
    CloudGrid { source: Source, ch: usize, channel: Channel, date: String, auth: CloudAuth },
    /// The debug log's look at a Plex channel's stream (what the player would have to read).
    CloudProbe { channel: Channel, auth: CloudAuth },
    /// Watch a Plex channel: its stream's URL (and, in a debug build, its shape in the log).
    CloudTune { serial: u32, channel: Channel, label: String, airing: Option<Airing>, auth: CloudAuth },
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
    Tuned { serial: u32, result: Result<Tuned, LiveError> },
    Released,
    Blur { art: String, result: Result<[[f32; 3]; 4], LiveError> },
    CloudTabs(Result<Vec<CloudTab>, LiveError>),
    CloudChannels { source: Source, result: Result<Vec<Channel>, LiveError> },
    CloudGrid { source: Source, ch: usize, date: String, result: Result<Vec<Airing>, LiveError> },
    CloudProbe(String),
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
        let Some(client) = plx_plex::plex::client_for(sid) else {
            // A server that is gone cannot be asked; its release has nobody left to ask.
            if matches!(job, Job::Release { .. }) {
                return true;
            }
            return false;
        };
        let releasing = matches!(job, Job::Release { .. });
        if releasing {
            RELEASING.fetch_add(1, Ordering::AcqRel);
        }
        let ok = plx_base::task::spawn_off_frame("livetv", move |off| {
            post.post(execute(client, off, sid, job));
            if releasing {
                RELEASING.fetch_sub(1, Ordering::AcqRel);
            }
        });
        if !ok && releasing {
            RELEASING.fetch_sub(1, Ordering::AcqRel);
        }
        ok
    }
}

/// How long a channel-day's programs are trusted before they are read again while the guide is in
/// use: what a recording scheduled from another device, or a schedule change, takes to show.
pub const GUIDE_FRESH_S: i64 = 15 * 60;

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

/// Do `job` against `client` (server `sid`), on a worker. Public so a test runner can execute jobs
/// against a loopback client too.
pub fn execute(client: &plx_plex::plex::Client, off: &plx_base::task::OffFrame, sid: ServerId, job: Job) -> Landing {
    match job {
        Job::Tune { serial, dvr_key, channel, label, airing, session } => {
            let result = tune(client, off, &dvr_key, &channel, &session)
                .map(|(subscription_id, item, stream)| Tuned { sid, subscription_id, item, stream, channel: label, airing, cloud: false });
            Landing::Tuned { serial, result }
        }
        Job::Release { subscription_id, session } => {
            release(client, &subscription_id, &session);
            Landing::Released
        }
        Job::CloudTabs { auth } => Landing::CloudTabs(auth.client().cloud_tabs()),
        Job::CloudChannels { source, auth } => {
            let result = auth.client().cloud_channels(source.genre());
            Landing::CloudChannels { source, result }
        }
        Job::CloudGrid { source, ch, channel, date, auth } => {
            let result = auth.client().cloud_grid(&channel, &date);
            Landing::CloudGrid { source, ch, date, result }
        }
        Job::CloudProbe { channel, auth } => Landing::CloudProbe(auth.client().cloud_stream_shape(&channel)),
        Job::CloudTune { serial, channel, label, airing, auth } => {
            let client = auth.client();
            let shape = client.cloud_stream_shape(&channel);
            if !shape.is_empty() {
                plx_base::eventlog::log(&format!("livetv dbg: Plex channel stream: {shape}"));
            }
            let result = client.cloud_stream_url(&channel).ok_or(LiveError::Invalid).map(|url| Tuned {
                sid,
                subscription_id: String::new(),
                item: plx_plex::plex::live_tv::TunedItem {
                    channel_title: channel.title.clone(),
                    channel_vcn: channel.vcn.clone(),
                    ..Default::default()
                },
                // a provider's HLS is H.264/AAC MPEG-TS (seen on the TV); its frame rate and
                // raster the demuxer learns from the stream itself
                stream: plx_plex::plex::live_tv::LiveStream {
                    url,
                    encoder_session: String::new(),
                    video_codec: "h264".into(),
                    audio_codec: "aac".into(),
                    width: 0,
                    height: 0,
                    fps: 0.0,
                },
                channel: label,
                airing,
                cloud: true,
            });
            Landing::Tuned { serial, result }
        }
        Job::Blur { art } => {
            let result = client.ultrablur_colors(&art);
            Landing::Blur { art, result }
        }
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

/// A tune and its encode. Whatever fails after the tune took a tuner releases it before answering,
/// so an `Err` holds nothing on the server.
///
/// The lineup `id` is what the tune path takes; a channel the server rejects by that id (400/404)
/// is asked once more by its number ("4.1", the form the spec's own example uses).
fn tune(
    client: &plx_plex::plex::Client,
    off: &plx_base::task::OffFrame,
    dvr_key: &str,
    channel: &plx_plex::plex::live_tv::Channel,
    session: &str,
) -> Result<(String, plx_plex::plex::live_tv::TunedItem, plx_plex::plex::live_tv::LiveStream), LiveError> {
    let t = match client.live_tune(dvr_key, &channel.identifier, session) {
        Err(LiveError::Status(status @ (400 | 404))) if !channel.vcn.is_empty() && channel.vcn != channel.identifier => {
            plx_base::eventlog::log(&format!("livetv: tune by lineup id answered HTTP {status}; asking by channel number"));
            client.live_tune(dvr_key, &channel.vcn, session)?
        }
        other => other?,
    };
    if !t.playable() {
        if !t.subscription_id.is_empty() {
            let _ = client.live_release(&t.subscription_id);
        }
        return Err(if t.has_conflicts() { LiveError::Busy } else { LiveError::Unreadable });
    }
    match client.live_stream(off, &t.item.key, session) {
        Ok(stream) => Ok((t.subscription_id, t.item, stream)),
        Err(e) => {
            release(client, &t.subscription_id, session);
            Err(e)
        }
    }
}

/// Stop a tune's encode and delete its subscription (a 404 is done; a silent server is tried twice
/// more, a second apart).
fn release(client: &plx_plex::plex::Client, subscription_id: &str, session: &str) {
    if !session.is_empty() {
        let _ = client.live_stream_stop(session);
    }
    if subscription_id.is_empty() {
        return;
    }
    for attempt in 0..3 {
        match client.live_release(subscription_id) {
            Err(LiveError::Transport) if attempt < 2 => std::thread::sleep(std::time::Duration::from_secs(1)),
            _ => break,
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
    /// The write running now (one at a time): a press repeated before it lands is not sent again.
    write_running: Option<Write>,
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
    /// The Watch press the page still wants (its serial), what it stands at, and the stream once
    /// it landed and until the application takes it.
    tune_serial: u32,
    tune_wanted: bool,
    tune_phase: TunePhase,
    tune_queue: Option<Job>,
    tuned: Option<Tuned>,
    /// Releases owed to servers (an unwanted tune), each against the server that tuned it.
    releases: VecDeque<(ServerId, Job)>,
    /// Tune jobs started and not yet landed (a superseded one still takes a tuner until then).
    tunes_running: usize,
    /// The last guide window a page asked for: a Retry and the minute's re-ask of failed days
    /// ask it again.
    window: Option<(std::ops::Range<usize>, i64, i64)>,
    now: i64,
    utc_offset: i64,
    offset_logged: bool,
    /// Guide art → its UltraBlur colours (`None`: asked, or the server had none), with the order
    /// art was first asked in, for forgetting the oldest past [`MAX_BLURS`].
    blurs: HashMap<String, Option<[[f32; 3]; 4]>>,
    blur_order: VecDeque<String>,
    /// Which lineup `channels`/`channels_phase`/`guide` are (the guide tab shown); the others a
    /// page has shown are put aside with their caches, and their answers still land there.
    source: Source,
    stash: HashMap<Source, Lineup>,
    /// Plex's own channels: their genre tabs, and who their reads act for.
    cloud_tabs: Vec<CloudTab>,
    cloud_auth: Option<CloudAuth>,
    generation: u32,
}

/// A lineup put aside (a guide tab not shown).
#[derive(Default)]
struct Lineup {
    phase: Phase,
    channels: Vec<Channel>,
    guide: GuideCache,
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
            write_running: None,
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
            tune_serial: 0,
            tune_wanted: false,
            tune_phase: TunePhase::Idle,
            tune_queue: None,
            tunes_running: 0,
            tuned: None,
            releases: VecDeque::new(),
            window: None,
            now: 0,
            utc_offset: 0,
            offset_logged: false,
            blurs: HashMap::new(),
            blur_order: VecDeque::new(),
            source: Source::Dvr,
            stash: HashMap::new(),
            cloud_tabs: Vec::new(),
            cloud_auth: None,
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

    /// Did the providers read fail (nothing is known about this server's Live TV yet)?
    pub fn providers_failed(&self) -> bool {
        matches!(self.providers_phase, Phase::Failed(_))
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
        self.cancel_tune();
        let keep = (self.now, self.utc_offset, self.generation, self.notice_serial, self.tune_serial);
        let releases = std::mem::take(&mut self.releases);
        let tunes_running = self.tunes_running;
        // A tune still in flight lands under the old epoch: `pump` releases it, so the inbox
        // stays (`with_runner` would orphan it) and only the epoch moves.
        let inbox = Arc::clone(&self.inbox);
        let runner = std::mem::replace(&mut self.runner, Box::new(NoRun));
        let epoch = self.epoch + 1;
        *self = Self::with_runner(runner);
        self.epoch = epoch;
        self.inbox = inbox;
        self.releases = releases;
        self.tunes_running = tunes_running;
        // the generation and serials never go back: a page compares them for "moved"
        (self.now, self.utc_offset, self.generation, self.notice_serial, self.tune_serial) = keep;
        self.bump();
    }

    /// Watch channel row `ch` (the page's OK on a channel that is on now). Supersedes an earlier
    /// press: its tune, if it lands, is released. `false` when nothing can be tuned (no lineup,
    /// a provider without a DVR).
    pub fn tune(&mut self, ch: usize, airing: Option<Airing>) -> bool {
        if self.source.is_cloud() {
            return self.watch_cloud(ch, airing);
        }
        let Some(dvr_key) = self.provider.as_ref().filter(|p| p.tunable()).map(|p| p.dvr_key.clone()) else { return false };
        let Some(channel) = self.channels.get(ch).cloned() else { return false };
        if self.sid.is_none() {
            return false;
        }
        self.cancel_tune();
        self.tune_serial = self.tune_serial.wrapping_add(1);
        self.tune_wanted = true;
        let label = channel.words();
        plx_base::eventlog::log(&format!("livetv: tune #{} asked for channel row {ch}", self.tune_serial));
        self.tune_phase = TunePhase::Tuning { channel: label.clone() };
        self.tune_queue = Some(Job::Tune { serial: self.tune_serial, dvr_key, channel, label, airing, session: lt::new_session_id() });
        self.bump();
        true
    }

    /// Watch a Plex channel: the same Watch flow as a tune (the press supersedes an earlier one,
    /// the page says it is on its way, the stream lands for the application to play), with no
    /// tuner behind it — the provider hands out the stream.
    fn watch_cloud(&mut self, ch: usize, airing: Option<Airing>) -> bool {
        let (Some(channel), Some(auth)) = (self.channels.get(ch).cloned(), self.cloud_auth.clone()) else { return false };
        if self.sid.is_none() {
            return false;
        }
        self.cancel_tune();
        self.tune_serial = self.tune_serial.wrapping_add(1);
        self.tune_wanted = true;
        let label = channel.words();
        plx_base::eventlog::log(&format!("livetv: watch #{} of Plex channel row {ch}", self.tune_serial));
        self.tune_phase = TunePhase::Tuning { channel: label.clone() };
        self.tune_queue = Some(Job::CloudTune { serial: self.tune_serial, channel, label, airing, auth });
        self.bump();
        true
    }

    /// Which lineup the guide shows.
    pub fn source(&self) -> &Source {
        &self.source
    }

    /// Show `source`'s lineup (a guide tab). The lineup shown goes aside with its cache; one never
    /// read (or that failed) is asked for. Plex's own channels need the profile's plex.tv identity.
    pub fn select_source(&mut self, source: Source) {
        if source == self.source {
            return;
        }
        if source.is_cloud() && self.cloud_auth.is_none() {
            self.cloud_auth = CloudAuth::capture();
            if self.cloud_auth.is_none() {
                plx_base::eventlog::log("livetv: Plex channels need a plex.tv sign-in");
                return;
            }
        }
        let shown = Lineup {
            phase: std::mem::take(&mut self.channels_phase),
            channels: std::mem::take(&mut self.channels),
            guide: std::mem::take(&mut self.guide),
        };
        let left = std::mem::replace(&mut self.source, source.clone());
        self.stash.insert(left, shown);
        let next = self.stash.remove(&source).unwrap_or_default();
        (self.channels_phase, self.channels, self.guide) = (next.phase, next.channels, next.guide);
        plx_base::eventlog::log(&format!("livetv: guide tab {}", match &self.source {
            Source::Dvr => "the DVR",
            Source::Plex => "Plex Channels",
            Source::PlexTab(_) => "a Plex genre",
        }));
        if matches!(self.channels_phase, Phase::Idle | Phase::Failed(_)) {
            self.ask_lineup();
        } else {
            self.rewant();
        }
        self.bump();
    }

    /// A host test stands in for the profile's plex.tv identity (it has no session to capture).
    #[cfg(test)]
    pub(crate) fn set_cloud_auth(&mut self, auth: CloudAuth) {
        self.cloud_auth = Some(auth);
    }

    /// Ask for the shown source's lineup.
    fn ask_lineup(&mut self) {
        match self.source.clone() {
            Source::Dvr => {
                if let Some(p) = &self.provider {
                    self.channels_phase = Phase::Loading;
                    self.queue.push_front(Job::Channels { provider: p.identifier.clone() });
                }
            }
            source => {
                if let Some(auth) = self.cloud_auth.clone() {
                    self.channels_phase = Phase::Loading;
                    self.queue.push_front(Job::CloudChannels { source, auth });
                }
            }
        }
    }

    /// A lineup answer for `source`: onto the one shown, or the one put aside.
    fn land_lineup(&mut self, source: Source, result: Result<Vec<Channel>, LiveError>) {
        let (phase, channels) = match result {
            Ok(c) => (Phase::Ready, c),
            Err(e) => (Phase::Failed(e.reason()), Vec::new()),
        };
        if source == self.source {
            if phase.is_ready() {
                self.channels = channels;
                self.guide.clear();
            }
            self.channels_phase = phase;
            self.rewant();
        } else if let Some(l) = self.stash.get_mut(&source) {
            if phase.is_ready() {
                l.channels = channels;
                l.guide.clear();
            }
            l.phase = phase;
        }
    }

    /// A guide-day answer for `source`: onto the cache shown, or the one put aside.
    fn land_day(&mut self, source: &Source, ch: usize, date: &str, result: Result<Vec<Airing>, LiveError>) {
        // Which it was, for the log: a day the server has nothing for reads "No information" in the
        // guide, a day it did not answer for reads as failed (and is asked again in a minute).
        match &result {
            Ok(a) if a.is_empty() => plx_base::eventlog::log(&format!("livetv: row {ch} has no programs for {date}")),
            Err(e) => plx_base::eventlog::log(&format!("livetv: row {ch}'s programs for {date} did not load: {}", e.reason())),
            Ok(_) => {}
        }
        let result = result.map_err(|e| e.reason());
        let now = if self.now > 0 { self.now } else { lt::wall_now() };
        if *source == self.source {
            self.guide.land(ch, date, result, now);
        } else if let Some(l) = self.stash.get_mut(source) {
            l.guide.land(ch, date, result, now);
        }
    }

    /// Once a minute while the guide is in use: days read [`GUIDE_FRESH_S`] ago are read again,
    /// the shown ones now (their programs stay on screen meanwhile), a stashed tab's when it is
    /// shown again.
    pub fn refresh_stale(&mut self) {
        let now = if self.now > 0 { self.now } else { lt::wall_now() };
        for l in self.stash.values_mut() {
            l.guide.expire(now, GUIDE_FRESH_S);
        }
        if self.guide.expire(now, GUIDE_FRESH_S) {
            plx_base::eventlog::log("livetv: the guide shown is read again (older than its freshness)");
            self.rewant();
        }
    }

    /// Read the recordings lists again without saying so (a periodic refresh: the lists shown stay
    /// until the new ones land, and an empty page keeps its advice rather than "Loading…").
    pub fn refresh_dvr_quietly(&mut self) {
        // not before the lists were ever asked for, nor over a read on its way; after a read that
        // failed, the next refresh is the retry
        if self.sid.is_none() || self.queue.iter().any(|j| matches!(j, Job::Dvr)) || matches!(self.dvr_phase, Phase::Idle | Phase::Loading) {
            return;
        }
        self.queue.push_back(Job::Dvr);
    }

    /// The page no longer wants its Watch press (BACK while tuning, the page left). A tune that
    /// already landed and was not taken is released now; one in flight is released when it lands.
    pub fn cancel_tune(&mut self) {
        if !self.tune_wanted && self.tuned.is_none() && matches!(self.tune_phase, TunePhase::Idle) {
            return;
        }
        self.tune_wanted = false;
        self.tune_queue = None;
        if let Some(t) = self.tuned.take() {
            self.owe_release(t);
        }
        self.tune_phase = TunePhase::Idle;
        self.bump();
    }

    /// The landed stream, once: the caller (the application) installs it into the media route,
    /// whose lease releases it from then on.
    pub fn take_tuned(&mut self) -> Option<Tuned> {
        let t = self.tuned.take()?;
        self.tune_wanted = false;
        self.tune_phase = TunePhase::Idle;
        self.bump();
        Some(t)
    }

    /// At exit: a tune landed and not played is released now (with every release still owed).
    /// A tune still in flight cannot be named yet; the server reclaims it.
    pub fn release_for_exit(&mut self) {
        self.cancel_tune();
        self.start();
    }

    /// A taken stream the application could not play: release it.
    pub fn release_tuned(&mut self, t: Tuned) {
        self.owe_release(t);
        self.bump();
    }

    /// A Watch the application holds back until a tuner comes back: the page shows it as tuning
    /// `channel` meanwhile (and its BACK cancels it, through the application).
    pub fn show_tuning(&mut self, channel: String) {
        self.tune_phase = TunePhase::Tuning { channel };
        self.bump();
    }

    /// Is a tuner still to come back from this store: a tune owed a release, a release running, or
    /// a tune still on its way (a superseded one holds a tuner until it lands and is released)? A
    /// DVR with no tuner to spare answers the next tune "busy" until none is.
    pub fn tuners_settling(&self) -> bool {
        !self.releases.is_empty() || RELEASING.load(Ordering::Acquire) > 0 || self.tunes_running > 0
    }

    /// Did the last Watch press fail (its notice not yet dismissed)?
    pub fn tune_failed(&self) -> bool {
        matches!(self.tune_phase, TunePhase::Failed { .. })
    }

    /// Forget a failed tune's message (the page showed it).
    pub fn clear_tune_failure(&mut self) {
        if matches!(self.tune_phase, TunePhase::Failed { .. }) {
            self.tune_phase = TunePhase::Idle;
            self.bump();
        }
    }

    fn owe_release(&mut self, t: Tuned) {
        plx_base::eventlog::log("livetv: releasing an unwanted tune");
        self.releases.push_back((t.sid, Job::Release { subscription_id: t.subscription_id, session: t.stream.encoder_session }));
    }

    /// The guide window a page shows: channel rows `rows` between `from` and `to` (epoch seconds).
    /// Asks for every channel-day it does not hold, nearest rows first.
    pub fn want_window(&mut self, rows: std::ops::Range<usize>, from: i64, to: i64) {
        self.window = Some((rows.clone(), from, to));
        enum From {
            Dvr(String),
            Cloud(Source, CloudAuth),
        }
        let from_where = match (&self.source, &self.provider, &self.cloud_auth) {
            (Source::Dvr, Some(p), _) => From::Dvr(p.identifier.clone()),
            (Source::Dvr, None, _) => return,
            (source, _, Some(auth)) => From::Cloud(source.clone(), auth.clone()),
            (_, _, None) => return,
        };
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
            self.queue.push_front(match &from_where {
                From::Dvr(provider) => Job::Grid { provider: provider.clone(), ch, channel, date },
                From::Cloud(source, auth) => Job::CloudGrid { source: source.clone(), ch, channel, date, auth: auth.clone() },
            });
        }
        self.guide.evict();
        self.bump();
    }

    /// Ask again for every guide day that failed (the page's Retry).
    pub fn retry_failed(&mut self) {
        self.guide.reset_failed();
        self.rewant();
        if matches!(self.dvr_phase, Phase::Failed(_)) {
            self.refresh_dvr();
        }
        if matches!(self.channels_phase, Phase::Failed(_)) {
            self.ask_lineup();
        }
        if matches!(self.providers_phase, Phase::Failed(_)) {
            self.providers_phase = Phase::Loading;
            self.queue.push_front(Job::Providers);
        }
        self.bump();
    }

    /// Ask again for the last window (after failed days were reset to unasked).
    fn rewant(&mut self) {
        if let Some((rows, from, to)) = self.window.clone() {
            self.want_window(rows, from, to);
        }
    }

    /// The airings of channel row `ch` in `[from, to)`, from the cache.
    pub fn airings(&self, ch: usize, from: i64, to: i64) -> Vec<Airing> {
        self.guide.airings(ch, from, to)
    }

    /// How many channel rows the lineup shown has (the live player's CH▲/CH▼ wrap around them).
    pub fn channel_count(&self) -> usize {
        self.channels.len()
    }

    /// How channel row `ch` is named ("4.1 KXYZ"), for the player's lines.
    pub fn channel_words(&self, ch: usize) -> Option<String> {
        self.channels.get(ch).map(Channel::words)
    }

    /// The airing on now on channel row `ch`, from the guide cache (the player's record disc).
    pub fn on_now(&self, ch: usize) -> Option<Airing> {
        let now = if self.now > 0 { self.now } else { lt::wall_now() };
        self.guide.airings(ch, now, now + 1).into_iter().find(|a| a.airs_at(now))
    }

    /// Is channel row `ch` still loading for `[from, to)`?
    pub fn row_pending(&self, ch: usize, from: i64, to: i64) -> bool {
        self.guide.pending(ch, &lt::days_covering(from, to))
    }

    /// Did every day of channel row `ch` for `[from, to)` fail?
    pub fn row_failed(&self, ch: usize, from: i64, to: i64) -> bool {
        self.guide.failed(ch, &lt::days_covering(from, to))
    }

    /// The page's hero shows `art`: ask its UltraBlur colours, once (the page tints itself with
    /// them; until they land it keeps its own ground).
    pub fn want_blur(&mut self, art: &str) {
        if self.sid.is_none() || art.is_empty() || self.blurs.contains_key(art) {
            return;
        }
        if self.blur_order.len() >= MAX_BLURS {
            if let Some(old) = self.blur_order.pop_front() {
                self.blurs.remove(&old);
            }
        }
        self.blurs.insert(art.to_string(), None);
        self.blur_order.push_back(art.to_string());
        self.queue.push_back(Job::Blur { art: art.to_string() });
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
        // The same write twice before the first lands (OK pressed again on a program whose record
        // mark only appears when the answer does) would be a duplicate rule, or a failure notice
        // right after the success.
        if self.write_running.as_ref() == Some(&w) || self.write_queue.contains(&w) {
            plx_base::eventlog::log("livetv: the same DVR change is already on its way");
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
                let offset = plx_platform::i18n::utc_offset_s(minute);
                if offset != self.utc_offset || !self.offset_logged {
                    // Once, and on a change: the guide's times are UTC plus this, and whether the
                    // set's libc knows the viewer's zone is a device question the log answers.
                    self.offset_logged = true;
                    plx_base::eventlog::log(&format!("livetv: guide local time is UTC{:+}:{:02}", offset / 3_600, (offset.abs() % 3_600) / 60));
                }
                self.utc_offset = offset;
                // A day that failed (a timeout, a busy server) is asked again once a minute while
                // the page still shows it; nothing else would ever re-ask it.
                if self.guide.has_failed() {
                    self.guide.reset_failed();
                    self.rewant();
                }
                self.bump();
            }
        }
        let landed: Vec<(u64, Landing)> = std::mem::take(&mut *self.inbox.lock().unwrap_or_else(|e| e.into_inner()));
        for (epoch, landing) in landed {
            if matches!(landing, Landing::Tuned { .. }) {
                self.tunes_running = self.tunes_running.saturating_sub(1);
            }
            if epoch != self.epoch {
                // A late tune still holds a tuner: release it; every other late answer is moot.
                if let Landing::Tuned { result: Ok(t), .. } = landing {
                    self.owe_release(t);
                }
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
                        plx_base::eventlog::log(&format!(
                            "livetv: server sid={} has {} ({} provider(s); the chosen one {})",
                            self.sid.map_or(0, |s| s.raw()),
                            if self.provider.is_some() { "Live TV" } else { "no Live TV" },
                            list.len(),
                            match &self.provider {
                                Some(p) if p.tunable() => "tunes and has a guide",
                                Some(_) => "has a guide but no DVR to tune",
                                None => "-",
                            }
                        ));
                        if let Some(p) = &self.provider {
                            self.channels_phase = Phase::Loading;
                            self.queue.push_front(Job::Channels { provider: p.identifier.clone() });
                            self.dvr_phase = Phase::Loading;
                            self.queue.push_back(Job::Dvr);
                            // the guide's tab row also offers Plex's own channels
                            self.cloud_auth = CloudAuth::capture();
                            if let Some(auth) = self.cloud_auth.clone() {
                                self.queue.push_back(Job::CloudTabs { auth });
                            }
                        }
                    }
                    Err(LiveError::Status(403)) => {
                        plx_base::eventlog::log("livetv: providers answered 403: this account may not use the DVR");
                        self.providers_phase = Phase::Ready;
                        self.availability = Availability::None;
                    }
                    Err(e) => {
                        plx_base::eventlog::log(&format!("livetv: providers failed: {}", e.reason()));
                        self.providers_phase = Phase::Failed(e.reason())
                    }
                }
            }
            Landing::Channels(r) => {
                self.in_flight = self.in_flight.saturating_sub(1);
                self.land_lineup(Source::Dvr, r);
            }
            Landing::Grid { ch, date, result } => {
                self.in_flight = self.in_flight.saturating_sub(1);
                self.land_day(&Source::Dvr, ch, &date, result);
            }
            Landing::CloudChannels { source, result } => {
                self.in_flight = self.in_flight.saturating_sub(1);
                if let Err(e) = &result {
                    plx_base::eventlog::log(&format!("livetv: Plex channels lineup failed: {}", e.reason()));
                }
                self.land_lineup(source, result);
            }
            Landing::CloudGrid { source, ch, date, result } => {
                self.in_flight = self.in_flight.saturating_sub(1);
                self.land_day(&source, ch, &date, result);
            }
            Landing::CloudTabs(r) => {
                self.in_flight = self.in_flight.saturating_sub(1);
                match r {
                    Ok(tabs) => self.cloud_tabs = tabs,
                    Err(e) => plx_base::eventlog::log(&format!("livetv: Plex channels tabs failed: {}", e.reason())),
                }
            }
            Landing::CloudProbe(shape) => {
                self.in_flight = self.in_flight.saturating_sub(1);
                plx_base::eventlog::log(&format!("livetv dbg: Plex channel stream: {shape}"));
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
            Landing::Tuned { serial, result } => match result {
                Ok(t) if self.tune_wanted && serial == self.tune_serial => {
                    plx_base::eventlog::log(&format!(
                        "livetv: tune #{serial} landed: stream {}/{} {}x{}, subscription {}",
                        t.stream.video_codec,
                        t.stream.audio_codec,
                        t.stream.width,
                        t.stream.height,
                        if t.subscription_id.is_empty() { "none" } else { "held" }
                    ));
                    self.tune_phase = TunePhase::Tuning { channel: t.channel.clone() };
                    if let Some(old) = self.tuned.replace(t) {
                        self.owe_release(old);
                    }
                }
                Ok(t) => self.owe_release(t),
                Err(e) if self.tune_wanted && serial == self.tune_serial => {
                    plx_base::eventlog::log(&format!("livetv: tune #{serial} failed: {}", e.reason()));
                    self.tune_wanted = false;
                    let channel = match &self.tune_phase {
                        TunePhase::Tuning { channel } => channel.clone(),
                        _ => String::new(),
                    };
                    self.tune_phase = TunePhase::Failed { channel, reason: e.reason() };
                }
                Err(_) => {}
            },
            Landing::Released => {}
            Landing::Blur { art, result } => {
                self.in_flight = self.in_flight.saturating_sub(1);
                if let (Ok(colors), Some(slot)) = (result, self.blurs.get_mut(&art)) {
                    *slot = Some(colors);
                    self.bump();
                }
            }
            Landing::Write { write, result } => {
                self.writing = false;
                self.write_running = None;
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
        while let Some((owner, job)) = self.releases.pop_front() {
            let post = Post { epoch: self.epoch, inbox: Arc::clone(&self.inbox) };
            if !self.runner.run(owner, job.clone(), post) {
                // the OS refused a thread: try again next frame
                self.releases.push_front((owner, job));
                break;
            }
        }
        let Some(sid) = self.sid else { return };
        if let Some(job) = self.tune_queue.take() {
            let post = Post { epoch: self.epoch, inbox: Arc::clone(&self.inbox) };
            if self.runner.run(sid, job.clone(), post) {
                self.tunes_running += 1;
            } else {
                self.tune_queue = Some(job);
            }
        }
        if !self.writing {
            if let Some(w) = self.write_queue.pop_front() {
                let post = Post { epoch: self.epoch, inbox: Arc::clone(&self.inbox) };
                if self.runner.run(sid, Job::Write(w.clone()), post) {
                    self.writing = true;
                    self.write_running = Some(w);
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
            sid: self.sid,
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
            tune: self.tune_phase.clone(),
            blurs: self.blurs.iter().filter_map(|(art, c)| c.map(|c| (art.clone(), c))).collect(),
            source: self.source.clone(),
            cloud_tabs: self.cloud_tabs.clone(),
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
