//! The Live TV page's model: keys survive the guide filling in, moves keep the time anchor and
//! never scroll into the past, the window follows focus, and each target offers the actions (and
//! asks for the writes) its recording state allows.

use super::model::*;
use plx_data::live_tv::{Availability, Phase, RecordKind, Snapshot};
use plx_data::stores::live_tv::Write;
use plx_plex::plex::live_tv::{Airing, Channel, EpgProvider, Grab, GrabStatus, Subscription};
use plx_ui::screen::Dir;

const NOON: i64 = 1_760_011_200; // 2025-10-09T12:00:00Z
const BASE: i64 = NOON - 12 * 3_600 - 2 * 86_400;

fn airing(ch: usize, begins: i64, mins: i64, guid: &str) -> Airing {
    Airing {
        guid: guid.into(),
        title: format!("{guid}"),
        begins_at: begins,
        ends_at: begins + mins * 60,
        channel_identifier: format!("c{ch}"),
        ..Airing::default()
    }
}

/// `n` channels; row `r` airs 30-minute programs from 11:00 to 17:00, except row 1 which airs
/// hour-long ones, and row 2 which has nothing yet.
fn view(n: usize) -> LiveTvView {
    let snap = Snapshot {
        availability: Availability::Available,
        provider: Some(EpgProvider { identifier: "p".into(), dvr_key: "1".into(), recordable: true, grid: true, ..Default::default() }),
        channels_phase: Phase::Ready,
        channels: (0..n).map(|i| Channel { identifier: format!("c{i}"), title: format!("Ch {i}"), ..Default::default() }).collect(),
        now: NOON + 10 * 60,
        ..Snapshot::default()
    };
    let rows = (0..n)
        .map(|r| {
            let step = if r == 1 { 60 } else { 30 };
            let airings = if r == 2 {
                Vec::new()
            } else {
                (0..(6 * 60 / step)).map(|k| airing(r, NOON - 3_600 + k * step * 60, step, &format!("g{r}-{k}"))).collect()
            };
            RowAirings { airings, pending: r == 2, failed: false }
        })
        .collect();
    LiveTvView { snap, rows }
}

fn frame() -> Frame {
    Frame { base: BASE, win_start: NOON, row_top: 0, now: NOON + 10 * 60, grown: [(0, 0); ROWS_SHOWN] }
}

fn cell(row: usize, begins: i64) -> u32 {
    cell_elem(row, slot_of(BASE, begins))
}

#[test]
fn keys_round_trip_and_stay_clear_of_the_reserved_ranges() {
    let e = cell(2047, NOON);
    assert_eq!(cell_of(e), Some((2047, slot_of(BASE, NOON))));
    assert!(e < REC_BASE && e < HEAD_GUIDE && e < ACTION_BASE && e < plx_ui::dispatch::STRIP_BASE);
    assert_eq!(cell_of(HEAD_GUIDE), None);
    assert_eq!(rec_of(REC_BASE + 3), Some(3));
    assert_eq!(action_of(ACTION_BASE + 2), Some(2));
    assert_eq!(rec_of(HEAD_GUIDE), None);
}

#[test]
fn the_cell_at_a_time_is_the_airing_on_then_else_the_placeholder() {
    let v = view(4);
    assert_eq!(v.cell_at(BASE, 0, NOON + 600), cell(0, NOON));
    assert_eq!(v.cell_at(BASE, 1, NOON + 1_900), cell(1, NOON));
    assert_eq!(v.cell_at(BASE, 2, NOON), cell_elem(2, PLACEHOLDER));
    let (from, to) = (NOON, NOON + WINDOW_S);
    assert!(v.holds(BASE, cell_elem(2, PLACEHOLDER), from, to));
    assert!(!v.holds(BASE, cell_elem(0, PLACEHOLDER), from, to), "a row with a program in the window has no placeholder");
    assert!(v.holds(BASE, cell_elem(0, PLACEHOLDER), NOON + 86_400, NOON + 86_400 + WINDOW_S), "nothing in a later window: it has one");
    assert!(!v.holds(BASE, cell(9, NOON), from, to), "no such row");
}

#[test]
fn up_and_down_keep_the_time_the_focused_cell_starts_on_screen() {
    let v = view(5);
    let f = frame();
    // 12:30 on row 0 (30-min) -> row 1 airs 12:00-13:00 -> that one
    assert_eq!(guide_neighbour(&v, &f, cell(0, NOON + 1_800), Dir::Down), Some(cell(1, NOON)));
    // from row 1's 12:00 back up keeps 12:00
    assert_eq!(guide_neighbour(&v, &f, cell(1, NOON), Dir::Up), Some(cell(0, NOON)));
    // into the row with nothing yet: its placeholder
    assert_eq!(guide_neighbour(&v, &f, cell(1, NOON), Dir::Down), Some(cell_elem(2, PLACEHOLDER)));
    // up from the first row is the guide's edge (the header link takes it)
    assert_eq!(guide_neighbour(&v, &f, cell(0, NOON), Dir::Up), None);
    // down from the last row too
    assert_eq!(guide_neighbour(&v, &f, cell(4, NOON), Dir::Down), None);
}

#[test]
fn left_stops_at_the_airing_on_now_then_the_channel_and_right_walks_the_row() {
    let v = view(3);
    let f = frame();
    assert_eq!(guide_neighbour(&v, &f, cell(0, NOON), Dir::Right), Some(cell(0, NOON + 1_800)));
    assert_eq!(guide_neighbour(&v, &f, cell(0, NOON + 1_800), Dir::Left), Some(cell(0, NOON)));
    // 11:30-12:00 ended before now's half hour: no program further left, the row's channel is
    assert_eq!(guide_neighbour(&v, &f, cell(0, NOON), Dir::Left), Some(channel_elem(0)));
    assert_eq!(guide_neighbour(&v, &f, cell_elem(2, PLACEHOLDER), Dir::Left), Some(channel_elem(2)));
    assert_eq!(guide_neighbour(&v, &f, cell_elem(2, PLACEHOLDER), Dir::Right), None);
}

/// **A row's channel is a stop of its own**: UP/DOWN walk the channels, RIGHT goes back into the
/// row at the program on now, a page moves a page of channels, and its key is clear of the
/// others.
#[test]
fn the_channel_column_walks_the_channels_and_right_enters_the_row_on_now() {
    let v = view(9);
    let f = frame();
    assert_eq!(channel_neighbour(&v, &f, 0, Dir::Up), None);
    assert_eq!(channel_neighbour(&v, &f, 0, Dir::Down), Some(channel_elem(1)));
    assert_eq!(channel_neighbour(&v, &f, 8, Dir::Down), None);
    assert_eq!(channel_neighbour(&v, &f, 0, Dir::Left), None);
    assert_eq!(channel_neighbour(&v, &f, 1, Dir::Right), Some(cell(1, NOON)), "the hour-long program on now");
    assert_eq!(channel_neighbour(&v, &f, 2, Dir::Right), Some(cell_elem(2, PLACEHOLDER)));
    assert_eq!(guide_page(&v, &f, channel_elem(1), Dir::Down), Some(channel_elem(1 + ROWS_SHOWN)));
    assert_eq!(guide_page(&v, &f, channel_elem(7), Dir::Down), Some(channel_elem(8)));
    assert_eq!(row_of(channel_elem(5)), Some(5));
    assert_eq!(row_of(cell(5, NOON)), Some(5));
    assert_eq!(channel_of(cell(5, NOON)), None);
    let e = channel_elem(MAX_ROWS - 1);
    assert!(cell_of(e).is_none() && rec_of(e).is_none() && tab_of(e).is_none() && action_of(e).is_none());
    // following a channel scrolls its row into view, the window where it was
    let moved = follow(&v, &f, channel_elem(6));
    assert_eq!((moved.row_top, moved.win_start), (6 + 1 - ROWS_SHOWN, f.win_start));
    assert!(elem_rect(&v, &f, channel_elem(0)).is_some_and(|r| r.x == geom::CHANNEL_X));
}

/// **The Favorites tab leads the tab row** once the account's favorites were read and name a
/// channel (or it is the tab shown); the DVR's follows, then Plex's channels and their genres.
#[test]
fn the_favorites_tab_leads_once_the_account_has_favorites() {
    use plx_data::live_tv::{ChannelKey, Source};
    let mut snap = view(3).snap;
    assert_eq!(tab_sources(&snap), vec![Source::Dvr]);
    snap.favorites_phase = Phase::Ready;
    assert_eq!(tab_sources(&snap), vec![Source::Dvr], "no favorite names a channel: no tab");
    snap.favorites = vec![ChannelKey::of(&snap.channels[1])];
    snap.cloud_tabs = vec![plx_plex::plex::live_tv::CloudTab { identifier: "news".into(), title: "News".into() }];
    assert_eq!(tab_sources(&snap), vec![Source::Favorites, Source::Dvr, Source::Plex, Source::PlexTab("news".into())]);
    snap.favorites.clear();
    snap.source = Source::Favorites;
    assert_eq!(tab_sources(&snap)[0], Source::Favorites, "the last favorite taken off: the tab stays while shown");
    // the channel panel offers what the account's list allows
    snap.favorite_dvr = true;
    let ch = snap.channels[0].clone();
    assert_eq!(channel_actions(&snap, &ch), vec![Action::Watch, Action::Favorite, Action::Close]);
    snap.favorites = vec![ChannelKey::of(&ch)];
    assert_eq!(channel_actions(&snap, &ch), vec![Action::Watch, Action::Unfavorite, Action::Close]);
    snap.favorites.clear();
    snap.favorite_dvr = false;
    assert_eq!(channel_actions(&snap, &ch), vec![Action::Watch, Action::Close], "the entry's form unknown: nothing to add with");
}

#[test]
fn the_window_follows_focus_right_and_back_but_never_before_now() {
    let v = view(10);
    let f = frame();
    // 13:30 starts in the window's last half hour: it moves to the second tick
    let moved = follow(&v, &f, cell(0, NOON + 3 * 1_800));
    assert_eq!(moved.win_start, NOON + 3_600);
    // back to 12:30: it ends before the window's first quarter -> the window starts there
    let back = follow(&v, &moved, cell(0, NOON + 1_800));
    assert_eq!(back.win_start, NOON + 1_800);
    let home = follow(&v, &back, cell(0, NOON));
    assert_eq!(home.win_start, NOON, "and never before the half hour now is in");
    // rows scroll to keep the focused row in view
    let down = follow(&v, &f, cell(8, NOON));
    assert_eq!(down.row_top, 8 + 1 - ROWS_SHOWN);
    let up = follow(&v, &down, cell(1, NOON));
    assert_eq!(up.row_top, 1);
}

#[test]
fn the_wanted_window_covers_the_rows_shown_and_reaches_past_the_window() {
    let v = view(20);
    let f = Frame { row_top: 5, ..frame() };
    let w = want(&v, &f);
    assert_eq!(w.rows, 3..(5 + ROWS_SHOWN + 2), "two rows of margin each way");
    assert!(w.to > f.win_start + WINDOW_S);
    assert_eq!(w.from, NOON, "nothing that ended before now's half hour is asked for");
    // scrolled right, LEFT's way back is read too (back to now at most)
    let later = Frame { win_start: NOON + 4 * 3_600, ..f };
    assert_eq!(want(&v, &later).from, NOON + 3_600);
}

#[test]
fn a_cell_that_starts_before_the_window_scrolls_it_back_to_its_start() {
    let v = view(3);
    // row 1 airs hour-long programs: focused at 13:00-14:00 with the window at 13:30
    let f = Frame { win_start: NOON + 3 * 1_800, ..frame() };
    assert_eq!(follow(&v, &f, cell(1, NOON + 3_600)).win_start, NOON + 3_600);
}

#[test]
fn an_airing_on_now_offers_watch_first_and_record_and_close() {
    let v = view(2);
    let a = &v.rows[0].airings[2]; // 12:00-12:30, now is 12:10
    let actions = airing_actions(&v.snap, a);
    assert_eq!(actions.first(), Some(&Action::Watch));
    assert!(actions.contains(&Action::Record));
    assert!(!actions.contains(&Action::RecordSeries), "a program with no show offers no series rule");
    assert_eq!(actions.last(), Some(&Action::Close));
    let later = &v.rows[0].airings[5];
    assert!(!airing_actions(&v.snap, later).contains(&Action::Watch), "a later airing cannot be watched yet");
}

#[test]
fn a_scheduled_episode_offers_cancel_and_its_write_deletes_its_rule() {
    let v = view(2);
    let mut a = v.rows[0].airings[4].clone();
    a.grandparent_guid = "show".into();
    a.subscription_id = "41".into();
    let actions = airing_actions(&v.snap, &a);
    assert!(!actions.contains(&Action::Record));
    assert!(actions.contains(&Action::RecordSeries));
    assert!(actions.contains(&Action::CancelRecording));
    let target = Target::Airing { row: 0, airing: a.clone() };
    assert_eq!(write_for(&v.snap, &target, Action::CancelRecording), Some(Write::CancelRule { id: "41".into() }));
    assert_eq!(
        write_for(&v.snap, &target, Action::RecordSeries),
        Some(Write::Record { airing: a.clone(), kind: RecordKind::Series })
    );
}

#[test]
fn a_series_covered_airing_cancels_the_series_rule_or_skips_its_one_recording() {
    let mut v = view(2);
    let mut a = v.rows[0].airings[4].clone();
    a.grandparent_guid = "show".into();
    a.show_subscription_id = "7".into();
    v.snap.scheduled = vec![Grab { key: "/media/grabbers/operations/9".into(), status: GrabStatus::Scheduled, airing: a.clone(), ..Grab::default() }];
    let actions = airing_actions(&v.snap, &a);
    assert!(actions.contains(&Action::CancelSeries) && actions.contains(&Action::CancelRecording));
    assert!(!actions.contains(&Action::RecordSeries));
    let target = Target::Airing { row: 0, airing: a };
    assert_eq!(write_for(&v.snap, &target, Action::CancelSeries), Some(Write::CancelRule { id: "7".into() }));
    assert_eq!(
        write_for(&v.snap, &target, Action::CancelRecording),
        Some(Write::StopGrab { key: "/media/grabbers/operations/9".into() })
    );
}

#[test]
fn an_airing_being_recorded_offers_stop_and_its_write_deletes_the_operation() {
    let mut v = view(2);
    let mut a = v.rows[0].airings[2].clone();
    a.subscription_id = "5".into();
    v.snap.scheduled = vec![Grab { key: "/media/grabbers/operations/3".into(), status: GrabStatus::InProgress, airing: a.clone(), ..Grab::default() }];
    let actions = airing_actions(&v.snap, &a);
    assert_eq!(&actions[..2], &[Action::Watch, Action::StopRecording]);
    let target = Target::Airing { row: 0, airing: a };
    assert_eq!(
        write_for(&v.snap, &target, Action::StopRecording),
        Some(Write::StopGrab { key: "/media/grabbers/operations/3".into() })
    );
}

/// **A Recordings row's Cancel takes a one-off's rule with it** (device 2026-10-27: cancelling
/// only the recording left a "Single" rule in the priority list); a series' row skips the one
/// recording, and a recording under way is stopped, its rule kept.
#[test]
fn a_recordings_row_cancel_deletes_a_one_offs_rule_and_skips_a_series_recording() {
    let mut v = view(2);
    let grab = |rule: &str, status| Grab { key: format!("/media/grabbers/operations/{rule}"), subscription_id: rule.into(), status, ..Grab::default() };
    v.snap.rules = vec![
        Subscription { id: "1".into(), kind: 1, ..Subscription::default() },
        Subscription { id: "2".into(), kind: 2, ..Subscription::default() },
    ];
    let single = Target::Grab(grab("1", GrabStatus::Scheduled));
    assert_eq!(write_for(&v.snap, &single, Action::CancelRecording), Some(Write::CancelRule { id: "1".into() }));
    let series = Target::Grab(grab("2", GrabStatus::Scheduled));
    assert_eq!(write_for(&v.snap, &series, Action::CancelRecording), Some(Write::StopGrab { key: "/media/grabbers/operations/2".into() }));
    let unknown = Target::Grab(grab("9", GrabStatus::Scheduled));
    assert_eq!(write_for(&v.snap, &unknown, Action::CancelRecording), Some(Write::StopGrab { key: "/media/grabbers/operations/9".into() }));
    let running = Target::Grab(grab("1", GrabStatus::InProgress));
    assert_eq!(write_for(&v.snap, &running, Action::StopRecording), Some(Write::StopGrab { key: "/media/grabbers/operations/1".into() }));
}

#[test]
fn a_provider_that_cannot_tune_offers_no_watch() {
    let mut v = view(2);
    v.snap.provider.as_mut().unwrap().dvr_key.clear();
    v.snap.provider.as_mut().unwrap().recordable = false;
    let a = &v.rows[0].airings[2];
    assert_eq!(airing_actions(&v.snap, a), vec![Action::Close]);
}

#[test]
fn the_recordings_list_runs_recordings_first_then_rules_in_priority_order() {
    let mut snap = Snapshot::default();
    let g = |id: &str, begins: i64, status: GrabStatus| Grab { id: id.into(), key: format!("/k/{id}"), status, airing: airing(0, begins, 30, id), ..Grab::default() };
    snap.scheduled = vec![g("later", NOON + 7_200, GrabStatus::Scheduled), g("now", NOON, GrabStatus::InProgress), g("soon", NOON + 3_600, GrabStatus::Scheduled), g("done", NOON - 3_600, GrabStatus::Complete)];
    snap.rules = vec![Subscription { id: "a".into(), ..Subscription::default() }, Subscription { id: "b".into(), ..Subscription::default() }];
    let e = rec_entries(&snap);
    let ids: Vec<String> = e.iter().map(|r| match r { RecEntry::Grab(g, _) => g.id.clone(), RecEntry::Rule(s) => s.id.clone() }).collect();
    assert_eq!(ids, vec!["now", "soon", "later", "a", "b"]);
    // headings: recording now, the day of the next two, the rules
    assert!(rec_heads(&e, 0) && rec_heads(&e, 1) && !rec_heads(&e, 2) && rec_heads(&e, 3) && !rec_heads(&e, 4));
    assert_eq!(rec_y(&e, 0, 0), geom::RECS_Y + geom::REC_HEADING_H);
    assert_eq!(rec_y(&e, 0, 3), geom::RECS_Y + 3.0 * geom::REC_HEADING_H + 3.0 * geom::REC_PITCH);
    // a rule moves to the top with no `after`, to the bottom after the last other rule
    let first = Target::Rule(snap.rules[1].clone());
    assert_eq!(write_for(&snap, &first, Action::MoveTop), Some(Write::MoveRule { id: "b".into(), after: None }));
    let last = Target::Rule(snap.rules[0].clone());
    assert_eq!(write_for(&snap, &last, Action::MoveBottom), Some(Write::MoveRule { id: "a".into(), after: Some("b".into()) }));
    assert_eq!(rule_actions(&snap, &snap.rules[0]), vec![Action::MoveBottom, Action::EditOptions, Action::DeleteRule, Action::Close]);
    assert_eq!(grab_actions(&snap.scheduled[1]), vec![Action::StopRecording, Action::Close]);
    assert_eq!(grab_actions(&snap.scheduled[0]), vec![Action::CancelRecording, Action::Close]);
}

#[test]
fn upcoming_recordings_are_grouped_by_their_local_day_and_rules_alone_get_a_note() {
    let day = 86_400;
    let mut snap = Snapshot { now: 20 * day + 10 * 3_600, utc_offset: -7 * 3_600, ..Snapshot::default() };
    let g = |id: &str, begins: i64| Grab { id: id.into(), status: GrabStatus::Scheduled, airing: airing(0, begins, 30, id), ..Grab::default() };
    // 18:00 UTC is 11:00 local (today); 23:00 UTC 16:00 local (today); the next day's 08:00 UTC is 01:00 local (tomorrow)
    snap.scheduled = vec![g("a", 20 * day + 18 * 3_600), g("b", 20 * day + 23 * 3_600), g("c", 21 * day + 8 * 3_600)];
    let e = rec_entries(&snap);
    let groups: Vec<RecGroup> = e.iter().filter_map(|r| match r { RecEntry::Grab(_, gr) => Some(*gr), RecEntry::Rule(_) => None }).collect();
    let today = local_day(snap.now, snap.utc_offset);
    assert_eq!(groups, vec![RecGroup::Day(today), RecGroup::Day(today), RecGroup::Day(today + 1)]);
    assert!(rec_heads(&e, 0) && !rec_heads(&e, 1) && rec_heads(&e, 2));
    assert!(!rec_note(&e), "recordings are listed: no note");

    snap.scheduled.clear();
    snap.rules = vec![Subscription { id: "r".into(), ..Subscription::default() }];
    let e = rec_entries(&snap);
    assert!(rec_note(&e));
    assert_eq!(rec_y(&e, 0, 0), geom::RECS_Y + 2.0 * geom::REC_HEADING_H + geom::REC_NOTE_H, "the note and its heading above the rules' heading");
}

#[test]
fn a_long_list_scrolls_to_keep_the_focused_row_in_view() {
    let snap = Snapshot {
        rules: (0..20).map(|i| Subscription { id: i.to_string(), ..Subscription::default() }).collect(),
        ..Snapshot::default()
    };
    let e = rec_entries(&snap);
    let top = rec_follow(&e, 0, 15);
    assert!(top > 0);
    assert!(rec_y(&e, top, 15) + geom::REC_H <= geom::RECS_BOTTOM);
    assert_eq!(rec_follow(&e, top, 2), 2);
}

/// **A read of the lists keeps the focus on the row it was on**, by the row's identity: a recording
/// scheduled above it or one that completed moves its index and the focus follows; a row that
/// left the list, or one that stood still, asks for no move.
#[test]
fn a_read_of_the_lists_moves_the_focus_with_the_row_it_was_on() {
    let g = |id: &str, begins: i64, status: GrabStatus| Grab { id: id.into(), key: format!("/k/{id}"), status, airing: airing(0, begins, 30, id), ..Grab::default() };
    let rules = vec![Subscription { id: "a".into(), ..Subscription::default() }, Subscription { id: "b".into(), ..Subscription::default() }];
    let before = rec_entries(&Snapshot { scheduled: vec![g("soon", NOON + 3_600, GrabStatus::Scheduled), g("later", NOON + 7_200, GrabStatus::Scheduled)], rules: rules.clone(), ..Snapshot::default() });
    let ids: Vec<RecId> = before.iter().map(RecEntry::id).collect();
    assert_eq!(ids, [RecId::Grab("/k/soon".into()), RecId::Grab("/k/later".into()), RecId::Rule("a".into()), RecId::Rule("b".into())]);
    assert!(before[0].is(&ids[0]) && !before[0].is(&ids[1]) && !before[2].is(&RecId::Grab("a".into())), "a rule and a recording never share an identity");
    // One started recording and one was scheduled before "later": the row moved down two.
    let after = rec_entries(&Snapshot { scheduled: vec![g("now", NOON, GrabStatus::InProgress), g("sooner", NOON + 1_800, GrabStatus::Scheduled), g("soon", NOON + 3_600, GrabStatus::Scheduled), g("later", NOON + 7_200, GrabStatus::Scheduled)], rules: rules.clone(), ..Snapshot::default() });
    assert_eq!(rec_index_of(&after, &ids[1]), Some(3));
    assert_eq!(rec_reseat(Some(&ids[1]), Some(REC_BASE + 1), &after), Some((REC_BASE + 1, REC_BASE + 3)), "the focus follows 'later'");
    assert_eq!(rec_reseat(Some(&ids[3]), Some(REC_BASE + 3), &after), Some((REC_BASE + 3, REC_BASE + 5)), "a rule, too");
    // "soon" completed: the rows after it move up one; "soon" itself is gone, so its focus clamps as before.
    let done = rec_entries(&Snapshot { scheduled: vec![g("soon", NOON + 3_600, GrabStatus::Complete), g("later", NOON + 7_200, GrabStatus::Scheduled)], rules, ..Snapshot::default() });
    assert_eq!(rec_reseat(Some(&ids[1]), Some(REC_BASE + 1), &done), Some((REC_BASE + 1, REC_BASE)));
    assert_eq!(rec_reseat(Some(&ids[0]), Some(REC_BASE), &done), None, "the row left the list");
    assert_eq!(rec_reseat(Some(&ids[2]), Some(REC_BASE + 2), &before), None, "nothing moved");
    assert_eq!(rec_reseat(Some(&ids[1]), Some(HEAD_RECS), &after), None, "focus off the rows");
    assert_eq!(rec_reseat(None, Some(REC_BASE + 1), &after), None, "no row anchored");
    assert_eq!(rec_reseat(Some(&ids[1]), None, &after), None);
    // A recording the server sent without an operation key has no identity to follow.
    let keyless = rec_entries(&Snapshot { scheduled: vec![Grab { id: "x".into(), status: GrabStatus::Scheduled, airing: airing(0, NOON, 30, "x"), ..Grab::default() }, g("later", NOON + 7_200, GrabStatus::Scheduled)], ..Snapshot::default() });
    assert_eq!(keyless[0].id(), RecId::Grab(String::new()));
    assert_eq!(rec_reseat(Some(&keyless[0].id()), Some(REC_BASE + 1), &keyless), None);
}

/// A list that shrinks under the focused row (a rule deleted with its recordings) shows what fits,
/// from the top, never a blank page.
#[test]
fn a_shrunken_recordings_list_settles_back_into_view() {
    let long = rec_entries(&Snapshot { rules: (0..15).map(|i| Subscription { id: i.to_string(), ..Subscription::default() }).collect(), ..Snapshot::default() });
    let top = rec_follow(&long, 0, 14);
    assert!(top > 0);
    let short = rec_entries(&Snapshot { rules: (0..4).map(|i| Subscription { id: i.to_string(), ..Subscription::default() }).collect(), ..Snapshot::default() });
    assert_eq!(rec_settle(&short, top, 14), 0, "all four fit: from the top");
    assert_eq!(rec_settle(&[], top, 14), 0);
    assert_eq!(rec_settle(&long, top, 14), top, "nothing changed: nothing moves");
}

/// DOWN onto a row whose window has a gap at the anchor lands on that row's last program before
/// it, not on one hours away (which dragged the window there).
#[test]
fn moving_down_into_a_gap_lands_on_the_windows_own_program() {
    let mut v = view(4);
    // row 3: 12:00-12:30, then nothing until 18:30
    v.rows[3].airings = vec![airing(3, NOON, 30, "early"), airing(3, NOON + 6 * 3_600 + 1_800, 30, "late")];
    let f = frame();
    assert_eq!(guide_neighbour(&v, &f, cell(0, NOON + 3_600), Dir::Down).and_then(|e| guide_neighbour(&v, &f, e, Dir::Down)).and_then(|e| guide_neighbour(&v, &f, e, Dir::Down)), Some(cell(3, NOON)), "the 12:00 program, in the window");
    assert_eq!(follow(&v, &f, cell(3, NOON)).win_start, f.win_start, "the window stays");
}

/// A grown row is taller and the rows below it move down by as much; rows above and rows
/// scrolled off are untouched, and a row named twice grows once.
#[test]
fn a_grown_row_is_taller_and_pushes_the_rows_below_it() {
    let f = Frame { grown: [(1, 1000), (1, 500), (0, 0), (0, 0)], ..frame() };
    assert_eq!(row_y(&f, 0), geom::ROWS_Y);
    assert_eq!(row_y(&f, 1), geom::ROWS_Y + geom::ROW_PITCH);
    assert_eq!(row_h(&f, 1), geom::ROW_H + ROW_EXTRA, "the larger of its two entries, once");
    assert_eq!(row_y(&f, 2), geom::ROWS_Y + 2.0 * geom::ROW_PITCH + ROW_EXTRA);
    assert_eq!(row_h(&f, 2), geom::ROW_H);
    let half = Frame { grown: [(0, 500), (3, 1000), (0, 0), (0, 0)], ..frame() };
    assert_eq!(row_y(&half, 1), geom::ROWS_Y + geom::ROW_PITCH + ROW_EXTRA * 0.5);
    assert_eq!(row_y(&half, 3), geom::ROWS_Y + 3.0 * geom::ROW_PITCH + ROW_EXTRA * 0.5, "row 3's own growth moves nothing above it");
    let scrolled = Frame { row_top: 2, grown: [(1, 1000), (0, 0), (0, 0), (0, 0)], ..frame() };
    assert_eq!(row_y(&scrolled, 3), geom::ROWS_Y + geom::ROW_PITCH, "a grown row scrolled off counts for nothing");
    assert_eq!(cell_rect(&f, 1, NOON, NOON + 1_800).unwrap().h, geom::ROW_H + ROW_EXTRA);
}

/// A page move goes four rows at a time, stops at either end, and lands an off-air row on its
/// placeholder.
#[test]
fn a_page_moves_four_rows_and_stops_at_the_ends() {
    let v = view(10);
    let f = frame();
    assert_eq!(guide_page(&v, &f, cell(0, NOON), Dir::Down), Some(cell(4, NOON)));
    assert_eq!(guide_page(&v, &f, cell(8, NOON), Dir::Down), Some(cell(9, NOON)), "the last row, short of a whole page");
    assert_eq!(guide_page(&v, &f, cell(9, NOON), Dir::Down), None);
    assert_eq!(guide_page(&v, &f, cell(9, NOON), Dir::Up), Some(cell(5, NOON)));
    assert_eq!(guide_page(&v, &f, cell(3, NOON), Dir::Up), Some(cell(0, NOON)));
    assert_eq!(guide_page(&v, &f, cell(0, NOON), Dir::Up), None);
    assert_eq!(guide_page(&v, &f, cell(6, NOON), Dir::Up), Some(cell_elem(2, PLACEHOLDER)), "row 2 has nothing: its placeholder");
    assert_eq!(guide_page(&v, &f, cell(0, NOON), Dir::Right), None);
}

/// UP/DOWN onto a long program already well in view leaves the window where the viewer had it,
/// and onto a channel with nothing in the window lands on its placeholder, not hours ahead.
#[test]
fn moving_up_and_down_keeps_the_window_and_meets_off_air_channels_on_their_placeholder() {
    let mut v = view(4);
    // row 3: one long film, 11:00-16:00
    v.rows[3].airings = vec![airing(3, NOON - 3_600, 300, "film")];
    let f = Frame { win_start: NOON + 2 * 3_600, ..frame() };
    let film = cell(3, NOON - 3_600);
    assert_eq!(follow(&v, &f, film).win_start, f.win_start, "the film fills the window: it stays");
    // a cell showing only its last half hour still brings its start in
    let tail = Frame { win_start: NOON + 3_600 + 1_800, ..f };
    let half = cell(0, NOON + 3_600);
    assert!(v.airing(BASE, half).is_some());
    assert_eq!(follow(&v, &tail, half).win_start, NOON + 3_600, "little of it shows: its start comes in");
    // row 2 has nothing at all; row 0's programs end at 17:00
    assert_eq!(guide_neighbour(&v, &f, cell(1, NOON + 2 * 3_600), Dir::Down), Some(cell_elem(2, PLACEHOLDER)));
    let evening = Frame { win_start: NOON + 6 * 3_600, ..frame() };
    assert_eq!(guide_neighbour(&v, &evening, cell_elem(1, PLACEHOLDER), Dir::Up), Some(cell_elem(0, PLACEHOLDER)), "off the air in this window");
}

#[test]
fn rows_around_the_window_come_nearest_first_below_before_above() {
    let rows = |top, n, depth| rows_around(top, n, depth).collect::<Vec<_>>();
    // a window of rows 10..14 of 40: the page below and the page above, one ring at a time
    assert_eq!(rows(10, 40, 0..ROWS_SHOWN), [14, 9, 15, 8, 16, 7, 17, 6]);
    // the next ring picks up where the first stopped
    assert_eq!(rows(10, 40, ROWS_SHOWN..ROWS_SHOWN + 2), [18, 5, 19, 4]);
    // at the top there is nothing above; at the foot nothing below
    assert_eq!(rows(0, 40, 0..3), [4, 5, 6]);
    assert_eq!(rows(36, 40, 0..3), [35, 34, 33]);
    // a lineup shorter than the window has no rows around it
    assert_eq!(rows(0, 3, 0..ROWS_SHOWN), Vec::<usize>::new());
    // never a row twice, never one on screen
    let all = rows(2, 9, 0..20);
    assert_eq!(all, [6, 1, 7, 0, 8]);
}

/// **A recording that conflicts** is preferred by raising its rule to the top of the priority
/// list (it then wins the tuners), or cancelled by deleting it; closing leaves it below the others.
#[test]
fn a_conflicting_recording_is_preferred_by_raising_its_rule_or_cancelled_by_deleting_it() {
    let snap = Snapshot::default();
    let t = Target::Conflict { rule: "42".into(), title: "T".into(), conflicts: vec!["Other".into()] };
    assert_eq!(write_for(&snap, &t, Action::Prefer), Some(Write::MoveRule { id: "42".into(), after: None }));
    assert_eq!(write_for(&snap, &t, Action::CancelNew), Some(Write::CancelRule { id: "42".into() }));
    assert_eq!(write_for(&snap, &t, Action::Close), None);
}

fn setting(id: &str, kind: plx_plex::plex::live_tv::SettingKind, value: &str, options: &[(&str, &str)]) -> plx_plex::plex::live_tv::Setting {
    plx_plex::plex::live_tv::Setting {
        id: id.into(),
        label: id.into(),
        kind,
        value: value.into(),
        options: options.iter().map(|(v, l)| plx_plex::plex::live_tv::SettingOption { value: (*v).into(), label: (*l).into() }).collect(),
        ..Default::default()
    }
}

/// **The options panel shows the visible settings, everyday ones first**, and each kind of value
/// steps its own way: an option list round, a switch in the server's spelling, a number never
/// below 0, a text not at all; the edits apply by setting id and leave the others as they were.
#[test]
fn recording_options_show_the_visible_settings_and_step_each_kind_of_value() {
    use plx_plex::plex::live_tv::SettingKind;
    let mut hidden = setting("hidden", SettingKind::Text, "x", &[]);
    hidden.hidden = true;
    let mut only_new = setting("onlyNewAirings", SettingKind::Bool, "0", &[]);
    only_new.advanced = true;
    let padding = setting("startOffsetMinutes", SettingKind::Int, "0", &[("0", "None"), ("5", "5 minutes"), ("10", "10 minutes")]);
    let keep = setting("keep", SettingKind::Int, "3", &[]);
    let all = vec![hidden, only_new.clone(), padding.clone(), keep.clone()];
    let shown: Vec<&str> = shown_settings(&all).iter().map(|s| s.id.as_str()).collect();
    assert_eq!(shown, ["startOffsetMinutes", "keep", "onlyNewAirings"], "hidden left out, advanced last");
    assert_eq!(step_value(&padding, "0", 1), "5");
    assert_eq!(step_value(&padding, "10", 1), "0", "round");
    assert_eq!(step_value(&padding, "0", -1), "10");
    assert_eq!(step_value(&padding, "7", 1), "0", "a value not offered steps to the first");
    assert_eq!(step_value(&only_new, "0", 1), "1");
    assert_eq!(step_value(&only_new, "true", 1), "false", "the server's spelling kept");
    assert_eq!(step_value(&keep, "3", -1), "2");
    assert_eq!(step_value(&keep, "0", -1), "0", "never below 0");
    let text = setting("name", SettingKind::Text, "abc", &[]);
    assert_eq!(step_value(&text, "abc", 1), "abc");
    let edited = with_edits(&all, &[("keep".into(), "5".into())]);
    assert_eq!(edited.iter().map(|s| s.value.as_str()).collect::<Vec<_>>(), ["x", "0", "0", "5"]);
}

/// A rule's panel offers its recording options.
#[test]
fn a_rule_offers_its_recording_options() {
    let mut snap = Snapshot::default();
    let rule = Subscription { id: "7".into(), ..Default::default() };
    snap.rules = vec![rule.clone()];
    assert_eq!(rule_actions(&snap, &rule), vec![Action::EditOptions, Action::DeleteRule, Action::Close]);
}

fn hub(identifier: &str, kind: &str, n: usize, timed: bool) -> plx_plex::plex::live_tv::LiveHub {
    plx_plex::plex::live_tv::LiveHub {
        identifier: identifier.into(),
        title: identifier.into(),
        kind: kind.into(),
        items: (0..n)
            .map(|i| Airing {
                begins_at: if timed { NOON } else { 0 },
                ends_at: if timed { NOON + 1_800 } else { 0 },
                channel_identifier: format!("c{i}"),
                ..Airing::default()
            })
            .collect(),
        ..Default::default()
    }
}

/// **What's On's rows are the server's hubs**, a hub of channels drawn as channel tiles; the
/// channels watched here lead them only when the server sent no row of its own for them.
#[test]
fn whats_on_rows_are_the_servers_hubs_and_the_recent_channels_lead_when_it_sends_none() {
    let mut snap = Snapshot { hubs: vec![hub("p.onNow", "mixed", 3, true)], ..Snapshot::default() };
    assert_eq!(wo_rows(&snap, "Recent").len(), 1);
    snap.recent = vec![Channel { identifier: "c9".into(), title: "Nine".into(), ..Default::default() }];
    let rows = wo_rows(&snap, "Recent");
    assert_eq!((rows.len(), rows[0].title.as_str(), rows[0].channels), (2, "Recent", true));
    assert_eq!(rows[0].items[0].channel_identifier, "c9");
    assert!(!rows[1].channels);
    snap.hubs.insert(0, hub("channels", "channels", 2, false));
    let rows = wo_rows(&snap, "Recent");
    assert_eq!(rows.len(), 2, "the server's own row of recent channels stands instead");
    assert!(rows[0].channels);
}

/// **What's On moves along a row, and between rows at the same place on screen**; a row scrolls
/// its cards and the page its rows to keep the focused card whole.
#[test]
fn whats_on_moves_along_rows_and_between_them_at_the_same_place_on_screen() {
    let snap = Snapshot { hubs: vec![hub("a", "mixed", 20, true), hub("b", "channel", 3, false), hub("c", "mixed", 20, true)], ..Snapshot::default() };
    let rows = wo_rows(&snap, "Recent");
    let firsts = vec![5, 0, 0];
    assert_eq!(wo_neighbour(&rows, &firsts, wo_elem(0, 7), Dir::Right), Some(wo_elem(0, 8)));
    assert_eq!(wo_neighbour(&rows, &firsts, wo_elem(0, 19), Dir::Right), None);
    assert_eq!(wo_neighbour(&rows, &firsts, wo_elem(0, 0), Dir::Left), None);
    // card 7 is the third shown in row 0 (its first shown is 5): the third of row 1, clamped
    assert_eq!(wo_neighbour(&rows, &firsts, wo_elem(0, 7), Dir::Down), Some(wo_elem(1, 2)));
    assert_eq!(wo_neighbour(&rows, &firsts, wo_elem(1, 1), Dir::Down), Some(wo_elem(2, 1)));
    assert_eq!(wo_neighbour(&rows, &firsts, wo_elem(0, 3), Dir::Up), None, "the header's link takes it");
    assert_eq!(wo_of(wo_elem(2, 19)), Some((2, 19)));
    assert!(wo_of(cell(0, NOON)).is_none() && cell_of(wo_elem(0, 0)).is_none() && rec_of(wo_elem(0, 0)).is_none());
    let per = wo_per_screen();
    assert_eq!(wo_col_follow(0, per), 1, "one past the screen's last card scrolls one");
    assert_eq!(wo_col_follow(5, 2), 2);
    assert_eq!(wo_col_follow(5, 6), 5);
    // the third row is below the foot from the top: following it scrolls the rows
    let top = wo_follow(&rows, 0, 2, geom::WO_Y);
    assert!(top > 0 && wo_row_y(&rows, top, 2, geom::WO_Y) + wo_row_h(&rows[2]) <= geom::WO_BOTTOM);
    let r = wo_card_rect(&rows, 0, 0, 1, 2, geom::WO_Y).expect("rect");
    assert_eq!((r.w, r.h), (geom::WO_CARD_W, geom::WO_TILE_H), "a channel row's tile is square");
}

#[test]
fn a_whats_on_channel_card_names_the_station_without_its_number() {
    let ch = |vcn: &str, title: &str| Airing { channel_vcn: vcn.into(), channel_title: title.into(), ..Airing::default() };
    assert_eq!(wo_channel_name(&ch("4.1", "4.1 KXYZDT")), "KXYZ");
    assert_eq!(wo_channel_name(&ch("4.1", "4.10 KXYZDT")), "4.10 KXYZ", "another number is not this one");
    assert_eq!(wo_channel_name(&ch("", "KXYZDT2")), "KXYZ");
    assert_eq!(wo_channel_name(&ch("7.1", "")), "");
    assert_eq!(wo_channel_name(&ch("4.1", "4.1 KXYZDT (NET)")), "KXYZ (NET)", "a network after the call sign stays");
}

/// A recording the server marks failed before its airing began lost its tuner (a conflict); one
/// that failed once under way did fail.
#[test]
fn a_recording_failed_before_its_airing_is_a_conflict() {
    let g = |begins: i64| Grab { status: GrabStatus::Error, airing: Airing { begins_at: begins, ..Airing::default() }, ..Grab::default() };
    assert!(grab_conflicted(&g(2_000), 1_000));
    assert!(!grab_conflicted(&g(500), 1_000), "under way: a failure");
    assert!(!grab_conflicted(&g(2_000), 0), "no clock yet");
    let scheduled = Grab { status: GrabStatus::Scheduled, ..g(2_000) };
    assert!(!grab_conflicted(&scheduled, 1_000));
}
