//! The Live TV store's lifecycle with a scripted runner: what it asks for and in what order,
//! how landings move the snapshot, the guide window's day requests and their cap, writes in press
//! order with their notices and guide overlays, and a reset dropping late answers.

use super::*;
use plx_plex::plex::live_tv::{Airing, Channel, EpgProvider, Subscription};
use std::cell::RefCell;
use std::rc::Rc;

/// Records every job and the post it would answer through; answers nothing by itself.
#[derive(Default, Clone)]
struct Script {
    jobs: Rc<RefCell<Vec<(Job, Post)>>>,
    refuse: Rc<RefCell<bool>>,
}

impl Runner for Script {
    fn run(&mut self, _sid: ServerId, job: Job, post: Post) -> bool {
        if *self.refuse.borrow() {
            return false;
        }
        self.jobs.borrow_mut().push((job, post));
        true
    }
}

impl Script {
    fn take(&self) -> Vec<(Job, Post)> {
        std::mem::take(&mut *self.jobs.borrow_mut())
    }
}

fn sid() -> ServerId {
    ServerId::from_raw(3)
}

fn provider() -> EpgProvider {
    EpgProvider { identifier: "tv.plex.providers.epg.cloud:1".into(), dvr_key: "1".into(), title: "DVR".into(), recordable: true, grid: true }
}

fn channel(n: usize) -> Channel {
    Channel { identifier: format!("c{n}"), grid_key: format!("c{n}"), vcn: format!("{n}.1"), title: format!("Channel {n}"), ..Channel::default() }
}

fn airing(ch: usize, begins: i64, guid: &str, show: &str) -> Airing {
    Airing {
        guid: guid.into(),
        grandparent_guid: show.into(),
        title: format!("Program {begins}"),
        begins_at: begins,
        ends_at: begins + 1800,
        channel_identifier: format!("c{ch}"),
        ..Airing::default()
    }
}

/// A store opened on a server whose providers and lineup (`n` channels) have landed.
fn ready(script: &Script, n: usize) -> LiveTvStore {
    let mut s = LiveTvStore::with_runner(Box::new(script.clone()));
    s.open(sid());
    s.pump();
    let jobs = script.take();
    assert!(matches!(jobs[0].0, Job::Providers));
    jobs[0].1.post(Landing::Providers(Ok(vec![provider()])));
    s.pump();
    let jobs = script.take();
    let kinds: Vec<_> = jobs.iter().map(|(j, _)| std::mem::discriminant(j)).collect();
    assert!(kinds.contains(&std::mem::discriminant(&Job::Dvr)));
    for (job, post) in jobs {
        match job {
            Job::Channels { .. } => post.post(Landing::Channels(Ok((0..n).map(channel).collect()))),
            Job::Dvr => post.post(Landing::Dvr(Ok((vec![], vec![])))),
            other => panic!("unexpected {other:?}"),
        }
    }
    s.pump();
    s
}

const NOON: i64 = 1_760_011_200; // 2025-10-09T12:00:00Z

#[test]
fn opening_asks_for_providers_then_the_lineup_and_the_dvr_lists() {
    let script = Script::default();
    let s = ready(&script, 5);
    let snap = s.snapshot();
    assert_eq!(snap.availability, Availability::Available);
    assert_eq!(snap.provider.as_ref().unwrap().dvr_key, "1");
    assert_eq!(snap.channels.len(), 5);
    assert!(snap.channels_phase.is_ready() && snap.dvr_phase.is_ready());
}

#[test]
fn a_server_without_an_epg_provider_is_not_available() {
    let script = Script::default();
    let mut s = LiveTvStore::with_runner(Box::new(script.clone()));
    s.open(sid());
    s.pump();
    let (_, post) = script.take().remove(0);
    post.post(Landing::Providers(Ok(vec![])));
    s.pump();
    assert_eq!(s.snapshot().availability, Availability::None);
    assert!(script.take().is_empty(), "nothing else is asked of a server with no DVR");
}

#[test]
fn a_403_on_providers_reads_as_no_live_tv_and_a_transport_failure_as_retryable() {
    let script = Script::default();
    let mut s = LiveTvStore::with_runner(Box::new(script.clone()));
    s.open(sid());
    s.pump();
    script.take().remove(0).1.post(Landing::Providers(Err(LiveError::Transport)));
    s.pump();
    assert!(matches!(s.snapshot().providers_phase, Phase::Failed(_)));
    s.retry_failed();
    s.pump();
    let (job, post) = script.take().remove(0);
    assert!(matches!(job, Job::Providers));
    post.post(Landing::Providers(Err(LiveError::Status(403))));
    s.pump();
    assert_eq!(s.snapshot().availability, Availability::None);
}

#[test]
fn a_window_asks_each_channel_day_once_nearest_first_and_spans_utc_midnight() {
    let script = Script::default();
    let mut s = ready(&script, 10);
    // 23:00 -> 01:30 UTC crosses midnight: two dates per row
    let from = NOON + 11 * 3600;
    s.want_window(2..5, from, from + 9000);
    s.pump();
    let jobs = script.take();
    assert_eq!(jobs.len(), IN_FLIGHT, "no more than the read budget at once");
    let first: Vec<(usize, String)> = jobs.iter().map(|(j, _)| match j { Job::Grid { ch, date, .. } => (*ch, date.clone()), _ => panic!() }).collect();
    assert_eq!(first[0], (2, "2025-10-09".into()));
    assert_eq!(first[1], (2, "2025-10-10".into()));
    // the same window again asks for nothing new
    s.want_window(2..5, from, from + 9000);
    for (job, post) in jobs {
        if let Job::Grid { ch, date, .. } = job {
            post.post(Landing::Grid { ch, date, result: Ok(vec![airing(ch, from, "g", "s")]) });
        }
    }
    s.pump();
    let rest = script.take();
    assert_eq!(rest.len(), 2, "the remaining two of six channel-days start as the first finish");
    assert_eq!(s.airings(2, from, from + 9000).len(), 1, "an airing in both days' answers is one airing");
    assert!(!s.row_pending(2, from, from + 9000));
    assert!(s.row_pending(4, from, from + 9000));
}

#[test]
fn a_failed_day_is_not_asked_again_until_retry() {
    let script = Script::default();
    let mut s = ready(&script, 3);
    s.want_window(0..1, NOON, NOON + 3600);
    s.pump();
    let (job, post) = script.take().remove(0);
    let Job::Grid { ch, date, .. } = job else { panic!() };
    post.post(Landing::Grid { ch, date, result: Err(LiveError::Transport) });
    s.pump();
    assert!(s.row_failed(0, NOON, NOON + 3600));
    s.want_window(0..1, NOON, NOON + 3600);
    s.pump();
    assert!(script.take().is_empty(), "a scroll does not hammer a failing server");
    s.retry_failed();
    s.want_window(0..1, NOON, NOON + 3600);
    s.pump();
    assert_eq!(script.take().len(), 1);
}

#[test]
fn the_cache_evicts_the_least_recently_wanted_days() {
    let script = Script::default();
    let mut s = ready(&script, 200);
    for row in (0..200).step_by(2) {
        s.want_window(row..row + 2, NOON, NOON + 3600);
    }
    assert!(s.guide().len() <= crate::live_tv::DAYS_KEPT + 2, "{}", s.guide().len());
    assert!(s.guide().get(198, "2025-10-09").is_some(), "the latest window is kept");
}

#[test]
fn writes_run_one_at_a_time_in_press_order_and_end_in_notices_and_overlays() {
    let script = Script::default();
    let mut s = ready(&script, 2);
    s.want_window(0..1, NOON, NOON + 3600);
    s.pump();
    let (job, post) = script.take().remove(0);
    let Job::Grid { ch, date, .. } = job else { panic!() };
    post.post(Landing::Grid { ch, date, result: Ok(vec![airing(0, NOON, "plex://episode/e1", "plex://show/s1")]) });
    s.pump();
    let a = s.airings(0, NOON, NOON + 3600).remove(0);
    s.write(Write::Record { airing: a.clone(), kind: RecordKind::Series });
    s.write(Write::StopGrab { key: "/media/grabbers/operations/g1".into() });
    s.pump();
    let jobs = script.take();
    assert_eq!(jobs.len(), 1, "one write at a time");
    let (job, post) = jobs.into_iter().next().unwrap();
    assert!(matches!(job, Job::Write(Write::Record { kind: RecordKind::Series, .. })));
    assert_eq!(s.snapshot().writes_in_flight, 2);
    post.post(Landing::Write { write: Write::Record { airing: a.clone(), kind: RecordKind::Series }, result: Ok(WriteDone { rule: "41".into(), conflicts: vec![] }) });
    s.pump();
    let marked = s.airings(0, NOON, NOON + 3600).remove(0);
    assert_eq!(marked.show_subscription_id, "41", "the series rule overlays the guide at once");
    let notice = s.snapshot().notice.unwrap();
    assert!(notice.ok);
    assert_eq!(notice.op, WriteOp::RecordSeries);
    let jobs = script.take();
    assert!(jobs.iter().any(|(j, _)| matches!(j, Job::Write(Write::StopGrab { .. }))), "the next write starts");
    assert!(jobs.iter().any(|(j, _)| matches!(j, Job::Dvr)), "the DVR lists are read again");
    // a cancel clears the overlay
    for (job, post) in jobs {
        if let Job::Write(w) = job {
            post.post(Landing::Write { write: w, result: Ok(WriteDone::default()) });
        }
    }
    s.pump();
    s.write(Write::CancelRule { id: "41".into() });
    s.pump();
    let (job, post) = script.take().into_iter().find(|(j, _)| matches!(j, Job::Write(_))).unwrap();
    let Job::Write(w) = job else { unreachable!() };
    post.post(Landing::Write { write: w, result: Ok(WriteDone::default()) });
    s.pump();
    assert_eq!(s.airings(0, NOON, NOON + 3600)[0].show_subscription_id, "");
}

#[test]
fn a_refused_write_says_why_and_overlays_nothing() {
    let script = Script::default();
    let mut s = ready(&script, 1);
    let a = airing(0, NOON, "plex://episode/e1", "plex://show/s1");
    s.write(Write::Record { airing: a.clone(), kind: RecordKind::This });
    s.pump();
    let (_, post) = script.take().into_iter().find(|(j, _)| matches!(j, Job::Write(_))).unwrap();
    post.post(Landing::Write { write: Write::Record { airing: a, kind: RecordKind::This }, result: Err(LiveError::Refused) });
    s.pump();
    let n = s.snapshot().notice.unwrap();
    assert!(!n.ok);
    assert!(n.reason.contains("debug build"));
}

#[test]
fn a_reset_drops_late_answers_and_keeps_the_generation_moving_forward() {
    let script = Script::default();
    let mut s = ready(&script, 2);
    let gen = s.generation();
    s.want_window(0..2, NOON, NOON + 3600);
    s.pump();
    let late = script.take();
    s.reset();
    assert!(s.generation() > gen);
    for (job, post) in late {
        if let Job::Grid { ch, date, .. } = job {
            post.post(Landing::Grid { ch, date, result: Ok(vec![airing(ch, NOON, "g", "s")]) });
        }
    }
    s.pump();
    assert!(s.snapshot().channels.is_empty());
    assert!(s.guide().is_empty(), "a late grid lands nowhere");
    // a different server resets too
    s.open(ServerId::from_raw(4));
    assert_eq!(s.server(), Some(ServerId::from_raw(4)));
}

#[test]
fn a_refused_thread_is_retried_on_the_next_pump() {
    let script = Script::default();
    *script.refuse.borrow_mut() = true;
    let mut s = LiveTvStore::with_runner(Box::new(script.clone()));
    s.open(sid());
    s.pump();
    assert!(script.take().is_empty());
    *script.refuse.borrow_mut() = false;
    s.pump();
    assert!(matches!(script.take()[0].0, Job::Providers));
}

#[test]
fn dvr_lists_land_into_the_snapshot() {
    let script = Script::default();
    let mut s = ready(&script, 1);
    s.refresh_dvr();
    s.pump();
    let (job, post) = script.take().into_iter().find(|(j, _)| matches!(j, Job::Dvr)).unwrap();
    let _ = job;
    post.post(Landing::Dvr(Ok((vec![Subscription { id: "7".into(), title: "Show".into(), ..Default::default() }], vec![]))));
    s.pump();
    assert_eq!(s.snapshot().rules[0].id, "7");
}

#[test]
fn days_covering_names_every_utc_day_a_window_touches() {
    assert_eq!(crate::live_tv::days_covering(NOON, NOON + 60), vec!["2025-10-09".to_string()]);
    assert_eq!(crate::live_tv::days_covering(NOON + 11 * 3600, NOON + 13 * 3600).len(), 2);
    assert!(!crate::live_tv::plausible_now(0));
    assert!(crate::live_tv::plausible_now(NOON));
}
