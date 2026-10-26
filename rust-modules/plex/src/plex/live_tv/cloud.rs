//! **Plex's own channels** — the guide's *Plex Channels* tab ("Live TV on Plex"): the plex.tv EPG
//! provider at `epg.provider.plex.tv`, read with the ACCOUNT's credential, not a server's. No PMS,
//! DVR or tuner is involved: a channel is a stream the provider hands out.
//!
//! There is no published documentation; this is the protocol community clients agree on
//! (2024–2026), and every answer is read by the same tolerant parsers as a server's guide:
//!
//! * `GET /` — the provider. Its `Feature type="grid"` lists `GridChannelFilter{identifier,title}`:
//!   the guide's genre tabs (Featured, Movies, News, …).
//! * `GET /lineups/plex/channels[?genre=<identifier>]` — the lineup, or one tab of it: `Channel[]`
//!   with `id`, `gridKey` (stable; `id` changes when Plex moves a channel between its servers),
//!   `vcn`, `title`, `callSign`, `thumb`, `drm`, and `Media[0].Part[0].key` — the channel's HLS.
//! * `GET /grid?channelGridKey=&date=YYYY-MM-DD` — one channel's programs for one UTC day, the shape
//!   a server's grid answers.
//!
//! Every request carries `X-Plex-Provider-Version: 7.2`. A `drm` channel needs Widevine, which this
//! app does not have, so the lineup leaves it out.

use serde_json::Value;

use super::parse;
use super::{Airing, Channel, CloudTab, FavoriteChannel, FavoriteOp, LiveError, LiveResult};
use crate::plex::account::AccountClient;
use crate::plex::client::QueryBuilder;

/// The provider's origin.
pub const BASE: &str = "https://epg.provider.plex.tv";

/// The protocol version community clients send; older ones are answered with older shapes.
const PROVIDER_VERSION: &str = "X-Plex-Provider-Version: 7.2";

/// The account's favorite channels, every client's: read whole, written whole.
const FAVORITES: &str = "/settings/favoriteChannels";

impl AccountClient {
    /// One GET to the provider, its JSON or the error a non-2xx / unreadable answer is.
    fn cloud_get(&self, path_and_query: &str) -> LiveResult<Value> {
        let url = format!("{BASE}{path_and_query}");
        let mut headers = self.headers();
        headers.push(PROVIDER_VERSION.to_string());
        let resp = plx_net::net::request_evidence(&url, &headers, "GET", None, plx_net::net::API, false, None, None);
        match resp {
            Ok(r) if (200..300).contains(&r.status) => serde_json::from_slice(&r.body).map_err(|_| LiveError::Unreadable),
            Ok(r) => Err(LiveError::Status(i32::from(r.status))),
            Err(f) => match f.status {
                Some(status) if !(200..300).contains(&status) => Err(LiveError::Status(i32::from(status))),
                _ => Err(LiveError::Transport),
            },
        }
    }

    /// One PUT of a JSON body to the provider. `Ok` on any 2xx.
    fn cloud_put(&self, path: &str, body: &[u8]) -> LiveResult<()> {
        let url = format!("{BASE}{path}");
        let mut headers = self.headers();
        headers.push(PROVIDER_VERSION.to_string());
        // the provider refuses a body it is not told is JSON (a community client saw HTTP 400)
        headers.push("Content-Type: application/json".to_string());
        let resp = plx_net::net::request_evidence(&url, &headers, "PUT", Some(body), plx_net::net::API, false, None, None);
        let status = match resp {
            Ok(r) => r.status,
            Err(f) => f.status.ok_or(LiveError::Transport)?,
        };
        plx_base::eventlog::log(&format!("livetv: favorite channels saved -> HTTP {status}"));
        if (200..300).contains(&status) { Ok(()) } else { Err(LiveError::Status(i32::from(status))) }
    }

    /// **The account's favorite channels**, in the account's order (shared with the stock
    /// clients, whose guide opens on them).
    pub fn favorite_channels(&self) -> LiveResult<Vec<FavoriteChannel>> {
        let list = parse::favorites(&self.cloud_get(FAVORITES)?);
        let on_server = list.iter().filter(|f| f.on_server()).count();
        let mut sources: Vec<&str> = list.iter().map(|f| f.source.as_str()).collect();
        sources.sort_unstable();
        sources.dedup();
        // A release build's event log too: what the list's ids look like is what matching it to a
        // lineup needs to know, and a shape names no channel.
        plx_base::eventlog::log(&format!(
            "livetv: favorites read: {} entr{}, {on_server} on a server, {} source(s), ids {:?}",
            list.len(),
            if list.len() == 1 { "y" } else { "ies" },
            sources.len(),
            list.iter().map(|f| super::debug::id_shape(&f.id)).collect::<Vec<_>>()
        ));
        Ok(list)
    }

    /// Change the account's favorites: the list as the provider holds it now, `op` applied, written
    /// back whole (every other entry as it came). Answers the list written. A debug build writes
    /// only under the owner's live switch, as every Live TV change ([`super::guard`]).
    pub fn edit_favorite_channels(&self, op: &FavoriteOp) -> LiveResult<Vec<FavoriteChannel>> {
        if !super::guard::allow("favorite channels", "") {
            return Err(LiveError::Refused);
        }
        let Some(now) = parse::favorites_whole(&self.cloud_get(FAVORITES)?) else {
            plx_base::eventlog::log("livetv: favorites not written: the list read holds what this app does not keep");
            return Err(LiveError::Unreadable);
        };
        let next = op.apply(&now);
        if next == now {
            return Ok(now);
        }
        let body = format!("[{}]", next.iter().map(|f| f.raw.as_str()).collect::<Vec<_>>().join(","));
        self.cloud_put(FAVORITES, body.as_bytes())?;
        Ok(next)
    }

    /// The genre tabs of Plex's channels.
    pub fn cloud_tabs(&self) -> LiveResult<Vec<CloudTab>> {
        let tabs = parse::cloud_tabs(&self.cloud_get("/")?);
        super::debug::parsed(&format!("cloud tabs parsed: {}", tabs.len()));
        Ok(tabs)
    }

    /// The lineup of Plex's channels, or of one tab of it (`genre`: a [`CloudTab::identifier`]),
    /// without the channels this app cannot play (DRM, or no stream at all).
    pub fn cloud_channels(&self, genre: Option<&str>) -> LiveResult<Vec<Channel>> {
        let path = match genre {
            Some(g) => QueryBuilder::new("/lineups/plex/channels").str("genre", g).build(),
            None => "/lineups/plex/channels".to_string(),
        };
        let all = parse::channels(&self.cloud_get(&path)?);
        let total = all.len();
        let playable: Vec<Channel> = all
            .into_iter()
            .filter(|c| !c.drm && !c.part_key.is_empty())
            .map(|c| Channel { cloud: true, ..c })
            .collect();
        super::debug::parsed(&format!(
            "cloud lineup parsed: {total} channel(s), {} playable, {} with a grid key",
            playable.len(),
            playable.iter().filter(|c| !c.grid_key.is_empty()).count()
        ));
        Ok(playable)
    }

    /// One Plex channel's airings for one UTC day (`date` = `YYYY-MM-DD`).
    pub fn cloud_grid(&self, channel: &Channel, date: &str) -> LiveResult<Vec<Airing>> {
        if channel.grid_key.is_empty() || date.is_empty() {
            return Err(LiveError::Invalid);
        }
        let path = QueryBuilder::new("/grid").str("channelGridKey", &channel.grid_key).str("date", date).build();
        Ok(parse::grid(&self.cloud_get(&path)?, Some(channel)))
    }

    /// The URL a Plex channel plays from, the account's token in its query (never a header: the
    /// provider answers with a redirect to an ad-inserting CDN, and libcurl carries custom headers
    /// across a redirect). `None` for a channel with no stream or an account with no token.
    pub fn cloud_stream_url(&self, channel: &Channel) -> Option<String> {
        if channel.part_key.is_empty() || !channel.part_key.starts_with('/') {
            return None;
        }
        let token = self.headers().into_iter().find_map(|h| h.strip_prefix("X-Plex-Token: ").map(str::to_string))?;
        // The port written out: the media plane parses a portless URL as a Plex server's (32400).
        Some(
            QueryBuilder::new(format!("{BASE}:443{}", channel.part_key))
                .int("includeAllStreams", 1)
                .str("X-Plex-Product", crate::plex::identity::PRODUCT)
                .str("X-Plex-Token", &token)
                .build(),
        )
    }

    /// **What a Plex channel's stream looks like**, for the debug log only: the master playlist's
    /// variants, codecs and resolutions, and the first variant's media playlist's segment count,
    /// discontinuities, encryption and how many hosts its segments come from. Counts and tags,
    /// never a URL or a token. The media layer's HLS reader takes one shape (a server's); this
    /// says how far a provider's stream is from it.
    pub fn cloud_stream_shape(&self, channel: &Channel) -> String {
        if !cfg!(feature = "devtriggers") {
            return String::new();
        }
        let Some(url) = self.cloud_stream_url(channel) else { return "no stream url".into() };
        let headers: Vec<String> = self.headers().into_iter().filter(|h| !h.starts_with("X-Plex-Token")).collect();
        let fetch = |u: &str, follow: bool| plx_net::net::request_evidence(u, &headers, "GET", None, plx_net::net::API, follow, Some(512 * 1024), None);
        // The provider answers the playlist itself (no redirect, seen on the TV); a redirect-following
        // request to a public host is refused by `net` before libcurl runs, so none is asked for.
        let master = match fetch(&url, false) {
            Ok(r) => r,
            Err(f) => return format!("master: no answer ({f:?})"),
        };
        // What the player's own transport sends: the URL alone, no identity headers (only its
        // User-Agent). A provider that holds such a request is why a channel would not open.
        let bare = std::time::Instant::now();
        let bare = match plx_net::net::request_evidence(&url, &[], "GET", None, plx_net::net::API, false, Some(512 * 1024), None) {
            Ok(r) => format!("bare: HTTP {} {} bytes in {} ms", r.status, r.body.len(), bare.elapsed().as_millis()),
            Err(f) => format!("bare: no answer in {} ms ({f:?})", bare.elapsed().as_millis()),
        };
        let text = String::from_utf8_lossy(&master.body).into_owned();
        let mut out = format!("{bare} | master: HTTP {} {} bytes; {}", master.status, master.body.len(), playlist_shape(&text));
        let variant = text.lines().map(str::trim).skip_while(|l| !l.starts_with("#EXT-X-STREAM-INF")).nth(1).map(str::to_string);
        match variant {
            Some(v) if v.starts_with("https://") || v.starts_with("http://") => match fetch(&v, false) {
                Ok(r) => out.push_str(&format!(" | first variant: HTTP {} {} bytes; {}", r.status, r.body.len(), playlist_shape(&String::from_utf8_lossy(&r.body)))),
                Err(f) => out.push_str(&format!(" | first variant: no answer ({f:?})")),
            },
            Some(_) => out.push_str(" | first variant: relative URI (resolved against the redirected master)"),
            None => {}
        }
        out
    }
}

/// A playlist's shape in counts and tags (see [`AccountClient::cloud_stream_shape`]).
fn playlist_shape(text: &str) -> String {
    let lines: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let tagged = |t: &str| lines.iter().filter(|l| l.starts_with(t)).count();
    let uris: Vec<&str> = lines.iter().copied().filter(|l| !l.starts_with('#')).collect();
    let absolute = uris.iter().filter(|u| u.starts_with("http://") || u.starts_with("https://")).count();
    let mut hosts: Vec<&str> = uris
        .iter()
        .filter_map(|u| u.split_once("://").map(|(_, rest)| rest.split('/').next().unwrap_or("")))
        .collect();
    hosts.sort_unstable();
    hosts.dedup();
    let mut attrs: Vec<String> = lines
        .iter()
        .filter(|l| l.starts_with("#EXT-X-STREAM-INF"))
        .flat_map(|l| {
            let codecs = l.split("CODECS=\"").nth(1).and_then(|r| r.split('"').next()).map(|c| format!("codecs={c}"));
            let res = l.split("RESOLUTION=").nth(1).map(|r| format!("res={}", r.split(',').next().unwrap_or("")));
            [codecs, res].into_iter().flatten()
        })
        .collect();
    attrs.sort_unstable();
    attrs.dedup();
    let ext: Vec<&str> = {
        let mut e: Vec<&str> = uris.iter().filter_map(|u| u.split('?').next().and_then(|p| p.rsplit('.').next())).filter(|e| e.len() <= 4).collect();
        e.sort_unstable();
        e.dedup();
        e
    };
    format!(
        "{} variant(s) [{}], {} segment(s), target {}, discontinuities {}, key {}, map {}, uris {} ({} absolute, {} host(s)), ext {:?}",
        tagged("#EXT-X-STREAM-INF"),
        attrs.join(" "),
        tagged("#EXTINF"),
        lines.iter().find_map(|l| l.strip_prefix("#EXT-X-TARGETDURATION:")).unwrap_or("-"),
        tagged("#EXT-X-DISCONTINUITY"),
        tagged("#EXT-X-KEY") + tagged("#EXT-X-SESSION-KEY"),
        tagged("#EXT-X-MAP"),
        uris.len(),
        absolute,
        hosts.len(),
        ext
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_playlist_shape_counts_without_naming_a_url() {
        let master = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1,RESOLUTION=1280x720,CODECS=\"avc1.4d401f,mp4a.40.2\"\nhttps://cdn.example/a/720.m3u8?t=secret\n\
                      #EXT-X-STREAM-INF:BANDWIDTH=2,RESOLUTION=1920x1080,CODECS=\"avc1.640028,mp4a.40.2\"\nhttps://cdn.example/a/1080.m3u8?t=secret\n";
        let s = playlist_shape(master);
        assert!(s.starts_with("2 variant(s)"), "{s}");
        assert!(s.contains("res=1920x1080") && s.contains("codecs=avc1.4d401f,mp4a.40.2"), "{s}");
        assert!(s.contains("2 absolute, 1 host(s)"), "{s}");
        assert!(!s.contains("secret") && !s.contains("cdn.example"), "no URL, host or token in the line: {s}");
        let media = "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXTINF:6,\nseg1.ts\n#EXT-X-DISCONTINUITY\n#EXTINF:6,\nhttps://ads.example/x.ts\n";
        let m = playlist_shape(media);
        assert!(m.contains("2 segment(s)") && m.contains("target 6") && m.contains("discontinuities 1"), "{m}");
    }

    #[test]
    fn the_cloud_lineup_reads_the_stream_key_and_the_drm_flag() {
        let body = serde_json::json!({"MediaContainer": {"Channel": [
            {"id": "a-1", "gridKey": "g1", "title": "One", "vcn": "1", "Media": [{"Part": [{"key": "/library/parts/a-1.m3u8"}]}]},
            {"id": "b-2", "gridKey": "g2", "title": "Two", "drm": true, "Media": [{"Part": [{"key": "/library/parts/b-2.m3u8"}]}]}
        ]}});
        let chans = parse::channels(&body);
        assert_eq!(chans[0].part_key, "/library/parts/a-1.m3u8");
        assert!(!chans[0].drm && chans[1].drm);
    }

    /// The favorites read as the provider answers them, `id` or `key`, and an edit keeps every
    /// other entry byte for byte (fields this app does not read included).
    #[test]
    fn favorites_parse_and_an_edit_keeps_the_other_entries_as_they_came() {
        let body = serde_json::json!({"MediaContainer": {"size": 3, "FavoriteChannel": [
            {"source": "server://KXYZ/tv.plex.providers.epg.cloud:2", "id": "a-1", "title": "One", "vcn": "4.1", "extra": {"x": 1}},
            {"source": "tv.plex.provider.epg", "key": "b-2", "title": "Two"},
            {"source": "server://KXYZ/p", "title": "no id"}
        ]}});
        let list = parse::favorites(&body);
        assert_eq!(list.iter().map(|f| f.id.as_str()).collect::<Vec<_>>(), ["a-1", "b-2"]);
        assert!(list[0].on_server() && !list[1].on_server());
        assert!(list[0].raw.contains("\"extra\""), "the entry's own fields ride along: {}", list[0].raw);
        let added = FavoriteChannel::new("server://KXYZ/tv.plex.providers.epg.cloud:2", "c-3", "Three", "", "5.1");
        let next = FavoriteOp::Add(added.clone()).apply(&list);
        assert_eq!(next.len(), 3);
        assert_eq!(next[..2], list[..], "the others unchanged, in order");
        assert_eq!(FavoriteOp::Add(added.clone()).apply(&next), next, "added once");
        let back: serde_json::Value = serde_json::from_str(&added.raw).expect("json");
        assert_eq!(back, serde_json::json!({"source": "server://KXYZ/tv.plex.providers.epg.cloud:2", "id": "c-3", "title": "Three", "vcn": "5.1"}));
        let less = FavoriteOp::Remove(vec![list[0].clone()]).apply(&next);
        assert_eq!(less.iter().map(|f| f.id.as_str()).collect::<Vec<_>>(), ["b-2", "c-3"]);
        // a bare array is read too
        assert_eq!(parse::favorites(&serde_json::json!([{"source": "s", "id": "z"}])).len(), 1);
    }

    /// **An edit never writes back less than it read** (review 2026-10-26): a list with an entry
    /// this app does not keep, or in a shape it does not know, is not written at all; an empty
    /// list that says so is.
    #[test]
    fn a_favorites_edit_starts_only_from_a_list_read_whole() {
        let entry = |id: &str| serde_json::json!({"source": "server://KXYZ/p", "id": id});
        let whole = serde_json::json!({"MediaContainer": {"size": 2, "FavoriteChannel": [entry("a"), entry("b")]}});
        assert_eq!(parse::favorites_whole(&whole).map(|l| l.len()), Some(2));
        let one = serde_json::json!({"MediaContainer": {"size": 1, "FavoriteChannel": entry("a")}});
        assert_eq!(parse::favorites_whole(&one).map(|l| l.len()), Some(1));
        let skipped = serde_json::json!({"MediaContainer": {"FavoriteChannel": [entry("a"), {"source": "s", "title": "no id"}]}});
        assert_eq!(parse::favorites_whole(&skipped), None, "an entry with no id would be dropped");
        assert_eq!(parse::favorites_whole(&serde_json::json!([entry("a"), 7])), None, "nor a stray element");
        let other = serde_json::json!({"MediaContainer": {"size": 2, "Metadata": [entry("a"), entry("b")]}});
        assert_eq!(parse::favorites_whole(&other), None, "another shape: the list would be replaced");
        assert_eq!(parse::favorites_whole(&serde_json::json!({"favorites": []})), None);
        let empty = serde_json::json!({"MediaContainer": {"size": 0}});
        assert_eq!(parse::favorites_whole(&empty), Some(Vec::new()));
        assert_eq!(parse::favorites_whole(&serde_json::json!([])), Some(Vec::new()));
    }

    #[test]
    fn the_provider_root_lists_its_genre_tabs() {
        let body = serde_json::json!({"MediaContainer": {"MediaProvider": [{"Feature": [
            {"type": "content"},
            {"type": "grid", "GridChannelFilter": [{"identifier": "movies", "title": "Movies"}, {"identifier": "news", "title": "News"}]}
        ]}]}});
        let tabs = parse::cloud_tabs(&body);
        assert_eq!(tabs.iter().map(|t| t.title.as_str()).collect::<Vec<_>>(), ["Movies", "News"]);
    }
}
