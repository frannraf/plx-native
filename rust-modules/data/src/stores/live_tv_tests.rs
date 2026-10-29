//! The Live TV store's lifecycle through its job outbox: what it asks for and in what order, how
//! landings move the snapshot, the guide window's day requests and their cap, writes in press
//! order with their notices and guide overlays, a reset dropping late answers, and a job the
//! runner could not start going back to its queue.

use super::*;
use plx_plex::plex::live_tv::{Airing, Channel, EpgProvider, Subscription};

/// Where one emitted job's answer goes, as the application's runner holds it. It answers nothing
/// by itself.
struct Reply {
    sid: ServerId,
    addr: Addr,
    port: LiveTvPort,
}

impl Reply {
    fn post(&self, landing: Landing) {
        self.port.done(self.addr, landing);
    }

    fn refuse(&self) {
        self.port.refused(self.addr);
    }
}

/// Every job the store emitted since the last take, as the runner is handed them.
fn take(s: &mut LiveTvStore) -> Vec<(Job, Reply)> {
    s.take_jobs()
        .into_iter()
        .map(|job| {
            let StoreJob::LiveTv(job, port) = job else { panic!("the Live TV store emits Live TV jobs") };
            let plx_machine::job::Job { addr, req: LiveTvReq { sid, job } } = job;
            (job, Reply { sid, addr, port })
        })
        .collect()
}

fn sid() -> ServerId {
    ServerId::from_raw(3)
}

fn provider() -> EpgProvider {
    EpgProvider { identifier: "tv.plex.providers.epg.cloud:1".into(), dvr_key: "1".into(), title: "DVR".into(), recordable: true, grid: true, ..Default::default() }
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
fn ready(n: usize) -> LiveTvStore {
    ready_with((0..n).map(channel).collect())
}

/// A store opened on a server whose providers and lineup (`channels`) have landed.
fn ready_with(channels: Vec<Channel>) -> LiveTvStore {
    let mut s = LiveTvStore::default();
    s.open(sid());
    s.pump();
    let jobs = take(&mut s);
    assert!(matches!(jobs[0].0, Job::Providers));
    jobs[0].1.post(Landing::Providers(Ok(vec![provider()])));
    s.pump();
    let jobs = take(&mut s);
    let kinds: Vec<_> = jobs.iter().map(|(j, _)| std::mem::discriminant(j)).collect();
    assert!(kinds.contains(&std::mem::discriminant(&Job::Dvr { serial: 0 })));
    for (job, post) in jobs {
        match job {
            Job::Channels { .. } => post.post(Landing::Channels(Ok(channels.clone()))),
            Job::Dvr { serial } => post.post(Landing::Dvr { serial, result: Ok((vec![], vec![])) }),
            other => panic!("unexpected {other:?}"),
        }
    }
    s.pump();
    s
}

/// A guide read's channel and date, as its answer names them.
fn grid_of(job: &Job) -> (ChannelKey, String) {
    match job {
        Job::Grid { channel, date, .. } | Job::CloudGrid { channel, date, .. } => (ChannelKey::of(channel), date.clone()),
        other => panic!("not a guide read: {other:?}"),
    }
}

/// The one DVR read among `jobs`, with the serial its answer must carry.
fn dvr_job(jobs: Vec<(Job, Reply)>) -> (u32, Reply) {
    let (job, post) = jobs.into_iter().find(|(j, _)| matches!(j, Job::Dvr { .. })).expect("a DVR read");
    let Job::Dvr { serial } = job else { unreachable!() };
    (serial, post)
}

const NOON: i64 = 1_760_011_200; // 2025-10-09T12:00:00Z

#[test]
fn opening_asks_for_providers_then_the_lineup_and_the_dvr_lists() {
    let s = ready(5);
    let snap = s.snapshot();
    assert_eq!(snap.availability, Availability::Available);
    assert_eq!(snap.provider.as_ref().unwrap().dvr_key, "1");
    assert_eq!(snap.channels.len(), 5);
    assert!(snap.channels_phase.is_ready() && snap.dvr_phase.is_ready());
}

#[test]
fn a_server_without_an_epg_provider_is_not_available() {
    let mut s = LiveTvStore::default();
    s.open(sid());
    s.pump();
    let (_, post) = take(&mut s).remove(0);
    post.post(Landing::Providers(Ok(vec![])));
    s.pump();
    assert_eq!(s.snapshot().availability, Availability::None);
    assert!(take(&mut s).is_empty(), "nothing else is asked of a server with no DVR");
}

#[test]
fn a_403_on_providers_reads_as_no_live_tv_and_a_transport_failure_as_retryable() {
    let mut s = LiveTvStore::default();
    s.open(sid());
    s.pump();
    take(&mut s).remove(0).1.post(Landing::Providers(Err(LiveError::Transport)));
    s.pump();
    assert!(matches!(s.snapshot().providers_phase, Phase::Failed(_)));
    s.retry_failed();
    s.pump();
    let (job, post) = take(&mut s).remove(0);
    assert!(matches!(job, Job::Providers));
    post.post(Landing::Providers(Err(LiveError::Status(403))));
    s.pump();
    assert_eq!(s.snapshot().availability, Availability::None);
}

#[test]
fn a_window_asks_each_channel_day_once_nearest_first_and_spans_utc_midnight() {
    let mut s = ready(10);
    // 23:00 -> 01:30 UTC crosses midnight: two dates per row
    let from = NOON + 11 * 3600;
    s.want_window(2..5, from, from + 9000);
    s.pump();
    let jobs = take(&mut s);
    assert_eq!(jobs.len(), IN_FLIGHT, "no more than the read budget at once");
    let first: Vec<(ChannelKey, String)> = jobs.iter().map(|(j, _)| grid_of(j)).collect();
    let c2 = ChannelKey::of(&channel(2));
    assert_eq!(first[0], (c2.clone(), "2025-10-09".into()));
    assert_eq!(first[1], (c2, "2025-10-10".into()));
    // the same window again asks for nothing new
    s.want_window(2..5, from, from + 9000);
    for (job, post) in jobs {
        let (key, date) = grid_of(&job);
        post.post(Landing::Grid { key, date, result: Ok(vec![airing(2, from, "g", "s")]) });
    }
    s.pump();
    let rest = take(&mut s);
    assert_eq!(rest.len(), 2, "the remaining two of six channel-days start as the first finish");
    assert_eq!(s.airings(2, from, from + 9000).len(), 1, "an airing in both days' answers is one airing");
    assert!(!s.row_pending(2, from, from + 9000));
    assert!(s.row_pending(4, from, from + 9000));
}

#[test]
fn a_failed_day_is_not_asked_again_until_retry() {
    let mut s = ready(3);
    s.want_window(0..1, NOON, NOON + 3600);
    s.pump();
    let (job, post) = take(&mut s).remove(0);
    let (key, date) = grid_of(&job);
    post.post(Landing::Grid { key, date, result: Err(LiveError::Transport) });
    s.pump();
    assert!(s.row_failed(0, NOON, NOON + 3600));
    s.want_window(0..1, NOON, NOON + 3600);
    s.pump();
    assert!(take(&mut s).is_empty(), "a scroll does not hammer a failing server");
    // Retry asks the last window again by itself: the page's window did not move
    s.retry_failed();
    s.pump();
    let (job, post) = take(&mut s).remove(0);
    // …and while that second read is on its way the row still reads as failed, not "Loading…"
    assert!(s.row_failed(0, NOON, NOON + 3600) && !s.row_pending(0, NOON, NOON + 3600), "a retried day keeps its failed look until it answers");
    let (key, date) = grid_of(&job);
    post.post(Landing::Grid { key, date, result: Ok(vec![airing(0, NOON, "g", "s")]) });
    s.pump();
    assert!(!s.row_failed(0, NOON, NOON + 3600), "an answer clears it");
}

#[test]
fn retry_reads_a_failed_dvr_list_again() {
    let mut s = ready(2);
    s.refresh_dvr();
    s.pump();
    let (serial, post) = dvr_job(take(&mut s));
    post.post(Landing::Dvr { serial, result: Err(LiveError::Transport) });
    s.pump();
    assert!(matches!(s.snapshot().dvr_phase, Phase::Failed(_)));
    s.retry_failed();
    s.pump();
    assert!(take(&mut s).iter().any(|(j, _)| matches!(j, Job::Dvr { .. })), "Try again on the Recordings read-out reads the lists again");
}

/// A read waiting in the queue answers for every ask until it starts: with the read budget full
/// (grid reads on their way), a quiet refresh and then a write's re-read queue ONE read, under
/// one rank, and its answer is the lists.
#[test]
fn one_queued_dvr_read_serves_every_ask_until_it_starts() {
    let mut s = ready(IN_FLIGHT);
    s.want_window(0..IN_FLIGHT, NOON, NOON + 3600);
    s.pump();
    let grids = take(&mut s);
    assert!(grids.len() == IN_FLIGHT && grids.iter().all(|(j, _)| matches!(j, Job::Grid { .. })), "{}", grids.len());
    s.refresh_dvr_quietly();
    s.pump();
    assert!(take(&mut s).is_empty(), "the budget is full: the read waits");
    s.write(Write::CancelRule { id: "1".into() });
    s.pump();
    let (job, post) = take(&mut s).into_iter().find(|(j, _)| matches!(j, Job::Write(_))).unwrap();
    let Job::Write(write) = job else { unreachable!() };
    post.post(Landing::Write { write, result: Ok(WriteDone::default()) });
    s.pump();
    assert!(take(&mut s).is_empty(), "the write's re-read is the one already waiting");
    for (job, post) in grids {
        let (key, date) = grid_of(&job);
        post.post(Landing::Grid { key, date, result: Ok(vec![]) });
    }
    s.pump();
    let jobs = take(&mut s);
    assert_eq!(jobs.iter().filter(|(j, _)| matches!(j, Job::Dvr { .. })).count(), 1, "one read, once there is room");
    let (serial, post) = dvr_job(jobs);
    post.post(Landing::Dvr { serial, result: Ok((vec![Subscription { id: "x".into(), ..Default::default() }], vec![])) });
    s.pump();
    assert_eq!(s.snapshot().rules[0].id, "x", "…and its answer is the lists");
}

/// Two reads of the lists on their way at once (a refresh, then the re-read a write asks for):
/// the newer one is the lists, and the older one landing after it is history.
#[test]
fn an_older_dvr_read_landing_after_a_newer_one_is_history() {
    let mut s = ready(1);
    s.refresh_dvr();
    s.pump();
    let (first, first_post) = dvr_job(take(&mut s));
    // a write lands while that read is on its way: the lists are asked for again, outranking it
    s.write(Write::CancelRule { id: "1".into() });
    s.pump();
    let (job, post) = take(&mut s).into_iter().find(|(j, _)| matches!(j, Job::Write(_))).unwrap();
    let Job::Write(write) = job else { unreachable!() };
    post.post(Landing::Write { write, result: Ok(WriteDone::default()) });
    s.pump();
    let (second, second_post) = dvr_job(take(&mut s));
    assert_ne!(second, first);
    second_post.post(Landing::Dvr { serial: second, result: Ok((vec![Subscription { id: "new".into(), ..Default::default() }], vec![])) });
    s.pump();
    assert_eq!(s.snapshot().rules[0].id, "new");
    first_post.post(Landing::Dvr { serial: first, result: Ok((vec![Subscription { id: "old".into(), ..Default::default() }], vec![])) });
    s.pump();
    assert_eq!(s.snapshot().rules[0].id, "new", "the older read does not put the lists back");
    assert!(matches!(s.snapshot().dvr_phase, Phase::Ready));
    // …nor does its failure mark the lists failed
    s.refresh_dvr_quietly();
    s.pump();
    let (third, third_post) = dvr_job(take(&mut s));
    s.write(Write::CancelRule { id: "2".into() });
    s.pump();
    let (job, post) = take(&mut s).into_iter().find(|(j, _)| matches!(j, Job::Write(_))).unwrap();
    let Job::Write(write) = job else { unreachable!() };
    post.post(Landing::Write { write, result: Ok(WriteDone::default()) });
    s.pump();
    let (fourth, fourth_post) = dvr_job(take(&mut s));
    fourth_post.post(Landing::Dvr { serial: fourth, result: Ok((vec![], vec![])) });
    s.pump();
    third_post.post(Landing::Dvr { serial: third, result: Err(LiveError::Transport) });
    s.pump();
    assert!(matches!(s.snapshot().dvr_phase, Phase::Ready), "{:?}", s.snapshot().dvr_phase);
}

#[test]
fn the_cache_evicts_the_least_recently_wanted_days() {
    let mut s = ready(200);
    for row in (0..200).step_by(2) {
        s.want_window(row..row + 2, NOON, NOON + 3600);
    }
    assert!(s.guide().len() <= crate::live_tv::DAYS_KEPT + 2, "{}", s.guide().len());
    assert!(s.guide().get(&ChannelKey::of(&channel(198)), "2025-10-09").is_some(), "the latest window is kept");
}

#[test]
fn writes_run_one_at_a_time_in_press_order_and_end_in_notices_and_overlays() {
    let mut s = ready(2);
    s.want_window(0..1, NOON, NOON + 3600);
    s.pump();
    let (job, post) = take(&mut s).remove(0);
    let (key, date) = grid_of(&job);
    post.post(Landing::Grid { key, date, result: Ok(vec![airing(0, NOON, "plex://episode/e1", "plex://show/s1")]) });
    s.pump();
    let a = s.airings(0, NOON, NOON + 3600).remove(0);
    s.write(Write::Record { airing: a.clone(), kind: RecordKind::Series });
    s.write(Write::StopGrab { key: "/media/grabbers/operations/g1".into() });
    s.pump();
    let jobs = take(&mut s);
    assert_eq!(jobs.len(), 1, "one write at a time");
    let (job, post) = jobs.into_iter().next().unwrap();
    assert!(matches!(job, Job::Write(Write::Record { kind: RecordKind::Series, .. })));
    assert_eq!(s.snapshot().writes_in_flight, 2);
    post.post(Landing::Write { write: Write::Record { airing: a.clone(), kind: RecordKind::Series }, result: Ok(WriteDone { rule: "41".into(), ..WriteDone::default() }) });
    s.pump();
    let marked = s.airings(0, NOON, NOON + 3600).remove(0);
    assert_eq!(marked.show_subscription_id, "41", "the series rule overlays the guide at once");
    let notice = s.snapshot().notice.unwrap();
    assert!(notice.ok);
    assert_eq!(notice.op, WriteOp::RecordSeries);
    assert_eq!(notice.rule, "41", "the rule a conflict panel would prefer or cancel");
    let jobs = take(&mut s);
    assert!(jobs.iter().any(|(j, _)| matches!(j, Job::Write(Write::StopGrab { .. }))), "the next write starts");
    assert!(jobs.iter().any(|(j, _)| matches!(j, Job::Dvr { .. })), "the DVR lists are read again");
    // a cancel clears the overlay
    for (job, post) in jobs {
        if let Job::Write(w) = job {
            post.post(Landing::Write { write: w, result: Ok(WriteDone::default()) });
        }
    }
    s.pump();
    s.write(Write::CancelRule { id: "41".into() });
    s.pump();
    let (job, post) = take(&mut s).into_iter().find(|(j, _)| matches!(j, Job::Write(_))).unwrap();
    let Job::Write(w) = job else { unreachable!() };
    post.post(Landing::Write { write: w, result: Ok(WriteDone::default()) });
    s.pump();
    assert_eq!(s.airings(0, NOON, NOON + 3600)[0].show_subscription_id, "");
}

#[test]
fn a_refused_write_says_why_and_overlays_nothing() {
    let mut s = ready(1);
    let a = airing(0, NOON, "plex://episode/e1", "plex://show/s1");
    s.write(Write::Record { airing: a.clone(), kind: RecordKind::This });
    s.pump();
    let (_, post) = take(&mut s).into_iter().find(|(j, _)| matches!(j, Job::Write(_))).unwrap();
    post.post(Landing::Write { write: Write::Record { airing: a, kind: RecordKind::This }, result: Err(LiveError::Refused) });
    s.pump();
    let n = s.snapshot().notice.unwrap();
    assert!(!n.ok);
    assert!(n.reason.contains("debug build"));
}

#[test]
fn a_reset_drops_late_answers_and_keeps_the_generation_moving_forward() {
    let mut s = ready(2);
    let gen = s.generation();
    s.want_window(0..2, NOON, NOON + 3600);
    s.pump();
    let late = take(&mut s);
    s.reset();
    assert!(s.generation() > gen);
    for (job, post) in late {
        if matches!(job, Job::Grid { .. }) {
            let (key, date) = grid_of(&job);
            post.post(Landing::Grid { key, date, result: Ok(vec![airing(0, NOON, "g", "s")]) });
        }
    }
    s.pump();
    assert!(s.snapshot().channels.is_empty());
    assert!(s.guide().is_empty(), "a late grid lands nowhere");
    // a different server resets too
    s.open(ServerId::from_raw(4));
    assert_eq!(s.server(), Some(ServerId::from_raw(4)));
}

/// **A job the runner could not start goes back to its queue** (no thread, or no client for its
/// server) and is asked again on the next pump, giving back what it took: a read its slot of the
/// budget, in the order the reads were first asked; a write its turn, ahead of a later press; a
/// tune its press, only while that press is still the one wanted.
#[test]
fn a_job_the_runner_refuses_is_asked_again_on_the_next_pump() {
    let mut s = LiveTvStore::default();
    s.open(sid());
    s.pump();
    let jobs = take(&mut s);
    assert!(matches!(jobs[..], [(Job::Providers, _)]));
    jobs[0].1.refuse();
    assert!(take(&mut s).is_empty(), "nothing is asked again before the next pump");
    s.pump();
    assert!(matches!(take(&mut s)[..], [(Job::Providers, _)]), "the read is asked again");

    let mut s = ready(IN_FLIGHT + 1);
    s.want_window(0..IN_FLIGHT + 1, NOON, NOON + 3600);
    s.pump();
    let reads = take(&mut s);
    assert_eq!(reads.len(), IN_FLIGHT);
    for (_, reply) in &reads {
        reply.refuse();
    }
    s.pump();
    let again = take(&mut s);
    let days = |jobs: &[(Job, Reply)]| jobs.iter().map(|(j, _)| grid_of(j)).collect::<Vec<_>>();
    assert_eq!(days(&again), days(&reads), "the whole budget again, in the order first asked");

    s.write(Write::CancelRule { id: "1".into() });
    s.write(Write::CancelRule { id: "2".into() });
    s.pump();
    let (_, reply) = take(&mut s).into_iter().find(|(j, _)| matches!(j, Job::Write(_))).expect("the first write goes out");
    reply.refuse();
    s.pump();
    let writes: Vec<Job> = take(&mut s).into_iter().map(|(j, _)| j).filter(|j| matches!(j, Job::Write(_))).collect();
    assert_eq!(writes, [Job::Write(Write::CancelRule { id: "1".into() })], "the refused write is still first, and alone");
    assert_eq!(s.snapshot().writes_in_flight, 2);

    let mut s = ready(3);
    assert!(s.tune(1, None));
    s.pump();
    let (first, _, reply) = tune_job(take(&mut s));
    reply.refuse();
    s.pump();
    let (again, _, reply) = tune_job(take(&mut s));
    assert_eq!(again, first, "the same press is tuned again");
    reply.refuse();
    s.cancel_tune();
    s.pump();
    assert!(!take(&mut s).iter().any(|(j, _)| matches!(j, Job::Tune { .. })), "a cancelled press is not");
    assert!(!s.tuners_settling(), "a tune that never started holds no tuner");
}

/// **The identity is asked for as soon as the DVR is found**, once, so Plex's channel tabs and the
/// account's favorites are read before the page opens (the guide opens on the favorites).
#[test]
fn the_dvr_found_asks_once_for_the_identity_and_seating_it_reads_the_favorites() {
    use crate::live_tv::CloudAuth;
    let mut s = ready(2);
    assert!(s.take_cloud_auth_wanted(), "found with no identity: asked for it");
    assert!(!s.take_cloud_auth_wanted(), "once");
    s.seat_cloud_auth(Some(CloudAuth { client_id: "cid".into(), credential: "secret".into() }));
    s.pump();
    let jobs = take(&mut s);
    assert!(jobs.iter().any(|(j, _)| matches!(j, Job::Favorites { .. })), "the favorites are read");
    assert!(jobs.iter().any(|(j, _)| matches!(j, Job::CloudTabs { .. })), "and Plex's channel tabs");
    // a reset (a profile or server switch) forgets the identity, and the next DVR found asks again
    s.reset();
    assert!(!s.take_cloud_auth_wanted(), "nothing found yet");
}

/// **A release refused just before quitting still goes at exit**: the exit takes the refusal back
/// before it releases, so the tuner is not left to the server's own timeout.
#[test]
fn a_release_refused_just_before_exit_is_sent_at_exit() {
    let mut s = ready(3);
    s.release_tuned(tuned("3", "sess"));
    s.pump();
    let (_, reply) = take(&mut s).into_iter().find(|(j, _)| matches!(j, Job::Release { .. })).expect("the release goes out");
    reply.refuse();
    s.release_for_exit();
    assert_eq!(releases(&take(&mut s)), vec!["3".to_string()]);
}

/// **A release the runner could not start is owed again**, across a server switch too, against
/// the server that tuned; and the store says a tuner is still coming back until a release answers.
#[test]
fn a_refused_release_is_owed_again_and_the_tuner_is_back_only_when_it_answers() {
    let mut s = ready(3);
    s.tune(0, None);
    s.pump();
    let (serial, session, reply) = tune_job(take(&mut s));
    reply.post(Landing::Tuned { serial, result: Ok(tuned("9", &session)) });
    s.pump();
    // landed and not taken: a reset releases it
    s.reset();
    s.pump();
    let jobs = take(&mut s);
    assert_eq!(releases(&jobs), vec!["9".to_string()]);
    assert!(s.tuners_settling(), "a release on its way: the tuner is not back yet");
    let (_, reply) = jobs.into_iter().find(|(j, _)| matches!(j, Job::Release { .. })).unwrap();
    s.open(ServerId::from_raw(4));
    reply.refuse();
    s.pump();
    let jobs = take(&mut s);
    assert_eq!(releases(&jobs), vec!["9".to_string()], "owed again after the switch");
    let (_, reply) = jobs.into_iter().find(|(j, _)| matches!(j, Job::Release { .. })).unwrap();
    assert_eq!(reply.sid, sid(), "against the server that tuned it, not the one now read");
    reply.post(Landing::Released);
    s.pump();
    assert!(!s.tuners_settling());
    assert!(releases(&take(&mut s)).is_empty(), "and released once");
}

#[test]
fn dvr_lists_land_into_the_snapshot() {
    let mut s = ready(1);
    s.refresh_dvr();
    s.pump();
    let (serial, post) = dvr_job(take(&mut s));
    post.post(Landing::Dvr { serial, result: Ok((vec![Subscription { id: "7".into(), title: "Show".into(), ..Default::default() }], vec![])) });
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

fn tuned(sub: &str, session: &str) -> Tuned {
    Tuned {
        sid: sid(),
        subscription_id: sub.into(),
        item: plx_plex::plex::live_tv::TunedItem { key: "/livetv/sessions/u".into(), ..Default::default() },
        stream: plx_plex::plex::live_tv::LiveStream { url: "http://x/start.m3u8".into(), encoder_session: session.into(), ..Default::default() },
        channel: "C1".into(),
        airing: None,
        cloud: false,
        capture: None,
    }
}

/// The one tune job of a take, with its serial and session.
fn tune_job(jobs: Vec<(Job, Reply)>) -> (u32, String, Reply) {
    let mut found = jobs.into_iter().filter_map(|(j, p)| match j {
        Job::Tune { serial, session, .. } => Some((serial, session, p)),
        _ => None,
    });
    let one = found.next().expect("a tune job");
    assert!(found.next().is_none(), "one tune at a time");
    one
}

fn releases(jobs: &[(Job, Reply)]) -> Vec<String> {
    jobs.iter().filter_map(|(j, _)| match j { Job::Release { subscription_id, .. } => Some(subscription_id.clone()), _ => None }).collect()
}

#[test]
fn a_watch_press_tunes_off_the_frame_and_the_app_takes_the_stream_once() {
    let mut s = ready(3);
    assert!(s.tune(1, None));
    assert_eq!(s.snapshot().tune, TunePhase::Tuning { channel: "1.1 Channel 1".into() });
    s.pump();
    let (serial, session, post) = tune_job(take(&mut s));
    assert!(!session.is_empty(), "a fresh encode session per press");
    post.post(Landing::Tuned { serial, result: Ok(tuned("77", &session)) });
    s.pump();
    let t = s.take_tuned().expect("the stream is offered");
    assert_eq!(t.subscription_id, "77");
    assert!(s.take_tuned().is_none(), "offered once");
    assert_eq!(s.snapshot().tune, TunePhase::Idle);
    s.pump();
    assert!(releases(&take(&mut s)).is_empty(), "a taken tune is the route's to release");
}

#[test]
fn a_tune_nobody_wants_any_more_is_released_against_the_server_that_tuned_it() {
    let mut s = ready(3);
    s.tune(0, None);
    s.pump();
    let (serial, session, post) = tune_job(take(&mut s));
    s.cancel_tune();
    assert_eq!(s.snapshot().tune, TunePhase::Idle);
    post.post(Landing::Tuned { serial, result: Ok(tuned("41", &session)) });
    s.pump();
    assert!(s.take_tuned().is_none(), "a cancelled press is never played");
    let jobs = take(&mut s);
    assert_eq!(releases(&jobs), vec!["41".to_string()]);
    assert!(jobs.iter().all(|(_, reply)| reply.sid == sid()), "against the server that tuned it");
    let stopped = jobs.iter().any(|(j, _)| matches!(j, Job::Release { session: s, .. } if *s == session));
    assert!(stopped, "the release names the encode to stop");
}

#[test]
fn a_second_press_supersedes_the_first_whose_tune_is_released() {
    let mut s = ready(3);
    s.tune(0, None);
    s.pump();
    let (first, session1, post1) = tune_job(take(&mut s));
    s.tune(2, None);
    s.pump();
    let (second, session2, post2) = tune_job(take(&mut s));
    assert_ne!(first, second);
    assert_ne!(session1, session2);
    post2.post(Landing::Tuned { serial: second, result: Ok(tuned("2", &session2)) });
    post1.post(Landing::Tuned { serial: first, result: Ok(tuned("1", &session1)) });
    s.pump();
    assert_eq!(s.take_tuned().unwrap().subscription_id, "2");
    assert_eq!(releases(&take(&mut s)), vec!["1".to_string()]);
}

#[test]
fn a_failed_tune_says_why_until_the_page_clears_it() {
    let mut s = ready(3);
    s.tune(0, None);
    s.pump();
    let (serial, _, post) = tune_job(take(&mut s));
    post.post(Landing::Tuned { serial, result: Err(LiveError::Busy) });
    s.pump();
    match s.snapshot().tune {
        TunePhase::Failed { channel, reason } => {
            assert_eq!(channel, "0.1 Channel 0");
            assert_eq!(reason, LiveError::Busy.reason());
        }
        other => panic!("{other:?}"),
    }
    s.clear_tune_failure();
    assert_eq!(s.snapshot().tune, TunePhase::Idle);
}

#[test]
fn a_reset_releases_an_untaken_tune_and_one_that_lands_after_it() {
    let mut s = ready(3);
    s.tune(0, None);
    s.pump();
    let (serial, session, post) = tune_job(take(&mut s));
    post.post(Landing::Tuned { serial, result: Ok(tuned("5", &session)) });
    s.pump();
    // landed, not taken: the server switch releases it
    s.reset();
    s.pump();
    assert_eq!(releases(&take(&mut s)), vec!["5".to_string()]);
    // one in flight across a reset lands under the old epoch and is released too
    s.open(sid());
    s.pump();
    take(&mut s).remove(0).1.post(Landing::Providers(Ok(vec![provider()])));
    s.pump();
    for (job, post) in take(&mut s) {
        match job {
            Job::Channels { .. } => post.post(Landing::Channels(Ok((0..3).map(channel).collect()))),
            Job::Dvr { serial } => post.post(Landing::Dvr { serial, result: Ok((vec![], vec![])) }),
            _ => {}
        }
    }
    s.pump();
    s.tune(1, None);
    s.pump();
    let (serial, session, post) = tune_job(take(&mut s));
    s.reset();
    post.post(Landing::Tuned { serial, result: Ok(tuned("6", &session)) });
    s.pump();
    assert_eq!(releases(&take(&mut s)), vec!["6".to_string()]);
    assert!(s.take_tuned().is_none());
}

#[test]
fn nothing_can_be_tuned_without_a_tunable_provider_or_a_lineup() {
    let mut s = LiveTvStore::default();
    assert!(!s.tune(0, None), "no server");
    s.open(sid());
    assert!(!s.tune(0, None), "no lineup yet");
    let mut s = ready(2);
    assert!(!s.tune(5, None), "no such row");
}

/// A Plex tab shows its own lineup; the DVR's waits aside, cache and all, and comes back unread.
#[test]
fn a_plex_tab_shows_its_own_lineup_and_the_dvr_comes_back_from_aside() {
    use crate::live_tv::{CloudAuth, Source};
    let mut s = ready(3);
    s.set_cloud_auth(CloudAuth { client_id: "cid".into(), credential: "secret".into() });
    s.select_source(Source::Plex);
    let snap = s.snapshot();
    assert_eq!(snap.source, Source::Plex);
    assert!(snap.channels.is_empty() && snap.channels_phase.is_loading());
    s.pump();
    let (job, post) = take(&mut s).into_iter().find(|(j, _)| matches!(j, Job::CloudChannels { .. })).expect("the Plex lineup is asked");
    let Job::CloudChannels { source, .. } = job else { unreachable!() };
    assert_eq!(source, Source::Plex);
    post.post(Landing::CloudChannels { source: Source::Plex, result: Ok((0..5).map(channel).collect()) });
    s.pump();
    assert_eq!(s.snapshot().channels.len(), 5);
    s.select_source(Source::Dvr);
    s.pump();
    let snap = s.snapshot();
    assert_eq!(snap.channels.len(), 3, "the DVR's lineup came back from aside");
    assert!(snap.channels_phase.is_ready());
    assert!(!take(&mut s).iter().any(|(j, _)| matches!(j, Job::Channels { .. })), "and is not read again");
    let shown = format!("{:?}", CloudAuth { client_id: "c".into(), credential: "secret".into() });
    assert!(!shown.contains("secret"), "the credential never prints: {shown}");
}

/// A DVR with no tuner to spare answers the next tune "busy" until every tuner it handed out is
/// back: the store says while one is still coming (a tune on its way, a superseded one included,
/// or a release owed), so the application can hold its next Watch until it is.
#[test]
fn the_store_says_while_a_tuner_is_still_coming_back() {
    let mut s = ready(3);
    assert!(!s.tuners_settling());
    assert!(s.tune(1, None));
    s.pump();
    let (serial, session, post) = tune_job(take(&mut s));
    assert!(s.tuners_settling(), "a tune on its way holds a tuner");
    s.cancel_tune();
    assert!(s.tuners_settling(), "superseded, it still lands on a tuner");
    post.post(Landing::Tuned { serial, result: Ok(tuned("77", &session)) });
    s.pump();
    let jobs = take(&mut s);
    assert_eq!(releases(&jobs), vec!["77".to_string()], "the unwanted tune goes back");
    assert!(s.tuners_settling(), "…and the tuner is not back until its release answers");
    let (_, reply) = jobs.into_iter().find(|(j, _)| matches!(j, Job::Release { .. })).unwrap();
    reply.post(Landing::Released);
    s.pump();
    assert!(!s.tuners_settling());

    assert!(s.tune(2, None));
    s.pump();
    let (serial, _, post) = tune_job(take(&mut s));
    post.post(Landing::Tuned { serial, result: Err(LiveError::Busy) });
    s.pump();
    assert!(s.tune_failed() && !s.tuners_settling());
    s.clear_tune_failure();
    assert!(!s.tune_failed());
}

/// OK pressed twice on Record before the first answer lands sends one write, not a duplicate rule;
/// once it has landed the same press is a new write again.
#[test]
fn a_repeated_dvr_change_is_sent_once_while_the_first_is_on_its_way() {
    let mut s = ready(2);
    let w = Write::Record { airing: airing(0, 9_000, "g", "show"), kind: RecordKind::This };
    s.write(w.clone());
    s.write(w.clone());
    s.pump();
    let writes: Vec<_> = take(&mut s).into_iter().filter(|(j, _)| matches!(j, Job::Write(_))).collect();
    assert_eq!(writes.len(), 1);
    s.write(w.clone());
    s.pump();
    assert!(!take(&mut s).iter().any(|(j, _)| matches!(j, Job::Write(_))), "still running: not sent again");
    let (_, post) = writes.into_iter().next().unwrap();
    post.post(Landing::Write { write: w.clone(), result: Ok(WriteDone::default()) });
    s.pump();
    take(&mut s);
    s.write(w);
    s.pump();
    assert!(take(&mut s).iter().any(|(j, _)| matches!(j, Job::Write(_))), "landed: a new press is a new write");
}

/// A channel-day read a while ago is read again, its programs showing meanwhile; one read lately
/// is left alone.
#[test]
fn a_stale_channel_day_is_read_again_while_its_programs_stay_on_screen() {
    use crate::live_tv::GuideCache;
    let mut g = GuideCache::default();
    let dates = vec!["2026-10-22".to_string()];
    let c0 = ChannelKey::of(&channel(0));
    assert_eq!(g.want([&c0].into_iter(), &dates).len(), 1);
    g.land(&c0, "2026-10-22", Ok(vec![airing(0, NOON, "g", "s")]), 1_000);
    assert!(!g.expire(1_000 + 14 * 60, 15 * 60), "read lately: left alone");
    assert!(g.expire(1_000 + 15 * 60, 15 * 60));
    assert_eq!(g.airings(&c0, NOON, NOON + 1).len(), 1, "its programs still show");
    assert_eq!(g.want([&c0].into_iter(), &dates).len(), 1, "and it is asked again");
    assert!(!g.pending(&c0, &dates), "it answered once: not on its way to the page, so no Loading…");
}

/// **A channel's days are the channel's, not a row's**: the same Plex channel in another tab (a
/// genre, the favorites) is not read again, and shows its programs at once in its new row.
#[test]
fn a_channel_is_read_once_whichever_tab_or_row_shows_it() {
    use crate::live_tv::{CloudAuth, Source};
    let cloud = |n: usize| Channel { cloud: true, part_key: format!("/library/parts/{n}.m3u8"), ..channel(n) };
    let mut s = ready(2);
    s.set_cloud_auth(CloudAuth { client_id: "cid".into(), credential: "secret".into() });
    s.select_source(Source::Plex);
    s.pump();
    let (_, post) = take(&mut s).into_iter().find(|(j, _)| matches!(j, Job::CloudChannels { .. })).expect("the Plex lineup");
    post.post(Landing::CloudChannels { source: Source::Plex, result: Ok((0..3).map(cloud).collect()) });
    s.pump();
    s.want_window(0..3, NOON, NOON + 3600);
    s.pump();
    let reads = take(&mut s);
    assert!(reads.len() == 3 && reads.iter().all(|(j, _)| matches!(j, Job::CloudGrid { .. })), "a Plex channel's guide is Plex's: {}", reads.len());
    for (job, post) in reads {
        let (key, date) = grid_of(&job);
        post.post(Landing::Grid { key, date, result: Ok(vec![airing(0, NOON, "g", "s")]) });
    }
    s.pump();
    // a genre tab that lists channel 2 first: nothing to read for it, and its programs show
    s.select_source(Source::PlexTab("news".into()));
    s.pump();
    let (_, post) = take(&mut s).into_iter().find(|(j, _)| matches!(j, Job::CloudChannels { .. })).expect("the genre's lineup");
    post.post(Landing::CloudChannels { source: Source::PlexTab("news".into()), result: Ok(vec![cloud(2), cloud(7)]) });
    s.pump();
    s.want_window(0..2, NOON, NOON + 3600);
    s.pump();
    let reads: Vec<ChannelKey> = take(&mut s).iter().map(|(j, _)| grid_of(j).0).collect();
    assert_eq!(reads, vec![ChannelKey::of(&cloud(7))], "only the channel not yet read is asked");
    assert_eq!(s.airings(0, NOON, NOON + 3600).len(), 1, "the row of channel 2 shows its programs at once");
    assert!(!s.row_pending(0, NOON, NOON + 3600));
    // …and the DVR's own rows keep theirs too: a DVR channel and a Plex channel never share a key
    assert_ne!(ChannelKey::of(&channel(2)), ChannelKey::of(&cloud(2)));
}

/// **A What's On card names its channel** by its lineup id, else a grid key, else the number of a
/// DVR channel (the server's channel hubs name theirs in a form of their own); the surest way wins
/// wherever it finds one, and a channel the tab shown does not list is shown in its own tab.
#[test]
fn a_whats_on_card_names_its_channel_by_lineup_id_else_grid_key_else_a_dvr_number() {
    use crate::live_tv::{CloudAuth, Source};
    let cloud = |n: usize| Channel { cloud: true, part_key: format!("/library/parts/{n}.m3u8"), ..channel(n) };
    let dvr = vec![Channel { grid_key: "g0".into(), ..channel(0) }, channel(1), channel(2)];
    let mut s = ready_with(dvr.clone());
    let key = |c: &Channel| Some(ChannelKey::of(c));
    assert_eq!(s.key_of_card("c1", ""), key(&dvr[1]), "by lineup id");
    assert_eq!(s.key_of_card("g0", ""), key(&dvr[0]), "by grid key");
    assert_eq!(s.key_of_card("KXYZ-hub", "2.1"), key(&dvr[2]), "by number when the id is neither");
    assert_eq!(s.key_of_card("c1", "0.1"), key(&dvr[1]), "the lineup id before another channel's number");
    assert_eq!(s.key_of_card("KXYZ-hub", ""), None);
    assert_eq!(s.key_of_card("KXYZ-hub", "9.1"), None);
    assert_eq!(s.key_of_card("", ""), None, "an empty id names nothing");
    // Plex's own channels shown: a number names no channel of theirs, only the DVR's
    s.set_cloud_auth(CloudAuth { client_id: "cid".into(), credential: "secret".into() });
    s.select_source(Source::Plex);
    s.pump();
    let (_, post) = take(&mut s).into_iter().find(|(j, _)| matches!(j, Job::CloudChannels { .. })).expect("the Plex lineup");
    post.post(Landing::CloudChannels { source: Source::Plex, result: Ok(vec![cloud(5), cloud(6)]) });
    s.pump();
    assert_eq!(s.key_of_card("c6", ""), key(&cloud(6)), "a Plex channel by its lineup id");
    assert_eq!(s.key_of_card("KXYZ-hub", "6.1"), None, "never by its number");
    assert_eq!(s.row_of_card("KXYZ-hub", "2.1"), Some(2), "the DVR channel numbered so…");
    assert_eq!(*s.source(), Source::Dvr, "…in the DVR's tab, shown for it");
    assert_eq!(s.key_of(2), key(&dvr[2]));
    assert_eq!(s.row_of_card("g0", ""), Some(0), "in the tab shown");
    assert_eq!(*s.source(), Source::Dvr);
}

const MACHINE: &str = "m1";

fn favorite(source: &str, id: &str, vcn: &str) -> plx_plex::plex::live_tv::FavoriteChannel {
    plx_plex::plex::live_tv::FavoriteChannel::new(source, id, "", "", vcn)
}

fn dvr_source() -> String {
    format!("server://{MACHINE}/{}", provider().identifier)
}

/// **A favorite names its channel** by where it is and its id: a DVR channel through this
/// server's identity (lineup id, grid key, else its number), one of Plex's own by an entry that
/// is not a server's.
#[test]
fn a_favorite_names_a_dvr_channel_through_this_servers_identity_and_a_plex_one_otherwise() {
    use crate::live_tv::favorite_names;
    let p = provider().identifier;
    let ch = Channel { grid_key: "g7".into(), ..channel(7) };
    assert!(favorite_names(&favorite(&dvr_source(), "c7", ""), &ch, MACHINE, &p), "by lineup id");
    assert!(favorite_names(&favorite(&dvr_source(), "g7", ""), &ch, MACHINE, &p), "by grid key");
    assert!(favorite_names(&favorite(&dvr_source(), "other", "7.1"), &ch, MACHINE, &p), "by number when the id is neither");
    assert!(!favorite_names(&favorite("server://m2/x", "c7", ""), &ch, MACHINE, &p), "another server's");
    assert!(!favorite_names(&favorite("tv.plex.provider.epg", "c7", ""), &ch, MACHINE, &p), "a Plex entry never names a DVR channel");
    let plex = Channel { cloud: true, ..channel(7) };
    assert!(favorite_names(&favorite("tv.plex.provider.epg", "c7", ""), &plex, MACHINE, &p));
    assert!(!favorite_names(&favorite(&dvr_source(), "c7", ""), &plex, MACHINE, &p));
    assert!(favorite_names(&favorite(&dvr_source(), "c7", ""), &ch, "", ""), "an identity not yet learned matches any");
}

/// Lands the favorites read the store asked for.
fn land_favorites(s: &mut LiveTvStore, list: Vec<plx_plex::plex::live_tv::FavoriteChannel>) {
    s.pump();
    let (_, post) = take(s).into_iter().find(|(j, _)| matches!(j, Job::Favorites { .. })).expect("the favorites are read");
    post.post(Landing::Favorites { machine: MACHINE.into(), result: Ok(list) });
    s.pump();
}

/// **The Favorites tab is the account's list made into a lineup**: the channels it names, DVR
/// and Plex alike, in the account's order, with Plex's lineup read aside for the Plex ones. The
/// guide opens on it while no tab was picked and nothing was shown yet.
#[test]
fn the_favorites_tab_lists_the_named_channels_in_the_accounts_order_and_the_guide_opens_on_it() {
    use crate::live_tv::{CloudAuth, Source};
    let cloud = |n: usize| Channel { cloud: true, part_key: format!("/library/parts/{n}.m3u8"), ..channel(n) };
    let mut s = ready(4);
    s.set_cloud_auth(CloudAuth { client_id: "cid".into(), credential: "secret".into() });
    s.ask_favorites();
    land_favorites(&mut s, vec![
        favorite("tv.plex.provider.epg", "c9", ""),
        favorite(&dvr_source(), "c2", ""),
        favorite(&dvr_source(), "nowhere", ""),
        favorite(&dvr_source(), "c0", ""),
    ]);
    // nothing was shown yet and no tab picked: the guide opens on the favorites
    let snap = s.snapshot();
    assert_eq!(snap.source, Source::Favorites);
    assert!(snap.channels_phase.is_loading(), "Plex's lineup is still being read for the Plex favorite");
    let (job, post) = take(&mut s).into_iter().find(|(j, _)| matches!(j, Job::CloudChannels { .. })).expect("Plex's lineup, read aside");
    assert!(matches!(job, Job::CloudChannels { source: Source::Plex, .. }));
    post.post(Landing::CloudChannels { source: Source::Plex, result: Ok((8..11).map(cloud).collect()) });
    s.pump();
    let snap = s.snapshot();
    assert!(snap.channels_phase.is_ready());
    assert_eq!(snap.channels.iter().map(|c| c.identifier.as_str()).collect::<Vec<_>>(), ["c9", "c2", "c0"], "the account's order; an entry naming nothing is left out");
    assert!(snap.channels[0].cloud && !snap.channels[1].cloud);
    assert!(snap.is_favorite(&channel(2)) && !snap.is_favorite(&channel(1)));
    assert!(snap.favorite_dvr && snap.favorite_cloud);
    // a tab picked is kept: the guide no longer moves to the favorites by itself
    s.select_source(Source::Dvr);
    s.land_on_favorites();
    assert_eq!(s.snapshot().source, Source::Dvr);
}

/// **Adding a channel writes the account's form of entry**, through the write queue, and the list
/// it wrote is the favorites at once; the DVR's lists are not read again for it.
#[test]
fn adding_a_favorite_writes_this_servers_entry_and_its_answer_is_the_list() {
    use crate::live_tv::{CloudAuth, Source};
    let mut s = ready(3);
    // the page shows the DVR's guide: a favorite added there does not move the guide off it
    s.want_window(0..3, NOON, NOON + 3600);
    s.set_cloud_auth(CloudAuth { client_id: "cid".into(), credential: "secret".into() });
    s.ask_favorites();
    land_favorites(&mut s, vec![]);
    assert_eq!(s.snapshot().source, Source::Dvr, "no favorites: the guide stays on the DVR");
    assert!(!s.snapshot().is_favorite(&channel(1)));
    assert!(s.set_favorite(1, true));
    s.pump();
    let (job, post) = take(&mut s).into_iter().find(|(j, _)| matches!(j, Job::Write(_))).expect("the change goes out");
    let Job::Write(Write::Favorite { op: FavoriteOp::Add(entry), add: true, .. }) = &job else { panic!("{job:?}") };
    assert_eq!((entry.source.as_str(), entry.id.as_str(), entry.vcn.as_str()), (dvr_source().as_str(), "c1", "1.1"));
    let Job::Write(write) = job else { unreachable!() };
    let written = vec![favorite(&dvr_source(), "c1", "1.1")];
    post.post(Landing::Write { write, result: Ok(WriteDone { favorites: Some(written), ..WriteDone::default() }) });
    s.pump();
    let snap = s.snapshot();
    assert!(snap.is_favorite(&channel(1)));
    assert_eq!(snap.notice.map(|n| n.op), Some(WriteOp::FavoriteAdded));
    assert!(!take(&mut s).iter().any(|(j, _)| matches!(j, Job::Dvr { .. })), "a favorites change is not a DVR change");
    // taking it off removes every entry that names it
    assert!(s.set_favorite(1, false));
    s.pump();
    let (job, _) = take(&mut s).into_iter().find(|(j, _)| matches!(j, Job::Write(_))).expect("the change goes out");
    assert!(matches!(job, Job::Write(Write::Favorite { op: FavoriteOp::Remove(ref gone), add: false, .. }) if gone.len() == 1));
    assert!(!s.set_favorite(2, false), "not a favorite: nothing to take off");
}

/// **Recording options are read for the panel that asked**: a new recording's from the program's
/// template (the entry its kind records), a rule's from the rule; an earlier ask's answer is not
/// the panel's; the panel's write closes it, overlays the guide as a record does, and an edit
/// says it was saved.
#[test]
fn recording_options_are_read_for_the_panel_and_its_write_records_with_them() {
    use crate::live_tv::OptionsFor;
    use plx_plex::plex::live_tv::{EditSubscription, TemplateChoice};
    let mut s = ready(1);
    let a = airing(0, NOON, "plex://episode/e1", "plex://show/s1");
    s.ask_options(OptionsFor::Record { airing: a.clone(), kind: RecordKind::Series });
    s.pump();
    let (job, first) = take(&mut s).into_iter().find(|(j, _)| matches!(j, Job::Options { .. })).expect("the options are read");
    let Job::Options { serial: old, .. } = job else { unreachable!() };
    assert!(s.snapshot().options.is_some_and(|o| o.phase.is_loading()));
    // asked again before the first answer: the first answer is history
    s.ask_options(OptionsFor::Record { airing: a.clone(), kind: RecordKind::Series });
    s.pump();
    let (job, post) = take(&mut s).into_iter().find(|(j, _)| matches!(j, Job::Options { .. })).unwrap();
    let Job::Options { serial, .. } = job else { unreachable!() };
    first.post(Landing::Options { serial: old, result: Err(LiveError::Transport) });
    s.pump();
    assert!(s.snapshot().options.is_some_and(|o| o.phase.is_loading()), "an earlier ask's answer is not the panel's");
    let choice = TemplateChoice { kind: 2, parameters: "hints%5Bx%5D=1".into(), target_section: "4".into(), section_title: "DVR".into(), ..Default::default() };
    post.post(Landing::Options { serial, result: Ok(OptionsAnswer::Choice(choice.clone())) });
    s.pump();
    let o = s.snapshot().options.expect("open");
    assert!(o.phase.is_ready());
    assert_eq!(o.library(), "DVR");
    // the panel's Record: the options close, the write runs, the guide is marked
    s.write(Write::RecordWith { airing: a.clone(), kind: RecordKind::Series, choice: choice.clone() });
    assert!(s.snapshot().options.is_none());
    s.pump();
    let (job, post) = take(&mut s).into_iter().find(|(j, _)| matches!(j, Job::Write(_))).unwrap();
    let Job::Write(w) = job else { unreachable!() };
    post.post(Landing::Write { write: w, result: Ok(WriteDone { rule: "51".into(), ..WriteDone::default() }) });
    s.pump();
    assert_eq!(s.snapshot().notice.map(|n| (n.op, n.rule)), Some((WriteOp::RecordSeries, "51".to_string())));
    // a rule's options: read from the rule, saved as an edit
    s.ask_options(OptionsFor::Rule { id: "51".into() });
    s.pump();
    let (job, _) = take(&mut s).into_iter().find(|(j, _)| matches!(j, Job::Options { .. })).unwrap();
    assert!(matches!(job, Job::Options { of: OptionsFor::Rule { ref id }, .. } if id == "51"));
    s.write(Write::EditRule { id: "51".into(), edit: EditSubscription::default() });
    s.pump();
    let (job, post) = take(&mut s).into_iter().find(|(j, _)| matches!(j, Job::Write(_))).unwrap();
    let Job::Write(w) = job else { unreachable!() };
    post.post(Landing::Write { write: w, result: Ok(WriteDone::default()) });
    s.pump();
    assert_eq!(s.snapshot().notice.map(|n| n.op), Some(WriteOp::EditRule));
    // closed without a write
    s.ask_options(OptionsFor::Rule { id: "51".into() });
    s.close_options();
    assert!(s.snapshot().options.is_none());
}

/// **What's On is read when it opens and kept fresh**: one read at a time, its rows kept when a
/// later read fails; the channels watched lately are remembered, the latest first.
#[test]
fn whats_on_is_read_once_at_a_time_kept_through_a_failed_refresh_and_remembers_the_channels_watched() {
    use plx_plex::plex::live_tv::LiveHub;
    let mut s = ready(3);
    s.refresh_hubs();
    s.refresh_hubs();
    s.pump();
    let reads: Vec<_> = take(&mut s).into_iter().filter(|(j, _)| matches!(j, Job::Hubs { .. })).collect();
    assert_eq!(reads.len(), 1, "one read answers both asks");
    assert!(s.snapshot().hubs_phase.is_loading());
    let (_, post) = reads.into_iter().next().unwrap();
    post.post(Landing::Hubs(Ok(vec![LiveHub { title: "On Now".into(), items: vec![airing(0, NOON, "g", "s")], ..Default::default() }])));
    s.pump();
    assert_eq!(s.snapshot().hubs.len(), 1);
    s.refresh_hubs();
    s.pump();
    let (_, post) = take(&mut s).into_iter().find(|(j, _)| matches!(j, Job::Hubs { .. })).unwrap();
    post.post(Landing::Hubs(Err(LiveError::Transport)));
    s.pump();
    let snap = s.snapshot();
    assert!(snap.hubs_phase.is_ready() && snap.hubs.len() == 1, "a failed refresh keeps what was shown");
    // the channels tuned lately, the latest first, as the lineup names them
    s.tune(2, None);
    s.tune(0, None);
    s.tune(2, None);
    let recent: Vec<String> = s.snapshot().recent.iter().map(|c| c.identifier.clone()).collect();
    assert_eq!(recent, ["c2", "c0"]);
}
