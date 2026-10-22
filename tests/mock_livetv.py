#!/usr/bin/env python3
"""A SYNTHETIC Live TV / DVR server surface for `tests/mock_pms.py --livetv`: one EPG provider with a
lineup of channels, a guide that runs on the wall clock, recording rules and operations that can be
created, edited, re-prioritised and cancelled, and live tunes that hand out a GROWING HLS stream —
so the app's guide, record flow and live player can be driven end to end with no tuner, no
account and no household byte. Shapes follow `docs/pms-api.md` §10 (what the Rust layer
`plex::live_tv` parses); titles are drawn from the mock's closed alphabet (`s` + eight hex digits).

What is modelled, and how far:

* `GET /media/providers` — a library provider and `tv.plex.providers.epg.cloud:<dvr>` with
  `parentID` = the DVR key, `Feature type=grid` and `type=subscribe flavor=record`.
* `GET /<provider>/lineups/dvr/channels` and `GET /<provider>/grid?channelGridKey=&date=` — a
  deterministic schedule: program boundaries sit on half hours and are a pure function of the
  channel and the UTC instant, so a date's answer never changes and a show airs at the same slot
  every day (an "All episodes" rule therefore has more than one airing to find).
* `GET/POST/PUT/DELETE /media/subscriptions[...]`, `/template`, `/scheduled`, `PUT …/{id}/move`,
  `DELETE /media/grabbers/operations/{id}` — rules live in memory in priority order; their grabs are
  derived from the schedule for the next 24 h (in progress when now is inside the airing, complete
  for one that ended in the last 6 h); a cancelled grab stays cancelled.
* `POST /livetv/dvrs/<dvr>/channels/<id>/tune` — a rolling subscription with one grab whose Video
  key is `/livetv/sessions/<uuid>`, a capture window, and the programs around now. With every tuner
  busy (`--livetv-tuners N`, default 2: live tunes plus recordings in progress) the answer carries
  `Conflicts` instead, and the subscription key the caller must still release. A tune without
  `X-Plex-Session-Identifier` is answered 400, as a real server answered one.
* `GET /video/:/transcode/universal/decision?path=/livetv/sessions/…` answers an H.264/AAC HLS
  transcode; `start.m3u8` answers a one-variant master whose child media playlist grows by one
  2-second MPEG-TS segment per 2 seconds of wall clock (no ENDLIST, a sliding window of
  `WINDOW` segments). The segments are a short ring of IDR-led parts cut once by ffmpeg into a cache
  directory; without ffmpeg the start answers 503 and everything else still works.

Every write is recorded through the owner's `note_write` (the mock's WRITE log).
"""
import hashlib
import json
import os
import pathlib
import shutil
import subprocess
import tempfile
import threading
import time
import urllib.parse

DVR_KEY = "1"
PROVIDER = f"tv.plex.providers.epg.cloud:{DVR_KEY}"
PROVIDER_ID = 2
CHANNELS = 8
HALF_HOUR = 1800
SEGMENT_S = 2
RING = 10          # distinct segment files the live stream cycles through
WINDOW = 30        # segments a live media playlist lists at most
STARTUP_SEGMENTS = 3  # a fresh session already has this many segments, as a transcoder runs ahead
LIBRARY_TV = "2"
LIBRARY_MOVIES = "1"

STATUS_ORDER = ("scheduled", "inprogress", "complete")


def _h(*parts):
    """A stable 32-bit hash of the parts (never Python's salted `hash`)."""
    return int.from_bytes(hashlib.sha256("|".join(map(str, parts)).encode()).digest()[:4], "big")


def _word(*parts):
    """A closed-alphabet title: `s` + eight lowercase hex digits."""
    return f"s{_h(*parts):08x}"


def _hex24(*parts):
    return hashlib.sha256("|".join(map(str, parts)).encode()).hexdigest()[:24]


def channel_id(n):
    return f"sc{n:03d}-{_word('channel', n)}"


def utc_date(ts):
    return time.strftime("%Y-%m-%d", time.gmtime(ts))


def _utc_midnight(date):
    import calendar
    return calendar.timegm(time.strptime(date, "%Y-%m-%d"))


class LiveTvFixture:
    """The Live TV surface. `now` is injectable so tests can pin the clock; serving uses the wall
    clock, because a guide that never moves is not a guide."""

    def __init__(self, now=time.time, tuners=2, media_cache=None, ffmpeg=None):
        self.now = now
        self.tuners = tuners
        self.lock = threading.Lock()
        self.rules = []            # priority order: dicts
        self.cancelled = set()     # grab ids
        self.tunes = {}            # subscription id -> tune dict
        self.sessions = {}         # encoder session -> {"t0", "uuid"}
        self.next_id = 100
        self.next_uuid = 1
        self.media_cache = pathlib.Path(media_cache) if media_cache else None
        self.ffmpeg = ffmpeg or shutil.which("ffmpeg")
        self._segments = None

    # ---- the schedule ---------------------------------------------------------------------

    def programs(self, ch, start, end):
        """The programs on channel `ch` overlapping [start, end). Every UTC hour holds either one
        hour-long program or two half-hour ones, decided by a hash of the channel and the hour, so
        every boundary is a pure function of time."""
        out = []
        hour = start - (start % 3600)
        while hour < end:
            if _h("split", ch, hour % 86400) % 3 == 0:
                spans = [(hour, hour + 3600)]
            else:
                spans = [(hour, hour + HALF_HOUR), (hour + HALF_HOUR, hour + 3600)]
            for b, e in spans:
                if e > start and b < end:
                    out.append(self.program(ch, b, e))
            hour += 3600
        return out

    def program(self, ch, begins, ends):
        slot_of_day = (begins % 86400) // HALF_HOUR
        movie = _h("kind", ch, slot_of_day) % 4 == 0
        show = _word("show", ch, slot_of_day)
        rk = 500000 + (_h("rk", ch, begins) % 400000)
        cid = channel_id(ch)
        media = {"id": f"m{rk}", "beginsAt": begins, "endsAt": ends, "duration": (ends - begins) * 1000,
                 "channelIdentifier": cid, "channelVcn": f"{ch + 2}.1", "channelTitle": _word("channel", ch),
                 "channelCallSign": _word("call", ch), "channelThumb": "", "protocol": "livetv",
                 "onAir": begins <= self.now() < ends, "premiere": _h("new", ch, begins) % 5 == 0,
                 "videoResolution": "720"}
        if movie:
            title = _word("movie", ch, begins)
            m = {"ratingKey": str(rk), "key": f"/{PROVIDER}/metadata/{rk}", "type": "movie", "title": title,
                 "guid": f"plex://movie/{_hex24('movie', ch, begins)}", "year": 2000 + _h("y", rk) % 26,
                 "summary": _word("sum", rk), "thumb": "", "Media": [media]}
        else:
            ep = _h("ep", ch, begins) % 20 + 1
            m = {"ratingKey": str(rk), "key": f"/{PROVIDER}/metadata/{rk}", "type": "episode",
                 "title": _word("episode", ch, begins), "grandparentTitle": show,
                 "guid": f"plex://episode/{_hex24('episode', ch, begins)}",
                 "grandparentGuid": f"plex://show/{_hex24('show', ch, slot_of_day)}",
                 "parentIndex": 1 + _h("season", ch) % 5, "index": ep, "year": 2026,
                 "summary": _word("sum", rk), "thumb": "", "Media": [media]}
        return m

    def find_program(self, guid, around=None):
        """The airing (channel, program) whose guid is `guid`, searched over the next day."""
        now = int(self.now()) if around is None else around
        for ch in range(CHANNELS):
            for p in self.programs(ch, now - 6 * 3600, now + 24 * 3600):
                if p["guid"] == guid or p.get("grandparentGuid") == guid:
                    return ch, p
        return None

    # ---- rules and grabs ------------------------------------------------------------------

    def _matches(self, rule, prog):
        kind = rule["type"]
        if kind == 2:
            return prog.get("grandparentGuid") == rule["guid"]
        return prog["guid"] == rule["guid"]

    def grabs_of(self, rule, now):
        out = []
        for ch in range(CHANNELS):
            for prog in self.programs(ch, now - 6 * 3600, now + 24 * 3600):
                if not self._matches(rule, prog):
                    continue
                media = prog["Media"][0]
                gid = f"g{rule['id']}-{media['beginsAt']}"
                if media["endsAt"] <= now:
                    status = "complete"
                elif media["beginsAt"] <= now:
                    status = "inprogress"
                else:
                    status = "scheduled"
                if gid in self.cancelled:
                    status = "cancelled"
                if rule["type"] == 4 and status == "complete" and media["endsAt"] < now - 6 * 3600:
                    continue
                pct = 0.0
                if status == "inprogress":
                    pct = round(100.0 * (now - media["beginsAt"]) / (media["endsAt"] - media["beginsAt"]), 1)
                out.append({"id": gid, "key": f"/media/grabbers/operations/{gid}",
                            "mediaSubscriptionID": int(rule["id"]), "status": status, "percent": pct,
                            "grabberIdentifier": "tv.plex.grabbers.hdhomerun", "grabberProtocol": "stream",
                            "provider": PROVIDER, "Metadata": prog})
        out.sort(key=lambda g: g["Metadata"]["Media"][0]["beginsAt"])
        return out

    def busy(self, start, end, now, exclude_rule=None):
        """Tuner holders overlapping [start, end): live tunes now and active rule grabs."""
        holders = []
        # a live tune holds its tuner from now on; it collides with anything already under way
        if start <= now:
            for sid, t in self.tunes.items():
                holders.append(("tune", sid, t))
        for rule in self.rules:
            if rule["id"] == exclude_rule:
                continue
            for g in self.grabs_of(rule, now):
                m = g["Metadata"]["Media"][0]
                if g["status"] in ("scheduled", "inprogress") and m["beginsAt"] < end and start < m["endsAt"]:
                    holders.append(("grab", g["id"], g))
        return holders

    def settings(self, kind, values=None):
        values = values or {}
        rows = [
            {"id": "minVideoQuality", "label": "Resolution", "summary": "", "type": "int", "default": "0",
             "value": values.get("minVideoQuality", "0"), "enumValues": "0:Prefer%20HD|720:HD%20only|1080:Full%20HD%20only"},
            {"id": "startOffsetMinutes", "label": "Start recording", "type": "int", "default": "0",
             "value": values.get("startOffsetMinutes", "0"),
             "enumValues": "0:On%20time|1:1%20minute%20early|5:5%20minutes%20early|15:15%20minutes%20early"},
            {"id": "endOffsetMinutes", "label": "Stop recording", "type": "int", "default": "0",
             "value": values.get("endOffsetMinutes", "0"),
             "enumValues": "0:On%20time|1:1%20minute%20late|5:5%20minutes%20late|30:30%20minutes%20late"},
            {"id": "oneShot", "type": "bool", "hidden": True, "default": "false",
             "value": values.get("oneShot", "true" if kind in (1, 4) else "false")},
        ]
        if kind == 2:
            rows.insert(0, {"id": "onlyNewAirings", "label": "Record", "type": "bool", "default": "false",
                            "value": values.get("onlyNewAirings", "0"), "advanced": False,
                            "summary": "Only new airings"})
            rows.append({"id": "comskipEnabled", "label": "Remove commercials", "type": "int", "advanced": True,
                         "default": "-1", "value": values.get("comskipEnabled", "-1"),
                         "enumValues": "-1:Use%20DVR%20setting|0:Disabled|1:Enabled"})
        return rows

    def rule_json(self, rule, now, include_grabs=False):
        hint = {"type": {1: "movie", 2: "show", 4: "episode"}[rule["type"]], "title": rule["title"],
                "guid": rule["guid"], "year": 2026}
        if rule["type"] == 4:
            hint["grandparentTitle"] = rule.get("show_title", "")
        row = {"key": rule["id"], "type": rule["type"], "title": rule["title"],
               "targetLibrarySectionID": rule["section"], "targetSectionLocationID": rule.get("location", ""),
               "librarySectionTitle": "TV Shows" if rule["section"] == LIBRARY_TV else "Movies",
               "locationPath": "/data/synthetic", "createdAt": rule["created"],
               "airingsType": "New Airings Only" if rule["prefs"].get("onlyNewAirings") == "1" else "New and Repeat Airings",
               ("Directory" if rule["type"] == 2 else "Video"): hint,
               "Setting": self.settings(rule["type"], rule["prefs"])}
        if include_grabs:
            row["MediaGrabOperation"] = self.grabs_of(rule, now)
        return row

    def template(self, guid, now):
        found = self.find_program(guid, now)
        if found is None:
            return None
        ch, prog = found
        enc = lambda s: urllib.parse.quote(str(s), safe="")
        params = lambda kind, g: "&".join([
            f"hints%5Bguid%5D={enc(g)}", f"hints%5BratingKey%5D={enc(prog['ratingKey'])}",
            f"hints%5Btitle%5D={enc(prog['title'] if kind != 2 else prog.get('grandparentTitle', ''))}",
            f"hints%5Btype%5D={kind}", f"params%5BmediaProviderID%5D={PROVIDER_ID}",
            f"params%5BlibraryType%5D={2 if prog['type'] == 'episode' else 1}"])
        if prog["type"] == "movie":
            choices = [(1, "This Movie", prog["guid"], True)]
            section = LIBRARY_MOVIES
        else:
            choices = [(4, "This Episode", prog["guid"], False), (2, "All Episodes", prog["grandparentGuid"], True)]
            section = LIBRARY_TV
        subs = []
        for kind, title, g, selected in choices:
            subs.append({"title": title, "type": kind, "selected": selected, "parameters": params(kind, g),
                         "targetLibrarySectionID": section, "airingsType": "New and Repeat Airings",
                         ("Directory" if kind == 2 else "Video"): {"type": {1: "movie", 2: "show", 4: "episode"}[kind],
                                                                    "title": prog.get("grandparentTitle") if kind == 2 else prog["title"],
                                                                    "guid": g},
                         "Setting": self.settings(kind)})
        return {"size": 1, "SubscriptionTemplate": [{"MediaSubscription": subs}]}

    # ---- live tunes -----------------------------------------------------------------------

    def tune_json(self, sid, tune, now):
        ch = tune["channel"]
        progs = self.programs(ch, now - 3 * HALF_HOUR, now + 4 * HALF_HOUR)
        current = next((p for p in progs if p["Media"][0]["beginsAt"] <= now < p["Media"][0]["endsAt"]), progs[0])
        item = json.loads(json.dumps(current))
        item["key"] = f"/livetv/sessions/{tune['uuid']}"
        item["live"] = 1
        item["TranscodeSession"] = {"key": f"/transcode/sessions/{tune['uuid']}", "timeStamp": float(tune["t0"]),
                                    "minOffsetAvailable": 0.0, "maxOffsetAvailable": float(max(0, now - tune["t0"]))}
        media = item["Media"][0]
        media["uuid"] = tune["uuid"]
        media["Part"] = [{"key": f"/livetv/sessions/{tune['uuid']}/index.m3u8", "Stream": [
            {"streamType": 1, "codec": "mpeg2video", "width": 1920, "height": 1080, "frameRate": 29.97,
             "scanType": "interlaced", "index": 0},
            {"streamType": 2, "codec": "ac3", "channels": 6, "samplingRate": 48000, "index": 1}]}]
        grab = {"id": f"t{sid}", "key": f"/media/grabbers/operations/t{sid}", "mediaSubscriptionID": int(sid),
                "status": "inprogress", "rolling": 1, "Video": item}
        return {"size": 1, "MediaSubscription": [{"key": sid, "type": 4, "title": current["title"],
                                                  "MediaGrabOperation": [grab],
                                                  "Timeline": {"Video": progs}}]}

    def conflict_json(self, sid, holders):
        confs = []
        for kind, ident, obj in holders:
            if kind == "tune":
                confs.append({"key": f"/media/grabbers/operations/t{ident}", "mediaSubscriptionID": int(ident),
                              "rolling": 1, "Metadata": {"title": _word("viewer", ident), "type": "episode"}})
            else:
                confs.append({"key": obj["key"], "mediaSubscriptionID": obj["mediaSubscriptionID"], "rolling": 0,
                              "Metadata": obj["Metadata"]})
        return {"size": 1, "MediaSubscription": {"key": sid, "MediaGrabOperation": {
            "id": f"t{sid}", "status": "error", "Conflicts": {"MediaGrabOperation": confs}}}}

    # ---- HLS ------------------------------------------------------------------------------

    def segments(self):
        """The ring of MPEG-TS segment files, cut once (`None` without ffmpeg)."""
        if self._segments is not None:
            return self._segments or None
        if not self.ffmpeg:
            self._segments = []
            return None
        cache = self.media_cache or pathlib.Path(os.environ.get("PLX_BUILD_CACHE", pathlib.Path.home() / ".cache" / "plxnative")) / "mock-livetv-v1"
        files = [cache / f"seg{i:03d}.ts" for i in range(RING)]
        if not all(f.exists() and f.stat().st_size > 0 for f in files):
            cache.mkdir(parents=True, exist_ok=True)
            work = pathlib.Path(tempfile.mkdtemp(prefix="mock-livetv-", dir=cache))
            # One segment's worth more than the ring: the segment muxer folds a short tail into the
            # part before it, so exactly RING * SEGMENT_S seconds cuts RING - 1 parts.
            seconds = (RING + 1) * SEGMENT_S
            cmd = [self.ffmpeg, "-hide_banner", "-loglevel", "error", "-y",
                   "-f", "lavfi", "-i", f"testsrc2=size=1280x720:rate=30:duration={seconds}",
                   "-f", "lavfi", "-i", f"sine=frequency=440:sample_rate=48000:duration={seconds}",
                   "-c:v", "libx264", "-preset", "veryfast", "-profile:v", "high", "-pix_fmt", "yuv420p",
                   "-g", str(30 * SEGMENT_S), "-keyint_min", str(30 * SEGMENT_S), "-sc_threshold", "0",
                   "-force_key_frames", f"expr:gte(t,n_forced*{SEGMENT_S})",
                   "-c:a", "aac", "-b:a", "128k", "-ac", "2",
                   "-f", "segment", "-segment_time", str(SEGMENT_S), "-segment_format", "mpegts",
                   "-reset_timestamps", "1", str(work / "seg%03d.ts")]
            try:
                subprocess.run(cmd, check=True, timeout=120)
                for f in files:
                    os.replace(work / f.name, f)
            except (OSError, subprocess.SubprocessError):
                shutil.rmtree(work, ignore_errors=True)
                self._segments = []
                return None
            shutil.rmtree(work, ignore_errors=True)
        self._segments = [f.read_bytes() for f in files]
        return self._segments

    def master(self, session):
        return ("#EXTM3U\n#EXT-X-VERSION:3\n"
                '#EXT-X-STREAM-INF:BANDWIDTH=4000000,RESOLUTION=1280x720,CODECS="avc1.640028,mp4a.40.2"\n'
                f"session/{session}/base/index.m3u8\n")

    def media_playlist(self, session, now):
        s = self.sessions[session]
        produced = STARTUP_SEGMENTS + int((now - s["t0"]) // SEGMENT_S)
        first = max(0, produced - WINDOW)
        lines = ["#EXTM3U", "#EXT-X-VERSION:3", f"#EXT-X-TARGETDURATION:{SEGMENT_S}",
                 f"#EXT-X-MEDIA-SEQUENCE:{first}"]
        for n in range(first, produced):
            lines.append(f"#EXTINF:{SEGMENT_S:.6f},")
            lines.append(f"{n}.ts")
        return "\n".join(lines) + "\n"

    # ---- dispatch -------------------------------------------------------------------------

    def handle(self, method, p, q, headers, note_write, raw_query=""):
        """`(status, content_type, bytes)` for a Live TV path, else None (not ours)."""
        j = lambda obj, status=200: (status, "application/json", json.dumps({"MediaContainer": obj}).encode())
        now = int(self.now())
        segs = [s for s in p.split("/") if s]
        with self.lock:
            if p == "/media/providers":
                return j({"size": 2, "MediaProvider": [
                    {"identifier": "com.plexapp.plugins.library", "title": "Library", "types": "video",
                     "protocols": "stream,download", "Feature": [{"type": "content"}]},
                    {"id": PROVIDER_ID, "parentID": int(DVR_KEY), "identifier": PROVIDER,
                     "providerIdentifier": "tv.plex.providers.epg.cloud", "title": "Live TV & DVR",
                     "types": "video", "protocols": "livetv", "epgSource": "cloud", "Feature": [
                         {"type": "grid", "key": f"/{PROVIDER}/grid"},
                         {"type": "subscribe", "flavor": "record", "key": "/media/subscriptions"},
                         {"type": "content", "key": f"/{PROVIDER}/hubs/discover"}]}]})
            if p == f"/{PROVIDER}/lineups/dvr/channels":
                return j({"size": CHANNELS, "Channel": [
                    {"id": channel_id(n), "gridKey": channel_id(n), "vcn": f"{n + 2}.1", "isHd": n % 2 == 0,
                     "title": _word("channel", n), "callSign": _word("call", n), "thumb": "", "language": "en"}
                    for n in range(CHANNELS)]})
            if p == f"/{PROVIDER}/grid":
                key, date = q.get("channelGridKey", ""), q.get("date", "")
                start = _safe_midnight(date)
                ch = next((n for n in range(CHANNELS) if channel_id(n) == key), None)
                if start is None or ch is None:
                    return j({"size": 0, "Metadata": []}, 400 if start is None else 200)
                progs = self.programs(ch, start, start + 86400)
                return j({"size": len(progs), "Metadata": progs})
            if p == "/media/subscriptions/template":
                body = self.template(q.get("guid", ""), now)
                return j(body) if body is not None else j({"size": 0}, 404)
            if p == "/media/subscriptions/scheduled":
                grabs = [g for rule in self.rules for g in self.grabs_of(rule, now)]
                grabs.sort(key=lambda g: g["Metadata"]["Media"][0]["beginsAt"])
                return j({"size": len(grabs), "MediaGrabOperation": grabs})
            if p == "/media/subscriptions" and method == "GET":
                inc = q.get("includeGrabs") == "1"
                rows = [self.rule_json(r, now, inc) for r in self.rules]
                return j({"size": len(rows), "MediaSubscription": rows})
            if p == "/media/subscriptions" and method == "POST":
                note_write(method, p + "?" + raw_query)
                return self.create(q, now, j)
            if len(segs) == 3 and segs[:2] == ["media", "subscriptions"]:
                sid = segs[2]
                if method == "GET":
                    if sid in self.tunes:
                        return j(self.tune_json(sid, self.tunes[sid], now))
                    rule = self.rule(sid)
                    return j({"size": 1, "MediaSubscription": [self.rule_json(rule, now, True)]}) if rule else j({"size": 0}, 404)
                if method == "DELETE":
                    note_write(method, p)
                    if self.tunes.pop(sid, None) is not None:
                        return (200, "text/html", b"")
                    rule = self.rule(sid)
                    if rule is None:
                        return (404, "text/html", b"")
                    self.rules.remove(rule)
                    return (200, "text/html", b"")
                if method == "PUT":
                    note_write(method, p + "?" + raw_query)
                    rule = self.rule(sid)
                    if rule is None:
                        return (404, "text/html", b"")
                    rule["prefs"].update(_prefs(q))
                    if q.get("targetLibrarySectionID"):
                        rule["section"] = q["targetLibrarySectionID"]
                    return j({"size": 1, "MediaSubscription": [self.rule_json(rule, now, True)]})
            if len(segs) == 4 and segs[:2] == ["media", "subscriptions"] and segs[3] == "move" and method == "PUT":
                note_write(method, p + ("?" + raw_query if raw_query else ""))
                rule = self.rule(segs[2])
                if rule is None:
                    return (404, "text/html", b"")
                self.rules.remove(rule)
                after = self.rule(q.get("after", "")) if q.get("after") else None
                self.rules.insert(self.rules.index(after) + 1 if after else 0, rule)
                return j({"size": len(self.rules), "MediaSubscription": [self.rule_json(r, now) for r in self.rules]})
            if len(segs) == 4 and segs[:3] == ["media", "grabbers", "operations"] and method == "DELETE":
                note_write(method, p)
                gid = segs[3]
                if gid.startswith("t") and gid[1:] in self.tunes:
                    self.tunes.pop(gid[1:])
                    return (200, "text/html", b"")
                if any(g["id"] == gid for r in self.rules for g in self.grabs_of(r, now)):
                    self.cancelled.add(gid)
                    return (200, "text/html", b"")
                return (404, "text/html", b"")
            if (len(segs) == 6 and segs[:2] == ["livetv", "dvrs"] and segs[3] == "channels"
                    and segs[5] == "tune" and method == "POST"):
                note_write(method, p)
                if segs[2] != DVR_KEY:
                    return (404, "text/html", b"")
                if not q.get("X-Plex-Session-Identifier"):
                    # what a real server (2026-10-22) answered a tune that named no session
                    return (400, "text/html", b"<html><head><title>Bad Request</title></head>"
                                              b"<body><h1>400 Bad Request</h1></body></html>")
                ch = next((n for n in range(CHANNELS) if channel_id(n) == urllib.parse.unquote(segs[4])), None)
                if ch is None:
                    return (500, "text/html", b"<html><body><h1>500 Tuning failed</h1></body></html>")
                sid = str(self.next_id)
                self.next_id += 1
                holders = self.busy(now, now + 1, now)
                if len(holders) >= self.tuners:
                    return j(self.conflict_json(sid, holders))
                uuid = f"0e0e0e0e-0000-4000-8000-{self.next_uuid:012d}"
                self.next_uuid += 1
                self.tunes[sid] = {"channel": ch, "uuid": uuid, "t0": now}
                return j(self.tune_json(sid, self.tunes[sid], now))
            if p == "/activities":
                acts = [{"uuid": t["uuid"], "type": "grabber.grab", "cancellable": 0,
                         "Context": {"subscriptionID": sid, "conflicts": 0, "signalQuality": 100}}
                        for sid, t in self.tunes.items()]
                return j({"size": len(acts), "Activity": acts})
            # The transcoder, for live sessions only.
            if p == "/video/:/transcode/universal/decision" and q.get("path", "").startswith("/livetv/sessions/"):
                return j(self.decision(q.get("path", "")))
            if p == "/video/:/transcode/universal/start.m3u8" and q.get("path", "").startswith("/livetv/sessions/"):
                if self.segments() is None:
                    return (503, "text/plain", b"mock_livetv: ffmpeg is needed for live segments")
                session = q.get("session") or q.get("X-Plex-Session-Identifier") or "live"
                if session not in self.sessions:
                    self.sessions[session] = {"t0": now, "path": q.get("path")}
                return (200, "application/vnd.apple.mpegurl", self.master(session).encode())
            if (len(segs) == 8 and segs[:5] == ["video", ":", "transcode", "universal", "session"]
                    and segs[6] == "base"):
                session, leaf = segs[5], segs[7]
                if session not in self.sessions:
                    return None
                if leaf == "index.m3u8":
                    return (200, "application/vnd.apple.mpegurl", self.media_playlist(session, now).encode())
                if leaf.endswith(".ts") and leaf[:-3].isdigit():
                    data = self.segments()
                    n = int(leaf[:-3])
                    produced = STARTUP_SEGMENTS + int((now - self.sessions[session]["t0"]) // SEGMENT_S)
                    if data is None or n >= produced:
                        return (404, "text/plain", b"")
                    return (200, "video/mp2t", data[n % len(data)])
            if p == "/video/:/transcode/universal/stop":
                self.sessions.pop(q.get("session", ""), None)
                return None  # the owner answers and logs it
        return None

    def rule(self, sid):
        return next((r for r in self.rules if r["id"] == str(sid)), None)

    def create(self, q, now, j):
        hints = {k[6:-1]: v for k, v in q.items() if k.startswith("hints[") and k.endswith("]")}
        guid = hints.get("guid", "")
        try:
            kind = int(q.get("type", "0"))
        except ValueError:
            kind = 0
        section = q.get("targetLibrarySectionID", "")
        if not guid or kind not in (1, 2, 4) or not section:
            return (400, "text/html", b"<html><body><h1>400 Bad Request</h1></body></html>")
        if any(r["guid"] == guid and r["type"] == kind for r in self.rules):
            return (409, "text/html", b"<html><body><h1>409 Conflict</h1></body></html>")
        found = self.find_program(guid, now)
        title = hints.get("title") or (found[1].get("grandparentTitle") if found and kind == 2 else found[1]["title"] if found else guid)
        sid = str(self.next_id)
        self.next_id += 1
        rule = {"id": sid, "type": kind, "guid": guid, "title": title, "section": section,
                "location": q.get("targetSectionLocationID", ""), "prefs": _prefs(q), "created": now,
                "show_title": found[1].get("grandparentTitle", "") if found else ""}
        self.rules.append(rule)
        grabs = self.grabs_of(rule, now)
        conflicts = []
        nxt = next((g for g in grabs if g["status"] in ("scheduled", "inprogress")), None)
        if nxt is not None:
            m = nxt["Metadata"]["Media"][0]
            holders = self.busy(m["beginsAt"], m["endsAt"], now, exclude_rule=sid)
            if len(holders) >= self.tuners:
                conflicts = holders
        row = self.rule_json(rule, now)
        if conflicts:
            row["MediaGrabOperation"] = [{"id": nxt["id"], "status": "scheduled", "Metadata": nxt["Metadata"],
                                          "Conflicts": {"MediaGrabOperation": [
                                              {"key": o["key"] if k == "grab" else f"/media/grabbers/operations/t{i}",
                                               "mediaSubscriptionID": o["mediaSubscriptionID"] if k == "grab" else int(i),
                                               "rolling": 0 if k == "grab" else 1,
                                               "Metadata": o["Metadata"] if k == "grab" else {"title": _word("viewer", i)}}
                                              for k, i, o in conflicts]}}]
        else:
            row["MediaGrabOperation"] = grabs[:1]
        return j({"size": 1, "MediaSubscription": [row]})

    def decision(self, path):
        uuid = path.rstrip("/").split("/")[-1]
        tune = next((t for t in self.tunes.values() if t["uuid"] == uuid), None)
        ch = tune["channel"] if tune else 0
        now = int(self.now())
        progs = self.programs(ch, now, now + 1)
        row = json.loads(json.dumps(progs[0])) if progs else {"type": "episode", "title": _word("live")}
        row["key"] = path
        row["Media"] = [{"protocol": "hls", "container": "mpegts", "videoCodec": "h264", "audioCodec": "aac",
                         "width": 1280, "height": 720, "audioChannels": 2, "videoResolution": "720",
                         "Part": [{"decision": "transcode", "container": "mpegts", "protocol": "hls", "Stream": [
                             {"streamType": 1, "codec": "h264", "width": 1280, "height": 720, "frameRate": 30,
                              "decision": "transcode", "index": 0},
                             {"streamType": 2, "codec": "aac", "channels": 2, "samplingRate": 48000,
                              "decision": "transcode", "index": 1}]}]}]
        return {"size": 1, "generalDecisionCode": 1000, "generalDecisionText": "Transcode OK",
                "mdeDecisionCode": 1000, "transcodeDecisionCode": 1000, "Metadata": [row]}


def _prefs(q):
    return {k[6:-1]: v for k, v in q.items() if k.startswith("prefs[") and k.endswith("]")}


def _safe_midnight(date):
    if len(date) != 10:
        return None
    try:
        return _utc_midnight(date)
    except ValueError:
        return None
