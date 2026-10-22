//! Live TV and DVR, data side: the snapshot a Live TV page draws from, the guide's day cache, and
//! the wall-clock seam. The store that owns them is `stores::live_tv`; the protocol is
//! `plx_plex::plex::live_tv`.
//!
//! **Why the clock is read here.** A guide is a picture of wall-clock time: what is on now, where
//! the now-line sits, which UTC day a window needs. The `wall` gate keeps clock reads out of
//! `stores/`, `ui/`, `screens/` and the application, so the one read lives in this IO seam
//! ([`wall_now`]), beside the data it dates, as `crate::watchlist`'s `today` does. A television
//! without a battery clock can read 1970 until NTP has run; [`plausible_now`] says so, and the
//! store waits rather than asking the server for a guide of 1970.

use plx_plex::plex::live_tv::{Airing, Channel, EpgProvider, Grab, Subscription};
use std::collections::HashMap;

/// The wall clock, epoch seconds.
pub fn wall_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// Is `now` a believable date (after 2024)? Before NTP the set's clock reads 1970.
pub fn plausible_now(now: i64) -> bool {
    now > 1_700_000_000
}

/// The UTC days (`YYYY-MM-DD`) a window `[from, to)` touches — one or two for a guide window.
pub fn days_covering(from: i64, to: i64) -> Vec<String> {
    let mut out = Vec::new();
    let mut day = from.div_euclid(86_400) * 86_400;
    while day < to.max(from + 1) {
        out.push(plx_plex::plex::live_tv::utc_date(day));
        day += 86_400;
    }
    out
}

/// Where a load stands. `Failed` carries the user-safe reason.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Phase {
    #[default]
    Idle,
    Loading,
    Ready,
    Failed(String),
}

impl Phase {
    pub fn is_loading(&self) -> bool {
        matches!(self, Phase::Loading)
    }
    pub fn is_ready(&self) -> bool {
        matches!(self, Phase::Ready)
    }
}

/// What the server offers, decided once its providers answered.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Availability {
    /// Not asked yet, or asking.
    #[default]
    Unknown,
    /// The server has a DVR with a guide.
    Available,
    /// The server answered and has no Live TV (or this account may not use it).
    None,
}

/// One channel-day of the guide.
#[derive(Clone, Debug, Default)]
pub struct Day {
    pub phase: Phase,
    pub airings: Vec<Airing>,
    /// The cache clock at the last time a window wanted this day (eviction order).
    pub wanted_at: u64,
}

/// The guide's day cache: one entry per `(channel index, UTC date)`, bounded by [`DAYS_KEPT`]
/// with the least recently wanted evicted first. A day a window still wants is never evicted.
#[derive(Clone, Debug, Default)]
pub struct GuideCache {
    days: HashMap<(usize, String), Day>,
    clock: u64,
}

/// Channel-days the guide keeps: with ~20-50 airings a day that is a few thousand airings, the
/// order of what one official client keeps (about 2000).
pub const DAYS_KEPT: usize = 96;

impl GuideCache {
    pub fn get(&self, ch: usize, date: &str) -> Option<&Day> {
        self.days.get(&(ch, date.to_string()))
    }
    pub fn len(&self) -> usize {
        self.days.len()
    }
    pub fn is_empty(&self) -> bool {
        self.days.is_empty()
    }
    /// Mark every `(channel, date)` of a window wanted; return the ones never asked for, in row
    /// order. A failed day is not asked again until [`GuideCache::reset_failed`] (the page's
    /// Retry), so a dead server is not hammered by every scroll.
    pub fn want(&mut self, rows: impl Iterator<Item = usize>, dates: &[String]) -> Vec<(usize, String)> {
        self.clock += 1;
        let mut need = Vec::new();
        for ch in rows {
            for d in dates {
                let day = self.days.entry((ch, d.clone())).or_default();
                day.wanted_at = self.clock;
                if matches!(day.phase, Phase::Idle) {
                    need.push((ch, d.clone()));
                }
            }
        }
        need
    }
    /// Every failed day goes back to unasked, so the next window asks again.
    pub fn reset_failed(&mut self) {
        for d in self.days.values_mut() {
            if matches!(d.phase, Phase::Failed(_)) {
                d.phase = Phase::Idle;
            }
        }
    }
    pub fn set_loading(&mut self, ch: usize, date: &str) {
        if let Some(d) = self.days.get_mut(&(ch, date.to_string())) {
            d.phase = Phase::Loading;
        }
    }
    /// A fetch landed. A day evicted meanwhile is not resurrected.
    pub fn land(&mut self, ch: usize, date: &str, result: Result<Vec<Airing>, String>) {
        if let Some(d) = self.days.get_mut(&(ch, date.to_string())) {
            match result {
                Ok(a) => {
                    d.airings = a;
                    d.phase = Phase::Ready;
                }
                Err(why) => d.phase = Phase::Failed(why),
            }
        }
    }
    /// Evict least-recently-wanted days past [`DAYS_KEPT`], never one wanted by the last window.
    /// A day still loading may go too: its answer then lands nowhere ([`GuideCache::land`]), and
    /// a window that comes back asks for it again.
    pub fn evict(&mut self) {
        if self.days.len() <= DAYS_KEPT {
            return;
        }
        let mut order: Vec<((usize, String), u64)> = self.days.iter().map(|(k, d)| (k.clone(), d.wanted_at)).collect();
        order.sort_by_key(|(_, at)| *at);
        let excess = self.days.len() - DAYS_KEPT;
        for (k, at) in order.into_iter().take(excess) {
            if at < self.clock {
                self.days.remove(&k);
            }
        }
    }
    /// Every airing of channel `ch` overlapping `[from, to)`, across its cached days, sorted and
    /// de-duplicated (an airing across UTC midnight is in both days).
    pub fn airings(&self, ch: usize, from: i64, to: i64) -> Vec<Airing> {
        let mut out: Vec<Airing> = self
            .days
            .iter()
            .filter(|((c, _), d)| *c == ch && d.phase.is_ready())
            .flat_map(|(_, d)| d.airings.iter())
            .filter(|a| a.ends_at > from && a.begins_at < to)
            .cloned()
            .collect();
        out.sort_by_key(|a| a.begins_at);
        out.dedup_by_key(|a| a.begins_at);
        out
    }
    /// Is any day of channel `ch` covering `[from, to)` still loading or unasked?
    pub fn pending(&self, ch: usize, dates: &[String]) -> bool {
        dates.iter().any(|d| self.get(ch, d).is_none_or(|day| matches!(day.phase, Phase::Idle | Phase::Loading)))
    }
    /// Did every day of channel `ch` in `dates` fail?
    pub fn failed(&self, ch: usize, dates: &[String]) -> bool {
        !dates.is_empty() && dates.iter().all(|d| self.get(ch, d).is_some_and(|day| matches!(day.phase, Phase::Failed(_))))
    }
    /// Apply a recording change to every cached airing it covers: `rule` now covers airings of
    /// `guid` (an episode or movie rule) or of the show `guid` (a series rule), or, with
    /// `rule == ""`, rule `old` no longer covers anything.
    pub fn overlay(&mut self, change: &Overlay) {
        for day in self.days.values_mut() {
            for a in &mut day.airings {
                match change {
                    Overlay::Episode { guid, rule } if &a.guid == guid => a.subscription_id = rule.clone(),
                    Overlay::Series { show_guid, rule } if &a.grandparent_guid == show_guid => {
                        a.show_subscription_id = rule.clone()
                    }
                    Overlay::Removed { rule } => {
                        if &a.subscription_id == rule {
                            a.subscription_id.clear();
                        }
                        if &a.show_subscription_id == rule {
                            a.show_subscription_id.clear();
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    pub fn clear(&mut self) {
        self.days.clear();
    }
}

/// A recording change the guide shows before (and whether or not) the grid is read again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Overlay {
    Episode { guid: String, rule: String },
    Series { show_guid: String, rule: String },
    Removed { rule: String },
}

/// What a record press asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordKind {
    /// This airing's program only (an episode or a movie).
    This,
    /// Every airing of the show.
    Series,
}

/// The last write's outcome, for the page to say what happened (a short notice).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriteNotice {
    /// Bumps per write, so a page can tell a new notice from the one it already showed.
    pub serial: u32,
    pub ok: bool,
    /// What was done ("record", "cancel recording", "stop recording", "move recording") — the page
    /// words it; `reason` is the user-safe failure.
    pub op: WriteOp,
    pub reason: String,
    /// A create that answered conflicts: the rule exists and collides with these recordings.
    pub conflicts: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteOp {
    Record,
    RecordSeries,
    CancelRule,
    StopRecording,
    MoveRule,
}

/// Everything a Live TV page draws, by value. Built by the store when something moved.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub generation: u32,
    pub availability: Availability,
    pub providers_phase: Phase,
    pub provider: Option<EpgProvider>,
    pub channels_phase: Phase,
    pub channels: Vec<Channel>,
    /// Epoch seconds, refreshed once a minute; 0 until the clock is plausible.
    pub now: i64,
    /// The local offset from UTC at `now`, seconds (0 when the platform cannot say).
    pub utc_offset: i64,
    pub dvr_phase: Phase,
    pub rules: Vec<Subscription>,
    pub scheduled: Vec<Grab>,
    pub notice: Option<WriteNotice>,
    pub writes_in_flight: u32,
}
