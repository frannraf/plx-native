"""The mock's Live TV / DVR surface (`tests/mock_livetv.py`, `mock_pms.py --livetv`): the shapes the
Rust layer `plex::live_tv` parses, the schedule's determinism, the rule/grab/tune lifecycle with
tuner conflicts, the write log, and the growing HLS stream.

Run: python3 -I tests/test_mock_livetv.py   (-I drops the script dir, so it is added below)
"""
import json
import pathlib
import shutil
import sys
import tempfile
import unittest
import urllib.parse

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))

import mock_livetv  # noqa: E402
from mock_pms import Library, MockPms  # noqa: E402

NOW = 1_760_000_000  # 2025-10-09T08:53:20Z, pinned
PROVIDER = mock_livetv.PROVIDER


class Clock:
    def __init__(self, t):
        self.t = t

    def __call__(self):
        return self.t


def make(tuners=2, clock=None, cache=None):
    pms = MockPms(Library(seed=3, movies=4, shows=2, seasons=1, episodes=2))
    pms.livetv = mock_livetv.LiveTvFixture(now=clock or Clock(NOW), tuners=tuners, media_cache=cache)
    return pms


def call(pms, method, path):
    return pms.handle(method, path, b"", {})


def mc(pms, method, path, want=200):
    status, ctype, data = call(pms, method, path)
    assert status == want, (method, path, status, data[:300])
    assert ctype == "application/json", ctype
    return json.loads(data)["MediaContainer"]


def first_episode(pms, ch=0):
    lineup = mc(pms, "GET", f"/{PROVIDER}/lineups/dvr/channels")["Channel"]
    key = lineup[ch]["gridKey"]
    rows = mc(pms, "GET", f"/{PROVIDER}/grid?channelGridKey={urllib.parse.quote(key)}&date=2025-10-09")["Metadata"]
    return next(r for r in rows if r["type"] == "episode" and r["Media"][0]["beginsAt"] > NOW)


class Guide(unittest.TestCase):
    def test_providers_name_the_epg_provider_its_dvr_and_features(self):
        rows = mc(make(), "GET", "/media/providers")["MediaProvider"]
        epg = [r for r in rows if r["identifier"].startswith("tv.plex.providers.epg")]
        self.assertEqual(len(epg), 1)
        self.assertEqual(epg[0]["parentID"], 1)
        types = {(f["type"], f.get("flavor")) for f in epg[0]["Feature"]}
        self.assertIn(("grid", None), types)
        self.assertIn(("subscribe", "record"), types)

    def test_lineup_and_grid_cover_a_whole_utc_day_without_gaps(self):
        pms = make()
        lineup = mc(pms, "GET", f"/{PROVIDER}/lineups/dvr/channels")["Channel"]
        self.assertEqual(len(lineup), mock_livetv.CHANNELS)
        key = lineup[3]["gridKey"]
        rows = mc(pms, "GET", f"/{PROVIDER}/grid?channelGridKey={key}&date=2025-10-09")["Metadata"]
        spans = [(r["Media"][0]["beginsAt"], r["Media"][0]["endsAt"]) for r in rows]
        midnight = 1_759_968_000
        self.assertEqual(spans[0][0], midnight)
        self.assertEqual(spans[-1][1], midnight + 86400)
        for (b1, e1), (b2, _) in zip(spans, spans[1:]):
            self.assertEqual(e1, b2, "programs abut")
        self.assertTrue(all(e - b in (1800, 3600) for b, e in spans))
        on_air = [r for r in rows if r["Media"][0]["onAir"]]
        self.assertEqual(len(on_air), 1)

    def test_the_schedule_is_a_pure_function_of_time(self):
        a = make(clock=Clock(NOW)).livetv.programs(2, NOW, NOW + 7200)
        b = make(clock=Clock(NOW + 999)).livetv.programs(2, NOW, NOW + 7200)
        strip = lambda rows: [(r["guid"], r["Media"][0]["beginsAt"]) for r in rows]
        self.assertEqual(strip(a), strip(b))

    def test_a_bad_grid_request_is_refused(self):
        pms = make()
        status, _, _ = call(pms, "GET", f"/{PROVIDER}/grid?channelGridKey=x&date=2025-1-9")
        self.assertEqual(status, 400)
        empty = mc(pms, "GET", f"/{PROVIDER}/grid?channelGridKey=unknown&date=2025-10-09")
        self.assertEqual(empty["size"], 0)


class Dvr(unittest.TestCase):
    def test_template_offers_this_episode_and_all_episodes_with_verbatim_parameters(self):
        pms = make()
        ep = first_episode(pms)
        t = mc(pms, "GET", "/media/subscriptions/template?guid=" + urllib.parse.quote(ep["guid"], safe=""))
        subs = t["SubscriptionTemplate"][0]["MediaSubscription"]
        self.assertEqual([s["title"] for s in subs], ["This Episode", "All Episodes"])
        self.assertEqual([s["type"] for s in subs], [4, 2])
        self.assertTrue(subs[0]["parameters"].startswith("hints%5Bguid%5D="))
        self.assertIn("minVideoQuality", [st["id"] for st in subs[0]["Setting"]])

    def test_record_edit_move_cancel_and_delete_round_trip_through_the_write_log(self):
        pms = make()
        ep = first_episode(pms)
        subs = mc(pms, "GET", "/media/subscriptions/template?guid=" + urllib.parse.quote(ep["guid"], safe=""))[
            "SubscriptionTemplate"][0]["MediaSubscription"]
        series = subs[1]
        post = (f"/media/subscriptions?includeGrabs=1&targetLibrarySectionID={series['targetLibrarySectionID']}"
                f"&type=2&prefs%5BminVideoQuality%5D=720&{series['parameters']}")
        made = mc(pms, "POST", post)["MediaSubscription"][0]
        sid = made["key"]
        self.assertEqual(made["type"], 2)
        self.assertNotIn("Conflicts", json.dumps(made))
        # a second identical rule is a 409, as on PMS
        self.assertEqual(call(pms, "POST", post)[0], 409)
        rules = mc(pms, "GET", "/media/subscriptions")["MediaSubscription"]
        self.assertEqual([r["key"] for r in rules], [sid])
        self.assertEqual({s["id"]: s["value"] for s in rules[0]["Setting"]}["minVideoQuality"], "720")
        sched = mc(pms, "GET", "/media/subscriptions/scheduled")["MediaGrabOperation"]
        self.assertGreaterEqual(len(sched), 1, "a daily show has airings in the next day")
        self.assertTrue(all(g["mediaSubscriptionID"] == int(sid) for g in sched))
        # edit
        mc(pms, "PUT", f"/media/subscriptions/{sid}?includeGrabs=1&prefs%5BminVideoQuality%5D=1080")
        rules = mc(pms, "GET", "/media/subscriptions")["MediaSubscription"]
        self.assertEqual({s["id"]: s["value"] for s in rules[0]["Setting"]}["minVideoQuality"], "1080")
        # cancel one grab: the rule stays, the grab reads cancelled
        gkey = sched[0]["key"]
        self.assertEqual(call(pms, "DELETE", gkey)[0], 200)
        after = mc(pms, "GET", "/media/subscriptions/scheduled")["MediaGrabOperation"]
        self.assertEqual(next(g for g in after if g["key"] == gkey)["status"], "cancelled")
        # a second rule, moved to the top
        movie_post = (f"/media/subscriptions?targetLibrarySectionID=1&type=1"
                      f"&hints%5Bguid%5D={urllib.parse.quote('plex://movie/x', safe='')}")
        sid2 = mc(pms, "POST", movie_post)["MediaSubscription"][0]["key"]
        self.assertEqual([r["key"] for r in mc(pms, "GET", "/media/subscriptions")["MediaSubscription"]], [sid, sid2])
        call(pms, "PUT", f"/media/subscriptions/{sid2}/move")
        self.assertEqual([r["key"] for r in mc(pms, "GET", "/media/subscriptions")["MediaSubscription"]], [sid2, sid])
        call(pms, "PUT", f"/media/subscriptions/{sid2}/move?after={sid}")
        self.assertEqual([r["key"] for r in mc(pms, "GET", "/media/subscriptions")["MediaSubscription"]], [sid, sid2])
        # delete
        self.assertEqual(call(pms, "DELETE", f"/media/subscriptions/{sid}")[0], 200)
        self.assertEqual(call(pms, "DELETE", f"/media/subscriptions/{sid}")[0], 404)
        writes = [(m, urllib.parse.urlsplit(p).path) for m, p, _ in pms.writes]
        self.assertIn(("POST", "/media/subscriptions"), writes)
        self.assertIn(("DELETE", gkey), writes)
        self.assertIn(("PUT", f"/media/subscriptions/{sid2}/move"), writes)

    def test_a_rule_that_needs_a_busy_tuner_is_created_with_conflicts(self):
        pms = make(tuners=1)
        lineup = mc(pms, "GET", f"/{PROVIDER}/lineups/dvr/channels")["Channel"]
        mc(pms, "POST", f"/livetv/dvrs/1/channels/{lineup[0]['id']}/tune")
        # a program airing now on another channel needs the only tuner
        now_rows = mc(pms, "GET", f"/{PROVIDER}/grid?channelGridKey={lineup[1]['gridKey']}&date=2025-10-09")["Metadata"]
        cur = next(r for r in now_rows if r["Media"][0]["onAir"])
        post = (f"/media/subscriptions?targetLibrarySectionID=2&type=4"
                f"&hints%5Bguid%5D={urllib.parse.quote(cur['guid'], safe='')}")
        made = mc(pms, "POST", post)["MediaSubscription"][0]
        conflicts = made["MediaGrabOperation"][0]["Conflicts"]["MediaGrabOperation"]
        self.assertEqual(len(conflicts), 1)
        self.assertEqual(conflicts[0]["rolling"], 1, "the holder is a live viewer")
        self.assertTrue(made["key"], "the conflicted rule exists and has an id")


class Tune(unittest.TestCase):
    def test_a_tune_hands_out_a_session_key_a_capture_window_and_programs_and_releases(self):
        clock = Clock(NOW)
        pms = make(clock=clock)
        cid = mc(pms, "GET", f"/{PROVIDER}/lineups/dvr/channels")["Channel"][2]["id"]
        sub = mc(pms, "POST", f"/livetv/dvrs/1/channels/{cid}/tune")["MediaSubscription"][0]
        grab = sub["MediaGrabOperation"][0]
        video = grab["Video"]
        self.assertTrue(video["key"].startswith("/livetv/sessions/"))
        self.assertEqual(video["Media"][0]["uuid"], video["key"].rsplit("/", 1)[-1])
        self.assertIn("TranscodeSession", video)
        self.assertGreaterEqual(len(sub["Timeline"]["Video"]), 3)
        clock.t += 120
        refreshed = mc(pms, "GET", f"/media/subscriptions/{sub['key']}")["MediaSubscription"][0]
        ts = refreshed["MediaGrabOperation"][0]["Video"]["TranscodeSession"]
        self.assertEqual(ts["maxOffsetAvailable"], 120.0, "the capture window grows with the clock")
        acts = mc(pms, "GET", "/activities")["Activity"]
        self.assertEqual(acts[0]["Context"]["subscriptionID"], sub["key"])
        self.assertEqual(call(pms, "DELETE", f"/media/subscriptions/{sub['key']}")[0], 200)
        self.assertEqual(mc(pms, "GET", "/activities")["size"], 0, "the tuner is free again")

    def test_with_every_tuner_busy_a_tune_answers_conflicts_and_a_key(self):
        pms = make(tuners=1)
        ids = [c["id"] for c in mc(pms, "GET", f"/{PROVIDER}/lineups/dvr/channels")["Channel"]]
        mc(pms, "POST", f"/livetv/dvrs/1/channels/{ids[0]}/tune")
        second = mc(pms, "POST", f"/livetv/dvrs/1/channels/{ids[1]}/tune")["MediaSubscription"]
        self.assertTrue(second["key"])
        confs = second["MediaGrabOperation"]["Conflicts"]["MediaGrabOperation"]
        self.assertEqual(len(confs), 1)
        self.assertTrue(confs[0]["key"].startswith("/media/grabbers/operations/t"))
        # stopping the other viewer through its grab key frees the tuner
        self.assertEqual(call(pms, "DELETE", confs[0]["key"])[0], 200)
        third = mc(pms, "POST", f"/livetv/dvrs/1/channels/{ids[1]}/tune")["MediaSubscription"][0]
        self.assertIn("Video", third["MediaGrabOperation"][0])

    def test_tuning_an_unknown_channel_fails_like_pms(self):
        pms = make()
        self.assertEqual(call(pms, "POST", "/livetv/dvrs/1/channels/nope/tune")[0], 500)
        self.assertEqual(call(pms, "POST", "/livetv/dvrs/9/channels/x/tune")[0], 404)

    def test_the_live_decision_is_an_h264_aac_hls_transcode(self):
        pms = make()
        cid = mc(pms, "GET", f"/{PROVIDER}/lineups/dvr/channels")["Channel"][0]["id"]
        key = mc(pms, "POST", f"/livetv/dvrs/1/channels/{cid}/tune")["MediaSubscription"][0][
            "MediaGrabOperation"][0]["Video"]["key"]
        d = mc(pms, "GET", "/video/:/transcode/universal/decision?" + urllib.parse.urlencode({"path": key, "protocol": "hls"}))
        part = d["Metadata"][0]["Media"][0]["Part"][0]
        self.assertEqual(part["decision"], "transcode")
        self.assertEqual({s["streamType"]: s["codec"] for s in part["Stream"]}, {1: "h264", 2: "aac"})

    @unittest.skipUnless(shutil.which("ffmpeg"), "ffmpeg cuts the live segments")
    def test_the_live_playlist_grows_with_the_clock_and_slides(self):
        cache = pathlib.Path(tempfile.mkdtemp(prefix="test-mock-livetv-"))
        try:
            clock = Clock(NOW)
            pms = make(clock=clock, cache=cache)
            cid = mc(pms, "GET", f"/{PROVIDER}/lineups/dvr/channels")["Channel"][0]["id"]
            key = mc(pms, "POST", f"/livetv/dvrs/1/channels/{cid}/tune")["MediaSubscription"][0][
                "MediaGrabOperation"][0]["Video"]["key"]
            q = urllib.parse.urlencode({"path": key, "session": "enc1", "protocol": "hls"})
            status, ctype, master = call(pms, "GET", "/video/:/transcode/universal/start.m3u8?" + q)
            self.assertEqual((status, ctype), (200, "application/vnd.apple.mpegurl"))
            lines = master.decode().splitlines()
            self.assertEqual(sum(l.startswith("#EXT-X-STREAM-INF") for l in lines), 1)
            child = lines[-1]
            self.assertEqual(child, "session/enc1/base/index.m3u8")
            base = "/video/:/transcode/universal/"
            media = call(pms, "GET", base + child)[2].decode()
            self.assertIn("#EXT-X-MEDIA-SEQUENCE:0", media)
            self.assertNotIn("#EXT-X-ENDLIST", media)
            self.assertEqual(media.count("#EXTINF"), mock_livetv.STARTUP_SEGMENTS)
            seg = call(pms, "GET", base + "session/enc1/base/0.ts")
            self.assertEqual(seg[:2], (200, "video/mp2t"))
            self.assertEqual(seg[2][0], 0x47, "an MPEG-TS sync byte")
            self.assertEqual(call(pms, "GET", base + "session/enc1/base/99.ts")[0], 404, "not produced yet")
            clock.t += 2 * 100
            later = call(pms, "GET", base + child)[2].decode()
            self.assertEqual(later.count("#EXTINF"), mock_livetv.WINDOW, "the window slides")
            self.assertIn(f"#EXT-X-MEDIA-SEQUENCE:{mock_livetv.STARTUP_SEGMENTS + 100 - mock_livetv.WINDOW}", later)
            for line in later.splitlines():
                self.assertEqual(line, line.strip(), "no stray whitespace: the app's parser refuses it")
        finally:
            shutil.rmtree(cache, ignore_errors=True)


class Default(unittest.TestCase):
    def test_without_livetv_the_paths_are_unknown_as_before(self):
        pms = MockPms(Library(seed=3, movies=4, shows=2, seasons=1, episodes=2))
        body = json.loads(call(pms, "GET", "/media/providers")[2])["MediaContainer"]
        self.assertNotIn("MediaProvider", body)


if __name__ == "__main__":
    unittest.main()
