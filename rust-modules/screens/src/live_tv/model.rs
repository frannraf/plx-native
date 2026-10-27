//! **The Live TV page's model**: its element keys, the guide's geometry, where a direction moves
//! focus, and which actions a focused airing, recording or rule offers. Pure functions over the
//! fed [`LiveTvView`]; the page (`super::page`) draws them and routes what they decide.
//!
//! **A guide cell's key is its channel row and its start minute** (`row << 20 | minute`), the
//! minute counted from a base the page fixes once it knows the time. An airing keeps its key while
//! days land around it and the window scrolls, so focus never jumps when the guide fills in. A row
//! with nothing to show (still loading, or a channel with no guide) has one placeholder key.

use plx_data::live_tv::{Snapshot, Source};
use plx_data::stores::live_tv::Write;
use plx_data::live_tv::RecordKind;
use plx_plex::plex::live_tv::{station_id, Airing, Channel, Grab, GrabStatus, Setting, SettingKind, Subscription};
use plx_ui::consts::{MARGIN_X, SCR_W};
use plx_ui::screen::Dir;
use plx_ui::Rect;

/// The guide's time step: the ruler's ticks and the window's alignment.
pub const HALF_HOUR: i64 = 1_800;
/// How much time the guide shows across.
pub const WINDOW_S: i64 = 2 * 3_600;
/// Channel rows shown at once.
pub const ROWS_SHOWN: usize = 4;
/// Channel rows the guide can key (`row << 20` stays below the reserved keys).
pub const MAX_ROWS: usize = 2_048;

// ---- element keys -------------------------------------------------------------------------------

/// The header's two mode pills.
pub const HEAD_GUIDE: u32 = 0xE000_0000;
pub const HEAD_RECS: u32 = 0xE000_0001;
pub const HEAD_WHATSON: u32 = 0xE000_0002;
/// A What's On card: `WO_BASE + (row << 12 | index)`.
pub const WO_BASE: u32 = 0x9000_0000;
/// The read-out's Try again.
pub const RETRY: u32 = 0xE000_0010;
/// A guide tab (the DVR, Plex Channels, a Plex genre): `TAB_BASE + index`.
pub const TAB_BASE: u32 = 0xC000_0000;
/// A Recordings-list row: `REC_BASE + index`.
pub const REC_BASE: u32 = 0xD000_0000;
/// A guide row's channel (its logo, left of its programs): `CHANNEL_BASE + row`. OK on it opens
/// the channel's panel (watch it, add it to the favorites or take it off).
pub const CHANNEL_BASE: u32 = 0xB000_0000;
/// The recording-options panel: a setting's row (`OPT_BASE + index`), then its two buttons.
pub const OPT_BASE: u32 = 0xA000_0000;
pub const OPT_GO: u32 = 0xA000_FF00;
pub const OPT_CANCEL: u32 = 0xA000_FF01;
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

pub fn channel_elem(row: usize) -> u32 {
    CHANNEL_BASE + row.min(MAX_ROWS - 1) as u32
}

/// The row of a channel's key.
pub fn channel_of(elem: u32) -> Option<usize> {
    (CHANNEL_BASE..CHANNEL_BASE + MAX_ROWS as u32).contains(&elem).then(|| (elem - CHANNEL_BASE) as usize)
}

pub fn wo_elem(row: usize, i: usize) -> u32 {
    WO_BASE + ((row.min(0xFFF) as u32) << 12) + i.min(0xFFF) as u32
}

/// `(row, index)` of a What's On card's key.
pub fn wo_of(elem: u32) -> Option<(usize, usize)> {
    (WO_BASE..WO_BASE + 0x0100_0000).contains(&elem).then(|| ((((elem - WO_BASE) >> 12) & 0xFFF) as usize, ((elem - WO_BASE) & 0xFFF) as usize))
}

/// The setting row of an options-panel key.
pub fn opt_of(elem: u32) -> Option<usize> {
    (OPT_BASE..OPT_BASE + 0x100).contains(&elem).then(|| (elem - OPT_BASE) as usize)
}

/// Is it one of the options panel's keys (a setting's row or a button)?
pub fn is_opt(elem: u32) -> bool {
    opt_of(elem).is_some() || elem == OPT_GO || elem == OPT_CANCEL
}

/// The settings an options panel shows, in its order: the visible ones (a hidden setting is sent
/// back as it is, never shown), the everyday ones first and the advanced after them.
pub fn shown_settings(settings: &[Setting]) -> Vec<&Setting> {
    let visible = settings.iter().filter(|s| !s.hidden);
    visible.clone().filter(|s| !s.advanced).chain(visible.filter(|s| s.advanced)).collect()
}

/// Is a switch's value on (`1`, `true`)?
pub fn switch_on(value: &str) -> bool {
    matches!(value.trim(), "1" | "true" | "True")
}

/// A setting's next value one step `by` (±1) along: an enumerated one's next option (round), a
/// switch flipped (in the spelling the server used), a number up or down (never below 0); a text
/// stays as it is.
pub fn step_value(s: &Setting, value: &str, by: i32) -> String {
    if !s.options.is_empty() {
        let n = s.options.len() as i64;
        let at = s.options.iter().position(|o| o.value == value).map_or(0, |i| i as i64);
        let next = if s.options.iter().any(|o| o.value == value) { (at + i64::from(by)).rem_euclid(n) } else { 0 };
        return s.options[next as usize].value.clone();
    }
    match s.kind {
        SettingKind::Bool => {
            let on = switch_on(value);
            let words = value.trim().eq_ignore_ascii_case("true") || value.trim().eq_ignore_ascii_case("false");
            match (on, words) {
                (true, true) => "false".into(),
                (false, true) => "true".into(),
                (true, false) => "0".into(),
                (false, false) => "1".into(),
            }
        }
        SettingKind::Int => (value.trim().parse::<i64>().unwrap_or(0) + i64::from(by)).max(0).to_string(),
        SettingKind::Text => value.to_string(),
    }
}

/// `settings` with the panel's edits (setting id → value) applied.
pub fn with_edits(settings: &[Setting], edits: &[(String, String)]) -> Vec<Setting> {
    settings
        .iter()
        .map(|s| match edits.iter().find(|(id, _)| *id == s.id) {
            Some((_, v)) => Setting { value: v.clone(), ..s.clone() },
            None => s.clone(),
        })
        .collect()
}

/// The guide row a key stands in: a program's, a placeholder's or a channel's.
pub fn row_of(elem: u32) -> Option<usize> {
    cell_of(elem).map(|(row, _)| row).or_else(|| channel_of(elem))
}

pub fn rec_of(elem: u32) -> Option<usize> {
    (REC_BASE..REC_BASE + 0x0100_0000).contains(&elem).then(|| (elem - REC_BASE) as usize)
}

pub fn tab_of(elem: u32) -> Option<usize> {
    (TAB_BASE..TAB_BASE + 0x100).contains(&elem).then(|| (elem - TAB_BASE) as usize)
}

/// The guide's tabs, in order: the account's favorites (once read, while it has any or they are
/// shown), the server's DVR, then (once Plex's own channels answered) all of them and each of
/// their genres. The page words them (the DVR's name, "Favorites", "Plex Channels").
pub fn tab_sources(snap: &Snapshot) -> Vec<Source> {
    let mut out = Vec::new();
    if snap.favorites_phase.is_ready() && (!snap.favorites.is_empty() || snap.source == Source::Favorites) {
        out.push(Source::Favorites);
    }
    out.push(Source::Dvr);
    if !snap.cloud_tabs.is_empty() {
        out.push(Source::Plex);
        out.extend(snap.cloud_tabs.iter().map(|t| Source::PlexTab(t.identifier.clone())));
    }
    out
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
    /// placeholder of a row with nothing in the window `[from, to)`)?
    pub fn holds(&self, base: i64, elem: u32, from: i64, to: i64) -> bool {
        let Some((row, slot)) = cell_of(elem) else { return false };
        if row >= self.channel_count() {
            return false;
        }
        if slot == PLACEHOLDER {
            return !self.shows_any(row, from, to);
        }
        self.airing(base, elem).is_some()
    }
    /// Does row `row` have a program in the window `[from, to)`?
    pub fn shows_any(&self, row: usize, from: i64, to: i64) -> bool {
        self.row(row).is_some_and(|r| r.airings.iter().any(|a| a.ends_at > from && a.begins_at < to))
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
    /// Rows grown taller, as the stock guide does for the focused one: `(row, per mille)`, the
    /// row focus is on growing to 1000 as the one it left shrinks to 0 (the page's springs). The
    /// rows below move down by what they grew ([`row_y`], [`row_h`]).
    pub grown: [(usize, u16); ROWS_SHOWN],
}

/// How much taller a grown row stands (px) at 1000 per mille.
pub const ROW_EXTRA: f32 = 36.0;

/// What row `row` has grown by, px.
pub fn row_extra(frame: &Frame, row: usize) -> f32 {
    let mille = frame.grown.iter().filter(|(r, g)| *r == row && *g > 0).map(|(_, g)| *g).max().unwrap_or(0);
    ROW_EXTRA * f32::from(mille.min(1000)) / 1000.0
}

/// Row `row`'s height: the row's, plus what it has grown.
pub fn row_h(frame: &Frame, row: usize) -> f32 {
    geom::ROW_H + row_extra(frame, row)
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
    /// The guide's tab row, between the hero and the ruler.
    pub const TABS_Y: f32 = 500.0;
    pub const TABS_H: f32 = 52.0;
    pub const RULER_Y: f32 = 566.0;
    pub const RULER_H: f32 = 34.0;
    pub const ROWS_Y: f32 = 606.0;
    pub const ROW_H: f32 = 92.0;
    pub const ROW_PITCH: f32 = 98.0;
    pub const CHANNEL_X: f32 = MARGIN_X;
    pub const CHANNEL_W: f32 = 170.0;
    pub const GRID_X: f32 = MARGIN_X + CHANNEL_W + 10.0;
    /// The corner a live channel plays on in after BACK: 16:9 at the backdrop band's height, at
    /// the top right, where the art would be.
    pub const MINI_H: f32 = 496.0;
    pub const MINI_W: f32 = 882.0;
    pub const MINI_X: f32 = SCR_W - MINI_W;
    pub const MINI_Y: f32 = 0.0;
    /// A guide cell's program art: portrait, the row's height less a margin.
    pub const POSTER_H: f32 = ROW_H - 16.0;
    pub const POSTER_W: f32 = POSTER_H * 2.0 / 3.0;
    pub const GRID_W: f32 = SCR_W - MARGIN_X - GRID_X;
    pub const CELL_GAP: f32 = 6.0;
    pub const RECS_Y: f32 = 290.0;
    pub const REC_H: f32 = 100.0;
    pub const REC_PITCH: f32 = 110.0;
    pub const REC_HEADING_H: f32 = 56.0;
    /// The note under an Upcoming heading when nothing is scheduled but rules exist.
    pub const REC_NOTE_H: f32 = 64.0;
    /// A Recordings row's poster: portrait, the row's height less a margin.
    pub const REC_POSTER_H: f32 = REC_H - 12.0;
    pub const REC_POSTER_W: f32 = REC_POSTER_H * 2.0 / 3.0;
    pub const RECS_BOTTOM: f32 = 1_020.0;
    pub const PANEL_W: f32 = 640.0;
    /// What's On: where its rows start (below the corner picture when a channel plays on in it),
    /// a row's heading, a card's width and its art (a program's poster, a channel's tile), the
    /// two lines under it, the gaps, and the foot rows end above.
    pub const WO_Y: f32 = 270.0;
    pub const WO_Y_CORNER: f32 = 520.0;
    pub const WO_HEAD_H: f32 = 52.0;
    pub const WO_CARD_W: f32 = 200.0;
    pub const WO_POSTER_H: f32 = 300.0;
    pub const WO_TILE_H: f32 = 200.0;
    pub const WO_TEXT_H: f32 = 76.0;
    pub const WO_GAP: f32 = 24.0;
    pub const WO_ROW_GAP: f32 = 24.0;
    pub const WO_BOTTOM: f32 = 1_060.0;
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
    Some(Rect::new(x0, y, (x1 - x0).max(4.0), row_h(frame, row)))
}

/// The rect of a row's placeholder (the whole window).
pub fn placeholder_rect(frame: &Frame, row: usize) -> Option<Rect> {
    if row < frame.row_top || row >= frame.row_top + ROWS_SHOWN {
        return None;
    }
    Some(Rect::new(geom::GRID_X, row_y(frame, row), geom::GRID_W - geom::CELL_GAP, row_h(frame, row)))
}

/// The top of row `row`: its place in the rows shown, moved down by what the grown rows above it
/// (among those shown) have grown.
pub fn row_y(frame: &Frame, row: usize) -> f32 {
    let mut y = geom::ROWS_Y + (row as f32 - frame.row_top as f32) * geom::ROW_PITCH;
    for r in frame.row_top..row.min(frame.row_top + ROWS_SHOWN) {
        y += row_extra(frame, r);
    }
    y
}

/// The rect a guide key is drawn at, where it would be drawn even when scrolled off (a row above
/// or below, a time left or right of the window), so a move's projection always has a source.
pub fn elem_rect(view: &LiveTvView, frame: &Frame, elem: u32) -> Option<Rect> {
    if let Some(row) = channel_of(elem) {
        return (row < view.channel_count()).then(|| Rect::new(geom::CHANNEL_X, row_y(frame, row), geom::CHANNEL_W, row_h(frame, row)));
    }
    let (row, slot) = cell_of(elem)?;
    let y = row_y(frame, row);
    if slot == PLACEHOLDER {
        return Some(Rect::new(geom::GRID_X, y, geom::GRID_W - geom::CELL_GAP, row_h(frame, row)));
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
    Some(Rect::new(x0, y, (x1 - x0).max(4.0), row_h(frame, row)))
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
            Some(cell_in_window(view, frame, target, anchor_time(view, frame, elem)))
        }
        Dir::Left | Dir::Right => {
            // LEFT past the first program (one on now; the guide does not scroll into the past)
            // is the row's channel
            let channel = Some(channel_elem(row));
            if slot == PLACEHOLDER {
                return if dir == Dir::Left { channel } else { None };
            }
            let airings = &view.row(row)?.airings;
            let i = airings.iter().position(|a| slot_of(frame.base, a.begins_at) == slot)?;
            let next = if dir == Dir::Left {
                let Some(prev) = i.checked_sub(1).and_then(|p| airings.get(p)) else { return channel };
                if prev.ends_at <= floor_half(frame.now) {
                    return channel;
                }
                prev
            } else {
                airings.get(i + 1)?
            };
            Some(cell_elem(row, slot_of(frame.base, next.begins_at)))
        }
    }
}

/// Where a direction moves focus from row `row`'s channel: UP/DOWN to the next row's channel,
/// RIGHT into the row at the program on now (the window's first time). LEFT is the guide's edge.
pub fn channel_neighbour(view: &LiveTvView, frame: &Frame, row: usize, dir: Dir) -> Option<u32> {
    match dir {
        Dir::Up => row.checked_sub(1).map(channel_elem),
        Dir::Down => (row + 1 < view.channel_count()).then(|| channel_elem(row + 1)),
        Dir::Right => Some(cell_in_window(view, frame, row, frame.now.max(frame.win_start))),
        Dir::Left => None,
    }
}

/// The cell an UP/DOWN (or a page) lands on in row `row` at time `t`, among the programs in the
/// window: the one on at `t`, else the last one in the window before it, else the first in it;
/// a row with nothing in the window (off the air until morning) lands on its placeholder, which
/// watches it. Never a program hours away (`cell_at` would find the next one anywhere in the day,
/// and the window followed it there).
pub fn cell_in_window(view: &LiveTvView, frame: &Frame, row: usize, t: i64) -> u32 {
    let (from, to) = (frame.win_start, frame.win_start + WINDOW_S);
    let Some(r) = view.row(row) else { return cell_elem(row, PLACEHOLDER) };
    let shown: Vec<&Airing> = r.airings.iter().filter(|a| a.ends_at > from && a.begins_at < to).collect();
    let pick = shown
        .iter()
        .find(|a| a.begins_at <= t && t < a.ends_at)
        .or_else(|| shown.iter().rev().find(|a| a.begins_at < t))
        .or_else(|| shown.first());
    match pick {
        Some(a) => cell_elem(row, slot_of(frame.base, a.begins_at)),
        None => cell_elem(row, PLACEHOLDER),
    }
}

/// Where a page move (CH▲/CH▼, PageUp/Down) takes focus inside the guide: [`ROWS_SHOWN`] rows up
/// or down, the last row at either end, at the time the focused cell keeps; `None` when already
/// at that end. A row with nothing in the window lands on its placeholder, as a step does.
pub fn guide_page(view: &LiveTvView, frame: &Frame, elem: u32, dir: Dir) -> Option<u32> {
    let row = row_of(elem)?;
    let n = view.channel_count();
    let target = match dir {
        Dir::Up => row.checked_sub(ROWS_SHOWN).unwrap_or(0),
        Dir::Down => (row + ROWS_SHOWN).min(n.checked_sub(1)?),
        Dir::Left | Dir::Right => return None,
    };
    if target == row {
        return None;
    }
    // a channel pages down the channels
    if channel_of(elem).is_some() {
        return Some(channel_elem(target));
    }
    Some(cell_in_window(view, frame, target, anchor_time(view, frame, elem)))
}

/// The frame after focus landed on `elem`: the rows and the window scroll just enough to show it,
/// and the window never starts before the half hour now is in.
pub fn follow(view: &LiveTvView, frame: &Frame, elem: u32) -> Frame {
    let mut f = *frame;
    let Some(row) = row_of(elem) else { return f };
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
        } else if a.begins_at < f.win_start && a.ends_at.min(win_end) - f.win_start <= HALF_HOUR {
            // a cell that starts before the window and shows little of itself brings its start in
            // (as far back as now); one already well in view (a long film met moving up or down)
            // leaves the window where the viewer had it
            f.win_start = floor_half(a.begins_at);
        }
    }
    f.win_start = f.win_start.max(earliest);
    f
}

/// The rows `depth` rows away from the guide's window of [`ROWS_SHOWN`] rows at `top` (of `n`),
/// nearest first and, at each distance, the one past its foot before the one over its head: what
/// the guide fetches ahead of a step or a page (its channel logos).
pub fn rows_around(top: usize, n: usize, depth: std::ops::Range<usize>) -> impl Iterator<Item = usize> {
    depth.flat_map(move |d| {
        let below = top + ROWS_SHOWN + d;
        [(below < n).then_some(below), top.checked_sub(d + 1)].into_iter().flatten()
    })
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

// ---- What's On ----------------------------------------------------------------------------------

/// One row of the What's On page as drawn: its heading, whether its cards are channels (logo
/// tiles) or programs (posters), and its airings.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WoRow {
    pub title: String,
    pub channels: bool,
    pub items: Vec<Airing>,
}

/// **What's On's rows**: the server's hubs in its order; when the server sent no row of
/// channels (its recent channels' hub, measured as `channels`), the ones watched here lead them
/// (`recent_title` names it).
pub fn wo_rows(snap: &Snapshot, recent_title: &str) -> Vec<WoRow> {
    let mut rows: Vec<WoRow> = snap.hubs.iter().map(|h| WoRow { title: h.title.clone(), channels: h.of_channels(), items: h.items.clone() }).collect();
    if !snap.hubs.iter().any(|h| h.of_channels()) && !snap.recent.is_empty() {
        let items = snap
            .recent
            .iter()
            .map(|c| Airing {
                channel_identifier: c.identifier.clone(),
                channel_vcn: c.vcn.clone(),
                channel_title: c.title.clone(),
                channel_thumb: c.thumb.clone(),
                ..Airing::default()
            })
            .collect();
        rows.insert(0, WoRow { title: recent_title.to_string(), channels: true, items });
    }
    rows
}

/// A channel card's name: the station as the guide names it, without the number a hub's channel
/// title leads with ("4.1 KXYZDT (NET)" → "KXYZ (NET)"; the number is the line under it).
pub fn wo_channel_name(a: &Airing) -> std::borrow::Cow<'_, str> {
    let title = a.channel_title.trim();
    let sign = match title.strip_prefix(a.channel_vcn.as_str()) {
        Some(rest) if !a.channel_vcn.is_empty() && rest.starts_with(char::is_whitespace) => rest.trim_start(),
        _ => title,
    };
    // the call sign is the first word; a network after it stays (a title still led by another
    // number is kept whole)
    let numbered = sign.starts_with(|c: char| c.is_ascii_digit());
    let (call, rest) = if numbered { (sign, "") } else { sign.split_once(char::is_whitespace).unwrap_or((sign, "")) };
    let station = station_id(call);
    match (station.is_empty(), rest.trim()) {
        (true, _) => title.into(),
        (false, "") => station.into(),
        (false, rest) => format!("{station} {rest}").into(),
    }
}

/// A row's art height: a channel's square tile, a program's portrait poster.
pub fn wo_art_h(row: &WoRow) -> f32 {
    if row.channels { geom::WO_TILE_H } else { geom::WO_POSTER_H }
}

/// A row's height: its heading, its cards' art, the two lines under them, the gap after.
pub fn wo_row_h(row: &WoRow) -> f32 {
    geom::WO_HEAD_H + wo_art_h(row) + geom::WO_TEXT_H + geom::WO_ROW_GAP
}

/// The y of row `r`'s heading with row `top` the first shown at `y0` (a row above `top` stands
/// above `y0`, so a move's projection always has a source).
pub fn wo_row_y(rows: &[WoRow], top: usize, r: usize, y0: f32) -> f32 {
    if r >= top {
        y0 + rows[top.min(rows.len())..r.min(rows.len())].iter().map(wo_row_h).sum::<f32>()
    } else {
        y0 - rows[r..top.min(rows.len())].iter().map(wo_row_h).sum::<f32>()
    }
}

/// The first row to show so that row `r` stands whole on screen.
pub fn wo_follow(rows: &[WoRow], top: usize, r: usize, y0: f32) -> usize {
    let Some(row) = rows.get(r) else { return top };
    let mut top = top.min(r);
    while top < r && wo_row_y(rows, top, r, y0) + wo_row_h(row) > geom::WO_BOTTOM {
        top += 1;
    }
    top
}

/// Cards a row shows across the screen.
pub fn wo_per_screen() -> usize {
    (((SCR_W - 2.0 * MARGIN_X + geom::WO_GAP) / (geom::WO_CARD_W + geom::WO_GAP)) as usize).max(1)
}

/// The first card a row shows so that card `i` stands whole on screen.
pub fn wo_col_follow(first: usize, i: usize) -> usize {
    let per = wo_per_screen();
    if i < first {
        i
    } else if i >= first + per {
        i + 1 - per
    } else {
        first
    }
}

/// The rect of card `i` of row `r` (its art), the row's first card shown `first`.
pub fn wo_card_rect(rows: &[WoRow], top: usize, first: usize, r: usize, i: usize, y0: f32) -> Option<Rect> {
    let row = rows.get(r)?;
    let x = MARGIN_X + (i as f32 - first as f32) * (geom::WO_CARD_W + geom::WO_GAP);
    Some(Rect::new(x, wo_row_y(rows, top, r, y0) + geom::WO_HEAD_H, geom::WO_CARD_W, wo_art_h(row)))
}

/// Where a direction moves focus on What's On: along a row, or to the next row at the same place
/// on screen (`firsts`: each row's first card shown).
pub fn wo_neighbour(rows: &[WoRow], firsts: &[usize], elem: u32, dir: Dir) -> Option<u32> {
    let (r, i) = wo_of(elem)?;
    let len = rows.get(r)?.items.len();
    let at = |t: usize| {
        let n = rows[t].items.len();
        let on_screen = i.saturating_sub(firsts.get(r).copied().unwrap_or(0));
        let j = (firsts.get(t).copied().unwrap_or(0) + on_screen).min(n.saturating_sub(1));
        (n > 0).then(|| wo_elem(t, j))
    };
    match dir {
        Dir::Left => i.checked_sub(1).map(|j| wo_elem(r, j)),
        Dir::Right => (i + 1 < len).then(|| wo_elem(r, i + 1)),
        Dir::Up => r.checked_sub(1).and_then(at),
        Dir::Down => (r + 1 < rows.len()).then(|| r + 1).and_then(at),
    }
}

// ---- the Recordings list ------------------------------------------------------------------------

/// One row of the Recordings list: an upcoming (or running) recording and the group it is listed
/// under, or a recording rule.
#[derive(Clone, Debug, PartialEq)]
pub enum RecEntry {
    Grab(Grab, RecGroup),
    Rule(Subscription),
}

/// A row's identity across reads of the lists: a recording by its operation key, a rule by its
/// id — what the focus follows when a read moves the row ([`rec_reseat`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecId {
    Grab(String),
    Rule(String),
}

impl RecId {
    /// A recording the server sent without an operation key names nothing: two such rows would
    /// read as one.
    fn is_empty(&self) -> bool {
        match self {
            RecId::Grab(key) | RecId::Rule(key) => key.is_empty(),
        }
    }
}

impl RecEntry {
    pub fn id(&self) -> RecId {
        match self {
            RecEntry::Grab(g, _) => RecId::Grab(g.key.clone()),
            RecEntry::Rule(s) => RecId::Rule(s.id.clone()),
        }
    }

    /// Is this the row `id` names?
    pub fn is(&self, id: &RecId) -> bool {
        match (self, id) {
            (RecEntry::Grab(g, _), RecId::Grab(key)) => &g.key == key,
            (RecEntry::Rule(s), RecId::Rule(rule)) => &s.id == rule,
            _ => false,
        }
    }
}

/// Where the row `id` names stands in `entries`, if it still does.
pub fn rec_index_of(entries: &[RecEntry], id: &RecId) -> Option<usize> {
    entries.iter().position(|e| e.is(id))
}

/// **Where a read of the lists moved the row the focus stands on**: `(from, to)`, the row's
/// element now (`from`, the focused one) and in `entries`, the lists just read, when the row
/// `anchor` names is still listed and its index changed — a recording scheduled above it, one
/// that completed, a rule moved. `None` when focus is not on a row, the row left the list (the
/// index then clamps as before), it stands where it stood, or it has no identity to follow.
pub fn rec_reseat(anchor: Option<&RecId>, from: Option<u32>, entries: &[RecEntry]) -> Option<(u32, u32)> {
    let (from, id) = from.zip(anchor)?;
    rec_of(from)?;
    if id.is_empty() {
        return None;
    }
    let to = REC_BASE + rec_index_of(entries, id)? as u32;
    (to != from).then_some((from, to))
}

/// The heading an upcoming recording is listed under: the ones recording now, then one per local
/// day (its number of days since the epoch, local; one already begun but not yet recording counts
/// as today).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecGroup {
    Now,
    Day(i64),
}

/// The list: what will record, soonest first (running ones first of all) under a heading per day,
/// then the rules in the server's priority order.
pub fn rec_entries(snap: &Snapshot) -> Vec<RecEntry> {
    let mut grabs: Vec<&Grab> = snap.scheduled.iter().filter(|g| !matches!(g.status, GrabStatus::Complete | GrabStatus::Cancelled)).collect();
    grabs.sort_by_key(|g| (!g.status.is_recording(), g.airing.begins_at));
    let today = local_day(snap.now, snap.utc_offset);
    let mut out: Vec<RecEntry> = grabs
        .into_iter()
        .map(|g| {
            let group = if g.status.is_recording() { RecGroup::Now } else { RecGroup::Day(local_day(g.airing.begins_at, snap.utc_offset).max(today)) };
            RecEntry::Grab(g.clone(), group)
        })
        .collect();
    out.extend(snap.rules.iter().cloned().map(RecEntry::Rule));
    out
}

/// The local date of instant `t`, as days since the epoch.
pub fn local_day(t: i64, utc_offset: i64) -> i64 {
    (t + utc_offset).div_euclid(86_400)
}

/// Does a section heading stand above list row `j` (where the group of recordings changes, the
/// first rule)?
pub fn rec_heads(entries: &[RecEntry], j: usize) -> bool {
    match entries.get(j) {
        Some(RecEntry::Grab(_, group)) => j == 0 || !matches!(entries.get(j - 1), Some(RecEntry::Grab(_, prev)) if prev == group),
        Some(RecEntry::Rule(_)) => j == 0 || matches!(entries.get(j - 1), Some(RecEntry::Grab(..))),
        None => false,
    }
}

/// Rules but nothing scheduled: the list opens with an Upcoming heading and a note saying so (and
/// how to schedule something), above the rules.
pub fn rec_note(entries: &[RecEntry]) -> bool {
    !entries.is_empty() && !entries.iter().any(|e| matches!(e, RecEntry::Grab(..)))
}

/// The y of list row `i` with `top` the first row shown.
pub fn rec_y(entries: &[RecEntry], top: usize, i: usize) -> f32 {
    let mut y = geom::RECS_Y;
    if top == 0 && rec_note(entries) {
        y += geom::REC_HEADING_H + geom::REC_NOTE_H;
    }
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

/// The first row to show after the list changed: the focused row (or the last, if the list got
/// shorter than it) in view, and no blank room left at the bottom that rows above could fill (a
/// deleted rule takes its recordings with it: the list left one row showing, or none).
pub fn rec_settle(entries: &[RecEntry], top: usize, focus: usize) -> usize {
    let Some(last) = entries.len().checked_sub(1) else { return 0 };
    let focus = focus.min(last);
    let mut top = rec_follow(entries, top.min(focus), focus);
    while top > 0 && rec_y(entries, top - 1, last) + geom::REC_H <= geom::RECS_BOTTOM {
        top -= 1;
    }
    top
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
    Favorite,
    Unfavorite,
    /// A new recording that conflicts: raise its rule to the top (it wins the tuners)…
    Prefer,
    /// …or delete it.
    CancelNew,
    /// A rule's recording options.
    EditOptions,
    Close,
}

/// What the panel is about.
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    Airing { row: usize, airing: Airing },
    Grab(Grab),
    Rule(Subscription),
    /// A guide row's channel: watch it, add it to the favorites or take it off.
    Channel { row: usize, channel: Channel },
    /// A recording just made that collides with others (`conflicts`, their titles) for the
    /// tuners: prefer it, cancel it, or leave it below them.
    Conflict { rule: String, title: String, conflicts: Vec<String> },
}

/// The open panel: its target and its actions, in the order they are offered.
#[derive(Clone, Debug, PartialEq)]
pub struct Panel {
    pub target: Target,
    pub actions: Vec<Action>,
}

/// The recording operation that is this airing, if the server scheduled one.
pub fn grab_for<'a>(snap: &'a Snapshot, a: &Airing) -> Option<&'a Grab> {
    // A finished or cancelled operation the server still lists covers nothing (as in the list).
    snap.scheduled.iter().filter(|g| !matches!(g.status, GrabStatus::Complete | GrabStatus::Cancelled)).find(|g| {
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

/// The actions a channel offers: watch it (when it can be), add it to the favorites or take it off
/// (when the account's list can say so).
pub fn channel_actions(snap: &Snapshot, ch: &Channel) -> Vec<Action> {
    let mut out = Vec::new();
    if snap.can_watch(ch) {
        out.push(Action::Watch);
    }
    if snap.can_favorite(ch) {
        out.push(if snap.is_favorite(ch) { Action::Unfavorite } else { Action::Favorite });
    }
    out.push(Action::Close);
    out
}

/// A recording the server marks failed before its airing began: it lost its tuner to others of
/// higher priority (a conflict), and reads so rather than as a recording that failed.
pub fn grab_conflicted(g: &Grab, now: i64) -> bool {
    g.status == GrabStatus::Error && now > 0 && g.airing.begins_at > now
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
    out.push(Action::EditOptions);
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
        (Target::Grab(g), Action::StopRecording) => Some(Write::StopGrab { key: g.key.clone() }),
        // A one-off's own rule goes with its recording, as from the guide (cancelling only the
        // recording left a "Single" rule with nothing to record in the priority list, device
        // 2026-10-27); a series' recording skips this one airing.
        (Target::Grab(g), Action::CancelRecording) => match snap.rules.iter().find(|r| !g.subscription_id.is_empty() && r.id == g.subscription_id) {
            Some(rule) if rule.kind != 2 => Some(Write::CancelRule { id: rule.id.clone() }),
            _ => Some(Write::StopGrab { key: g.key.clone() }),
        },
        (Target::Rule(s), Action::DeleteRule) => Some(Write::CancelRule { id: s.id.clone() }),
        (Target::Rule(s), Action::MoveTop) => Some(Write::MoveRule { id: s.id.clone(), after: None }),
        // a rule moved to the top of the priority list wins a tuner conflict (`docs/pms-api.md` §10.4)
        (Target::Conflict { rule, .. }, Action::Prefer) => Some(Write::MoveRule { id: rule.clone(), after: None }),
        (Target::Conflict { rule, .. }, Action::CancelNew) => Some(Write::CancelRule { id: rule.clone() }),
        (Target::Rule(s), Action::MoveBottom) => {
            let last = snap.rules.iter().rev().find(|r| r.id != s.id)?;
            Some(Write::MoveRule { id: s.id.clone(), after: Some(last.id.clone()) })
        }
        _ => None,
    }
}
