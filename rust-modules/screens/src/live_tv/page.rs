//! **The Live TV page** (`AppArg::LiveTv`): a peer of Home on the tab strip, shown when the server
//! has a DVR. Two modes under one header: the **Guide** (channels down, time across, the airing on
//! now first) and **Recordings** (what will record, then the recording rules in priority order).
//!
//! OK on an airing that is on now watches it (recording what is playing is the player's offer);
//! OK on a later one opens the action panel: Record, Record Series, Cancel Recording, Cancel Series
//! Recording (and Watch for a channel with no tuner behind it never shows). On a recording: Stop or
//! Cancel; on a rule: Move to Top / Bottom, Delete. The page names no store: everything it asks for leaves as a
//! [`LiveTvOut`] the application routes (`app::content::live_tv_requests`), and everything it
//! draws arrives as a [`LiveTvView`] the application feeds it (`Bridge::live_tv_feed`), with the
//! guide window it wants read ([`LiveTvScreen::want`]).
//!
//! Watching is the application's: it tunes through the store and, when the stream lands, starts
//! the player over this page. While the tune is on its way the page says so; BACK cancels it.

use std::borrow::Cow;
use std::ffi::CString;

use plx_data::live_tv::{Availability, Phase, Source, TunePhase, WriteOp};
use plx_data::stores::live_tv::Write;
use plx_machine::machine::{
    By, Canon, Cx, Delivery, Edge, Effects, Enter, EntryId, FocusKey, FocusTarget, Fx, GroupId, Handled, InputEvent,
    InputKind, Key, LogicalState, Machine,
};
use plx_machine::present::Provenance;
use plx_platform::i18n::msg;
use plx_plex::plex::live_tv::{station_id, Airing, Channel, GrabStatus};
use plx_ui::consts::{MARGIN_X, SCR_H, SCR_W};
use plx_ui::label::{HAlign, Label, VAlign};
use plx_ui::screen::{
    Activate, At, AxisMask, Dir, DrawFrame, EdgeRule, ElemKind, FocusSource, Focusable, GroupKind, GroupSpec,
    HitSource, Hover, Link, Placed, RenderStrategy, Screen, ScreenEvent, Seat, Step, Stop,
};
use plx_ui::text_view::TextView;
use plx_ui::widgets::{resolve_tex_wh_on, AmbientWash, Button, ControlGround};
use plx_ui::{theme, Crop, Env, Painter, Rect, View};

use super::model::{self, geom, Action, Frame, LiveTvView, Panel, RecEntry, RecGroup, Target, Want};
use crate::registry::{AppFx, ContentLike, HomeTab};

/// The page's content: the guide's cells, or the Recordings list's rows (one group, by mode).
pub const CONTENT: GroupId = GroupId(0);
pub const HEAD: GroupId = GroupId(1);
pub const PANEL: GroupId = GroupId(2);
pub const RETRY_GROUP: GroupId = GroupId(3);
pub const TABS: GroupId = GroupId(4);

/// How long a notice stays up, ms of the tick clock.
const NOTICE_MS: u32 = 5_000;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Guide,
    Recordings,
}

/// What the page asks of the application.
#[derive(Clone, Debug, PartialEq)]
pub enum LiveTvOut {
    /// A press on the shared strip's other tabs.
    Tab(HomeTab),
    /// A press on the profile chip.
    Account,
    /// BACK with nothing left to close.
    Back,
    /// Watch channel row `row` (the airing the page showed as on now, if any).
    Watch { row: usize, airing: Option<Airing> },
    /// BACK while a tune is on its way.
    CancelTune,
    /// The page showed the tune's failure.
    DismissTuneFailure,
    /// OK on a guide tab: show that lineup.
    Source(Source),
    /// A DVR change.
    Write(Write),
    /// Try again on the failure read-out.
    Retry,
    /// The Recordings mode opened: read the lists again.
    RefreshDvr,
}

/// What the page is showing instead of content, if anything.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Readout {
    Loading,
    Unavailable,
    Failed,
    NoChannels,
    /// A Plex Channels category with no channels in it.
    NoTabChannels,
    RecsLoading,
    RecsFailed,
    RecsEmpty,
}

pub struct LiveTvScreen {
    entry: EntryId,
    view: LiveTvView,
    fed: bool,
    mode: Mode,
    frame: Frame,
    rec_top: usize,
    recs: Vec<RecEntry>,
    panel: Option<Panel>,
    /// The element the panel was opened from (focus goes back there when it closes).
    panel_from: u32,
    /// The notice and the tick time it goes at (`None` until the next tick stamps it).
    notice: Option<(String, Option<u32>)>,
    notice_serial: Option<u32>,
    tune_failure_shown: bool,
    own_enters: u32,
    /// The arrival still waits for the guide to land (no key pressed since).
    seat_pending: bool,
    focus: Option<u32>,
    /// The guide cell focus was last on: the details band keeps describing it while the panel
    /// (whose actions hold the focus) is about it.
    last_cell: Option<u32>,
    /// The page's ground, keyed to the hero airing's art and dissolving between programs.
    wash: AmbientWash,
    /// The tick clock at the last tick (the wash's step).
    last_tick: Option<u32>,
    /// Each guide tab's focus pill, 0 (a label) to 1 (the white pill): the one focus moves to
    /// grows as the one it left shrinks.
    tab_grow: Vec<plx_ui::Spring>,
    /// How far the tab row has scrolled left to keep the focused tab on screen.
    tab_scroll: plx_ui::Spring,
    /// Guide cells on the move, 0 (at rest) to 1 (grown): the row focus arrives at grows taller as
    /// the one it left shrinks back (the frame's `grown`, as the stock guide does). A cell at rest
    /// leaves the list.
    cell_grow: Vec<(u32, plx_ui::Spring)>,
}

impl LiveTvScreen {
    /// The heartbeat word, byte-identical to the route word (`app/words.rs`).
    pub const WORD: &'static str = "livetv";

    pub fn loading(entry: EntryId) -> Self {
        Self {
            entry,
            view: LiveTvView::default(),
            fed: false,
            mode: Mode::Guide,
            frame: Frame::default(),
            rec_top: 0,
            recs: Vec::new(),
            panel: None,
            panel_from: model::HEAD_GUIDE,
            notice: None,
            notice_serial: None,
            tune_failure_shown: false,
            own_enters: 0,
            seat_pending: false,
            focus: None,
            last_cell: None,
            wash: {
                let mut w = AmbientWash::flat(GROUND);
                w.jump(PLAIN_WASH);
                w
            },
            last_tick: None,
            tab_grow: Vec::new(),
            tab_scroll: plx_ui::Spring::at(0.0),
            cell_grow: Vec::new(),
        }
    }

    /// The frame's grown rows from the cells on the move: the most grown, as many as rows are
    /// shown (a key held down has three on the move at once), a row once (focus moving along a
    /// row keeps it tall).
    fn grown_rows(&self) -> [(usize, u16); model::ROWS_SHOWN] {
        let mut rows: Vec<(usize, u16)> = Vec::new();
        for (e, g) in &self.cell_grow {
            let Some((row, _)) = model::cell_of(*e) else { continue };
            let mille = (g.pos.clamp(0.0, 1.0) * 1000.0) as u16;
            match rows.iter_mut().find(|(r, _)| *r == row) {
                Some((_, m)) => *m = (*m).max(mille),
                None => rows.push((row, mille)),
            }
        }
        rows.sort_by(|a, b| b.1.cmp(&a.1));
        let mut out = [(0, 0); model::ROWS_SHOWN];
        for (slot, r) in rows.iter().take(model::ROWS_SHOWN).enumerate() {
            out[slot] = *r;
        }
        out
    }

    // ---- what the application reads and feeds ---------------------------------------------------

    /// The application hands the page what the store holds (when its generation or the wanted
    /// window moved).
    pub fn input(&mut self, view: LiveTvView) {
        let now = view.snap.now;
        if now > 0 {
            if self.frame.base == 0 {
                // Keys count minutes from two days before the first time the page knew: an airing
                // on screen is never before it, and 2^20 minutes is two years after it.
                self.frame.base = now - now.rem_euclid(86_400) - 2 * 86_400;
                self.frame.win_start = model::floor_half(now);
            }
            self.frame.now = now;
            self.frame.win_start = self.frame.win_start.max(model::floor_half(now));
        }
        // A notice the store kept from before the page mounted is not news.
        let serial = view.snap.notice.as_ref().map(|n| n.serial);
        if !self.fed {
            self.notice_serial = serial;
        } else if serial.is_some() && serial != self.notice_serial {
            self.notice_serial = serial;
            if let Some(n) = &view.snap.notice {
                self.notice = Some((notice_text(n), None));
            }
        }
        if !matches!(view.snap.tune, TunePhase::Failed { .. }) {
            self.tune_failure_shown = false;
        }
        self.fed = true;
        self.recs = model::rec_entries(&view.snap);
        self.rec_top = self.rec_top.min(self.recs.len().saturating_sub(1));
        if view.snap.source != self.view.snap.source {
            // another tab's lineup: its own rows, from the top
            self.frame.row_top = 0;
            self.last_cell = None;
        }
        let n = view.channel_count();
        self.frame.row_top = self.frame.row_top.min(n.saturating_sub(1));
        self.view = view;
        // What landed can move the focused element (a heading added above a Recordings row, an
        // airing filled in): keep it in view.
        let focus = self.focus.and_then(model::rec_of).unwrap_or(self.rec_top);
        self.rec_top = model::rec_settle(&self.recs, self.rec_top, focus);
        plx_machine::idle::invalidate();
    }

    /// The guide the page wants read now (`None` until it knows the time and the lineup).
    pub fn want(&self) -> Option<Want> {
        (self.mode == Mode::Guide && self.frame.base > 0 && self.view.channel_count() > 0)
            .then(|| model::want(&self.view, &self.frame))
    }

    /// The art the hero shows, for the application to ask its UltraBlur colours.
    pub fn hero_art(&self) -> Option<String> {
        (self.mode == Mode::Guide).then(|| self.hero().map(|(_, a)| a.art.clone())).flatten().filter(|a| !a.is_empty())
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn panel(&self) -> Option<&Panel> {
        self.panel.as_ref()
    }

    pub fn frame(&self) -> Frame {
        self.frame
    }

    pub fn notice(&self) -> Option<&str> {
        self.notice.as_ref().map(|(t, _)| t.as_str())
    }

    // ---- decisions --------------------------------------------------------------------------------

    fn readout(&self) -> Option<Readout> {
        let s = &self.view.snap;
        if let Phase::Failed(_) = s.providers_phase {
            return Some(Readout::Failed);
        }
        match s.availability {
            Availability::Unknown => return Some(Readout::Loading),
            Availability::None => return Some(Readout::Unavailable),
            Availability::Available => {}
        }
        // The Recordings are the server's, whatever guide tab is selected (its lineup may be
        // loading, failed or empty, and the tab row is not shown here to change it).
        if self.mode == Mode::Recordings {
            if self.recs.is_empty() {
                return Some(match &s.dvr_phase {
                    Phase::Failed(_) => Readout::RecsFailed,
                    Phase::Ready => Readout::RecsEmpty,
                    _ => Readout::RecsLoading,
                });
            }
            return None;
        }
        match &s.channels_phase {
            Phase::Failed(_) => return Some(Readout::Failed),
            Phase::Idle | Phase::Loading => return Some(Readout::Loading),
            Phase::Ready if s.channels.is_empty() && s.source.is_cloud() => return Some(Readout::NoTabChannels),
            Phase::Ready if s.channels.is_empty() => return Some(Readout::NoChannels),
            Phase::Ready => {}
        }
        if self.mode == Mode::Guide && self.frame.base == 0 {
            return Some(Readout::Loading);
        }
        None
    }

    fn shows_retry(&self) -> bool {
        matches!(self.readout(), Some(Readout::Failed | Readout::RecsFailed))
    }

    fn has_content(&self) -> bool {
        self.readout().is_none()
    }

    fn mode_pill(&self) -> u32 {
        match self.mode {
            Mode::Guide => model::HEAD_GUIDE,
            Mode::Recordings => model::HEAD_RECS,
        }
    }

    fn key(&self, elem: u32) -> FocusKey<u32> {
        FocusKey { entry: self.entry, elem }
    }

    /// Ask the engine to seat `focus` as the page's own seat (the `Enter` that comes back is
    /// counted, so it is not mistaken for an arrival).
    fn seat_own<H: ContentLike>(&mut self, focus: FocusTarget<u32>, fx: &mut Effects<'_, H>) {
        self.own_enters = self.own_enters.saturating_add(1);
        fx.push(Fx::Deliver(fx.from(), Delivery::Screen(ScreenEvent::Enter(Enter::Fresh { focus }))));
    }

    /// Move focus a page (`whole`) or a row down the guide or the Recordings list, from the focused
    /// row; nothing while a panel is up or focus is elsewhere.
    fn page<H: ContentLike>(&mut self, dir: Dir, whole: bool, cx: &Cx<'_, H>, fx: &mut Effects<'_, H>) {
        if self.panel.is_some() {
            return;
        }
        let Some(e) = cx.focus.current.filter(|k| k.entry == self.entry).map(|k| k.elem) else { return };
        let to = if model::cell_of(e).is_some() {
            if whole {
                model::guide_page(&self.view, &self.frame, e, dir)
            } else {
                model::guide_neighbour(&self.view, &self.frame, e, dir)
            }
        } else if let Some(i) = model::rec_of(e) {
            let by = if whole { 6 } else { 1 };
            let n = self.recs.len();
            let target = match dir {
                Dir::Up => i.saturating_sub(by),
                _ => (i + by).min(n.saturating_sub(1)),
            };
            (target != i && n > 0).then(|| model::REC_BASE + target as u32)
        } else {
            None
        };
        if let Some(to) = to {
            self.seat_elem(to, fx);
            fx.invalidate(Provenance::Input);
        }
    }

    fn seat_elem<H: ContentLike>(&mut self, elem: u32, fx: &mut Effects<'_, H>) {
        let key = self.key(elem);
        self.seat_own(FocusTarget::Elem(key), fx);
    }

    /// The cell on now in the first row shown.
    fn now_cell(&self) -> u32 {
        let t = self.frame.now.max(self.frame.win_start);
        self.view.cell_at(self.frame.base, self.frame.row_top, t)
    }

    fn route<H: ContentLike>(&mut self, out: LiveTvOut, fx: &mut Effects<'_, H>) {
        fx.push(Fx::App(AppFx::LiveTv(out)));
        fx.invalidate(Provenance::Input);
    }

    fn set_mode<H: ContentLike>(&mut self, mode: Mode, fx: &mut Effects<'_, H>) {
        if self.mode == mode {
            return;
        }
        self.mode = mode;
        if mode == Mode::Recordings {
            self.rec_top = 0;
            self.route(LiveTvOut::RefreshDvr, fx);
        }
        fx.invalidate(Provenance::Input);
    }

    fn open_panel<H: ContentLike>(&mut self, from: u32, target: Target, fx: &mut Effects<'_, H>) {
        let actions = match &target {
            Target::Airing { airing, .. } => model::airing_actions(&self.view.snap, airing),
            Target::Grab(g) => model::grab_actions(g),
            Target::Rule(s) => model::rule_actions(&self.view.snap, s),
        };
        self.panel = Some(Panel { target, actions });
        self.panel_from = from;
        self.seat_elem(model::ACTION_BASE, fx);
        fx.invalidate(Provenance::Input);
    }

    fn close_panel<H: ContentLike>(&mut self, fx: &mut Effects<'_, H>) {
        if self.panel.take().is_some() {
            let back = self.panel_from;
            self.seat_elem(back, fx);
            fx.invalidate(Provenance::Input);
        }
    }

    fn act<H: ContentLike>(&mut self, i: usize, fx: &mut Effects<'_, H>) {
        let Some(panel) = self.panel.clone() else { return };
        let Some(action) = panel.actions.get(i).copied() else { return };
        self.close_panel(fx);
        match (action, &panel.target) {
            (Action::Close, _) => {}
            (Action::Watch, Target::Airing { row, airing }) => {
                self.route(LiveTvOut::Watch { row: *row, airing: Some(airing.clone()) }, fx)
            }
            (action, target) => {
                if let Some(w) = model::write_for(&self.view.snap, target, action) {
                    self.route(LiveTvOut::Write(w), fx);
                }
            }
        }
    }

    /// A non-strip element activated (OK, or a pointer press).
    fn activate<H: ContentLike>(&mut self, elem: u32, fx: &mut Effects<'_, H>) -> Handled {
        if self.panel.is_some() && model::action_of(elem).is_none() {
            return Handled::Yes;
        }
        if elem >= plx_ui::dispatch::STRIP_BASE {
            return match Self::strip_press(elem) {
                Some(Some(out)) => {
                    self.route(out, fx);
                    Handled::Yes
                }
                Some(None) => Handled::Yes,
                None => Handled::No,
            };
        }
        if let Some(i) = model::action_of(elem) {
            self.act(i, fx);
            return Handled::Yes;
        }
        if self.panel.is_some() {
            return Handled::Yes;
        }
        if let Some(i) = model::tab_of(elem) {
            if let Some(source) = model::tab_sources(&self.view.snap).get(i) {
                if *source != self.view.snap.source {
                    self.route(LiveTvOut::Source(source.clone()), fx);
                }
            }
            return Handled::Yes;
        }
        match elem {
            model::HEAD_GUIDE => self.set_mode(Mode::Guide, fx),
            model::HEAD_RECS => self.set_mode(Mode::Recordings, fx),
            model::RETRY => self.route(LiveTvOut::Retry, fx),
            _ => {
                if let Some((row, slot)) = model::cell_of(elem) {
                    if slot == model::PLACEHOLDER {
                        // a row with no guide: OK watches the channel itself
                        if self.view.snap.can_watch() {
                            self.route(LiveTvOut::Watch { row, airing: None }, fx);
                        }
                    } else if let Some((_, airing)) = self.view.airing(self.frame.base, elem) {
                        // A show on now plays at once (recording it is the player's to offer); a
                        // later one opens its record choices.
                        if self.view.snap.can_watch() && self.frame.now > 0 && airing.airs_at(self.frame.now) {
                            let airing = airing.clone();
                            self.route(LiveTvOut::Watch { row, airing: Some(airing) }, fx);
                        } else if self.view.snap.can_watch() && self.frame.now > 0 && airing.ends_at <= self.frame.now {
                            // one that already ended (still drawn at the window's left): nothing to
                            // record, so OK watches its channel, on now
                            self.route(LiveTvOut::Watch { row, airing: None }, fx);
                        } else if self.view.snap.source.is_cloud() {
                            // a Plex channel's later program: nothing to record it with
                        } else {
                            let target = Target::Airing { row, airing: airing.clone() };
                            self.open_panel(elem, target, fx);
                        }
                    }
                } else if let Some(i) = model::rec_of(elem) {
                    let target = match self.recs.get(i) {
                        Some(RecEntry::Grab(g, _)) => Target::Grab(g.clone()),
                        Some(RecEntry::Rule(s)) => Target::Rule(s.clone()),
                        None => return Handled::No,
                    };
                    self.open_panel(elem, target, fx);
                } else {
                    return Handled::No;
                }
            }
        }
        Handled::Yes
    }

    /// A press on the shared strip: the other tabs and the chip are requests; the Live TV pill is
    /// the page the user is already on.
    fn strip_press(elem: u32) -> Option<Option<LiveTvOut>> {
        use plx_ui::widgets::{
            TAB_KEY_CHIP, TAB_KEY_HOME, TAB_KEY_LIVE_TV, TAB_KEY_MOVIES, TAB_KEY_SEARCH, TAB_KEY_SHOWS, TAB_KEY_WATCHLIST,
        };
        match elem {
            TAB_KEY_HOME => Some(Some(LiveTvOut::Tab(HomeTab::Home))),
            TAB_KEY_MOVIES => Some(Some(LiveTvOut::Tab(HomeTab::Movies))),
            TAB_KEY_SHOWS => Some(Some(LiveTvOut::Tab(HomeTab::Shows))),
            TAB_KEY_WATCHLIST => Some(Some(LiveTvOut::Tab(HomeTab::Watchlist))),
            TAB_KEY_SEARCH => Some(Some(LiveTvOut::Tab(HomeTab::Search))),
            TAB_KEY_CHIP => Some(Some(LiveTvOut::Account)),
            TAB_KEY_LIVE_TV => Some(None),
            _ => None,
        }
    }

    /// BACK: the panel closes, a tune is cancelled, the content gives way to the header, the
    /// header to the strip, and from the strip the page leaves.
    fn back<H: ContentLike>(&mut self, cx: &Cx<'_, H>, fx: &mut Effects<'_, H>) -> Handled {
        if self.panel.is_some() {
            self.close_panel(fx);
            return Handled::Yes;
        }
        if matches!(self.view.snap.tune, TunePhase::Tuning { .. }) {
            self.route(LiveTvOut::CancelTune, fx);
            return Handled::Yes;
        }
        let at = cx.focus.current.filter(|k| k.entry == self.entry).map(|k| k.elem);
        match at {
            Some(e) if e >= plx_ui::dispatch::STRIP_BASE => self.route(LiveTvOut::Back, fx),
            Some(e) if model::cell_of(e).is_some() || model::rec_of(e).is_some() => {
                let pill = self.mode_pill();
                self.seat_elem(pill, fx);
            }
            Some(_) => self.seat_own(FocusTarget::ContainerGroup(plx_ui::containers::tabs::STRIP), fx),
            None => self.route(LiveTvOut::Back, fx),
        }
        Handled::Yes
    }

    /// Focus landed on `elem`: scroll it into view.
    /// Focus moved to `elem`: the hero follows it, and (`scroll`) the guide's window or the
    /// Recordings list moves to keep it in view. A pointer's focus never scrolls: what it points at
    /// is on screen already, and moving the guide under a resting Magic Remote put a new program
    /// under it, which scrolled again (seen in review: the window ran away an hour a time).
    fn follow(&mut self, elem: u32, scroll: bool) {
        if self.focus == Some(elem) {
            return;
        }
        self.focus = Some(elem);
        if model::cell_of(elem).is_some() {
            self.last_cell = Some(elem);
            if !self.cell_grow.iter().any(|(e, _)| *e == elem) {
                self.cell_grow.push((elem, plx_ui::Spring::at(0.0)));
            }
        }
        if !scroll {
            plx_machine::idle::invalidate();
            return;
        }
        if model::cell_of(elem).is_some() {
            self.frame = model::follow(&self.view, &self.frame, elem);
        } else if let Some(i) = model::rec_of(elem) {
            self.rec_top = model::rec_follow(&self.recs, self.rec_top, i);
        }
        plx_machine::idle::invalidate();
    }

    fn tick<H: ContentLike>(&mut self, ms: u32, cx: &Cx<'_, H>, fx: &mut Effects<'_, H>) {
        if let Some(elem) = cx.focus.current.filter(|k| k.entry == self.entry).map(|k| k.elem) {
            self.follow(elem, true);
        }
        let dt = self.last_tick.map_or(0.0, |t| ms.wrapping_sub(t).min(100) as f32 / 1_000.0);
        self.last_tick = Some(ms);
        let target = self.wash_target();
        // decorative: the wash dissolving between programs moves no layout
        plx_machine::idle::decorative(|| self.wash.step(target, AmbientWash::K, dt));
        let tabs = model::tab_sources(&self.view.snap).len();
        self.tab_grow.resize(tabs, plx_ui::Spring::at(0.0));
        let focused_tab = self.focus.and_then(model::tab_of);
        for (i, g) in self.tab_grow.iter_mut().enumerate() {
            let to = if focused_tab == Some(i) { 1.0 } else { 0.0 };
            if !g.land(to, dt) {
                g.step(to, TAB_K, dt);
            }
        }
        let scroll = self.tab_scroll_target(cx.measure);
        if !self.tab_scroll.land(scroll, dt) {
            self.tab_scroll.step(scroll, TAB_K, dt);
        }
        // the focused cell grows, every other one on the list shrinks back and leaves it at rest
        let (focus, panel_up) = (self.focus, self.panel.is_some());
        for (e, g) in &mut self.cell_grow {
            let to = if focus == Some(*e) && !panel_up { 1.0 } else { 0.0 };
            if !g.land(to, dt) {
                g.step(to, CELL_K, dt);
            }
        }
        self.cell_grow.retain(|(e, g)| focus == Some(*e) || g.pos > 0.002 || g.vel.abs() > 0.002);
        self.frame.grown = self.grown_rows();
        if let Some((_, until)) = &mut self.notice {
            let at = *until.get_or_insert(ms.wrapping_add(NOTICE_MS));
            // the tick clock wraps: gone once `ms` passes `at`
            if ms.wrapping_sub(at) as i32 >= 0 {
                self.notice = None;
            }
            plx_machine::idle::invalidate();
        }
        if let TunePhase::Failed { channel, reason } = &self.view.snap.tune {
            if !self.tune_failure_shown {
                self.tune_failure_shown = true;
                self.notice = Some((msg::live_tv_tune_failed(channel, reason), None));
                self.route(LiveTvOut::DismissTuneFailure, fx);
            }
        }
        // The arrival follows the guide in when it lands, unless the reader moved first.
        if self.seat_pending && self.has_content() && self.own_enters == 0 {
            self.seat_pending = false;
            let elem = match self.mode {
                Mode::Guide => self.now_cell(),
                Mode::Recordings => model::REC_BASE,
            };
            self.seat_elem(elem, fx);
        }
    }

    /// Is the tab row up? In the Guide, once Plex's own channels answered (with only the DVR
    /// there is nothing to switch to).
    fn tabs_shown(&self) -> bool {
        self.mode == Mode::Guide
            && self.view.snap.availability == Availability::Available
            && !self.view.snap.cloud_tabs.is_empty()
            && self.panel.is_none()
    }

    /// Tab `i`'s words.
    fn tab_label(&self, source: &Source) -> String {
        match source {
            Source::Dvr => self
                .view
                .snap
                .provider
                .as_ref()
                .map(|p| if p.server_name.is_empty() { p.title.clone() } else { p.server_name.clone() })
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| msg::live_tv_tab().to_string()),
            Source::Plex => msg::live_tv_tab_plex_channels().to_string(),
            Source::PlexTab(id) => self.view.snap.cloud_tabs.iter().find(|t| &t.identifier == id).map(|t| t.title.clone()).unwrap_or_default(),
        }
    }

    /// The tabs' rects at rest (unscrolled): each its words plus padding, a wider gap after
    /// Plex Channels (the genres that follow are its own).
    fn tab_rects(&self, measure: &dyn plx_machine::machine::Measure) -> Vec<Rect> {
        let mut x = MARGIN_X;
        model::tab_sources(&self.view.snap)
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let w = measure.width_str(&self.tab_label(s), theme::size::BODY, true) + 2.0 * TAB_PAD;
                let r = Rect::new(x, geom::TABS_Y, w, geom::TABS_H);
                x += w + if i == 1 { TAB_SPLIT } else { TAB_GAP };
                r
            })
            .collect()
    }

    /// How far the row scrolls so the focused tab stands inside the screen's margins.
    fn tab_scroll_target(&self, measure: &dyn plx_machine::machine::Measure) -> f32 {
        let Some(i) = self.focus.and_then(model::tab_of) else { return self.tab_scroll.pos };
        let rects = self.tab_rects(measure);
        let Some(r) = rects.get(i) else { return 0.0 };
        let right = SCR_W - MARGIN_X;
        let mut s = self.tab_scroll.pos;
        if r.x + r.w - s > right {
            s = r.x + r.w - right;
        }
        if r.x - s < MARGIN_X {
            s = r.x - MARGIN_X;
        }
        s.max(0.0)
    }

    /// The tab row: the tab shown underlined; the focused one in a white pill that grows in
    /// as focus arrives and shrinks away as it leaves (its spring, [`Self::tab_grow`]).
    fn draw_tabs<H: ContentLike>(&self, f: &mut DrawFrame<'_, '_, H>, p: Painter) {
        let sources = model::tab_sources(&self.view.snap);
        let rects = self.tab_rects(f.measure);
        let scroll = self.tab_scroll.pos;
        let shown = Rect::new(MARGIN_X - TAB_PAD, geom::TABS_Y - 8.0, SCR_W - 2.0 * MARGIN_X + 2.0 * TAB_PAD, geom::TABS_H + 20.0);
        let clip = p.clipped(shown);
        for (i, (source, at)) in sources.iter().zip(&rects).enumerate() {
            let r = Rect::new(at.x - scroll, at.y, at.w, at.h);
            let g = self.tab_grow.get(i).map_or(0.0, |s| s.pos.clamp(0.0, 1.0));
            if g > 0.01 {
                let (w, h) = (r.w * (0.6 + 0.4 * g), r.h * (0.7 + 0.3 * g));
                let pill = Rect::new(r.cx() - w * 0.5, r.y + (r.h - h) * 0.5, w, h);
                clip.rrect(pill, h * 0.5, h * 0.5, theme::with_a(theme::ACCENT, g));
            }
            let ink = theme::mix(theme::TEXT_PRIMARY, theme::ACCENT_INK, g);
            if let Ok(c) = CString::new(self.tab_label(source)) {
                Label::new(c.as_ptr(), theme::size::BODY, ink).bold().h(HAlign::Center).v(VAlign::Middle).draw(clip, r);
            }
            if *source == self.view.snap.source && g < 0.5 {
                clip.rrect(Rect::new(r.cx() - r.w * 0.3, r.y + r.h + 4.0, r.w * 0.6, 4.0), 2.0, 2.0, theme::with_a(theme::TEXT_PRIMARY, 1.0 - 2.0 * g));
            }
            if i == 1 {
                clip.rrect(Rect::new(r.x + r.w + TAB_SPLIT * 0.5 - 1.0, r.y + 12.0, 2.0, r.h - 24.0), 1.0, 1.0, theme::TEXT_SEPARATOR);
            }
            // A tab scrolled out of the row catches no pointer (one in the gutter did, and pulled
            // the row along under it); behind an open panel none does.
            let hit = r.intersect(shown);
            if self.panel.is_none() && hit.w > 1.0 && hit.h > 1.0 {
                f.stop(p, stop(self.key(model::TAB_BASE + i as u32), hit));
            }
        }
    }

    // ---- drawing ----------------------------------------------------------------------------------

    fn focused(&self, f: &DrawFrame<'_, '_, impl ContentLike>, elem: u32) -> bool {
        f.focus.current.is_some_and(|k| k.entry == self.entry && k.elem == elem)
    }

    fn draw_head<H: ContentLike>(&self, f: &mut DrawFrame<'_, '_, H>, p: Painter) {
        for (i, (elem, label)) in [(model::HEAD_GUIDE, msg::live_tv_mode_guide()), (model::HEAD_RECS, msg::live_tv_mode_recordings())]
            .into_iter()
            .enumerate()
        {
            let rect = head_rect(i);
            let Ok(text) = CString::new(label) else { continue };
            let on = self.focused(f, elem) && self.panel.is_none();
            Button::new(text.as_ptr(), theme::size::BODY, rect).focused(on).ground(ControlGround::Keyed).draw(&Env::inert(), p);
            if elem == self.mode_pill() {
                p.rrect(Rect::new(rect.cx() - 28.0, rect.y + rect.h + 8.0, 56.0, 4.0), 2.0, 2.0, theme::TEXT_PRIMARY);
            }
            if self.panel.is_none() {
                f.stop(p, stop(self.key(elem), rect));
            }
        }
        // the line at the right of the header: a tune on its way, else the last notice
        let line = match &self.view.snap.tune {
            TunePhase::Tuning { channel } => Some(msg::live_tv_tuning(channel)),
            _ => self.notice.as_ref().map(|(t, _)| t.clone()),
        };
        // In a dark pill beside the header's, so it reads over bright art, and short of the corner
        // picture when one plays there (it was drawn straight over the video).
        if let Some(line) = line {
            let pills = head_rect(1);
            let x = pills.x + geom::PILL_W + theme::space::LG;
            let right = if plx_media::player::video_window_is_mini() {
                plx_media::player::video_window()[0] as f32 - CORNER_GAP
            } else {
                SCR_W - MARGIN_X
            };
            let tw = f.measure.width_str(&line, theme::size::BODY, false).min(right - x - 48.0);
            if tw > 24.0 {
                let pill = Rect::new(x, pills.y, tw + 48.0, pills.h);
                p.rrect(pill, pill.h * 0.5, pill.h * 0.5, theme::with_a(INK_BLACK, 0.6));
                TextView::new(&line, theme::size::BODY, theme::TEXT_PRIMARY)
                    .with_measure(f.measure)
                    .max_lines(1)
                    .draw(p, Rect::new(pill.x + 24.0, pill.y + (pill.h - theme::size::BODY as f32) * 0.5 - 4.0, tw, 0.0));
            }
        }
    }

    fn draw_readout<H: ContentLike>(&self, f: &mut DrawFrame<'_, '_, H>, p: Painter, r: Readout) {
        let text = match r {
            // A "Loading" read-out is a placeholder: counted where it is drawn (`ui::placeholder`).
            Readout::Loading => {
                plx_ui::placeholder::note(p, plx_ui::placeholder::Reason::WorkingReadout, "livetv");
                msg::live_tv_state_loading()
            }
            Readout::Unavailable => msg::live_tv_state_unavailable(),
            Readout::Failed => msg::live_tv_state_failed(),
            Readout::NoChannels => msg::live_tv_state_no_channels(),
            Readout::NoTabChannels => msg::live_tv_state_no_tab_channels(),
            Readout::RecsLoading => {
                plx_ui::placeholder::note(p, plx_ui::placeholder::Reason::WorkingReadout, "livetv-recordings");
                msg::live_tv_recordings_loading()
            }
            Readout::RecsFailed => msg::live_tv_recordings_failed(),
            Readout::RecsEmpty => msg::live_tv_recordings_empty(),
        };
        let Ok(c) = CString::new(text) else { return };
        let band = Rect::new(0.0, plx_ui::widgets::StatusOverlay::FULL_ANCHOR_TOP, SCR_W, theme::size::HEADLINE as f32);
        Label::new(c.as_ptr(), theme::size::HEADLINE, theme::TEXT_SECONDARY).h(HAlign::Center).v(VAlign::CapTop).draw(p, band);
        if self.shows_retry() {
            let rect = retry_rect();
            if let Ok(label) = CString::new(msg::live_tv_action_retry()) {
                Button::new(label.as_ptr(), theme::size::BODY, rect)
                    .focused(self.focused(f, model::RETRY))
                    .ground(ControlGround::Keyed)
                    .draw(&Env::inert(), p);
            }
            if self.panel.is_none() {
                f.stop(p, stop(self.key(model::RETRY), rect));
            }
        }
    }

    /// The airing's recording state, as its badge words.
    fn badge(&self, a: &Airing) -> Option<String> {
        match model::grab_for(&self.view.snap, a).map(|g| &g.status) {
            Some(s) if s.is_recording() => Some(msg::live_tv_badge_recording().to_string()),
            Some(GrabStatus::Error) => Some(msg::live_tv_badge_failed().to_string()),
            Some(_) => Some(msg::live_tv_badge_scheduled().to_string()),
            None if a.is_recording_scheduled() => Some(msg::live_tv_badge_scheduled().to_string()),
            None if self.frame.now > 0 && a.airs_at(self.frame.now) => Some(msg::live_tv_badge_on_now().to_string()),
            None => None,
        }
    }

    /// A time of day for instant `t`, at the offset in force then (an airing past a daylight-saving
    /// change reads its own hour, as the player's bar does), else the guide's.
    fn time_of(&self, t: i64) -> String {
        let local = (t + plx_platform::i18n::utc_offset_s(t)).rem_euclid(86_400);
        plx_platform::i18n::current().time_of_day((local / 3_600) as u8, ((local % 3_600) / 60) as u8)
    }

    /// The airing the hero describes: the focused cell's (else the one on now in the first row
    /// shown), with its row.
    fn hero(&self) -> Option<(usize, &Airing)> {
        let (from, to) = (self.frame.win_start, self.frame.win_start + model::WINDOW_S);
        let focus = self.last_cell.filter(|e| self.view.holds(self.frame.base, *e, from, to)).unwrap_or_else(|| self.now_cell());
        self.view.airing(self.frame.base, focus)
    }

    /// The server the guide's art is fetched through.
    fn srv(&self) -> Option<u16> {
        self.view.snap.sid.filter(|s| s.is_set()).map(|s| s.raw())
    }

    /// The wash the page steps toward: the hero airing's UltraBlur corners held dark, else the
    /// plain deep blue (and always in Recordings).
    fn wash_target(&self) -> [[f32; 4]; 4] {
        let art = self.hero_art();
        match art.as_deref().and_then(|a| self.view.snap.blurs.get(a)) {
            Some(c) => {
                // ONE hue, as Plex's own guide wears: four corners of different colours read as a
                // muddy gradient (red to green on a news set). The most saturated corner is the
                // art's colour; the foot is the same hue, darker.
                let chroma = |k: &[f32; 3]| k.iter().cloned().fold(0.0, f32::max) - k.iter().cloned().fold(1.0, f32::min);
                let hue = c.iter().max_by(|a, b| chroma(a).total_cmp(&chroma(b))).copied().unwrap_or(c[0]);
                let (top, low) = (tint(hue, false), tint(hue, true));
                [top, top, low, low]
            }
            None => PLAIN_WASH,
        }
    }

    /// The page's ground: the wash, and in the Guide the hero airing's backdrop on the right of the
    /// band above the grid, its left edge dissolving into the wash and its foot fading into it.
    fn draw_ground(&self, p: Painter) {
        if plx_media::player::video_window_is_mini() {
            let [x, y, w, h] = plx_media::player::video_window();
            self.draw_wash_around(p, Rect::new(x as f32, y as f32, w as f32, h as f32));
            return;
        }
        self.wash.draw(p, Rect::FULL);
        if self.mode != Mode::Guide || self.readout().is_some() {
            return;
        }
        let (Some(srv), Some((_, a))) = (self.srv(), self.hero()) else { return };
        if a.art.is_empty() {
            return;
        }
        let (tex, tw, th) = resolve_tex_wh_on(srv, &a.art, ART_REQ.0, ART_REQ.1, 0);
        if tex == 0 {
            // placeholder-exempt: decoration over a ground that is already whole; nothing stands in
            return;
        }
        let r = Rect::new(geom::ART_X, 0.0, SCR_W - geom::ART_X, geom::ART_H);
        p.tex_uv(tex, r.cover_uv(tw, th, Crop::Centre), r, 0.0, theme::TINT_WHITE);
        // The left edge dissolves into the wash: strips of the wash's own colour over the art,
        // opaque at the fade's left and gone at its right, each a top-to-bottom gradient of the wash
        // down its height (the painter's gradients run top to bottom only, hence strips). Whole
        // pixels wide and placed on whole pixels: strips of a texture at fractional edges combed.
        let mut x = r.x;
        while x < r.x + ART_FADE_W {
            let t = (x - r.x + ART_FADE_STRIP * 0.5) / ART_FADE_W;
            let a = (1.0 - t) * (1.0 - t);
            let (top, foot) = (self.wash.sample(Rect::FULL, x, r.y), self.wash.sample(Rect::FULL, x, r.y + r.h));
            p.rect(
                Rect::new(x, r.y, ART_FADE_STRIP, r.h),
                0.0,
                [top[0], top[1], top[2], a],
                [foot[0], foot[1], foot[2], a],
                0.0,
            );
            x += ART_FADE_STRIP;
        }
        // The foot fades into the wash, in its colour where they meet.
        for j in 0..ART_FALL_SEGS {
            let (x, seg) = fall_segment(r.x, r.w, j);
            let c = self.wash.sample(Rect::FULL, x + seg * 0.5, r.y + r.h);
            let col = [c[0], c[1], c[2], 1.0];
            p.rect(Rect::new(x, r.y + r.h - ART_FALL, seg, ART_FALL), 0.0, theme::with_a(col, 0.0), col, 0.0);
        }
    }

    /// The wash around a channel playing on in the corner: everywhere but its window, which the
    /// video plane shows through, feathered on its left and its foot so the picture melts into the
    /// page the way the art does.
    fn draw_wash_around(&self, p: Painter, hole: Rect) {
        let at = |x: f32, y: f32| self.wash.sample(Rect::FULL, x, y);
        let resting = self.wash.at_rest();
        let part = |r: Rect| {
            if r.w > 0.5 && r.h > 0.5 {
                let k = [at(r.x, r.y), at(r.x + r.w, r.y), at(r.x + r.w, r.y + r.h), at(r.x, r.y + r.h)];
                p.ambient(r, 1.0, k, resting);
            }
        };
        let (right, foot) = (hole.x + hole.w, hole.y + hole.h);
        part(Rect::new(0.0, 0.0, hole.x, SCR_H));
        part(Rect::new(hole.x, 0.0, SCR_W - hole.x, hole.y));
        part(Rect::new(hole.x, foot, SCR_W - hole.x, SCR_H - foot));
        part(Rect::new(right, hole.y, SCR_W - right, hole.h));
        // Whole-pixel strips, as the art's fade (fractional edges comb), but narrow and on an S-curve:
        // over moving video, the art's 10 px t² steps (8% at the edge) read as bars on the TV.
        let mut x = hole.x;
        while x < hole.x + MINI_FEATHER_L {
            let s = 1.0 - (x - hole.x + MINI_FEATHER_STRIP * 0.5) / MINI_FEATHER_L;
            let a = s * s * (3.0 - 2.0 * s);
            let (top, low) = (at(x, hole.y), at(x, foot));
            p.rect(Rect::new(x, hole.y, MINI_FEATHER_STRIP, hole.h), 0.0, [top[0], top[1], top[2], a], [low[0], low[1], low[2], a], 0.0);
            x += MINI_FEATHER_STRIP;
        }
        for j in 0..ART_FALL_SEGS {
            let (x, seg) = fall_segment(hole.x, hole.w, j);
            let c = at(x + seg * 0.5, foot);
            let col = [c[0], c[1], c[2], 1.0];
            p.rect(Rect::new(x, foot - MINI_FEATHER_B, seg, MINI_FEATHER_B), 0.0, theme::with_a(col, 0.0), col, 0.0);
        }
    }

    /// The hero: the focused airing's title, its facts (with how long is left of one on now), its
    /// episode title and genres, and its summary, over the left of the backdrop.
    fn draw_details<H: ContentLike>(&self, f: &mut DrawFrame<'_, '_, H>, p: Painter) {
        let Some((row, a)) = self.hero() else { return };
        let x = MARGIN_X;
        let w = geom::DETAIL_W;
        let title = if a.show_title.is_empty() { a.title.clone() } else { a.show_title.clone() };
        TextView::new(&title, theme::size::DISPLAY, theme::TEXT_PRIMARY)
            .bold()
            .with_measure(f.measure)
            .max_lines(1)
            .draw(p, Rect::new(x, geom::DETAIL_Y, w, 0.0));
        let mut parts = Vec::new();
        match episode_words(a.season, a.episode) {
            Some(words) => parts.push(words),
            None if a.year > 0 => parts.push(a.year.to_string()),
            None => {}
        }
        parts.push(format!("{} \u{2013} {}", self.time_of(a.begins_at), self.time_of(a.ends_at)));
        if let Some(ch) = self.view.snap.channels.get(row) {
            parts.push(channel_words(ch));
        }
        if !a.content_rating.is_empty() {
            parts.push(a.content_rating.clone());
        }
        let facts = parts.join(" \u{b7} ");
        let y = geom::DETAIL_Y + 60.0;
        let line = f.measure.line_h(theme::size::BODY);
        TextView::new(&facts, theme::size::BODY, theme::TEXT_SECONDARY)
            .with_measure(f.measure)
            .max_lines(1)
            .draw(p, Rect::new(x, y, w, 0.0));
        let mut px = x + f.measure.width_str(&facts, theme::size::BODY, false).min(w) + 22.0;
        let on_now = self.frame.now > 0 && a.airs_at(self.frame.now);
        if on_now {
            px += self.draw_left_pill(f, p, px, y + line * 0.5, a) + 16.0;
        }
        // the recording state; "On Now" is the pill's to say
        if let Some(badge) = self.badge(a).filter(|b| b.as_str() != msg::live_tv_badge_on_now()) {
            TextView::new(&badge, theme::size::BODY, theme::TEXT_PRIMARY)
                .bold()
                .with_measure(f.measure)
                .max_lines(1)
                .draw(p, Rect::new(px, y, (x + w - px).max(0.0), 0.0));
        }
        let mut y = geom::DETAIL_Y + 100.0;
        let mut second = Vec::new();
        if !a.show_title.is_empty() && !a.title.is_empty() && a.title != a.show_title {
            second.push(a.title.clone());
        }
        if !a.genres.is_empty() {
            second.push(a.genres.join(", "));
        }
        if !second.is_empty() {
            TextView::new(&second.join(" \u{b7} "), theme::size::LABEL, theme::TEXT_PRIMARY)
                .with_measure(f.measure)
                .max_lines(1)
                .draw(p, Rect::new(x, y, w, 0.0));
            y += 38.0;
        }
        if !a.summary.is_empty() {
            TextView::new(&a.summary, theme::size::LABEL, theme::TEXT_READING)
                .with_measure(f.measure)
                .max_lines(2)
                .draw(p, Rect::new(x, y, w, 0.0));
        }
    }

    /// The "1h 37m left" pill of an airing on now, its left edge at `x`, centred on `cy`; returns
    /// its width. The ring counts down: the time left is lit, what has aired is dim.
    fn draw_left_pill<H: ContentLike>(&self, f: &DrawFrame<'_, '_, H>, p: Painter, x: f32, cy: f32, a: &Airing) -> f32 {
        let now = self.frame.now;
        let text = left_words(a.ends_at - now);
        let size = theme::size::CAPTION;
        let tw = f.measure.width_str(&text, size, true);
        let (h, ring, pad) = (40.0, 22.0, 12.0);
        let w = pad + ring + 10.0 + tw + pad;
        p.rrect(Rect::new(x, cy - h * 0.5, w, h), 10.0, 10.0, theme::with_a(INK_BLACK, 0.72));
        let span = (a.ends_at - a.begins_at).max(1) as f32;
        let aired = ((now - a.begins_at) as f32 / span).clamp(0.0, 1.0);
        draw_ring(p, x + pad + ring * 0.5, cy, ring * 0.5 - 2.0, aired);
        let lh = f.measure.line_h(size);
        TextView::new(&text, size, theme::TEXT_PRIMARY)
            .bold()
            .with_measure(f.measure)
            .max_lines(1)
            .draw(p, Rect::new(x + pad + ring + 10.0, cy - lh * 0.5, tw + 4.0, 0.0));
        w
    }

    /// A row's channel: its logo (else its call sign) over its number.
    fn draw_channel<H: ContentLike>(&self, f: &DrawFrame<'_, '_, H>, p: Painter, row: usize, ch: &Channel, y: f32, h: f32) {
        let r = Rect::new(geom::CHANNEL_X, y, geom::CHANNEL_W, h);
        p.rrect(r, 10.0, 10.0, theme::with_a(INK_BLACK, 0.32));
        let logo_box = Rect::new(r.x + 18.0, y + 10.0, r.w - 36.0, r.h - 44.0);
        let thumb = if ch.thumb.is_empty() {
            self.view.row(row).and_then(|rw| rw.airings.iter().map(|a| a.channel_thumb.as_str()).find(|t| !t.is_empty())).unwrap_or("")
        } else {
            ch.thumb.as_str()
        };
        let mut logo = false;
        if let (Some(srv), false) = (self.srv(), thumb.is_empty()) {
            let (tex, tw, th) = resolve_tex_wh_on(srv, thumb, LOGO_REQ.0, LOGO_REQ.1, 1);
            if tex != 0 && tw > 0.0 && th > 0.0 {
                p.tex(tex, contain(logo_box, tw, th), 0.0, theme::TINT_WHITE);
                logo = true;
            }
        }
        if !logo {
            TextView::new(ch.station(), theme::size::LABEL, theme::TEXT_PRIMARY)
                .bold()
                .with_measure(f.measure)
                .max_lines(1)
                .h(HAlign::Center)
                .draw(p, Rect::new(logo_box.x, logo_box.y + (logo_box.h - theme::size::LABEL as f32) * 0.5, logo_box.w, 0.0));
        }
        // Under a logo, the number and the station's own letters ("4.1 KXYZ"): a subchannel's logo is
        // its network's, which says nothing of whose signal it rides. Without a logo the
        // call sign is already the big label, so the number stands alone.
        // A Plex channel's logo is the whole of its name: no number, no internal abbreviation.
        let station = if logo && ch.part_key.is_empty() { station_id(&ch.call_sign) } else { "" };
        let words = match (ch.number(), station) {
            ("", s) => s.to_string(),
            (n, "") => n.to_string(),
            (n, s) => format!("{n} {s}"),
        };
        if !words.is_empty() {
            TextView::new(&words, theme::size::CAPTION, theme::TEXT_SECONDARY)
                .with_measure(f.measure)
                .max_lines(1)
                .h(HAlign::Center)
                .draw(p, Rect::new(r.x, y + r.h - 32.0, r.w, 0.0));
        }
    }

    fn draw_guide<H: ContentLike>(&self, f: &mut DrawFrame<'_, '_, H>, p: Painter) {
        self.draw_details(f, p);
        let fr = &self.frame;
        // the ruler: a band with a time at each half hour
        p.rrect(
            Rect::new(geom::GRID_X, geom::RULER_Y, geom::GRID_W - geom::CELL_GAP, geom::RULER_H),
            6.0,
            6.0,
            theme::with_a(INK_BLACK, 0.45),
        );
        for k in 0..(model::WINDOW_S / model::HALF_HOUR) {
            let t = fr.win_start + k * model::HALF_HOUR;
            let x = model::x_of(fr.win_start, t);
            let label = self.time_of(t);
            if let Ok(c) = CString::new(label) {
                Label::new(c.as_ptr(), theme::size::CAPTION, theme::TEXT_SECONDARY)
                    .v(VAlign::Middle)
                    .draw(p, Rect::new(x + 10.0, geom::RULER_Y, geom::GRID_W / 4.0, geom::RULER_H));
            }
            if k > 0 {
                p.rrect(Rect::new(x - 1.0, geom::RULER_Y + 8.0, 2.0, geom::RULER_H - 16.0), 1.0, 1.0, theme::TEXT_SEPARATOR);
            }
        }
        let n = self.view.channel_count();
        let rows = fr.row_top..(fr.row_top + model::ROWS_SHOWN).min(n);
        let panel_up = self.panel.is_some();
        for row in rows {
            let y = model::row_y(fr, row);
            if let Some(ch) = self.view.snap.channels.get(row) {
                self.draw_channel(f, p, row, ch, y, model::row_h(fr, row));
            }
            let Some(r) = self.view.row(row) else { continue };
            let shown: Vec<&Airing> = r.airings.iter().filter(|a| a.ends_at > fr.win_start && a.begins_at < fr.win_start + model::WINDOW_S).collect();
            if shown.is_empty() {
                let elem = model::cell_elem(row, model::PLACEHOLDER);
                let Some(rect) = model::placeholder_rect(fr, row) else { continue };
                let on = !panel_up && self.focused(f, elem);
                let words = if r.pending {
                    // a row whose programs are on their way: one line of text that has not arrived
                    plx_ui::placeholder::note(p, plx_ui::placeholder::Reason::SkeletonBar, "livetv-row");
                    msg::live_tv_row_loading()
                } else if r.failed {
                    // the server did not answer for this channel (asked again once a minute);
                    // the channel itself still plays
                    msg::live_tv_row_failed()
                } else {
                    msg::live_tv_row_no_info()
                };
                self.draw_cell(f, p, rect, words, "", on, false, false);
                // focusable whenever it stands for the whole window (OK on it watches the channel)
                if !panel_up {
                    f.stop(p, stop(self.key(elem), rect));
                }
                continue;
            }
            for a in shown {
                let Some(rect) = model::cell_rect(fr, row, a.begins_at, a.ends_at) else { continue };
                let elem = model::cell_elem(row, model::slot_of(fr.base, a.begins_at));
                let on = !panel_up && self.focused(f, elem);
                let rec = a.is_recording_scheduled() || model::grab_for(&self.view.snap, a).is_some();
                self.draw_airing_cell(f, p, rect, a, on, rec);
                if !panel_up {
                    f.stop(p, stop(self.key(elem), rect));
                }
            }
        }
        // now, on the ruler
        if fr.now >= fr.win_start && fr.now < fr.win_start + model::WINDOW_S {
            let x = model::x_of(fr.win_start, fr.now);
            p.rrect(Rect::new(x - 2.0, geom::RULER_Y, 4.0, geom::RULER_H), 2.0, 2.0, LEFT_RING);
        }
    }

    /// One airing's cell: on now, the part already aired is lit; wide enough, the program's art at
    /// its left; its title over its times (and, focused and on now, how long is left).
    fn draw_airing_cell<H: ContentLike>(&self, f: &DrawFrame<'_, '_, H>, p: Painter, rect: Rect, a: &Airing, on: bool, rec: bool) {
        let fr = &self.frame;
        let now = fr.now > 0 && a.airs_at(fr.now);
        let (fill, ink, ink2) = cell_colors(on, now);
        p.rrect(rect, 10.0, 10.0, fill);
        // A program on now fills from its left up to now, as the stock guide's does: a milky white
        // over a cell at rest; the focused cell is white already, so what is still to come dims
        // instead, and its elapsed part reads as the fill.
        if now {
            let nx = model::x_of(fr.win_start, fr.now).clamp(rect.x, rect.x + rect.w);
            if on {
                if nx < rect.x + rect.w - 1.0 {
                    let left = if nx <= rect.x + 1.0 { 10.0 } else { 0.0 };
                    p.rrect(Rect::new(nx, rect.y, rect.x + rect.w - nx, rect.h), left, 10.0, theme::with_a(INK_BLACK, 0.09));
                }
            } else if nx > rect.x + 1.0 {
                let right = if nx >= rect.x + rect.w - 1.0 { 10.0 } else { 0.0 };
                p.rrect(Rect::new(rect.x, rect.y, nx - rect.x, rect.h), 10.0, right, theme::with_a(theme::TEXT_PRIMARY, ON_NOW_FILL));
            }
        }
        if rect.w < 40.0 {
            return;
        }
        // A grown row's cell: its poster grows with it and its words stay centred in it.
        let dy = (rect.h - geom::ROW_H) * 0.5;
        let mut tx = rect.x + 16.0;
        if rect.w >= POSTER_MIN_CELL_W && !a.poster.is_empty() {
            if let Some(srv) = self.srv() {
                let ph = rect.h - 16.0;
                let pr = Rect::new(rect.x + 8.0, rect.y + 8.0, (ph * 2.0 / 3.0).round(), ph);
                let (tex, tw, th) = resolve_tex_wh_on(srv, &a.poster, POSTER_REQ.0, POSTER_REQ.1, 0);
                if tex != 0 {
                    p.tex_uv(tex, pr.cover_uv(tw, th, Crop::Centre), pr, 6.0, theme::TINT_WHITE);
                    tx = pr.x + pr.w + 14.0;
                } else if !plx_ui::tex::failed_on(srv, &a.poster, POSTER_REQ.0, POSTER_REQ.1, false) {
                    plx_ui::placeholder::skeleton(p, plx_ui::placeholder::Reason::CardSkeleton, "livetv-poster", pr, 6.0);
                    tx = pr.x + pr.w + 14.0;
                }
            }
        }
        let title = if a.show_title.is_empty() { a.title.as_str() } else { a.show_title.as_str() };
        let mut sub = format!("{} \u{2013} {}", self.time_of(a.begins_at), self.time_of(a.ends_at));
        if on && now {
            sub = format!("{sub} \u{b7} {}", left_words(a.ends_at - fr.now));
        } else if !a.show_title.is_empty() && !a.title.is_empty() && a.title != a.show_title && rect.w >= EPISODE_MIN_CELL_W {
            sub = format!("{sub} \u{b7} {}", a.title);
        }
        let dot = if rec { 24.0 } else { 0.0 };
        // a premiere's NEW, outlined in the top-right corner (left of the record dot)
        let mut badge_w = 0.0;
        if a.premiere && rect.w >= NEW_MIN_CELL_W {
            let words = msg::live_tv_badge_new();
            let tw = f.measure.width_str(words, theme::size::MICRO, true);
            let b = Rect::new(rect.x + rect.w - 12.0 - dot - tw - 16.0, rect.y + 10.0 + dy, tw + 16.0, 28.0);
            p.rring(b, 6.0, 1.5, theme::with_a(ink2, 0.8));
            if let Ok(c) = CString::new(words) {
                Label::new(c.as_ptr(), theme::size::MICRO, ink2).bold().h(HAlign::Center).v(VAlign::Middle).draw(p, b);
            }
            badge_w = b.w + 10.0;
        }
        let w = rect.x + rect.w - 14.0 - dot - badge_w - tx;
        // A sliver (a Plex channel's program that began a few minutes before the window) keeps
        // its fill and no words: "L…" over "1…" said nothing.
        if w >= CELL_TEXT_MIN_W {
            TextView::new(title, theme::size::LABEL, ink)
                .bold()
                .with_measure(f.measure)
                .max_lines(1)
                .draw(p, Rect::new(tx, rect.y + 16.0 + dy, w, 0.0));
            TextView::new(&sub, theme::size::CAPTION, ink2)
                .with_measure(f.measure)
                .max_lines(1)
                .draw(p, Rect::new(tx, rect.y + 52.0 + dy, w, 0.0));
        }
        if rec {
            p.rrect(Rect::new(rect.x + rect.w - 14.0 - 12.0, rect.y + 20.0 + dy, 12.0, 12.0), 6.0, 6.0, theme::DANGER);
        }
    }

    /// A cell with words and no airing: a row still loading, or one with no guide.
    #[allow(clippy::too_many_arguments)]
    fn draw_cell<H: ContentLike>(&self, f: &DrawFrame<'_, '_, H>, p: Painter, rect: Rect, title: &str, sub: &str, on: bool, now: bool, rec: bool) {
        let (fill, ink, ink2) = cell_colors(on, now);
        p.rrect(rect, 10.0, 10.0, fill);
        if rect.w < 40.0 {
            return;
        }
        let pad = 16.0;
        let dot = if rec { 22.0 } else { 0.0 };
        let w = rect.w - 2.0 * pad - dot;
        if w > 8.0 {
            TextView::new(title, theme::size::LABEL, ink)
                .bold()
                .with_measure(f.measure)
                .max_lines(1)
                .draw(p, Rect::new(rect.x + pad, rect.y + 18.0, w, 0.0));
            if !sub.is_empty() {
                TextView::new(sub, theme::size::CAPTION, ink2)
                    .with_measure(f.measure)
                    .max_lines(1)
                    .draw(p, Rect::new(rect.x + pad, rect.y + 56.0, w, 0.0));
            }
        }
        if rec {
            p.rrect(Rect::new(rect.x + rect.w - pad - 12.0, rect.y + 20.0, 12.0, 12.0), 6.0, 6.0, theme::DANGER);
        }
    }

    fn draw_recs<H: ContentLike>(&self, f: &mut DrawFrame<'_, '_, H>, p: Painter) {
        let panel_up = self.panel.is_some();
        let w = SCR_W - 2.0 * MARGIN_X;
        // A row beside the corner picture stops short of it (seen on the TV: a full-width row ran on
        // under the video, its fill showing through the feather).
        let corner = plx_media::player::video_window_is_mini().then(plx_media::player::video_window);
        let row_w = |y: f32| match corner {
            Some([cx, cy, _, ch]) if y < (cy + ch) as f32 => w.min(cx as f32 - CORNER_GAP - MARGIN_X),
            _ => w,
        };
        let heading = |p: Painter, words: &str, y: f32| {
            if let Ok(c) = CString::new(words) {
                Label::new(c.as_ptr(), theme::size::LABEL, theme::TEXT_TERTIARY)
                    .bold()
                    .v(VAlign::Middle)
                    .draw(p, Rect::new(MARGIN_X, y, w, geom::REC_HEADING_H));
            }
        };
        // Rules but nothing scheduled: say so, and how to schedule something, above the rules.
        if self.rec_top == 0 && model::rec_note(&self.recs) {
            heading(p, msg::live_tv_recordings_upcoming(), geom::RECS_Y);
            let note = Rect::new(MARGIN_X, geom::RECS_Y + geom::REC_HEADING_H, row_w(geom::RECS_Y), geom::REC_NOTE_H - 12.0);
            p.rrect(note, 12.0, 12.0, theme::with_a(theme::TEXT_PRIMARY, 0.04));
            TextView::new(msg::live_tv_recordings_none_scheduled(), theme::size::CAPTION, theme::TEXT_SECONDARY)
                .with_measure(f.measure)
                .max_lines(1)
                .draw(p, Rect::new(note.x + 20.0, note.y + (note.h - f.measure.line_h(theme::size::CAPTION)) * 0.5, note.w - 40.0, 0.0));
        }
        let today = model::local_day(self.view.snap.now, self.view.snap.utc_offset);
        let first_rule = self.recs.iter().position(|e| matches!(e, RecEntry::Rule(_))).unwrap_or(0);
        for i in self.rec_top..self.recs.len() {
            let y = model::rec_y(&self.recs, self.rec_top, i);
            if y + geom::REC_H > geom::RECS_BOTTOM {
                break;
            }
            if model::rec_heads(&self.recs, i) {
                let words = match &self.recs[i] {
                    RecEntry::Grab(_, RecGroup::Now) => msg::live_tv_recordings_now().to_string(),
                    RecEntry::Grab(_, RecGroup::Day(d)) if *d <= today => msg::live_tv_recordings_today().to_string(),
                    RecEntry::Grab(_, RecGroup::Day(d)) if *d == today + 1 => msg::live_tv_recordings_tomorrow().to_string(),
                    RecEntry::Grab(_, RecGroup::Day(d)) => self.date_words(*d),
                    RecEntry::Rule(_) => msg::live_tv_recordings_rules().to_string(),
                };
                heading(p, &words, y - geom::REC_HEADING_H);
            }
            let rect = Rect::new(MARGIN_X, y, row_w(y), geom::REC_H);
            let elem = model::REC_BASE + i as u32;
            let on = !panel_up && self.focused(f, elem);
            let (fill, ink, ink2) = if on {
                (theme::ACCENT, theme::ACCENT_INK, theme::ACCENT_INK)
            } else {
                (theme::with_a(theme::TEXT_PRIMARY, 0.07), theme::TEXT_PRIMARY, theme::TEXT_SECONDARY)
            };
            p.rrect(rect, 12.0, 12.0, fill);
            // the program's (a rule's show's) poster on the left
            let poster = match &self.recs[i] {
                RecEntry::Grab(g, _) => g.airing.poster.as_str(),
                RecEntry::Rule(s) => s.hint.thumb.as_str(),
            };
            let pr = Rect::new(rect.x + 8.0, rect.y + 6.0, geom::REC_POSTER_W, geom::REC_POSTER_H);
            self.draw_poster(p, poster, pr);
            let mut tx = pr.x + pr.w + 22.0;
            // a rule's place in the priority order, as the server ranks them
            if let RecEntry::Rule(_) = &self.recs[i] {
                let rank = (i + 1).saturating_sub(first_rule).to_string();
                TextView::new(&rank, theme::size::BODY, ink2)
                    .bold()
                    .with_measure(f.measure)
                    .max_lines(1)
                    .draw(p, Rect::new(tx, rect.y + (rect.h - theme::size::BODY as f32) * 0.5 - 6.0, 48.0, 0.0));
                tx += 52.0;
            }
            let (title, sub, mark) = self.rec_words(&self.recs[i]);
            let mark_w = if mark.is_some() { 260.0 } else { 0.0 };
            let tw = rect.x + rect.w - 24.0 - mark_w - tx;
            TextView::new(&title, theme::size::BODY, ink)
                .bold()
                .with_measure(f.measure)
                .max_lines(1)
                .draw(p, Rect::new(tx, rect.y + 16.0, tw, 0.0));
            TextView::new(&sub, theme::size::CAPTION, ink2)
                .with_measure(f.measure)
                .max_lines(1)
                .draw(p, Rect::new(tx, rect.y + 56.0, tw, 0.0));
            if let Some(mark) = mark {
                let recording = matches!(&self.recs[i], RecEntry::Grab(g, _) if g.status.is_recording());
                let failed = matches!(&self.recs[i], RecEntry::Grab(g, _) if g.status == GrabStatus::Error);
                let col = if on { ink } else if recording || failed { theme::DANGER } else { ink2 };
                let mr = Rect::new(rect.x + rect.w - 24.0 - mark_w, rect.y, mark_w, rect.h);
                if let Ok(c) = CString::new(mark) {
                    Label::new(c.as_ptr(), theme::size::LABEL, col).h(HAlign::Right).v(VAlign::Middle).draw(p, mr);
                }
                if recording {
                    // the red dot a recording wears in the guide, before its word
                    let lw = f.measure.width_str(mark, theme::size::LABEL, false);
                    let dot = Rect::new(mr.x + mr.w - lw - 26.0, rect.y + rect.h * 0.5 - 7.0, 14.0, 14.0);
                    p.rrect(dot, 7.0, 7.0, if on { ink } else { theme::DANGER });
                }
            }
            if !panel_up {
                f.stop(p, stop(self.key(elem), rect));
            }
        }
    }

    /// A Recordings row's words: its title, the line under it, and the state it wears on the right.
    fn rec_words(&self, entry: &RecEntry) -> (String, String, Option<&'static str>) {
        match entry {
            RecEntry::Grab(g, _) => {
                let a = &g.airing;
                let title = if a.show_title.is_empty() { a.title.clone() } else { a.show_title.clone() };
                let mut sub = vec![format!("{} \u{2013} {}", self.time_of(a.begins_at), self.time_of(a.ends_at))];
                // the channel as the guide names it, else as the recording does
                match self.view.snap.channels.iter().find(|c| !a.channel_identifier.is_empty() && c.identifier == a.channel_identifier) {
                    Some(ch) => sub.push(ch.words()),
                    None if !a.channel_title.is_empty() || !a.channel_vcn.is_empty() => {
                        let station = station_id(&a.channel_title);
                        sub.push(format!("{} {station}", a.channel_vcn).trim().to_string())
                    }
                    None => {}
                }
                if let Some(ep) = episode_words(a.season, a.episode) {
                    sub.push(ep);
                }
                if !a.show_title.is_empty() && !a.title.is_empty() && a.title != a.show_title {
                    sub.push(a.title.clone());
                }
                let mark = if g.status.is_recording() {
                    msg::live_tv_badge_recording()
                } else if g.status == GrabStatus::Error {
                    msg::live_tv_badge_failed()
                } else {
                    msg::live_tv_badge_scheduled()
                };
                (title, sub.join(" \u{b7} "), Some(mark))
            }
            RecEntry::Rule(s) => {
                let title = [&s.hint.show_title, &s.hint.title, &s.title].into_iter().find(|t| !t.is_empty()).cloned().unwrap_or_default();
                let kind = if s.kind == 2 { msg::live_tv_rule_series() } else { msg::live_tv_rule_single() };
                let mut sub = vec![kind.to_string()];
                if !s.section_title.is_empty() {
                    sub.push(s.section_title.clone());
                }
                (title, sub.join(" \u{b7} "), None)
            }
        }
    }

    /// A program's poster in `r`, through the server's photo transcoder; a quiet tile while it loads
    /// or when there is none.
    fn draw_poster(&self, p: Painter, path: &str, r: Rect) {
        if let (Some(srv), false) = (self.srv(), path.is_empty()) {
            let (tex, tw, th) = resolve_tex_wh_on(srv, path, POSTER_REQ.0, POSTER_REQ.1, 0);
            if tex != 0 {
                p.tex_uv(tex, r.cover_uv(tw, th, Crop::Centre), r, 6.0, theme::TINT_WHITE);
                return;
            }
            if !plx_ui::tex::failed_on(srv, path, POSTER_REQ.0, POSTER_REQ.1, false) {
                plx_ui::placeholder::skeleton(p, plx_ui::placeholder::Reason::CardSkeleton, "livetv-rec-poster", r, 6.0);
                return;
            }
        }
        p.rrect(r, 6.0, 6.0, theme::with_a(theme::TEXT_PRIMARY, 0.08));
    }

    /// A local date for a Recordings heading past tomorrow ("24 October"), from its day number.
    fn date_words(&self, day: i64) -> String {
        let (_, m, d) = civil(day);
        plx_platform::i18n::current()
            .month_day(m as u8, d as u8, plx_platform::i18n::MonthWidth::Long)
            .unwrap_or_else(|| format!("{d}/{m}"))
    }

    fn draw_panel<H: ContentLike>(&self, f: &mut DrawFrame<'_, '_, H>, p: Painter, panel: &Panel) {
        p.rrect(Rect::FULL, 0.0, 0.0, theme::with_a([0.0, 0.0, 0.0, 1.0], 0.55));
        let r = panel_rect(panel.actions.len());
        p.rect(r, 24.0, theme::PANEL_TOP, theme::PANEL_BOT, 0.0);
        let title = match &panel.target {
            Target::Airing { airing, .. } => {
                if airing.show_title.is_empty() { airing.title.clone() } else { airing.show_title.clone() }
            }
            Target::Grab(g) => {
                if g.airing.show_title.is_empty() { g.airing.title.clone() } else { g.airing.show_title.clone() }
            }
            Target::Rule(s) => [&s.hint.show_title, &s.hint.title, &s.title].into_iter().find(|t| !t.is_empty()).cloned().unwrap_or_default(),
        };
        TextView::new(&title, theme::size::HEADLINE, theme::TEXT_PRIMARY)
            .bold()
            .with_measure(f.measure)
            .max_lines(1)
            .h(HAlign::Center)
            .draw(p, Rect::new(r.x + 32.0, r.y + 32.0, r.w - 64.0, 0.0));
        for (i, action) in panel.actions.iter().enumerate() {
            let rect = action_rect(panel.actions.len(), i);
            let Ok(label) = CString::new(action_label(*action)) else { continue };
            let elem = model::ACTION_BASE + i as u32;
            Button::new(label.as_ptr(), theme::size::BODY, rect)
                .focused(self.focused(f, elem))
                .ground(ControlGround::Keyed)
                .draw(&Env::inert(), p);
            f.stop(p, stop(self.key(elem), rect));
        }
    }
}

fn stop(key: FocusKey<u32>, rect: Rect) -> Stop<u32> {
    Stop { key, rect, rest_rect: rect, clip: Rect::FULL, hover: Hover::Focus, activate: Activate::Press }
}

fn head_rect(i: usize) -> Rect {
    Rect::new(MARGIN_X + i as f32 * (geom::PILL_W + geom::PILL_GAP), geom::HEAD_Y, geom::PILL_W, geom::HEAD_H)
}

fn retry_rect() -> Rect {
    Rect::new((SCR_W - 280.0) * 0.5, plx_ui::widgets::StatusOverlay::FULL_ANCHOR_TOP + 80.0, 280.0, geom::HEAD_H)
}

fn panel_rect(n: usize) -> Rect {
    let h = 32.0 + 52.0 + 24.0 + n as f32 * geom::ACTION_PITCH + 20.0;
    Rect::new((SCR_W - geom::PANEL_W) * 0.5, ((SCR_H - h) * 0.5).max(60.0), geom::PANEL_W, h)
}

fn action_rect(n: usize, i: usize) -> Rect {
    let r = panel_rect(n);
    Rect::new(r.x + 48.0, r.y + 32.0 + 52.0 + 24.0 + i as f32 * geom::ACTION_PITCH, r.w - 96.0, geom::ACTION_H)
}

fn action_label(a: Action) -> &'static str {
    match a {
        Action::Watch => msg::live_tv_action_watch(),
        Action::Record => msg::live_tv_action_record(),
        Action::RecordSeries => msg::live_tv_action_record_series(),
        Action::StopRecording => msg::live_tv_action_stop_recording(),
        Action::CancelRecording => msg::live_tv_action_cancel_recording(),
        Action::CancelSeries => msg::live_tv_action_cancel_series(),
        Action::MoveTop => msg::live_tv_action_move_top(),
        Action::MoveBottom => msg::live_tv_action_move_bottom(),
        Action::DeleteRule => msg::live_tv_action_delete_rule(),
        Action::Close => msg::live_tv_action_close(),
    }
}

/// A write's outcome, in words.
/// An airing's season and episode words. A guide that numbers a daily show by year can send the
/// year as the episode index and the day's number as the season (a real guide: `parentIndex` 335,
/// `index` 2026, which Plex's own guide shows as "2026 E335"), so a year reads as the season,
/// written without the "S".
fn episode_words(season: i64, episode: i64) -> Option<String> {
    const YEAR: i64 = 1_900;
    if season <= 0 || episode <= 0 {
        return None;
    }
    if episode >= YEAR && season < YEAR {
        return Some(format!("{episode} {}", msg::core_episode(season)));
    }
    if season >= YEAR {
        return Some(format!("{season} {}", msg::core_episode(episode)));
    }
    Some(msg::live_tv_episode(&season.to_string(), &episode.to_string()))
}

/// The page's wash: a deep blue, darker toward the foot.
const GROUND: [f32; 4] = [0.051, 0.098, 0.192, 1.0];
const GROUND_LOW: [f32; 4] = [0.027, 0.047, 0.098, 1.0];
/// The wash with no art to key it (corner-major: top-left, top-right, bottom-right, bottom-left).
const PLAIN_WASH: [[f32; 4]; 4] = [GROUND, GROUND, GROUND_LOW, GROUND_LOW];
/// How bright an art colour may make the ground (Rec.709 luma), and how far the ground leans
/// toward it: Plex's own guide wears the program's colour, dark enough for its white text.
const TINT_LUMA: f32 = 0.20;
const TINT_W: f32 = 0.85;
/// A channel playing on in the corner melts into the page this far from its left edge and up
/// from its foot.
const MINI_FEATHER_L: f32 = 240.0;
const MINI_FEATHER_B: f32 = 140.0;
/// The corner picture's left feather strip: 120 strips, no step over 1.3% (the S-curve's steepest).
const MINI_FEATHER_STRIP: f32 = 2.0;
/// The narrowest room an airing cell writes its title and times in.
const CELL_TEXT_MIN_W: f32 = 72.0;
/// The room a Recordings row leaves before the corner picture.
const CORNER_GAP: f32 = 24.0;
/// The backdrop's foot: how tall its fade into the wash is, in how many colour samples across.
const ART_FALL: f32 = 200.0;
const ART_FALL_SEGS: usize = 12;

/// Segment `j` of the [`ART_FALL_SEGS`] a fall is drawn in across `x..x + w`: whole pixels, edge to
/// edge. The half-pixel overlap they had blended its column twice, a dark line between every two
/// (seen on the TV over the corner picture's foot).
fn fall_segment(x: f32, w: f32, j: usize) -> (f32, f32) {
    let edge = |k: usize| (x + w * k as f32 / ART_FALL_SEGS as f32 + 0.5) as i32 as f32;
    (edge(j), edge(j + 1) - edge(j))
}
/// The time left of an airing on now: the pill's ring, the ruler's now mark.
const LEFT_RING: [f32; 4] = [0.976, 0.659, 0.149, 1.0];
const INK_BLACK: [f32; 4] = [0.0, 0.0, 0.0, 1.0];
/// The sizes the guide asks its art at: the backdrop, a cell's poster, a channel's logo (PNG, for
/// its transparency).
const ART_REQ: (i32, i32) = (1280, 720);
const POSTER_REQ: (i32, i32) = (112, 168);
const LOGO_REQ: (i32, i32) = (240, 112);
/// The backdrop's left fade: how wide.
const ART_FADE_W: f32 = 620.0;
/// One strip of the art's left fade, in whole pixels: 155 of them, no step over 1.3% (10 px strips
/// stepped 3%, faint bars on the TV).
const ART_FADE_STRIP: f32 = 4.0;
/// A cell this wide shows its program's art; this wide, its episode title beside its times.
const POSTER_MIN_CELL_W: f32 = 300.0;
const EPISODE_MIN_CELL_W: f32 = 560.0;
/// The tab row: padding round a tab's words, the gap between tabs, the wider one after Plex
/// Channels, and the springs' stiffness (quick: a tab hop, not a page change).
const TAB_PAD: f32 = 26.0;
const TAB_GAP: f32 = 6.0;
const TAB_SPLIT: f32 = 34.0;
const TAB_K: f32 = 260.0;
/// The guide rows' grow spring (the frame's `grown`, [`model::ROW_EXTRA`] taller at 1).
const CELL_K: f32 = 260.0;

/// A cell this wide wears a premiere's NEW.
const NEW_MIN_CELL_W: f32 = 240.0;

/// An UltraBlur corner as the page's ground: held under [`TINT_LUMA`] (a scalar dim, so its hue
/// survives), leaned [`TINT_W`] of the way from the plain ground, and darker at the foot.
fn tint(c: [f32; 3], low: bool) -> [f32; 4] {
    let luma = 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
    let k = if luma > TINT_LUMA { TINT_LUMA / luma } else { 1.0 };
    let held = [c[0] * k, c[1] * k, c[2] * k, 1.0];
    let t = theme::mix(if low { GROUND_LOW } else { GROUND }, held, TINT_W);
    if low { [t[0] * 0.7, t[1] * 0.7, t[2] * 0.7, 1.0] } else { t }
}

/// A cell's fill and inks: focused, on now, or neither.
/// How white the elapsed part of a program on now is painted over its cell at rest.
const ON_NOW_FILL: f32 = 0.24;

fn cell_colors(on: bool, now: bool) -> ([f32; 4], [f32; 4], [f32; 4]) {
    if on {
        (theme::ACCENT, theme::ACCENT_INK, theme::ACCENT_INK)
    } else if now {
        (theme::with_a(theme::TEXT_PRIMARY, 0.13), theme::TEXT_PRIMARY, theme::TEXT_SECONDARY)
    } else {
        (theme::with_a(theme::TEXT_PRIMARY, 0.07), theme::TEXT_PRIMARY, theme::TEXT_SECONDARY)
    }
}

/// A channel as the hero names it: its number and call sign.
fn channel_words(ch: &Channel) -> String {
    ch.words()
}

/// "1h 37m left" / "36m left" for `secs` seconds, rounded up to the minute.
fn left_words(secs: i64) -> String {
    let mins = (secs.max(0) + 59) / 60;
    if mins >= 60 { msg::core_time_left_hours(mins / 60, mins % 60) } else { msg::core_time_left_minutes(mins) }
}

/// `tw`×`th` fitted inside `b`, centred: a logo is never cropped.
fn contain(b: Rect, tw: f32, th: f32) -> Rect {
    let s = (b.w / tw).min(b.h / th);
    let (w, h) = (tw * s, th * s);
    Rect::new(b.x + (b.w - w) * 0.5, b.y + (b.h - h) * 0.5, w, h)
}

/// The countdown ring's dots: the unit circle at the middles of twenty equal steps, clockwise
/// from twelve (screen y grows down). A table, so no transcendental runs on a screen.
const RING: [(f32, f32); 20] = [
    (0.1564, -0.9877), (0.4540, -0.8910), (0.7071, -0.7071), (0.8910, -0.4540), (0.9877, -0.1564),
    (0.9877, 0.1564), (0.8910, 0.4540), (0.7071, 0.7071), (0.4540, 0.8910), (0.1564, 0.9877),
    (-0.1564, 0.9877), (-0.4540, 0.8910), (-0.7071, 0.7071), (-0.8910, 0.4540), (-0.9877, 0.1564),
    (-0.9877, -0.1564), (-0.8910, -0.4540), (-0.7071, -0.7071), (-0.4540, -0.8910), (-0.1564, -0.9877),
];

/// A countdown ring of dots round (`cx`, `cy`), clockwise from twelve: the `aired` fraction dim,
/// the rest lit. Dots, because the painter has no arc.
fn draw_ring(p: Painter, cx: f32, cy: f32, r: f32, aired: f32) {
    const D: f32 = 4.0;
    for (i, (ux, uy)) in RING.iter().enumerate() {
        let t = (i as f32 + 0.5) / RING.len() as f32;
        let col = if t < aired { theme::with_a(theme::TEXT_PRIMARY, 0.22) } else { LEFT_RING };
        p.rrect(Rect::new(cx + r * ux - D * 0.5, cy + r * uy - D * 0.5, D, D), D * 0.5, D * 0.5, col);
    }
}

fn notice_text(n: &plx_data::live_tv::WriteNotice) -> String {
    if !n.ok {
        return msg::live_tv_notice_failed(&n.reason);
    }
    if !n.conflicts.is_empty() {
        return msg::live_tv_notice_conflicts(&n.conflicts.join(", "));
    }
    match n.op {
        WriteOp::Record => msg::live_tv_notice_recorded().to_string(),
        WriteOp::RecordSeries => msg::live_tv_notice_recorded_series().to_string(),
        WriteOp::CancelRule => msg::live_tv_notice_cancelled().to_string(),
        WriteOp::StopRecording => msg::live_tv_notice_stopped().to_string(),
        WriteOp::MoveRule => msg::live_tv_notice_moved().to_string(),
    }
}

/// Days since 1970-01-01 to `(year, month, day)` (the proleptic Gregorian calendar).
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

impl LogicalState for LiveTvScreen {
    fn write(&self, c: &mut Canon) {
        c.u32(self.entry.0)
            .u32(self.mode as u32)
            .u64(self.frame.win_start as u64)
            .u64(self.frame.row_top as u64)
            .u64(self.rec_top as u64)
            .u32(u32::from(self.panel.is_some()))
            .u32(self.view.snap.generation);
    }

    fn probe(&self, out: &mut String) {
        out.push_str(&format!(
            "livetv mode={:?} channels={} row_top={} recs={} panel={}",
            self.mode,
            self.view.channel_count(),
            self.frame.row_top,
            self.recs.len(),
            self.panel.as_ref().map_or(0, |p| p.actions.len()),
        ));
    }
}

impl<H: ContentLike> Machine<H> for LiveTvScreen {
    type Ev = ScreenEvent<H>;

    fn step(&mut self, ev: &Self::Ev, cx: &Cx<'_, H>, fx: &mut Effects<'_, H>) -> Handled {
        // The reader acting first makes the arrival theirs.
        if let ScreenEvent::Input(InputEvent { kind: InputKind::Key { edge: Edge::Down, .. } | InputKind::Click { .. }, .. }) = ev {
            self.seat_pending = false;
        }
        match ev {
            ScreenEvent::Tick(t) => {
                self.tick(t.ms, cx, fx);
                Handled::Yes
            }
            ScreenEvent::Enter(enter) => {
                match enter {
                    Enter::Fresh { .. } if self.own_enters > 0 => self.own_enters -= 1,
                    // The container's mint: a page shown for the first time in this visit.
                    Enter::Fresh { .. } => {
                        if self.has_content() {
                            let elem = match self.mode {
                                Mode::Guide => self.now_cell(),
                                Mode::Recordings => model::REC_BASE,
                            };
                            self.seat_elem(elem, fx);
                        } else {
                            self.seat_pending = true;
                            let pill = self.mode_pill();
                            self.seat_elem(pill, fx);
                        }
                    }
                    Enter::Restored => {}
                }
                fx.invalidate(Provenance::Nav);
                Handled::Yes
            }
            ScreenEvent::Uncover | ScreenEvent::StoreChanged(..) => {
                fx.invalidate(Provenance::Nav);
                Handled::Yes
            }
            ScreenEvent::FocusMoved { by, .. } => {
                if let Some(elem) = cx.focus.current.filter(|k| k.entry == self.entry).map(|k| k.elem) {
                    self.follow(elem, *by != By::Pointer);
                }
                fx.invalidate(Provenance::Input);
                Handled::Yes
            }
            ScreenEvent::Activate(elem) => self.activate(*elem, fx),
            ScreenEvent::PressCommit(_) => match cx.focus.current.filter(|k| k.entry == self.entry) {
                Some(key) => self.activate(key.elem, fx),
                None => Handled::No,
            },
            ScreenEvent::Input(InputEvent { kind: InputKind::Key { key: Key::Back, edge: Edge::Down, .. }, .. }) => {
                self.back(cx, fx)
            }
            // CH▲/CH▼ and PageUp/Down page the guide (four rows) and the Recordings list; the Magic
            // Remote's wheel steps a row per tick. The D-pad is one row per press, and a lineup of
            // several hundred channels is a long way one at a time.
            ScreenEvent::Input(InputEvent { kind: InputKind::Key { sym, wcode, edge: Edge::Down, .. }, .. }) => {
                match plx_ui::consts::page_dir(*sym, *wcode) {
                    Some(d) => {
                        self.page(if d < 0 { Dir::Up } else { Dir::Down }, true, cx, fx);
                        Handled::Yes
                    }
                    None => Handled::No,
                }
            }
            ScreenEvent::Input(InputEvent { kind: InputKind::Wheel { dy }, .. }) => {
                if *dy != 0.0 {
                    self.page(if *dy < 0.0 { Dir::Up } else { Dir::Down }, false, cx, fx);
                }
                Handled::Yes
            }
            _ => Handled::No,
        }
    }


}

impl<H: ContentLike> Focusable<H> for LiveTvScreen {
    fn groups(&self, _cx: &Cx<'_, H>, out: &mut Vec<GroupSpec>) {
        let spec = |id, kind, len, extent| GroupSpec {
            id,
            kind,
            seat: Seat::Projected,
            reachable: AxisMask::BOTH,
            edge: [EdgeRule::Stop; 4],
            extent,
            len,
            elem: ElemKind::Control,
        };
        if let Some(panel) = &self.panel {
            out.push(spec(PANEL, GroupKind::Column, panel.actions.len(), panel_rect(panel.actions.len())));
            return;
        }
        out.push(spec(HEAD, GroupKind::Row { wrap: false }, 2, Rect::new(MARGIN_X, geom::HEAD_Y, 2.0 * geom::PILL_W + geom::PILL_GAP, geom::HEAD_H)));
        if self.tabs_shown() {
            let n = model::tab_sources(&self.view.snap).len();
            out.push(spec(TABS, GroupKind::Row { wrap: false }, n, Rect::new(MARGIN_X, geom::TABS_Y, SCR_W - 2.0 * MARGIN_X, geom::TABS_H)));
        }
        if self.shows_retry() {
            out.push(spec(RETRY_GROUP, GroupKind::Row { wrap: false }, 1, retry_rect()));
        }
        if self.has_content() {
            match self.mode {
                Mode::Guide => out.push(spec(
                    CONTENT,
                    GroupKind::Free,
                    self.view.channel_count(),
                    Rect::new(geom::GRID_X, geom::ROWS_Y, geom::GRID_W, model::ROWS_SHOWN as f32 * geom::ROW_PITCH),
                )),
                Mode::Recordings => out.push(spec(
                    CONTENT,
                    GroupKind::Column,
                    self.recs.len(),
                    Rect::new(MARGIN_X, geom::RECS_Y, SCR_W - 2.0 * MARGIN_X, geom::RECS_BOTTOM - geom::RECS_Y),
                )),
            }
        }
    }

    fn group_of(&self, elem: &u32, _cx: &Cx<'_, H>) -> Option<GroupId> {
        let e = *elem;
        if self.panel.is_some() {
            return model::action_of(e).map(|_| PANEL);
        }
        match e {
            model::HEAD_GUIDE | model::HEAD_RECS => Some(HEAD),
            model::RETRY => self.shows_retry().then_some(RETRY_GROUP),
            _ if model::tab_of(e).is_some() => self.tabs_shown().then_some(TABS),
            _ if model::cell_of(e).is_some() => (self.mode == Mode::Guide && self.has_content()).then_some(CONTENT),
            _ if model::rec_of(e).is_some() => (self.mode == Mode::Recordings && self.has_content()).then_some(CONTENT),
            _ => None,
        }
    }

    fn neighbour(&self, key: FocusKey<u32>, dir: Dir, _cx: &Cx<'_, H>) -> Step<u32> {
        let e = key.elem;
        let to = |elem: u32| Step::Move(FocusKey { entry: key.entry, elem });
        if let Some(i) = model::action_of(e) {
            let n = self.panel.as_ref().map_or(0, |p| p.actions.len());
            return match dir {
                Dir::Up if i > 0 => to(e - 1),
                Dir::Down if i + 1 < n => to(e + 1),
                _ => Step::Edge,
            };
        }
        match e {
            model::HEAD_GUIDE if dir == Dir::Right => return to(model::HEAD_RECS),
            model::HEAD_RECS if dir == Dir::Left => return to(model::HEAD_GUIDE),
            model::HEAD_GUIDE | model::HEAD_RECS | model::RETRY => return Step::Edge,
            _ => {}
        }
        if let Some(i) = model::tab_of(e) {
            let n = model::tab_sources(&self.view.snap).len();
            return match dir {
                Dir::Left if i > 0 => to(e - 1),
                Dir::Right if i + 1 < n => to(e + 1),
                _ => Step::Edge,
            };
        }
        if model::cell_of(e).is_some() {
            return model::guide_neighbour(&self.view, &self.frame, e, dir).map_or(Step::Edge, to);
        }
        if let Some(i) = model::rec_of(e) {
            return match dir {
                Dir::Up if i > 0 => to(e - 1),
                Dir::Down if i + 1 < self.recs.len() => to(e + 1),
                _ => Step::Edge,
            };
        }
        Step::Edge
    }

    fn place(&self, elem: &u32, cx: &Cx<'_, H>, _at: At) -> Option<Placed> {
        let e = *elem;
        let placed = |rect: Rect, index: usize| Placed { rect, rest_rect: rect, clip: Rect::FULL, index: Some(index as u32) };
        if let Some(i) = model::action_of(e) {
            let n = self.panel.as_ref()?.actions.len();
            return (i < n).then(|| placed(action_rect(n, i), i));
        }
        match e {
            model::HEAD_GUIDE => return Some(placed(head_rect(0), 0)),
            model::HEAD_RECS => return Some(placed(head_rect(1), 1)),
            model::RETRY => return Some(placed(retry_rect(), 0)),
            _ => {}
        }
        if let Some(i) = model::tab_of(e) {
            let at = *self.tab_rects(cx.measure).get(i)?;
            let s = self.tab_scroll.pos;
            return Some(placed(Rect::new(at.x - s, at.y, at.w, at.h), i));
        }
        if let Some((row, _)) = model::cell_of(e) {
            return model::elem_rect(&self.view, &self.frame, e).map(|r| placed(r, row));
        }
        if let Some(i) = model::rec_of(e) {
            if i >= self.recs.len() {
                return None;
            }
            let y = model::rec_y(&self.recs, self.rec_top.min(i), i);
            return Some(placed(Rect::new(MARGIN_X, y, SCR_W - 2.0 * MARGIN_X, geom::REC_H), i));
        }
        None
    }

    fn reconcile(&self, want: FocusKey<u32>, _cx: &Cx<'_, H>) -> FocusKey<u32> {
        let e = want.elem;
        if want.entry != self.entry {
            return want;
        }
        let key = |elem| FocusKey { entry: self.entry, elem };
        // An open panel is modal for the pointer too: a hover on the strip or the dimmed guide
        // behind it lands back on the panel.
        if let Some(panel) = &self.panel {
            return match model::action_of(e) {
                Some(i) if i < panel.actions.len() => want,
                _ => key(model::ACTION_BASE),
            };
        }
        if e >= plx_ui::dispatch::STRIP_BASE {
            return want;
        }
        match e {
            model::HEAD_GUIDE | model::HEAD_RECS => return want,
            model::RETRY => return if self.shows_retry() { want } else { key(self.mode_pill()) },
            _ => {}
        }
        if let Some(i) = model::tab_of(e) {
            let n = model::tab_sources(&self.view.snap).len();
            return if self.tabs_shown() && i < n { want } else { key(self.mode_pill()) };
        }
        if let Some((row, slot)) = model::cell_of(e) {
            if self.mode != Mode::Guide || !self.has_content() {
                return key(self.mode_pill());
            }
            // An airing that ended while the page sat (the window moved on with now) gives way
            // to the one on now in its row.
            let ended = self.view.airing(self.frame.base, e).is_some_and(|(_, a)| a.ends_at <= self.frame.win_start);
            if self.view.holds(self.frame.base, e, self.frame.win_start, self.frame.win_start + model::WINDOW_S) && !ended {
                return want;
            }
            if ended {
                return key(self.view.cell_at(self.frame.base, row, self.frame.now.max(self.frame.win_start)));
            }
            let n = self.view.channel_count();
            let t = if slot == model::PLACEHOLDER { self.frame.now.max(self.frame.win_start) } else { self.frame.base + i64::from(slot) * 60 };
            return key(self.view.cell_at(self.frame.base, row.min(n.saturating_sub(1)), t));
        }
        if let Some(i) = model::rec_of(e) {
            if self.mode != Mode::Recordings || !self.has_content() {
                return key(self.mode_pill());
            }
            return if i < self.recs.len() { want } else { key(model::REC_BASE + self.recs.len().saturating_sub(1) as u32) };
        }
        want
    }

    fn seat(&self, group: GroupId, from: Placed, _cx: &Cx<'_, H>) -> FocusKey<u32> {
        let key = |elem| FocusKey { entry: self.entry, elem };
        match group {
            PANEL => key(model::ACTION_BASE),
            RETRY_GROUP => key(model::RETRY),
            HEAD => key(self.mode_pill()),
            TABS => {
                let at = model::tab_sources(&self.view.snap).iter().position(|s| *s == self.view.snap.source).unwrap_or(0);
                key(model::TAB_BASE + at as u32)
            }
            _ => match self.mode {
                Mode::Guide => {
                    let fr = &self.frame;
                    let cy = from.rect.cy();
                    let n = self.view.channel_count();
                    let row = if cy < geom::ROWS_Y {
                        fr.row_top
                    } else {
                        (fr.row_top + ((cy - geom::ROWS_Y) / geom::ROW_PITCH) as usize).min(n.saturating_sub(1))
                    };
                    key(self.view.cell_at(fr.base, row, fr.now.max(fr.win_start)))
                }
                Mode::Recordings => key(model::REC_BASE + self.rec_top.min(self.recs.len().saturating_sub(1)) as u32),
            },
        }
    }
}

impl<H: ContentLike> Screen<H> for LiveTvScreen {
    fn name(&self) -> &'static str {
        Self::WORD
    }
    fn state(&self) -> &dyn LogicalState {
        self
    }
    fn crumb(&self, _: &Cx<'_, H>) -> Option<Cow<'_, str>> {
        None
    }
    fn prepare(&mut self, _: &mut plx_ui::frame::Budget, _: &Cx<'_, H>) {}
    fn draw(&mut self, f: &mut DrawFrame<'_, '_, H>) {
        if plx_media::player::video_window_is_mini() {
            // a channel plays on in the corner: the frame is a hole the video plane shows through
            plx_gfx::gfx::frame_clear_through();
        } else {
            plx_gfx::gfx::frame_clear(theme::CLEAR_RGB.0, theme::CLEAR_RGB.1, theme::CLEAR_RGB.2);
        }
        let p = f.painter.alpha(f.page_alpha);
        self.draw_ground(p);
        self.draw_head(f, p);
        if self.tabs_shown() {
            self.draw_tabs(f, p);
        }
        match self.readout() {
            Some(r) => self.draw_readout(f, p, r),
            None => match self.mode {
                Mode::Guide => self.draw_guide(f, p),
                Mode::Recordings => self.draw_recs(f, p),
            },
        }
        if let Some(panel) = self.panel.clone() {
            self.draw_panel(f, p, &panel);
        }
    }
    fn render(&self) -> RenderStrategy {
        RenderStrategy::Page
    }
    fn focus_source(&self) -> FocusSource {
        FocusSource::Engine
    }
    fn hit_source(&self) -> HitSource {
        HitSource::Engine
    }
    fn links(&self, out: &mut Vec<Link>) {
        if self.panel.is_some() {
            return;
        }
        let strip = plx_ui::containers::tabs::STRIP;
        out.push(Link { from: strip, dir: Dir::Down, to: HEAD });
        out.push(Link { from: HEAD, dir: Dir::Up, to: strip });
        let below = if self.shows_retry() { RETRY_GROUP } else { CONTENT };
        let over = if self.tabs_shown() {
            out.push(Link { from: HEAD, dir: Dir::Down, to: TABS });
            out.push(Link { from: TABS, dir: Dir::Up, to: HEAD });
            TABS
        } else {
            HEAD
        };
        out.push(Link { from: over, dir: Dir::Down, to: below });
        out.push(Link { from: below, dir: Dir::Up, to: over });
    }
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }
    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_call_sign_reads_as_the_station_without_its_digital_suffix() {
        assert_eq!(station_id("KXYZDT"), "KXYZ");
        assert_eq!(station_id("KXYZDT2"), "KXYZ");
        assert_eq!(station_id("WXYZ-HD"), "WXYZ");
        assert_eq!(station_id("WXY-TV"), "WXY");
        assert_eq!(station_id("KQRS"), "KQRS");
        assert_eq!(station_id("WKDT"), "WKDT", "too few letters would be left: kept whole");
        assert_eq!(station_id(""), "");
        let ch = Channel { vcn: "4.2".into(), call_sign: "KXYZDT2".into(), title: "A Network".into(), ..Channel::default() };
        assert_eq!(channel_words(&ch), "4.2 KXYZ");
        // Plex's own channels: a placeholder number and an internal abbreviation (seen on the TV:
        // "000" and seven capitals); they go by their title.
        let plex = Channel { vcn: "000".into(), call_sign: "CHANPRS".into(), title: "A Channel".into(), part_key: "/library/parts/x.m3u8".into(), ..Channel::default() };
        assert_eq!((plex.number(), plex.station(), channel_words(&plex).as_str()), ("", "A Channel", "A Channel"));
        assert_eq!(Channel { vcn: "10.0".into(), ..Channel::default() }.number(), "10.0");
    }

    #[test]
    fn a_fall_is_cut_in_whole_pixel_segments_edge_to_edge() {
        let mut right = 1038.0;
        for j in 0..ART_FALL_SEGS {
            let (x, w) = fall_segment(1038.0, 882.0, j);
            assert_eq!(x, right, "segment {j} starts where the last ended");
            assert_eq!(x, x as i32 as f32, "on a whole pixel");
            right = x + w;
        }
        assert_eq!(right, 1920.0, "the last ends at the edge");
    }

    #[test]
    fn civil_dates_round_the_epoch_and_leap_days() {
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(civil(19_723), (2024, 1, 1));
        assert_eq!(civil(19_782), (2024, 2, 29));
        assert_eq!(civil(20_383), (2025, 10, 22));
    }

    #[test]
    fn the_strip_presses_map_to_tabs_and_the_live_tv_pill_is_here() {
        use plx_ui::widgets::{TAB_KEY_HOME, TAB_KEY_LIVE_TV, TAB_KEY_WATCHLIST};
        assert_eq!(LiveTvScreen::strip_press(TAB_KEY_HOME), Some(Some(LiveTvOut::Tab(HomeTab::Home))));
        assert_eq!(LiveTvScreen::strip_press(TAB_KEY_WATCHLIST), Some(Some(LiveTvOut::Tab(HomeTab::Watchlist))));
        assert_eq!(LiveTvScreen::strip_press(TAB_KEY_LIVE_TV), Some(None));
    }

    #[test]
    fn a_year_numbered_episode_reads_as_the_year_first() {
        assert_eq!(episode_words(2, 5), Some(msg::live_tv_episode("2", "5")));
        let swapped = episode_words(335, 2026).expect("words");
        assert!(swapped.starts_with("2026 ") && swapped.contains("335"), "{swapped}");
        let ordered = episode_words(2026, 335).expect("words");
        assert_eq!(ordered, swapped, "either order reads the same");
        assert_eq!(episode_words(0, 4), None);
    }
}
