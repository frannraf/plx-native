//! **The Recommended page's hero: the focused card, large.** The official app lands a library on
//! its Recommended tab with the focused title's backdrop behind the page and its title, year,
//! runtime, rating and synopsis in the top-left; the rows sit under that block and the hero
//! follows the focus as it moves along them. This is that block for [`super::recommended`]: the
//! same art, logo, text and scrim parts the Home billboard is drawn with, held here as one
//! paint-only component (never in the page's logical state: the page's canon is which card is
//! focused, and this draws from it).
//!
//! The art follows the card focus names ([`FocusHero::bind`], on the tick and again right before
//! the draw, so a press frame draws the card it landed on) by the Home billboard's rules: the new
//! picture dissolves in over the one on screen, which is held until the new one is up; a key held
//! along a row keeps the card it has until the key rests ([`FocusHero::note_key`]); focus leaving
//! the cards (the crumb, a linked heading) keeps the last card, as the official app does.
//!
//! The text block is part of the page's document: it scrolls away with the rows as the reader goes
//! down them (the way Home's grid takes the screen from its billboard), and Up to the first row,
//! or BACK, brings it back. The backdrop is the page's ground and stays, following the focus.
use plx_data::pms::PmsMovie;
use plx_machine::machine::{Edge, Key};
use plx_ui::consts::{MARGIN_X, SCR_H, SCR_W};
use plx_ui::hero_logo::{self, HeroLogo, LogoRung};
use plx_ui::hstack::Item;
use plx_ui::landing_hero::{base_scrim_ramp, COL_W};
use plx_ui::placeholder::{self, Reason};
use plx_ui::screen::DrawFrame;
use plx_ui::text_view::TextView;
use plx_ui::vstack::{Anchor, VStack};
use plx_ui::widgets::{resolve_tex_wh_on, AmbientWash};
use plx_ui::{theme, Painter, Rect, Spring};

use crate::registry::LibraryLike;

/// The room the hero takes above the rows: the first row's heading rests this far under the grid's
/// content line the Library page starts on. Room for the text ladder ([`hero_text_stack`]) and
/// the air under it: a film's title and three synopsis lines, a film's logo and two, an episode's
/// show logo, its own line and one; graded in the tests with the type sizes' line heights.
pub(super) const HERO_H: f32 = 316.0;
/// Where the hero's block starts: the grid's content line (y 244, `page_header::BLOCK_H` under the
/// header row's top); the official app's title stands near the same line under its tab row.
pub(super) const TEXT_TOP: f32 = plx_ui::page_header::TOP + plx_ui::page_header::BLOCK_H;
/// Air between the hero's text and the first row's heading at rest.
const TEXT_AIR: f32 = 28.0;
/// The line the text must end above: the first heading's rest, less the air.
const ROOM_BOTTOM: f32 = TEXT_TOP + HERO_H - TEXT_AIR;
/// Every logo draws this tall: the heroes' floor (`theme::logo::HERO_H_MIN`), as the ceiling too.
/// A hero-rung logo grows past its band UPWARD as paint, up to `HERO_H_MAX` on Home, whose band
/// stands low under open sky; here the crumb row is right above the block, so the band is the
/// drawn height, anchored at its top, and nothing overflows.
const LOGO_H: f32 = theme::logo::HERO_H_MIN;
/// The text title's rung: the one-line headline a step under the billboards' (`size::DISPLAY`;
/// the official app's hero title is about this size, and a 72 px line elides a long title to a
/// few words). Its band is one em, its baseline on the band's bottom.
const TITLE_TEXT_SZ: std::os::raw::c_int = theme::size::DISPLAY;
const TITLE_TEXT_H: f32 = TITLE_TEXT_SZ as f32;
/// The text title's column: a long title runs past the synopsis column before it elides (the
/// official app's runs to about x 1200), under the wedge all the way.
const TITLE_TEXT_W: f32 = 1104.0;
/// The backdrop's resolution, the Home billboard's.
const ART_W: i32 = 1280;
const ART_H: i32 = 720;
/// How long after the last direction-key `Repeat` the key still counts as held, for the case where
/// the key-up never arrives (Home's wash gate keeps the same grace; the panel repeats about every
/// 100 ms, so a live hold refreshes this several times over).
const REPEAT_GRACE_MS: u32 = 300;

/// The hero's logo rung: the heroes' area and floor, the height fixed at [`LOGO_H`], the text
/// fallback at [`TITLE_TEXT_SZ`].
fn logo_rung() -> LogoRung {
    LogoRung::Custom { area: theme::logo::HERO_AREA, h_min: LOGO_H, h_max: LOGO_H, fallback_sz: TITLE_TEXT_SZ }
}

/// One art layer: a texture by `(server, path)`, with the dimensions the resolve reported.
type Tex = (u32, f32, f32);
const NO_TEX: Tex = (0, 0.0, 0.0);

/// A layer's reveal: nothing while its texture is not resident.
fn reveal(tex: Tex, spring: &Spring) -> f32 {
    if tex.0 == 0 { 0.0 } else { spring.pos }
}

pub(super) struct FocusHero {
    /// The card shown, `(server, ratingKey)`.
    keyed: Option<(u16, String)>,
    /// The art the card names, `(server, path)`; `None` for a card without one.
    art_path: Option<(u16, String)>,
    /// The art that was on screen when the card changed, kept under the new one until it is up.
    held: Option<(u16, String)>,
    tex: Tex,
    outgoing_tex: Tex,
    /// The new art's reveal, 0..1.
    art: Spring,
    /// The held art's own reveal: what that picture had when it became the held one, risen to 1.
    outgoing_art: Spring,
    /// The card's art path while it is wanted on screen and has no texture yet (`bind` sets it,
    /// `draw_art` counts it: a placeholder count belongs at the draw). Empty otherwise.
    pending_art: String,
    /// The tick (ms) of the last direction-key `Repeat`, `None` once that key is released.
    repeat_ms: Option<u32>,
}

impl FocusHero {
    pub(super) fn new() -> Self {
        Self {
            keyed: None,
            art_path: None,
            held: None,
            tex: NO_TEX,
            outgoing_tex: NO_TEX,
            art: Spring::at(0.0),
            outgoing_art: Spring::at(0.0),
            pending_art: String::new(),
            repeat_ms: None,
        }
    }

    /// Forget the card and its pictures: the page switched library, and the next card's hero
    /// stands on the bare ground as the first one did.
    pub(super) fn clear(&mut self) {
        *self = Self::new();
    }

    /// The card the hero shows, as `(server, ratingKey)`; `None` before any card was focused.
    pub(super) fn keyed(&self) -> Option<(u16, &str)> {
        self.keyed.as_ref().map(|(sid, rk)| (*sid, rk.as_str()))
    }

    /// Read a direction key's edge off the page's input (every key reaches the page, handled or
    /// not): a `Repeat` marks the key as held, an `Up` releases it. A first `Down` marks nothing,
    /// so a single press re-keys the hero at once.
    pub(super) fn note_key(&mut self, key: Key, edge: Edge, ms: u32) {
        if !matches!(key, Key::Left | Key::Right | Key::Up | Key::Down) {
            return;
        }
        match edge {
            Edge::Repeat => self.repeat_ms = Some(ms),
            Edge::Up => self.repeat_ms = None,
            Edge::Down => {}
        }
    }

    /// Is a direction key being held right now? A hero re-keyed to every card of a held run
    /// restarts a two-layer, full-screen dissolve each frame and re-blurs the top bar's glass over
    /// it (Home's wash holds for the same reason), so the hero keeps its card for the run and
    /// re-keys once the key rests.
    fn held_key(&self, ms: u32) -> bool {
        self.repeat_ms.is_some_and(|r| ms.wrapping_sub(r) <= REPEAT_GRACE_MS)
    }

    /// Bind the layers to `item`, the card focus names at tick `ms`; `None` (focus off the cards)
    /// keeps the card shown. Idempotent: a second call with the same card changes nothing but the
    /// textures, which are resolved on EVERY call: the resolve is the texture cache's use, and a
    /// held picture nobody resolves is evicted under the page's posters.
    pub(super) fn bind(&mut self, item: Option<&PmsMovie>, ms: u32) {
        self.bind_with(item, ms, |(sid, art)| resolve_tex_wh_on(*sid, art, ART_W, ART_H, 0));
    }

    fn bind_with(&mut self, item: Option<&PmsMovie>, ms: u32, resolve: impl Fn(&(u16, String)) -> Tex) {
        let item = item.filter(|item| !item.rk.is_empty());
        if let Some(item) = item {
            let key = (item.sid.raw(), item.rk.clone());
            // A held key keeps the card it has; the first card is taken at once.
            if self.keyed.as_ref() != Some(&key) && !(self.keyed.is_some() && self.held_key(ms)) {
                // The picture on screen stays under the next one while that is fetched: the one
                // bound last when it shows more than what it has left uncovered of the one held
                // under it, else that held one (a card without art, or whose art never came,
                // leaves the held one up).
                let incoming = reveal(self.tex, &self.art);
                let outgoing = reveal(self.outgoing_tex, &self.outgoing_art);
                if incoming >= outgoing * (1.0 - incoming) {
                    self.held = self.art_path.take().filter(|_| incoming > 0.01);
                    self.outgoing_art.jump(incoming);
                }
                self.keyed = Some(key);
                self.art_path = (!item.art.is_empty()).then(|| (item.sid.raw(), item.art.clone()));
                self.tex = self.art_path.as_ref().map_or(NO_TEX, &resolve);
                // Over a held picture the new one dissolves in, resident or not; over nothing it
                // is up at once when it can be.
                self.art.jump(f32::from(self.tex.0 != 0 && self.held.is_none()));
                // The top bar's glass blurs what is under it, and that just changed.
                plx_gfx::gfx::control_ground_invalidate();
            }
        }
        self.tex = self.art_path.as_ref().map_or(NO_TEX, &resolve);
        self.outgoing_tex = self.held.as_ref().map_or(NO_TEX, &resolve);
        self.pending_art.clear();
        if let Some((_, art)) = self.art_path.as_ref().filter(|_| self.tex.0 == 0) {
            self.pending_art.push_str(art);
        }
    }

    /// Advance the reveals; let the held picture go once the new one is up, or once the held one
    /// is no longer resident and there is nothing left to hold.
    pub(super) fn step(&mut self, dt: f32) {
        // decorative: the dissolve must not count as the page being busy
        plx_machine::idle::decorative(|| {
            if self.tex.0 != 0 {
                self.art.step(1.0, AmbientWash::K, dt);
            }
            if self.outgoing_tex.0 != 0 {
                self.outgoing_art.step(1.0, AmbientWash::K, dt);
            }
        });
        if self.held.is_some() && (reveal(self.tex, &self.art) > 0.999 || self.outgoing_tex.0 == 0) {
            self.held = None;
            self.outgoing_tex = NO_TEX;
        }
    }

    /// How much picture is on screen: the more visible of the two layers, 0..1.
    pub(super) fn shown(&self) -> f32 {
        reveal(self.tex, &self.art).max(reveal(self.outgoing_tex, &self.outgoing_art))
    }

    /// The backdrop under the whole page: the held picture, the new one dissolving over it, and
    /// the Home billboard's scrims over both (the ramp darkening toward the rows, the wedge under
    /// the text).
    pub(super) fn draw_art(&self, p: Painter) {
        if !self.pending_art.is_empty() {
            placeholder::note(p, Reason::LibraryHeroBackdrop, &self.pending_art);
        }
        blit(p, self.outgoing_tex, reveal(self.outgoing_tex, &self.outgoing_art));
        blit(p, self.tex, reveal(self.tex, &self.art));
        let shown = self.shown();
        if shown <= 0.01 {
            return;
        }
        let [y0, knee, mid, foot] = base_scrim_ramp(shown);
        p.rect(Rect::new(0.0, y0, SCR_W, knee - y0), 0.0, theme::scrim(0.0), theme::scrim(mid), 0.0);
        p.rect(Rect::new(0.0, knee, SCR_W, SCR_H - knee), 0.0, theme::scrim(mid), theme::scrim(foot), 0.0);
        // The wedge under the copy, its knee lifted to the block's top: the shared one feathers in
        // down to where the billboards anchor their text, which is under this block's synopsis.
        plx_ui::widgets::hero_scrim_lifted(p, shown, TEXT_TOP);
    }

    /// The text block: the title (its logo when the server has one), an episode's own line, the
    /// meta line and the synopsis, stacked down from [`TEXT_TOP`] and kept clear of the rows;
    /// `dy` is the page's scroll, which carries the block off the top with the rows.
    pub(super) fn draw_text<H: LibraryLike>(&self, f: &DrawFrame<'_, '_, H>, p: Painter, item: &PmsMovie, dy: f32) {
        // Scrolled off the top with the rows: the block always ends above the room's bottom.
        if ROOM_BOTTOM + dy <= 0.0 {
            return;
        }
        let lines = HeroLines::of(item);
        let rung = logo_rung();
        // The band is the drawn height: the logo's once it is up, in the synopsis column, else
        // the text's em, in the title's wider one. The lines under it move down once when a logo
        // lands, as the title itself cuts from text to logo.
        let (title_h, title_w) = plx_ui::tex::logo_src(item.sid.raw(), lines.logo_rk)
            .map_or((TITLE_TEXT_H, TITLE_TEXT_W), |(_, w, h)| (hero_logo::fit(rung, w, h, COL_W).1, COL_W));
        let sub_view = lines.sub.as_deref().map(|sub| TextView::new(sub, theme::size::BODY, theme::TEXT_PRIMARY).max_lines(1));
        let meta_view = TextView::new(&lines.meta, theme::size::BODY, theme::TEXT_SECONDARY).max_lines(1);
        let sub_h = sub_view.as_ref().map(|v| v.measure_h(COL_W));
        let meta_h = meta_view.measure_h(COL_W);
        // The synopsis takes what is left over, three lines at most, fewer when the block would
        // reach the rows (a logo's band, an episode's extra line), none when not even one fits.
        let mut synopsis = None;
        for lines_max in (1..=plx_ui::HERO_SYN_MAXLINES).rev() {
            if item.summary.is_empty() {
                break;
            }
            let view = plx_ui::hero_synopsis(&item.summary, "").max_lines(lines_max);
            let h = view.measure_h(COL_W);
            if hero_text_stack(title_h, sub_h, meta_h, Some(h)).bottom <= ROOM_BOTTOM {
                synopsis = Some((view, h));
                break;
            }
        }
        let stack = hero_text_stack(title_h, sub_h, meta_h, synopsis.as_ref().map(|(_, h)| *h));
        HeroLogo::new(item.sid.raw(), lines.logo_rk, lines.title, rung)
            .draw(p, Rect::new(MARGIN_X, stack.title_y + dy, title_w, title_h), f.measure);
        if let (Some(view), Some(y)) = (sub_view, stack.sub_y) {
            view.draw(p, Rect::new(MARGIN_X, y + dy, COL_W, 0.0));
        }
        meta_view.draw(p, Rect::new(MARGIN_X, stack.meta_y + dy, COL_W, 0.0));
        if let (Some((view, _)), Some(y)) = (synopsis, stack.synopsis_y) {
            view.draw(p, Rect::new(MARGIN_X, y + dy, COL_W, 0.0));
        }
    }
}

/// One art layer over the whole screen, covering it as the billboard does.
fn blit(p: Painter, tex: Tex, alpha: f32) {
    if tex.0 == 0 || alpha <= 0.01 {
        return;
    }
    p.tex(tex.0, Rect::new(0.0, 0.0, SCR_W, SCR_H).cover(tex.1, tex.2), 0.0, theme::with_a(theme::TINT_WHITE, alpha));
}

/// Where the hero's lines stand: the title's top, the episode line's and the synopsis's when
/// there are any, the meta line's, and the block's bottom edge.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct HeroTextStack {
    pub title_y: f32,
    pub sub_y: Option<f32>,
    pub meta_y: f32,
    pub synopsis_y: Option<f32>,
    pub bottom: f32,
}

/// The ladder down from [`TEXT_TOP`]: title, `MD`, an episode's line, `SM`, meta, `SM`, synopsis.
pub(super) fn hero_text_stack(title_h: f32, sub_h: Option<f32>, meta_h: f32, synopsis_h: Option<f32>) -> HeroTextStack {
    let items = [
        Item::new(title_h),
        sub_h.map_or(Item::absent(), Item::new),
        Item::new(meta_h).before(if sub_h.is_some() { theme::space::SM } else { theme::space::MD }),
        synopsis_h.map_or(Item::absent(), |h| Item::new(h).before(theme::space::SM)),
    ];
    let stack = VStack::new(&items, theme::space::MD, Anchor::Top(TEXT_TOP));
    let mut spans = stack.placed();
    let title = spans.next().expect("the title is always present");
    let sub = sub_h.map(|_| spans.next().expect("the episode line is present"));
    let meta = spans.next().expect("the meta line is always present");
    let synopsis = synopsis_h.map(|_| spans.next().expect("the synopsis is present"));
    let bottom = synopsis.map_or(meta.y + meta.h, |s| s.y + s.h);
    HeroTextStack { title_y: title.y, sub_y: sub.map(|s| s.y), meta_y: meta.y, synopsis_y: synopsis.map(|s| s.y), bottom }
}

/// The hero's words for a card: what the title band shows (an episode shows its show), the
/// episode's own line, and the meta line the official app's hero reads (year, runtime, rating,
/// then how much of a started title is left).
#[derive(Debug, PartialEq)]
pub(super) struct HeroLines<'a> {
    pub title: &'a str,
    /// The ratingKey the title's logo is looked up by: an episode's show (an episode has no logo
    /// of its own).
    pub logo_rk: &'a str,
    pub sub: Option<String>,
    pub meta: String,
}

impl<'a> HeroLines<'a> {
    pub(super) fn of(item: &'a PmsMovie) -> Self {
        let episode = item.kind == 3;
        let title = if episode && !item.show_title.is_empty() { item.show_title.as_str() } else { item.title.as_str() };
        let logo_rk = if episode && !item.show_rk.is_empty() { item.show_rk.as_str() } else { item.rk.as_str() };
        let sub = episode.then(|| {
            let mut text = plx_ui::fmt::episode_address(i64::from(item.season_index), i64::from(item.ep_index));
            if !text.is_empty() && !item.title.is_empty() {
                text.push_str(" \u{b7} ");
            }
            text.push_str(&item.title);
            text
        }).filter(|s| !s.is_empty());
        let mut parts: Vec<String> = Vec::new();
        if item.year > 0 {
            parts.push(item.year.to_string());
        }
        let dur_ms = item.dur_ns / 1_000_000;
        if dur_ms >= 60_000 {
            parts.push(plx_ui::fmt::duration(dur_ms));
        }
        if !item.rating.is_empty() {
            parts.push(item.rating.clone());
        }
        if item.resume_ms > 0 && dur_ms > item.resume_ms {
            parts.push(plx_ui::fmt::time_left(dur_ms - item.resume_ms));
        }
        HeroLines { title, logo_rk, sub, meta: parts.join(" \u{b7} ") }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plx_plex::plex::ServerId;

    fn movie() -> PmsMovie {
        PmsMovie {
            sid: ServerId::from_raw(9),
            rk: "m1".into(),
            title: "A Film".into(),
            year: 2007,
            rating: "R".into(),
            dur_ns: 111 * 60 * 1_000_000_000,
            summary: "The legendary tale.".into(),
            art: "/library/metadata/1/art/1".into(),
            ..Default::default()
        }
    }

    /// A card by ratingKey, its art named after it.
    fn card(rk: &str) -> PmsMovie {
        PmsMovie { rk: rk.into(), art: format!("/art/{rk}"), ..movie() }
    }

    /// A texture cache in which the listed art paths are resident.
    fn resident<'a>(paths: &'a [&'a str]) -> impl Fn(&(u16, String)) -> Tex + 'a {
        move |(_, art)| if paths.iter().any(|p| *p == art) { (7, 1280.0, 720.0) } else { NO_TEX }
    }

    fn line_h(sz: std::os::raw::c_int) -> f32 {
        TextView::new("x", sz, theme::TEXT_PRIMARY).max_lines(1).line_h()
    }

    #[test]
    fn a_film_reads_year_runtime_rating_and_what_is_left() {
        let film = movie();
        let lines = HeroLines::of(&film);
        assert_eq!(lines.title, "A Film");
        assert_eq!(lines.logo_rk, "m1");
        assert_eq!(lines.sub, None);
        assert_eq!(lines.meta, "2007 \u{b7} 1h 51m \u{b7} R");
        let started = PmsMovie { resume_ms: 40 * 60_000, ..movie() };
        assert_eq!(HeroLines::of(&started).meta, "2007 \u{b7} 1h 51m \u{b7} R \u{b7} 1h 11m left");
        let bare = PmsMovie { year: 0, rating: String::new(), dur_ns: 0, ..movie() };
        assert_eq!(HeroLines::of(&bare).meta, "");
    }

    #[test]
    fn an_episode_shows_its_show_and_its_own_line() {
        let episode = PmsMovie {
            kind: 3,
            title: "Pilot".into(),
            show_title: "The Series".into(),
            show_rk: "s1".into(),
            season_index: 1,
            ep_index: 1,
            ..movie()
        };
        let lines = HeroLines::of(&episode);
        assert_eq!(lines.title, "The Series");
        assert_eq!(lines.logo_rk, "s1");
        assert_eq!(lines.sub.as_deref(), Some("S1 \u{b7} E1 \u{b7} Pilot"));
        // An episode whose show is not named keeps its own title on the band; one whose show has
        // a name but no key keeps the name and looks its logo up by its own key.
        let orphan = PmsMovie { show_title: String::new(), ..episode.clone() };
        assert_eq!(HeroLines::of(&orphan).title, "Pilot");
        let unkeyed = PmsMovie { show_rk: String::new(), ..episode };
        assert_eq!((HeroLines::of(&unkeyed).title, HeroLines::of(&unkeyed).logo_rk), ("The Series", "m1"));
    }

    /// The ladder starts on the content line and, in each of its shapes, ends above the rows with
    /// the synopsis lines the hero promises: a film's title and three lines, a film's logo and
    /// two, an episode's title, own line and two, an episode's show logo, own line and one.
    #[test]
    fn the_text_ladder_starts_on_the_content_line_and_ends_above_the_rows() {
        let (body, syn) = (line_h(theme::size::BODY), plx_ui::hero_syn_h(1));
        let film = hero_text_stack(TITLE_TEXT_H, None, body, Some(3.0 * syn));
        assert_eq!(film.title_y, TEXT_TOP);
        assert_eq!(film.sub_y, None);
        assert!((film.meta_y - (TEXT_TOP + TITLE_TEXT_H + theme::space::MD)).abs() < 0.01);
        assert!((film.synopsis_y.unwrap() - (film.meta_y + body + theme::space::SM)).abs() < 0.01);
        assert!((film.bottom - (film.synopsis_y.unwrap() + 3.0 * syn)).abs() < 0.01);
        assert!(film.bottom <= ROOM_BOTTOM, "a film's three synopsis lines fit above the rows: {}", film.bottom);
        let logo_film = hero_text_stack(LOGO_H, None, body, Some(2.0 * syn));
        assert!(logo_film.bottom <= ROOM_BOTTOM, "a film's logo and two lines fit: {}", logo_film.bottom);
        let episode = hero_text_stack(TITLE_TEXT_H, Some(body), body, Some(2.0 * syn));
        assert!((episode.sub_y.unwrap() - (TEXT_TOP + TITLE_TEXT_H + theme::space::MD)).abs() < 0.01);
        assert!((episode.meta_y - (episode.sub_y.unwrap() + body + theme::space::SM)).abs() < 0.01);
        assert!((episode.synopsis_y.unwrap() - (episode.meta_y + body + theme::space::SM)).abs() < 0.01);
        assert!(episode.bottom <= ROOM_BOTTOM, "an episode's own line and two synopsis lines fit: {}", episode.bottom);
        let logo_episode = hero_text_stack(LOGO_H, Some(body), body, Some(syn));
        assert!(logo_episode.bottom <= ROOM_BOTTOM, "an episode's show logo, own line and one fit: {}", logo_episode.bottom);
        // Without a synopsis the block ends under its meta line.
        let terse = hero_text_stack(LOGO_H, Some(body), body, None);
        assert!((terse.bottom - (terse.meta_y + body)).abs() < 0.01);
    }

    /// Every logo draws the heroes' floor height, whatever its shape (a very wide mark shorter,
    /// when the column binds), so the band is the drawn height and nothing reaches the crumb row.
    #[test]
    fn every_logo_draws_the_floor_height_and_stays_under_the_content_line() {
        for aspect in [0.5f32, 1.0, 2.0, 3.0, 5.0, 8.0, 12.0] {
            let (w, h) = hero_logo::fit(logo_rung(), aspect * 300.0, 300.0, COL_W);
            assert!(h <= LOGO_H + 1e-3, "a {aspect}:1 logo draws {h} tall");
            assert!(w <= COL_W + 1e-3, "a {aspect}:1 logo draws {w} wide");
            assert!((h - LOGO_H).abs() < 1e-3 || w >= COL_W - 1e-3, "a {aspect}:1 logo is short of the floor at {h} with room to spare");
        }
        assert!(TEXT_TOP + LOGO_H < ROOM_BOTTOM);
        // The text title's column stays under the wedge's reach.
        assert!(MARGIN_X + TITLE_TEXT_W < 0.8 * SCR_W);
    }

    #[test]
    fn the_hero_keeps_its_card_while_focus_is_off_the_cards() {
        let mut hero = FocusHero::new();
        assert_eq!(hero.keyed(), None);
        hero.bind_with(None, 0, resident(&[]));
        assert_eq!(hero.keyed(), None);
        hero.bind_with(Some(&card("m1")), 0, resident(&[]));
        assert_eq!(hero.keyed(), Some((9, "m1")));
        hero.bind_with(None, 0, resident(&[]));
        assert_eq!(hero.keyed(), Some((9, "m1")), "the crumb keeps the last card's hero");
        // A placeholder card, with no key yet, is not a card.
        hero.bind_with(Some(&PmsMovie { rk: String::new(), ..movie() }), 0, resident(&[]));
        assert_eq!(hero.keyed(), Some((9, "m1")));
        hero.clear();
        assert_eq!(hero.keyed(), None);
        assert_eq!(hero.shown(), 0.0);
    }

    /// Home's rules: a resident first picture is up at once; the next dissolves in over it, which
    /// is held at full until the new one is up, then let go.
    #[test]
    fn a_resident_first_picture_is_up_at_once_and_the_next_dissolves_over_it() {
        let up = resident(&["/art/m1", "/art/m2"]);
        let mut hero = FocusHero::new();
        hero.bind_with(Some(&card("m1")), 0, &up);
        assert_eq!((hero.art.pos, hero.shown(), hero.held.is_none()), (1.0, 1.0, true));
        assert!(hero.pending_art.is_empty());
        hero.bind_with(Some(&card("m2")), 0, &up);
        assert_eq!(hero.held.as_ref().map(|(_, art)| art.as_str()), Some("/art/m1"));
        assert_eq!((hero.art.pos, hero.outgoing_art.pos, hero.shown()), (0.0, 1.0, 1.0), "the old picture carries the frame");
        for _ in 0..600 {
            hero.step(1.0 / 60.0);
        }
        assert!(hero.art.pos > 0.999, "the new picture is up: {}", hero.art.pos);
        assert!(hero.held.is_none() && hero.outgoing_tex.0 == 0, "…and the old one is let go");
    }

    /// A card without art, or whose art has not come, leaves the picture on screen up; the card
    /// after it dissolves in over that picture, not over the bare ground.
    #[test]
    fn a_card_without_art_keeps_the_picture_up_for_the_card_after_it() {
        let up = resident(&["/art/m1", "/art/m3"]);
        let mut hero = FocusHero::new();
        hero.bind_with(Some(&card("m1")), 0, &up);
        hero.bind_with(Some(&PmsMovie { rk: "m2".into(), art: String::new(), ..movie() }), 0, &up);
        assert_eq!(hero.held.as_ref().map(|(_, art)| art.as_str()), Some("/art/m1"));
        assert!(hero.pending_art.is_empty(), "nothing is on its way for a card without art");
        for _ in 0..60 {
            hero.step(1.0 / 60.0);
        }
        assert_eq!(hero.shown(), 1.0, "the held picture stays up");
        hero.bind_with(Some(&card("m3")), 0, &up);
        assert_eq!(hero.held.as_ref().map(|(_, art)| art.as_str()), Some("/art/m1"), "the next picture comes in over it");
        assert_eq!(hero.art.pos, 0.0);
        // The same with art that is still being fetched: a counted wait, under which the old
        // picture stays; once the art lands it dissolves in over the old one.
        let mut hero = FocusHero::new();
        hero.bind_with(Some(&card("m1")), 0, &up);
        hero.bind_with(Some(&card("m4")), 0, &up);
        assert_eq!(hero.pending_art, "/art/m4");
        assert_eq!((hero.shown(), hero.held.is_some()), (1.0, true));
        for _ in 0..60 {
            hero.step(1.0 / 60.0);
        }
        assert!(hero.held.is_some(), "the old picture is held until the new one is resident");
        let landed = resident(&["/art/m1", "/art/m4"]);
        hero.bind_with(Some(&card("m4")), 0, &landed);
        assert!(hero.pending_art.is_empty() && hero.art.pos == 0.0, "the landed picture dissolves in from nothing");
    }

    /// A dissolve interrupted by the next card keeps its more visible layer under that card: the
    /// old picture while the new one was barely in, the new one once it was mostly in.
    #[test]
    fn an_interrupted_dissolve_keeps_its_more_visible_layer() {
        let up = resident(&["/art/m1", "/art/m2", "/art/m3", "/art/m4"]);
        let mut hero = FocusHero::new();
        hero.bind_with(Some(&card("m1")), 0, &up);
        hero.bind_with(Some(&card("m2")), 0, &up);
        hero.art.jump(0.3);
        hero.bind_with(Some(&card("m3")), 0, &up);
        assert_eq!(hero.held.as_ref().map(|(_, art)| art.as_str()), Some("/art/m1"), "the barely-in picture is let go");
        assert_eq!(hero.outgoing_art.pos, 1.0);
        hero.art.jump(0.8);
        hero.bind_with(Some(&card("m4")), 0, &up);
        assert_eq!(hero.held.as_ref().map(|(_, art)| art.as_str()), Some("/art/m3"), "the mostly-in picture is kept");
        assert_eq!(hero.outgoing_art.pos, 0.8, "…at the reveal it had, rising from there");
        assert_eq!(hero.art.pos, 0.0);
    }

    /// A direction key held along a row keeps the card the hero has until the key rests; a single
    /// press re-keys at once, and a key-up that never arrives is forgiven after the grace.
    #[test]
    fn a_held_direction_key_keeps_the_card_until_it_rests() {
        let up = resident(&[]);
        let mut hero = FocusHero::new();
        hero.bind_with(Some(&card("m1")), 0, &up);
        hero.note_key(Key::Right, Edge::Down, 50);
        hero.bind_with(Some(&card("m2")), 60, &up);
        assert_eq!(hero.keyed(), Some((9, "m2")), "a press re-keys at once");
        hero.note_key(Key::Right, Edge::Repeat, 160);
        hero.bind_with(Some(&card("m3")), 170, &up);
        assert_eq!(hero.keyed(), Some((9, "m2")), "a held key keeps the card");
        hero.note_key(Key::Right, Edge::Repeat, 260);
        hero.bind_with(Some(&card("m4")), 270, &up);
        assert_eq!(hero.keyed(), Some((9, "m2")));
        hero.note_key(Key::Right, Edge::Up, 300);
        hero.bind_with(Some(&card("m4")), 310, &up);
        assert_eq!(hero.keyed(), Some((9, "m4")), "the key rests: the card the focus is on");
        hero.note_key(Key::Down, Edge::Repeat, 400);
        hero.bind_with(Some(&card("m5")), 400 + REPEAT_GRACE_MS + 1, &up);
        assert_eq!(hero.keyed(), Some((9, "m5")), "a lost key-up is forgiven after the grace");
        hero.note_key(Key::Ok, Edge::Repeat, 1000);
        hero.bind_with(Some(&card("m6")), 1010, &up);
        assert_eq!(hero.keyed(), Some((9, "m6")), "only a direction key holds");
    }

    /// A held picture the cache evicted is let go: there is nothing left to hold.
    #[test]
    fn an_evicted_held_picture_is_let_go() {
        let mut hero = FocusHero::new();
        hero.bind_with(Some(&card("m1")), 0, resident(&["/art/m1"]));
        hero.bind_with(Some(&card("m2")), 0, resident(&["/art/m1"]));
        assert!(hero.held.is_some());
        hero.bind_with(Some(&card("m2")), 0, resident(&[]));
        hero.step(1.0 / 60.0);
        assert!(hero.held.is_none() && hero.shown() == 0.0);
    }
}
