//! Parsers and request builders of the Live TV / DVR surface, graded against the synthetic shapes
//! in `fixtures.rs`. No server: the HTTP doors are `client.rs`'s and are tested there.

use super::fixtures::*;
use super::*;

fn json(s: &str) -> Value {
    serde_json::from_str(s).expect("fixture is valid JSON")
}

/// The What's On hubs: a hub of programs gives each its airing, a hub of channels each channel,
/// an empty hub is left out, and the path asks what the stock clients ask.
#[test]
fn the_whats_on_hubs_read_programs_and_channels_and_skip_empty_ones() {
    let body = serde_json::json!({"MediaContainer": {"Hub": [
        {"title": "What's On Now", "hubIdentifier": "p.discover.onNow", "type": "mixed", "Metadata": [
            {"title": "A Show", "grandparentTitle": "Show", "type": "episode", "Media": [{"beginsAt": 100, "endsAt": 200, "channelIdentifier": "c1", "channelVcn": "4.1"}]},
            {"title": "No times", "type": "movie"}
        ]},
        {"title": "Recent Channels", "hubIdentifier": "p.recentChannels", "type": "channel", "Metadata": [
            {"title": "KXYZ", "channelIdentifier": "c2", "vcn": "5.1", "thumb": "/logo.png"}
        ]},
        {"title": "Empty", "hubIdentifier": "p.empty", "Metadata": []}
    ]}});
    let hubs = super::parse::hubs(&body);
    assert_eq!(hubs.len(), 2);
    assert_eq!((hubs[0].items.len(), hubs[0].items[0].channel_identifier.as_str(), hubs[0].items[0].begins_at), (1, "c1", 100), "an item without an airing or a channel is left out");
    assert!(!hubs[0].of_channels());
    let ch = &hubs[1].items[0];
    assert_eq!((ch.channel_identifier.as_str(), ch.channel_vcn.as_str(), ch.channel_thumb.as_str(), ch.begins_at), ("c2", "5.1", "/logo.png", 0));
    assert!(hubs[1].of_channels());
    let path = super::hubs_path("tv.plex.providers.epg.cloud:2", 16).expect("path");
    assert!(path.starts_with("/tv.plex.providers.epg.cloud:2/hubs/discover?count=16"), "{path}");
    assert!(path.contains("includeRecentChannels=1") && path.contains("includeStations=1"), "{path}");
    assert!(super::hubs_path("../x", 16).is_none());
}

/// A live timeline answer's capture window: the `CaptureBuffer` one when the playback's own
/// session rides beside it, the lone top-level one otherwise.
#[test]
fn a_timeline_answer_names_the_tuners_capture_window() {
    let both = serde_json::json!({"MediaContainer": {
        "TranscodeSession": {"timeStamp": 999.0, "minOffsetAvailable": 0.0, "maxOffsetAvailable": 1.0},
        "CaptureBuffer": {"TranscodeSession": [{"timeStamp": 1000.5, "minOffsetAvailable": 2.0, "maxOffsetAvailable": 600.0}]}
    }});
    let w = super::parse::timeline_capture(&both).expect("window");
    assert_eq!((w.time_stamp, w.min_offset, w.max_offset), (1000.5, 2.0, 600.0));
    assert_eq!(w.live_edge(), 1600.5);
    let lone = serde_json::json!({"MediaContainer": {"TranscodeSession": {"timeStamp": 5.0, "minOffsetAvailable": 0.0, "maxOffsetAvailable": 30.0}}});
    assert_eq!(super::parse::timeline_capture(&lone).map(|w| w.max_offset), Some(30.0));
    assert!(super::parse::timeline_capture(&serde_json::json!({"MediaContainer": {"size": 0}})).is_none());
}

/// The server's transcode list names a live session's capture window under its own key (a server
/// whose timeline answer carries none).
#[test]
fn the_transcode_list_names_a_sessions_capture_window_by_its_key() {
    let list = serde_json::json!({"MediaContainer": {"size": 2, "TranscodeSession": [
        {"key": "/transcode/sessions/other", "timeStamp": 1.0, "maxOffsetAvailable": 9.0},
        {"key": "/transcode/sessions/mine", "timeStamp": "1000.5", "minOffsetAvailable": 0, "maxOffsetAvailable": "42.25"}
    ]}});
    let w = super::parse::transcode_capture(&list, "mine").expect("window");
    assert_eq!((w.time_stamp, w.max_offset), (1000.5, 42.25));
    assert!(super::parse::transcode_capture(&list, "gone").is_none());
    assert!(super::parse::transcode_capture(&list, "").is_none());
    let bare = serde_json::json!({"MediaContainer": {"TranscodeSession": {"key": "/transcode/sessions/mine", "videoDecision": "transcode"}}});
    assert!(super::parse::transcode_capture(&bare, "mine").is_none(), "a session with no window");
}

#[test]
fn providers_keep_only_epg_providers_with_their_dvr_key_and_features() {
    let p = parse::providers(&json(PROVIDERS));
    assert_eq!(p.len(), 1);
    assert_eq!(p[0].identifier, "tv.plex.providers.epg.cloud:12");
    assert_eq!(p[0].dvr_key, "12", "parentID is the tune path's dvr id, numeric on the wire");
    assert!(p[0].grid && p[0].recordable && p[0].tunable());
    assert!(parse::providers(&json(PROVIDERS_NO_DVR)).is_empty());
}

#[test]
fn a_provider_without_a_parent_id_cannot_tune() {
    let body = json(r#"{"MediaContainer":{"MediaProvider":[{"identifier":"tv.plex.providers.epg.xmltv:3","Feature":[{"type":"grid"}]}]}}"#);
    let p = parse::providers(&body);
    assert_eq!(p.len(), 1);
    assert!(!p[0].tunable() && !p[0].recordable && p[0].grid);
}

#[test]
fn lineup_reads_mixed_scalar_types_and_skips_a_row_without_an_id() {
    let c = parse::channels(&json(LINEUP));
    assert_eq!(c.len(), 2);
    assert_eq!(c[0].identifier, "s0a1-c001");
    assert!(c[0].hd);
    assert_eq!(c[0].label(), "SAAA");
    assert_eq!(c[1].vcn, "4", "a numeric vcn reads as text");
    assert!(!c[1].hd);
    assert_eq!(c[1].label(), "Channel B", "no call sign: the title heads the row");
}

#[test]
fn grid_emits_one_airing_per_spanning_media_sorted_and_deduped() {
    let ch = Channel { identifier: "s0a1-c001".into(), grid_key: "s0a1-c001".into(), vcn: "2.1".into(), title: "Channel A".into(), ..Channel::default() };
    let a = parse::grid(&json(GRID), Some(&ch));
    assert_eq!(a.len(), 2, "the repeated start dedups; the zero-span media is dropped");
    assert_eq!(a[0].title, "Episode A");
    assert_eq!((a[0].begins_at, a[0].ends_at), (1_760_000_000, 1_760_001_800), "endsAt as a string still reads");
    assert_eq!((a[0].season, a[0].episode), (2, 5));
    assert!(a[0].on_air && a[0].premiere);
    assert_eq!(a[0].subscription_id, "", "subscriptionID 0 means no rule");
    assert_eq!(a[0].show_subscription_id, "41");
    assert!(a[0].is_recording_scheduled());
    assert_eq!(a[0].covering_subscription(), "41");
    assert_eq!(a[1].title, "Movie B");
    assert_eq!(a[1].channel_identifier, "s0a1-c001", "the requested channel fills a missing identity");
    assert!(a[1].airs_at(1_760_001_800) && !a[1].airs_at(1_760_007_200));
}

#[test]
fn subscriptions_read_settings_hints_and_nested_grabs_and_skip_keyless_rows() {
    let s = parse::subscriptions(&json(SUBSCRIPTIONS));
    assert_eq!(s.len(), 2);
    let show = &s[0];
    assert_eq!(show.id, "41");
    assert_eq!(show.kind, 2);
    assert_eq!(show.target_location, "", "location 0 is no location");
    assert_eq!(show.hint.title, "Show A");
    assert_eq!(show.settings.len(), 2);
    let res = &show.settings[0];
    assert_eq!(res.kind, SettingKind::Int);
    assert_eq!(res.value, "720");
    assert_eq!(res.value_label(), "HD only", "enum labels are percent-decoded");
    assert_eq!(res.options.len(), 3);
    let one_shot = &show.settings[1];
    assert_eq!((one_shot.kind, one_shot.value.as_str(), one_shot.hidden), (SettingKind::Bool, "0", true), "a JSON false goes back as 0");
    assert_eq!(show.grabs.len(), 1, "a lone grab object reads as a list of one");
    assert_eq!(show.grabs[0].status, GrabStatus::InProgress);
    assert!((show.grabs[0].percent - 37.5).abs() < f64::EPSILON);
    assert_eq!(s[1].id, "42", "a key written as its resource path reads as the bare id");
    assert_eq!(s[1].kind, 1);
}

#[test]
fn template_flattens_choices_and_keeps_parameters_verbatim() {
    let t = parse::template(&json(TEMPLATE));
    assert_eq!(t.len(), 2);
    assert_eq!(t[0].title, "This Episode");
    assert_eq!(t[0].kind, 4);
    assert!(!t[0].selected && t[1].selected);
    assert!(t[0].parameters.starts_with("hints%5Bguid%5D="), "parameters are not decoded");
    assert_eq!(t[0].settings[1].value, "1", "a bool \"true\" goes back as 1");
    assert!(t[1].settings[0].advanced);
}

#[test]
fn scheduled_sorts_soonest_first_and_derives_a_missing_grab_key() {
    let g = parse::scheduled(&json(SCHEDULED));
    assert_eq!(g.len(), 2);
    assert_eq!(g[0].id, "g1");
    assert!(g[0].status.is_recording());
    assert_eq!(g[1].key, "/media/grabbers/operations/g2", "the cancel key is derived from the id");
    assert_eq!(g[1].subscription_id, "42");
    assert_eq!(g[1].airing.channel_title, "Channel B");
}

#[test]
fn create_reads_the_new_rule_and_its_conflicts() {
    let ok = parse::created(&json(CREATED)).unwrap();
    assert_eq!(ok.subscription_id, "43");
    assert!(ok.conflicts.is_empty());
    let conflicted = parse::created(&json(CREATED_CONFLICTED)).unwrap();
    assert_eq!(conflicted.subscription_id, "44", "a conflicted rule exists and has an id to prefer or delete");
    assert_eq!(conflicted.conflicts.len(), 1);
    assert_eq!(conflicted.conflicts[0].grab_key, "/media/grabbers/operations/g1");
    assert!(!conflicted.conflicts[0].rolling);
}

#[test]
fn tune_reads_the_subscription_item_capture_window_and_programs() {
    let t = parse::tune(&json(TUNE)).unwrap();
    assert_eq!(t.subscription_id, "77");
    assert!(t.playable());
    assert_eq!(t.item.key, "/livetv/sessions/0e0e0e0e-0000-4000-8000-000000000001/1");
    assert_eq!(t.item.session, "0e0e0e0e-0000-4000-8000-000000000001");
    assert_eq!((t.item.width, t.item.height), (1920, 1080));
    assert_eq!(t.item.video_codec, "mpeg2video");
    assert_eq!(t.item.audio_codec, "ac3");
    assert_eq!(t.item.channel_title, "Channel A");
    let cap = t.capture.expect("capture window");
    assert!((cap.time_stamp - 1_760_000_100.5).abs() < 1e-6);
    assert!((cap.depth() - 42.25).abs() < 1e-9);
    assert!((cap.live_edge() - 1_760_000_142.75).abs() < 1e-6);
    assert_eq!(t.programs.len(), 2);
    assert_eq!(t.programs[0].title, "Episode A", "programs are sorted by start");
}

#[test]
fn a_conflicted_tune_has_nothing_to_play_but_keeps_the_subscription_to_release() {
    let t = parse::tune(&json(TUNE_CONFLICTED)).unwrap();
    assert_eq!(t.subscription_id, "78");
    assert!(!t.playable());
    assert_eq!(t.conflicts.len(), 2, "Conflicts as an array of objects reads too");
    assert!(t.conflicts[1].rolling, "a rolling conflict is another viewer, not a recording");
}

#[test]
fn the_spec_shape_tune_reads_without_a_subscription() {
    let t = parse::tune(&json(TUNE_SPEC_SHAPE)).unwrap();
    assert_eq!(t.subscription_id, "");
    assert_eq!(t.item.key, "/livetv/sessions/0e0e0e0e-0000-4000-8000-000000000002");
    assert_eq!(t.item.session, "0e0e0e0e-0000-4000-8000-000000000002");
    assert!(t.playable());
}

#[test]
fn an_in_band_refusal_is_a_server_error_not_an_empty_success() {
    let body = json(SERVER_REFUSAL);
    assert_eq!(
        parse::tune(&body),
        Err(LiveError::Server { status: -1, message: "No tuners available".into() })
    );
    assert!(matches!(ok_body(ValueOutcome::Response { status: 200, body: Some(body) }), Err(LiveError::Server { .. })));
}

#[test]
fn unreadable_and_failed_answers_map_to_their_errors() {
    assert_eq!(parse::tune(&json(r#"{"MediaContainer":{"size":0}}"#)), Err(LiveError::Unreadable));
    assert_eq!(ok_body(ValueOutcome::Transport).unwrap_err(), LiveError::Transport);
    assert_eq!(ok_body(ValueOutcome::Response { status: 403, body: None }).unwrap_err(), LiveError::Status(403));
    assert_eq!(ok_body(ValueOutcome::Response { status: 200, body: None }).unwrap_err(), LiveError::Unreadable);
    assert_eq!(ok_status(ValueOutcome::Response { status: 200, body: None }), Ok(()));
    assert!(parse::channels(&json(r#"[1,2]"#)).is_empty(), "a non-container body is an empty lineup, not a panic");
}

#[test]
fn the_grid_path_names_a_utc_day_and_a_channel_grid_key() {
    assert_eq!(utc_date(0), "1970-01-01");
    assert_eq!(utc_date(1_760_000_000), "2025-10-09");
    assert_eq!(utc_date(951_782_400), "2000-02-29");
    assert_eq!(utc_date(-1), "1969-12-31");
    assert_eq!(
        grid_path("tv.plex.providers.epg.cloud:12", "s0a1-c001", "2025-10-09").unwrap(),
        "/tv.plex.providers.epg.cloud:12/grid?channelGridKey=s0a1-c001&date=2025-10-09"
    );
    assert!(grid_path("tv.plex.providers.epg.cloud:12", "k", "2025-10-9").is_none());
    assert!(grid_path("/etc/passwd", "k", "2025-10-09").is_none(), "only an EPG provider prefixes a path");
    assert_eq!(lineup_path("tv.plex.providers.epg.cloud:12").unwrap(), "/tv.plex.providers.epg.cloud:12/lineups/dvr/channels");
}

#[test]
fn create_encodes_pref_keys_and_appends_the_template_parameters_verbatim() {
    let t = parse::template(&json(TEMPLATE));
    let mut req = CreateSubscription::from_choice(&t[0]);
    assert_eq!(req.prefs, vec![("startOffsetMinutes".into(), "0".into()), ("oneShot".into(), "1".into())]);
    req.prefs[0].1 = "5".into();
    let path = create_path(&req).unwrap();
    assert_eq!(
        path,
        "/media/subscriptions?includeGrabs=1&targetLibrarySectionID=3&type=4\
         &prefs%5BstartOffsetMinutes%5D=5&prefs%5BoneShot%5D=1\
         &hints%5Bguid%5D=plex%3A%2F%2Fepisode%2Fs00000000000000000000001&params%5BmediaProviderID%5D=13"
    );
    req.target_location = "7".into();
    assert!(create_path(&req).unwrap().contains("&targetSectionLocationID=7&type=4"));
}

#[test]
fn create_refuses_a_request_it_cannot_send_whole() {
    let base = CreateSubscription { kind: 4, parameters: "hints%5Bguid%5D=x".into(), target_section: "3".into(), ..Default::default() };
    assert!(create_path(&base).is_some());
    assert!(create_path(&CreateSubscription { target_section: String::new(), ..base.clone() }).is_none());
    assert!(create_path(&CreateSubscription { parameters: String::new(), ..base.clone() }).is_none());
    assert!(create_path(&CreateSubscription { parameters: "a=b c".into(), ..base.clone() }).is_none(), "a space cannot ride verbatim");
    assert!(create_path(&CreateSubscription { parameters: "a=b#c".into(), ..base.clone() }).is_none());
    assert!(create_path(&CreateSubscription { parameters: "X-Plex-Token=nope".into(), ..base }).is_none(), "a server fragment may not carry a token");
}

#[test]
fn edit_move_delete_cancel_and_tune_paths() {
    let edit = EditSubscription { target_section: "3".into(), target_location: String::new(), prefs: vec![("minVideoQuality".into(), "1080".into())] };
    assert_eq!(edit_path("41", &edit).unwrap(), "/media/subscriptions/41?includeGrabs=1&targetLibrarySectionID=3&prefs%5BminVideoQuality%5D=1080");
    assert!(edit_path("41/../x", &edit).is_none());
    assert_eq!(move_path("41", None).unwrap(), "/media/subscriptions/41/move");
    assert_eq!(move_path("41", Some("42")).unwrap(), "/media/subscriptions/41/move?after=42");
    assert!(move_path("41", Some("4 2")).is_none());
    assert_eq!(subscription_path("41").unwrap(), "/media/subscriptions/41");
    assert!(subscription_path("").is_none());
    assert_eq!(grab_path("/media/grabbers/operations/g1").unwrap(), "/media/grabbers/operations/g1");
    assert!(grab_path("/library/metadata/1").is_none(), "a server key may only name a grab");
    assert!(grab_path("/media/grabbers/operations/../x").is_none());
    assert_eq!(
        tune_path("12", "s0a1-c001", "sess-1").unwrap(),
        "/livetv/dvrs/12/channels/s0a1-c001/tune?X-Plex-Session-Identifier=sess-1"
    );
    assert_eq!(
        tune_path("12", "a/b c", "s").unwrap(),
        "/livetv/dvrs/12/channels/a%2Fb%20c/tune?X-Plex-Session-Identifier=s",
        "the channel is one encoded segment"
    );
    assert!(tune_path("", "c", "s").is_none());
    assert!(tune_path("12", "c", "").is_none(), "a tune names its session");
    assert_eq!(template_path("plex://episode/1").unwrap(), "/media/subscriptions/template?guid=plex%3A%2F%2Fepisode%2F1");
}

#[test]
fn the_write_gate_refuses_a_debug_build_on_a_real_server_unless_the_owner_armed_it() {
    use guard::{is_mock_version, permit};
    assert!(permit(false, false, false), "a release build always writes");
    assert!(!permit(true, false, false), "a debug build on a real server does not");
    assert!(permit(true, true, false), "the mock takes debug writes");
    assert!(permit(true, false, true), "the owner's switch lets a debug build write");
    assert!(is_mock_version("1.41.0.0000-synthetic"));
    assert!(!is_mock_version("1.43.3.10861-cd85035e7"));
    assert!(!is_mock_version(""));
}

#[test]
fn from_subscription_echoes_the_rule_it_edits() {
    let s = parse::subscriptions(&json(SUBSCRIPTIONS));
    let e = EditSubscription::from_subscription(&s[0]);
    assert_eq!(e.target_section, "3");
    assert_eq!(e.prefs, vec![("minVideoQuality".into(), "720".into()), ("oneShot".into(), "0".into())]);
}

/// A loopback PMS that answers `answers` in order and returns the request heads it saw.
fn serve(answers: Vec<(&'static str, &'static str)>) -> (u16, std::thread::JoinHandle<Vec<String>>) {
    use std::io::{Read, Write};
    use std::time::{Duration, Instant};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    listener.set_nonblocking(true).unwrap();
    let handle = std::thread::spawn(move || {
        let mut seen = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while seen.len() < answers.len() && Instant::now() < deadline {
            let Ok((mut socket, _)) = plx_base::testnet::accept(&listener) else {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            };
            socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut buf = [0; 8192];
            let n = socket.read(&mut buf).unwrap();
            let (status, body) = answers[seen.len()];
            write!(socket, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            seen.push(String::from_utf8_lossy(&buf[..n]).into_owned());
        }
        seen
    });
    (port, handle)
}

fn loopback_client(port: u16) -> Client {
    Client::new(crate::plex::ServerId::UNSET, "fixture", crate::plex::Origin::http("127.0.0.1", port as i32), "tok3n", "cid")
}

#[test]
fn a_release_sends_delete_and_reads_404_as_already_released() {
    let (port, server) = serve(vec![("200 OK", ""), ("404 Not Found", "")]);
    let client = loopback_client(port);
    assert_eq!(client.live_release("77"), Ok(()));
    assert_eq!(client.live_release("77"), Ok(()), "an already-gone tune is released");
    let seen = server.join().unwrap();
    assert_eq!(seen.len(), 2);
    assert!(seen[0].starts_with("DELETE /media/subscriptions/77?X-Plex-Token=tok3n HTTP/1.1"), "{}", seen[0]);
    assert!(seen[0].contains("Accept: application/json"));
    assert_eq!(client.live_release("../x"), Err(LiveError::Invalid), "never sent");
}

/// Registers the loopback as a server that answers with `version`, so the write gate reads it;
/// the guard holds the registry lock and resets it on drop.
struct Registered(#[allow(dead_code)] plx_base::testlock::Serial);
impl Drop for Registered {
    fn drop(&mut self) {
        crate::plex::servers::reset_for_test();
    }
}
fn registered_client(port: u16, version: &str) -> (Registered, &'static Client) {
    let guard = Registered(plx_base::testlock::serial());
    crate::plex::servers::reset_for_test();
    let id = crate::plex::servers::register_with_client_id("synthetic-mock", "127.0.0.1", i32::from(port), "tok3n", "cid");
    crate::plex::serverinfo::store_for_test(id, crate::plex::serverinfo::Subscription::Unknown, version);
    (guard, crate::plex::client_for(id).expect("registered"))
}

#[test]
fn a_read_parses_json_and_a_write_reports_its_status() {
    let (port, server) = serve(vec![
        ("200 OK", PROVIDERS),
        ("200 OK", TUNE),
        ("500 Internal Server Error", ""),
        ("200 OK", CREATED_CONFLICTED),
    ]);
    let (_registry, client) = registered_client(port, "1.41.0.0000-synthetic");
    let providers = client.live_tv_providers().unwrap();
    assert_eq!(providers[0].dvr_key, "12");
    let tuned = client.live_tune("12", "s0a1-c001", "sess-1").unwrap();
    assert_eq!(tuned.subscription_id, "77");
    assert_eq!(client.live_tune("12", "s0a1-c001", "sess-1"), Err(LiveError::Status(500)), "Tuning failed is a status");
    let req = CreateSubscription { kind: 4, parameters: "hints%5Bguid%5D=x".into(), target_section: "3".into(), ..Default::default() };
    let made = client.dvr_create(&req).unwrap();
    assert_eq!(made.subscription_id, "44");
    assert_eq!(made.conflicts.len(), 1);
    let seen = server.join().unwrap();
    assert!(seen[0].starts_with("GET /media/providers?"));
    assert!(seen[1].starts_with("POST /livetv/dvrs/12/channels/s0a1-c001/tune?X-Plex-Session-Identifier=sess-1&"), "{}", seen[1]);
    assert!(seen[3].starts_with("POST /media/subscriptions?includeGrabs=1&targetLibrarySectionID=3&type=4&hints%5Bguid%5D=x&X-Plex-Token="), "{}", seen[3]);
}

/// A debug build refuses every write to a server that is not the mock, before anything is sent;
/// the release of an earlier tune is never refused.
#[cfg(feature = "devtriggers")]
#[test]
fn a_debug_build_sends_no_write_to_a_real_server() {
    let (port, server) = serve(vec![("200 OK", "")]);
    let (_registry, client) = registered_client(port, "1.43.3.10861-cd85035e7");
    let req = CreateSubscription { kind: 4, parameters: "hints%5Bguid%5D=x".into(), target_section: "3".into(), ..Default::default() };
    assert_eq!(client.dvr_create(&req), Err(LiveError::Refused));
    assert_eq!(client.dvr_delete("41"), Err(LiveError::Refused));
    assert_eq!(client.dvr_move("41", None), Err(LiveError::Refused));
    assert_eq!(client.dvr_cancel_grab("/media/grabbers/operations/g1"), Err(LiveError::Refused));
    assert_eq!(client.live_tune("12", "c", "sess-1"), Err(LiveError::Refused));
    assert!(guard::last_write_refused());
    assert_eq!(client.live_release("77"), Ok(()), "a release goes out");
    let seen = server.join().unwrap();
    assert_eq!(seen.len(), 1, "only the release reached the server: {seen:?}");
    assert!(seen[0].starts_with("DELETE /media/subscriptions/77?"));
}

#[test]
fn a_live_stream_is_an_h264_aac_hls_transcode_of_the_tuned_key() {
    let decision = r#"{"MediaContainer":{"generalDecisionCode":1000,"Metadata":[{"Media":[{"width":1280,"height":720,
        "Part":[{"decision":"transcode","Stream":[{"streamType":1,"codec":"h264","frameRate":59.94},{"streamType":2,"codec":"aac"}]}]}]}]}}"#;
    let (port, server) = serve(vec![("200 OK", decision)]);
    let client = loopback_client(port);
    let off = plx_base::task::OffFrame::for_test();
    let s = client.live_stream(&off, "/livetv/sessions/0e0e-1", "enc-1", 0).unwrap();
    assert_eq!((s.video_codec.as_str(), s.audio_codec.as_str(), s.width, s.height), ("h264", "aac", 1280, 720));
    assert_eq!(s.encoder_session, "enc-1");
    assert_eq!(s.fps, 59.94, "the Load declares the encode's own rate");
    assert!(s.url.contains("/video/:/transcode/universal/start.m3u8?path=%2Flivetv%2Fsessions%2F0e0e-1"), "{}", s.url);
    assert!(s.url.contains("protocol=hls") && s.url.contains("directStream=0") && s.url.contains("session=enc-1"));
    let seen = server.join().unwrap();
    assert!(seen[0].starts_with("GET /video/:/transcode/universal/decision?path=%2Flivetv%2Fsessions%2F0e0e-1&"), "{}", seen[0]);
    assert_eq!(client.live_stream(&off, "livetv/sessions/x", "enc", 0), Err(LiveError::Invalid));
}

#[test]
fn a_refused_live_decision_says_why() {
    let refusal = r#"{"MediaContainer":{"generalDecisionCode":2000,"generalDecisionText":"Neither direct play nor conversion is available."}}"#;
    let (port, server) = serve(vec![("200 OK", refusal)]);
    let client = loopback_client(port);
    let off = plx_base::task::OffFrame::for_test();
    assert_eq!(
        client.live_stream(&off, "/livetv/sessions/x", "enc", 0),
        Err(LiveError::Server { status: 2000, message: "Neither direct play nor conversion is available.".into() })
    );
    server.join().unwrap();
}

#[test]
fn the_live_keepalive_reports_the_tuned_key_with_no_position() {
    let (port, server) = serve(vec![("200 OK", "")]);
    let client = loopback_client(port);
    assert!(client.live_timeline("/livetv/sessions/x", "9001", "playing", "enc"));
    let seen = server.join().unwrap();
    assert!(seen[0].starts_with("POST /:/timeline?ratingKey=9001&key=%2Flivetv%2Fsessions%2Fx&state=playing&time=0&duration=0&X-Plex-Session-Identifier=enc"), "{}", seen[0]);
    assert!(!client.live_timeline("", "1", "playing", "enc"));
}

#[test]
fn enum_labels_decode_like_decode_uri_component() {
    assert_eq!(parse::pct_decode("12%3A30%20AM"), "12:30 AM");
    assert_eq!(parse::pct_decode("a+b"), "a+b");
    assert_eq!(parse::pct_decode("100%"), "100%");
    assert_eq!(parse::pct_decode("%zz"), "%zz");
}

#[test]
fn ultrablur_reads_four_corners_from_an_object_or_an_array() {
    let one = serde_json::json!({"MediaContainer": {"size": 1, "UltraBlurColors": [
        {"topLeft": "44181b", "topRight": "55140b", "bottomRight": "9a3936", "bottomLeft": "#31313a"}
    ]}});
    let c = parse::ultrablur(&one).expect("four corners");
    assert!((c[0][0] - 0x44 as f32 / 255.0).abs() < 1e-6, "top-left red: {c:?}");
    assert!((c[3][2] - 0x3a as f32 / 255.0).abs() < 1e-6, "bottom-left blue, '#' tolerated: {c:?}");
    let obj = serde_json::json!({"MediaContainer": {"UltraBlurColors":
        {"topLeft": "000000", "topRight": "ffffff", "bottomRight": "0000ff", "bottomLeft": "ff0000"}}});
    assert_eq!(parse::ultrablur(&obj).expect("object form")[1], [1.0, 1.0, 1.0]);
    let short = serde_json::json!({"MediaContainer": {"UltraBlurColors": [{"topLeft": "000000"}]}});
    assert_eq!(parse::ultrablur(&short), None, "all four corners or none");
}
