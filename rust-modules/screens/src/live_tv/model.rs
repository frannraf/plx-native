//! **The Live TV page's model**: its element keys, the guide's geometry, where a direction moves
//! focus, and which actions a focused airing, recording or rule offers. Pure functions over the
//! fed [`LiveTvView`]; the page (`super::page`) draws them and routes what they decide.
//!
//! **A guide cell's key is its channel row and its start minute** (`row << 20 | minute`), the
//! minute counted from a base the page fixes once it knows the time. An airing keeps its key while
//! days land around it and the window scrolls, so focus never jumps when the guide fills in. A row
//! with nothing to show (still loading, or a channel with no guide) has one placeholder key.

use plx_data::live_tv::Snapshot;
use plx_data::stores::live_tv::Write;
use plx_data::live_tv::RecordKind;
use plx_plex::plex::live_tv::{Airing, Grab, Subscription};
use plx_ui::screen::Dir;
use plx_ui::Rect;

/// The guide's time step: the ruler's ticks and the window's alignment.
pub const HALF_HOUR: i64 = 1_800;
/// How much time the guide shows across.
pub const WINDOW_S: i64 = 2 * 3_600;
/// Channel rows shown at once.
pub const ROWS_SHOWN: usize = 5;
/// Channel rows the guide can key (`row << 20` stays below the reserved keys).
pub const MAX_ROWS: usize = 2_048;

// ---- element keys -------------------------------------------------------------------------------

/// The header's two mode pills.
pub const HEAD_GUIDE: u32 = 0xE000_0000;
pub const HEAD_RECS: u32 = 0xE000_0001;
/// The read-out's Try again.
pub const RETRY: u32 = 0xE000_0010;
/// A Recordings-list row: `REC_BASE + index`.
pub const REC_BASE: u32 = 0xD000_0000;
/// An action of the open panel: `ACTION_BASE + index`.
pub const ACTION_BASE: u32 = 0xF000_0000;
const ROW_SHIFT: u32 = 20;
const SLOT_MASK: u32 = (1 << ROW_SHIFT) - 1;
/// The slot of a row's placeholder cell (a row with no airing to focus).
pub const PLACEHOLDER: u32 = SLOT_MASK;

pub fn cell_elem(row: usize, slot: u32) -> u32 {
    ((row.min(MAX_ROWS - 1) as u32) << ROW_SHIFT) | (slot & SLOT_MASK)
}

/// `(row, slot)` of a guide cell's key.
pub fn cell_of(elem: u32) -> Option<(usize, u32)> {
    (elem < 0x8000_0000).then_some(((elem >> ROW_SHIFT) as usize, elem & SLOT_MASK))
}

pub fn rec_of(elem: u32) -> Option<usize> {
    (REC_BASE..REC_BASE + 0x0100_0000).contains(&elem).then(|| (elem - REC_BASE) as usize)
}

pub fn action_of(elem: u32) -> Option<usize> {
    (ACTION_BASE..ACTION_BASE + 0x100).contains(&elem).then(|| (elem - ACTION_BASE) as usize)
}

/// The slot (minutes after `base`) an airing starting at `begins_at` keys by. Clamped into the key
/// space below the placeholder.
pub fn slot_of(base: i64, begins_at: i64) -> u32 {
    ((begins_at - base).max(0) / 60).min(i64::from(PLACEHOLDER - 1)) as u32
}

/// `t` floored to its half hour.
pub fn floor_half(t: i64) -> i64 {
    t - t.rem_euclid(HALF_HOUR)
}

// ---- what the page is fed -----------------------------------------------------------------------

/// One channel row's airings for the window the page asked for.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RowAirings {
    pub airings: Vec<Airing>,
    /// Still loading (or never asked: a row outside the wanted rows).
    pub pending: bool,
    /// Every day of the window failed.
    pub failed: bool,
}

/// Everything the page draws, by value: the store's snapshot and, per channel row, the airings of
/// the window it last asked for ([`Want`]).
#[derive(Clone, Debug, Default)]
pub struct LiveTvView {
    pub snap: Snapshot,
    /// One per `snap.channels` row; rows outside the wanted range are empty and pending.
    pub rows: Vec<RowAirings>,
}

/// The guide the page wants read: channel rows and a time span. The application hands it to the
/// store (`want_window`) and feeds back those rows' airings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Want {
    pub rows: std::ops::Range<usize>,
    pub from: i64,
    pub to: i64,
}

impl LiveTvView {
    pub fn row(&self, row: usize) -> Option<&RowAirings> {
        self.rows.get(row)
    }
    pub fn channel_count(&self) -> usize {
        self.snap.channels.len().min(MAX_ROWS)
    }
    /// The airing a guide key names.
    pub fn airing(&self, base: i64, elem: u32) -> Option<(usize, &Airing)> {
        let (row, slot) = cell_of(elem)?;
        if slot == PLACEHOLDER {
            return None;
        }
        let a = self.row(row)?.airings.iter().find(|a| slot_of(base, a.begins_at) == slot)?;
        Some((row, a))
    }
    /// The key focus takes in `row` at time `t`: the airing on at `t`, else the next one, else
    /// the last one before it, else the row's placeholder.
    pub fn cell_at(&self, base: i64, row: usize, t: i64) -> u32 {
        let Some(r) = self.row(row) else { return cell_elem(row, PLACEHOLDER) };
        let pick = r
            .airings
            .iter()
            .find(|a| a.begins_at <= t && t < a.ends_at)
            .or_else(|| r.airings.iter().find(|a| a.begins_at >= t))
            .or_else(|| r.airings.last());
        match pick {
            Some(a) => cell_elem(row, slot_of(base, a.begins_at)),
            None => cell_elem(row, PLACEHOLDER),
        }
    }
    /// Is this guide key one the view can show (its row exists; its airing is there, or it is a
    /// placeholder of a row with no airings)?
    pub fn holds(&self, base: i64, elem: u32) -> bool {
        let Some((row, slot)) = cell_of(elem) else { return false };
        if row >= self.channel_count() {
            return false;
        }
        if slot == PLACEHOLDER {
            return self.row(row).is_none_or(|r| r.airings.is_empty());
        }
        self.airing(base, elem).is_some()
    }
}

// ---- the guide's geometry -----------------------------------------------------------------------

/// Where the guide stands: the base its keys count from, the window's left edge, the first
/// channel row shown, and now (all epoch seconds).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Frame {
    pub base: i64,
    pub win_start: i64,
    pub row_top: usize,
    pub now: i64,
}

pub mod geom {
    use plx_ui::consts::{MARGIN_X, SCR_W};

    pub const HEAD_Y: f32 = plx_ui::consts::GRID_TOP_Y;
    pub const HEAD_H: f32 = 60.0;
    pub const PILL_W: f32 = 240.0;
    pub const PILL_GAP: f32 = 16.0;
    /// Below the Guide/Recordings pills and the mark under the one shown.
    pub const DETAIL_Y: f32 = 282.0;
    /// The hero's text column: the backdrop art owns the right of the band above the guide.
    pub const DETAIL_W: f32 = 1_000.0;
    /// The backdrop art's box: the right of the screen, from the top down to the ruler.
    pub const ART_X: f32 = SCR_W * 0.40;
    pub const ART_H: f32 = RULER_Y;
    pub const RULER_Y: f32 = 496.0;
    pub const RULER_H: f32 = 34.0;
    pub const ROWS_Y: f32 = 536.0;
    pub const ROW_H: f32 = 92.0;
    pub const ROW_PITCH: f32 = 98.0;
    pub const CHANNEL_X: f32 = MARGIN_X;
    pub const CHANNEL_W: f32 = 170.0;
    pub const GRID_X: f32 = MARGIN_X + CHANNEL_W + 10.0;
    /// A guide cell's program art: portrait, the row's height less a margin.
    pub const POSTER_H: f32 = ROW_H - 16.0;
    pub const POSTER_W: f32 = POSTER_H * 2.0 / 3.0;
    pub const GRID_W: f32 = SCR_W - MARGIN_X - GRID_X;
    pub const CELL_GAP: f32 = 6.0;
    pub const RECS_Y: f32 = 290.0;
    pub const REC_H: f32 = 74.0;
    pub const REC_PITCH: f32 = 84.0;
    pub const REC_HEADING_H: f32 = 52.0;
    pub const RECS_BOTTOM: f32 = 1_020.0;
    pub const PANEL_W: f32 = 640.0;
    pub const ACTION_H: f32 = 64.0;
    pub const ACTION_PITCH: f32 = 76.0;
}

/// x of instant `t` in the guide for a window starting at `win_start`.
pub fn x_of(win_start: i64, t: i64) -> f32 {
    geom::GRID_X + (t - win_start) as f32 * geom::GRID_W / WINDOW_S as f32
}

/// The rect an airing's cell is drawn in (clipped to the window), `None` when off the window.
pub fn cell_rect(frame: &Frame, row: usize, begins_at: i64, ends_at: i64) -> Option<Rect> {
    if row < frame.row_top || row >= frame.row_top + ROWS_SHOWN {
        return None;
    }
    let win_end = frame.win_start + WINDOW_S;
    if ends_at <= frame.win_start || begins_at >= win_end {
        return None;
    }
    let x0 = x_of(frame.win_start, begins_at.max(frame.win_start));
    let x1 = x_of(frame.win_start, ends_at.min(win_end)) - geom::CELL_GAP;
    let y = row_y(frame, row);
    Some(Rect::new(x0, y, (x1 - x0).max(4.0), geom::ROW_H))
}

/// The rect of a row's placeholder (the whole window).
pub fn placeholder_rect(frame: &Frame, row: usize) -> Option<Rect> {
    if row < frame.row_top || row >= frame.row_top + ROWS_SHOWN {
        return None;
    }
    Some(Rect::new(geom::GRID_X, row_y(frame, row), geom::GRID_W - geom::CELL_GAP, geom::ROW_H))
}

pub fn row_y(frame: &Frame, row: usize) -> f32 {
    geom::ROWS_Y + (row as f32 - frame.row_top as f32) * geom::ROW_PITCH
}

/// The rect a guide key is drawn at, where it would be drawn even when scrolled off (a row above
/// or below, a time left or right of the window), so a move's projection always has a source.
pub fn elem_rect(view: &LiveTvView, frame: &Frame, elem: u32) -> Option<Rect> {
    let (row, slot) = cell_of(elem)?;
    let y = row_y(frame, row);
    if slot == PLACEHOLDER {
        return Some(Rect::new(geom::GRID_X, y, geom::GRID_W - geom::CELL_GAP, geom::ROW_H));
    }
    let (_, a) = view.airing(frame.base, elem)?;
    let win_end = frame.win_start + WINDOW_S;
    // clipped to the window when it shows there, else where it would stand
    let (b, e) = if a.ends_at > frame.win_start && a.begins_at < win_end {
        (a.begins_at.max(frame.win_start), a.ends_at.min(win_end))
    } else {
        (a.begins_at, a.ends_at)
    };
    let x0 = x_of(frame.win_start, b);
    let x1 = x_of(frame.win_start, e) - geom::CELL_GAP;
    Some(Rect::new(x0, y, (x1 - x0).max(4.0), geom::ROW_H))
}

/// The time an UP/DOWN move keeps: where the focused cell starts on screen.
pub fn anchor_time(view: &LiveTvView, frame: &Frame, elem: u32) -> i64 {
    match view.airing(frame.base, elem) {
        Some((_, a)) => a.begins_at.max(frame.win_start),
        None => frame.win_start.max(frame.now),
    }
}

/// Where a direction moves focus inside the guide; `None` is the guide's edge (UP from the first
/// row goes to the header by the page's link). LEFT stops at the airing on now: the guide does not
/// scroll into the past.
pub fn guide_neighbour(view: &LiveTvView, frame: &Frame, elem: u32, dir: Dir) -> Option<u32> {
    let (row, slot) = cell_of(elem)?;
    match dir {
        Dir::Up | Dir::Down => {
            let target = if dir == Dir::Up { row.checked_sub(1)? } else { row + 1 };
            if target >= view.channel_count() {
                return None;
            }
            Some(view.cell_at(frame.base, target, anchor_time(view, frame, elem)))
        }
        Dir::Left | Dir::Right => {
            if slot == PLACEHOLDER {
                return None;
            }
            let airings = &view.row(row)?.airings;
            let i = airings.iter().position(|a| slot_of(frame.base, a.begins_at) == slot)?;
            let next = if dir == Dir::Left {
                let prev = airings.get(i.checked_sub(1)?)?;
                if prev.ends_at <= floor_half(frame.now) {
                    return None;
                }
                prev
            } else {
                airings.get(i + 1)?
            };
            Some(cell_elem(row, slot_of(frame.base, next.begins_at)))
        }
    }
}

/// The frame after focus landed on `elem`: the rows and the window scroll just enough to show it,
/// and the window never starts before the half hour now is in.
pub fn follow(view: &LiveTvView, frame: &Frame, elem: u32) -> Frame {
    let mut f = *frame;
    let Some((row, _)) = cell_of(elem) else { return f };
    if row < f.row_top {
        f.row_top = row;
    } else if row >= f.row_top + ROWS_SHOWN {
        f.row_top = row + 1 - ROWS_SHOWN;
    }
    let earliest = floor_half(f.now);
    if let Some((_, a)) = view.airing(f.base, elem) {
        let win_end = f.win_start + WINDOW_S;
        if a.begins_at >= win_end - HALF_HOUR {
            // a cell starting in the last half hour (or past it) brings its start to the second tick
            f.win_start = floor_half(a.begins_at) - HALF_HOUR;
        } else if a.begins_at < f.win_start {
            // a cell that starts before the window brings its start in (as far back as now)
            f.win_start = floor_half(a.begins_at);
        }
    }
    f.win_start = f.win_start.max(earliest);
    f
}

/// The guide the page wants read for `frame`: the rows shown and two either side, from three hours
/// before the window (LEFT walks back there) but never before the half hour now is in (an airing
/// that ended is nothing to focus), to three hours past its end (RIGHT finds the next airing
/// already there).
pub fn want(view: &LiveTvView, frame: &Frame) -> Want {
    let n = view.channel_count();
    let from_row = frame.row_top.saturating_sub(2).min(n);
    let to_row = (frame.row_top + ROWS_SHOWN + 2).min(n);
    let from = (frame.win_start - 3 * 3_600).max(floor_half(frame.now));
    Want { rows: from_row..to_row, from, to: frame.win_start + WINDOW_S + 3 * 3_600 }
}

// ---- the Recordings list ------------------------------------------------------------------------

/// One row of the Recordings list: an upcoming (or running) recording, or a recording rule.
#[derive(Clone, Debug, PartialEq)]
pub enum RecEntry {
    Grab(Grab),
    Rule(Subscription),
}

/// The list: what will record, soonest first (running ones first of all), then the rules in the
/// server's priority order.
pub fn rec_entries(snap: &Snapshot) -> Vec<RecEntry> {
    let mut grabs: Vec<&Grab> = snap.scheduled.iter().filter(|g| !matches!(g.status, plx_plex::plex::live_tv::GrabStatus::Complete | plx_plex::plex::live_tv::GrabStatus::Cancelled)).collect();
    grabs.sort_by_key(|g| (!g.status.is_recording(), g.airing.begins_at));
    let mut out: Vec<RecEntry> = grabs.into_iter().cloned().map(RecEntry::Grab).collect();
    out.extend(snap.rules.iter().cloned().map(RecEntry::Rule));
    out
}

/// Does a section heading stand above list row `j` (the first recording, the first rule)?
pub fn rec_heads(entries: &[RecEntry], j: usize) -> bool {
    match entries.get(j) {
        Some(RecEntry::Grab(_)) => j == 0,
        Some(RecEntry::Rule(_)) => j == 0 || matches!(entries.get(j - 1), Some(RecEntry::Grab(_))),
        None => false,
    }
}

/// The y of list row `i` with `top` the first row shown.
pub fn rec_y(entries: &[RecEntry], top: usize, i: usize) -> f32 {
    let mut y = geom::RECS_Y;
    for j in top..i {
        if rec_heads(entries, j) {
            y += geom::REC_HEADING_H;
        }
        y += geom::REC_PITCH;
    }
    if rec_heads(entries, i) {
        y += geom::REC_HEADING_H;
    }
    y
}

/// The first row to show so that row `focus` is in view.
pub fn rec_follow(entries: &[RecEntry], top: usize, focus: usize) -> usize {
    let mut top = top.min(focus);
    while top < focus && rec_y(entries, top, focus) + geom::REC_H > geom::RECS_BOTTOM {
        top += 1;
    }
    top
}

// ---- actions ------------------------------------------------------------------------------------

/// What the action panel offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Watch,
    Record,
    RecordSeries,
    StopRecording,
    CancelRecording,
    CancelSeries,
    MoveTop,
    MoveBottom,
    DeleteRule,
    Close,
}

/// What the panel is about.
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    Airing { row: usize, airing: Airing },
    Grab(Grab),
    Rule(Subscription),
}

/// The open panel: its target and its actions, in the order they are offered.
#[derive(Clone, Debug, PartialEq)]
pub struct Panel {
    pub target: Target,
    pub actions: Vec<Action>,
}

/// The recording operation that is this airing, if the server scheduled one.
pub fn grab_for<'a>(snap: &'a Snapshot, a: &Airing) -> Option<&'a Grab> {
    snap.scheduled.iter().find(|g| {
        g.airing.begins_at == a.begins_at
            && ((!a.guid.is_empty() && g.airing.guid == a.guid)
                || (!a.channel_identifier.is_empty() && g.airing.channel_identifier == a.channel_identifier))
    })
}

/// The actions an airing offers now.
pub fn airing_actions(snap: &Snapshot, a: &Airing) -> Vec<Action> {
    let now = snap.now;
    let provider = snap.provider.as_ref();
    let tunable = provider.is_some_and(|p| p.tunable());
    let recordable = provider.is_some_and(|p| p.recordable || p.tunable());
    let grab = grab_for(snap, a);
    let mut out = Vec::new();
    if tunable && now > 0 && a.airs_at(now) {
        out.push(Action::Watch);
    }
    if grab.is_some_and(|g| g.status.is_recording()) {
        out.push(Action::StopRecording);
    }
    if recordable && (now == 0 || a.ends_at > now) {
        let covered = a.is_recording_scheduled() || grab.is_some();
        if !covered {
            out.push(Action::Record);
        }
        if !a.grandparent_guid.is_empty() && a.show_subscription_id.is_empty() {
            out.push(Action::RecordSeries);
        }
        if !a.subscription_id.is_empty() || grab.is_some_and(|g| !g.status.is_recording()) {
            out.push(Action::CancelRecording);
        }
        if !a.show_subscription_id.is_empty() {
            out.push(Action::CancelSeries);
        }
    }
    out.push(Action::Close);
    out
}

pub fn grab_actions(g: &Grab) -> Vec<Action> {
    let mut out = Vec::new();
    if g.status.is_recording() {
        out.push(Action::StopRecording);
    } else if !g.key.is_empty() {
        out.push(Action::CancelRecording);
    }
    out.push(Action::Close);
    out
}

pub fn rule_actions(snap: &Snapshot, s: &Subscription) -> Vec<Action> {
    let mut out = Vec::new();
    if snap.rules.len() > 1 {
        if snap.rules.first().is_some_and(|r| r.id != s.id) {
            out.push(Action::MoveTop);
        }
        if snap.rules.last().is_some_and(|r| r.id != s.id) {
            out.push(Action::MoveBottom);
        }
    }
    out.push(Action::DeleteRule);
    out.push(Action::Close);
    out
}

/// The store write an action on a target asks for (`None`: Watch and Close, which are not writes,
/// or an action that does not fit the target).
pub fn write_for(snap: &Snapshot, target: &Target, action: Action) -> Option<Write> {
    match (target, action) {
        (Target::Airing { airing, .. }, Action::Record) => Some(Write::Record { airing: airing.clone(), kind: RecordKind::This }),
        (Target::Airing { airing, .. }, Action::RecordSeries) => Some(Write::Record { airing: airing.clone(), kind: RecordKind::Series }),
        (Target::Airing { airing, .. }, Action::StopRecording) => {
            grab_for(snap, airing).map(|g| Write::StopGrab { key: g.key.clone() })
        }
        // An episode's own rule goes with its recording; a series' airing skips this one recording.
        (Target::Airing { airing, .. }, Action::CancelRecording) => {
            if !airing.subscription_id.is_empty() {
                Some(Write::CancelRule { id: airing.subscription_id.clone() })
            } else {
                grab_for(snap, airing).map(|g| Write::StopGrab { key: g.key.clone() })
            }
        }
        (Target::Airing { airing, .. }, Action::CancelSeries) => {
            (!airing.show_subscription_id.is_empty()).then(|| Write::CancelRule { id: airing.show_subscription_id.clone() })
        }
        (Target::Grab(g), Action::StopRecording | Action::CancelRecording) => Some(Write::StopGrab { key: g.key.clone() }),
        (Target::Rule(s), Action::DeleteRule) => Some(Write::CancelRule { id: s.id.clone() }),
        (Target::Rule(s), Action::MoveTop) => Some(Write::MoveRule { id: s.id.clone(), after: None }),
        (Target::Rule(s), Action::MoveBottom) => {
            let last = snap.rules.iter().rev().find(|r| r.id != s.id)?;
            Some(Write::MoveRule { id: s.id.clone(), after: Some(last.id.clone()) })
        }
        _ => None,
    }
}
