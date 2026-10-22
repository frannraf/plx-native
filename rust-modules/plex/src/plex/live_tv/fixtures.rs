//! SYNTHETIC Live TV / DVR answers for the parser tests. Not captures: every title, id and channel
//! is invented, shaped after `docs/pms-api.md` §10 (the spec's examples and the shapes the
//! open-source clients parse). Two shapes deliberately disagree with each other where PMS does —
//! an element as an object vs an array of one, `Video` vs `Metadata`, numbers as strings — because
//! surviving that is the parsers' whole job.

pub const PROVIDERS: &str = r#"{"MediaContainer":{"size":2,"MediaProvider":[
 {"identifier":"com.plexapp.plugins.library","title":"Library","Feature":[{"type":"content"}]},
 {"id":13,"parentID":12,"identifier":"tv.plex.providers.epg.cloud:12","providerIdentifier":"tv.plex.providers.epg.cloud",
  "title":"Live TV & DVR","protocols":"livetv","Feature":[
   {"type":"grid","key":"/tv.plex.providers.epg.cloud:12/grid"},
   {"type":"subscribe","flavor":"record","key":"/media/subscriptions"},
   {"type":"content"}]}]}}"#;

pub const PROVIDERS_NO_DVR: &str = r#"{"MediaContainer":{"size":1,"MediaProvider":
 {"identifier":"com.plexapp.plugins.library","title":"Library"}}}"#;

pub const LINEUP: &str = r#"{"MediaContainer":{"size":3,"Channel":[
 {"id":"s0a1-c001","gridKey":"s0a1-c001","vcn":"2.1","isHd":"1","title":"Channel A","callSign":"SAAA","thumb":"/thumb/a"},
 {"id":"s0a1-c002","gridKey":"c002","vcn":4,"isHd":false,"title":"Channel B","callSign":""},
 {"gridKey":"orphan","title":"No id"}]}}"#;

/// Two programs on one channel, one `Media` each; the first one twice (an airing that spans the
/// UTC midnight comes back in both days' answers) and one `Media` with no span.
pub const GRID: &str = r#"{"MediaContainer":{"size":"3","Metadata":[
 {"ratingKey":"9001","key":"/tv.plex.providers.epg.cloud:12/metadata/9001","guid":"plex://episode/s00000000000000000000001",
  "grandparentGuid":"plex://show/s00000000000000000000002","type":"episode","title":"Episode A","grandparentTitle":"Show A",
  "parentIndex":"2","index":5,"year":2026,"subscriptionID":"0","grandparentSubscriptionID":"41",
  "Media":[{"id":"m1","beginsAt":1760000000,"endsAt":"1760001800","channelIdentifier":"s0a1-c001","channelVcn":"2.1",
            "channelTitle":"Channel A","onAir":true,"premiere":"1"}]},
 {"ratingKey":"9001","type":"episode","title":"Episode A","Media":{"beginsAt":1760000000,"endsAt":1760001800}},
 {"ratingKey":"9002","type":"movie","title":"Movie B","guid":"plex://movie/s00000000000000000000003",
  "Media":[{"beginsAt":1760001800,"endsAt":1760007200},{"beginsAt":0,"endsAt":0}]}]}}"#;

pub const SUBSCRIPTIONS: &str = r#"{"MediaContainer":{"size":2,"MediaSubscription":[
 {"key":"41","type":2,"title":"Show A","targetLibrarySectionID":"3","targetSectionLocationID":0,
  "librarySectionTitle":"TV","locationPath":"/data/tv","airingsType":"New Airings Only",
  "Directory":{"type":"show","title":"Show A","guid":"plex://show/s00000000000000000000002","year":2020},
  "Setting":[{"id":"minVideoQuality","label":"Resolution","type":"int","default":"0","value":"720",
              "enumValues":"0:Any|720:HD%20only|1080:Full%20HD"},
             {"id":"oneShot","type":"bool","value":false,"hidden":true}],
  "MediaGrabOperation":{"id":"g1","key":"/media/grabbers/operations/g1","mediaSubscriptionID":41,"status":"inprogress",
   "percent":"37.5","Metadata":{"title":"Episode A","Media":[{"beginsAt":1760000000,"endsAt":1760001800}]}}},
 {"key":"/media/subscriptions/42","type":"1","title":"Movie B","targetLibrarySectionID":1,
  "Video":{"type":"movie","title":"Movie B"}},
 {"title":"no key"}]}}"#;

pub const TEMPLATE: &str = r#"{"MediaContainer":{"size":1,"SubscriptionTemplate":[{"MediaSubscription":[
 {"title":"This Episode","type":4,"targetLibrarySectionID":"3","selected":false,
  "parameters":"hints%5Bguid%5D=plex%3A%2F%2Fepisode%2Fs00000000000000000000001&params%5BmediaProviderID%5D=13",
  "Setting":[{"id":"startOffsetMinutes","type":"int","value":"0","enumValues":"0:None|5:5%20minutes"},
             {"id":"oneShot","type":"bool","value":"true","hidden":"1"}]},
 {"title":"All Episodes","type":2,"targetLibrarySectionID":"3","selected":"1",
  "parameters":"hints%5Bguid%5D=plex%3A%2F%2Fshow%2Fs00000000000000000000002&params%5BlibraryType%5D=2",
  "Setting":[{"id":"onlyNewAirings","type":"bool","value":"0","advanced":true}]}]}]}}"#;

pub const SCHEDULED: &str = r#"{"MediaContainer":{"size":2,"MediaGrabOperation":[
 {"id":"g2","mediaSubscriptionID":"42","status":"scheduled",
  "Metadata":{"title":"Movie B","type":"movie","Media":[{"beginsAt":1760001800,"endsAt":1760007200,"channelTitle":"Channel B"}]}},
 {"id":"g1","key":"/media/grabbers/operations/g1","mediaSubscriptionID":41,"status":"inprogress","percent":37.5,
  "Metadata":{"title":"Episode A","grandparentTitle":"Show A","Media":[{"beginsAt":1760000000,"endsAt":1760001800}]}}]}}"#;

pub const CREATED: &str = r#"{"MediaContainer":{"size":1,"MediaSubscription":[{"key":"43","type":4,"title":"Episode A",
 "MediaGrabOperation":[{"id":"g3","status":"scheduled"}]}]}}"#;

pub const CREATED_CONFLICTED: &str = r#"{"MediaContainer":{"MediaSubscription":{"key":"44",
 "MediaGrabOperation":{"id":"g4","Conflicts":{"MediaGrabOperation":[
  {"key":"/media/grabbers/operations/g1","mediaSubscriptionID":41,"rolling":"0","Metadata":{"title":"Episode A"}}]}}}}}"#;

/// The tune shape the open-source clients parse: subscription → one grab → the tuned item with
/// its capture window, plus the programs around now.
pub const TUNE: &str = r#"{"MediaContainer":{"size":1,"MediaSubscription":[{"key":"77","type":4,"title":"Episode A",
 "MediaGrabOperation":[{"id":"g7","mediaSubscriptionID":77,"status":"inprogress",
  "Video":{"key":"/livetv/sessions/0e0e0e0e-0000-4000-8000-000000000001/1","ratingKey":"9001","type":"episode",
   "title":"Episode A","grandparentTitle":"Show A","live":1,
   "TranscodeSession":{"key":"/transcode/sessions/x","timeStamp":"1760000100.5","minOffsetAvailable":0,"maxOffsetAvailable":"42.25"},
   "Media":[{"uuid":"0e0e0e0e-0000-4000-8000-000000000001","beginsAt":1760000000,"endsAt":1760001800,
     "channelIdentifier":"s0a1-c001","channelTitle":"Channel A","channelVcn":"2.1",
     "Part":[{"Stream":[{"streamType":1,"codec":"mpeg2video","width":1920,"height":1080},{"streamType":2,"codec":"ac3"}]}]}]}}],
 "Timeline":{"Video":[
   {"ratingKey":"9002","title":"Movie B","Media":[{"beginsAt":1760001800,"endsAt":1760007200}]},
   {"ratingKey":"9001","title":"Episode A","Media":[{"beginsAt":1760000000,"endsAt":1760001800}]}]}}]}}"#;

/// Every tuner busy: the grab carries conflicts and no tuned item, but the subscription has a key.
pub const TUNE_CONFLICTED: &str = r#"{"MediaContainer":{"MediaSubscription":{"key":"78",
 "MediaGrabOperation":{"id":"g8","Conflicts":[{"MediaGrabOperation":[
   {"key":"/media/grabbers/operations/g1","mediaSubscriptionID":41,"rolling":false,"Metadata":{"title":"Episode A"}},
   {"key":"/media/grabbers/operations/g9","mediaSubscriptionID":79,"rolling":true,"Metadata":{"title":"Live viewer"}}]}]}}}}"#;

/// The spec's documented shape: bare `Metadata` with the session key, no subscription.
pub const TUNE_SPEC_SHAPE: &str = r#"{"MediaContainer":{"size":1,"Metadata":[{"live":true,
 "key":"/livetv/sessions/0e0e0e0e-0000-4000-8000-000000000002","genuineMediaAnalysis":true,
 "Media":[{"uuid":"0e0e0e0e-0000-4000-8000-000000000002","Part":[{"Stream":[
   {"codec":"h264","streamType":1,"width":1920,"height":1080},{"codec":"ac3","streamType":2}]}]}]}]}}"#;

pub const SERVER_REFUSAL: &str = r#"{"MediaContainer":{"size":0,"status":-1,"message":"No tuners available"}}"#;
