//! The Live TV / DVR vocabulary the rest of the app reads: plain values parsed out of PMS's
//! answers by `parse.rs`, never the wire shapes themselves. Every field degrades to its empty
//! value when PMS leaves it out; identity fields that an action needs (a subscription id, a
//! channel identifier, a grab key) are the ones the parsers refuse to invent.

/// One EPG (guide) provider a server offers — a row of `GET /media/providers` whose identifier is
/// a `tv.plex.providers.epg.*` provider. One per DVR the server owner set up.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EpgProvider {
    /// `tv.plex.providers.epg.cloud:12` — the path prefix of its lineup and grid endpoints.
    pub identifier: String,
    /// The provider's `parentID`: the `{dvrId}` of `POST /livetv/dvrs/{dvrId}/channels/{id}/tune`.
    /// Empty when the server did not say, and then nothing on this provider can be tuned.
    pub dvr_key: String,
    pub title: String,
    /// `Feature type="subscribe" flavor="record"`: airings of this provider can be recorded.
    pub recordable: bool,
    /// `Feature type="grid"`: the provider serves a time grid (the guide).
    pub grid: bool,
}

impl EpgProvider {
    /// Can a channel of this provider be tuned (watched live)?
    pub fn tunable(&self) -> bool {
        !self.dvr_key.is_empty()
    }
}

/// One channel of a provider's lineup (`/{provider}/lineups/dvr/channels`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Channel {
    /// The lineup `id` — the `{channel}` segment of the tune path and an airing's
    /// `Media.channelIdentifier`.
    pub identifier: String,
    /// The `channelGridKey` the grid endpoint takes for this channel.
    pub grid_key: String,
    /// The virtual channel number viewers know it by ("2.1"). Display only.
    pub vcn: String,
    pub title: String,
    pub call_sign: String,
    pub thumb: String,
    pub hd: bool,
}

impl Channel {
    /// The short name a guide row heads with: the call sign, else the title.
    pub fn label(&self) -> &str {
        if self.call_sign.is_empty() { &self.title } else { &self.call_sign }
    }
}

/// One airing: a program on a channel between two wall-clock instants (`Media.beginsAt` /
/// `Media.endsAt`, epoch seconds). The grid answers a `Metadata` per program and a `Media` per
/// airing of it; the parser emits one [`Airing`] per `Media` that carries a real time span.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Airing {
    pub rating_key: String,
    /// The program's metadata key (what a detail read or the record flow fetches).
    pub key: String,
    /// The program guid — what `GET /media/subscriptions/template?guid=` takes.
    pub guid: String,
    /// The show's guid for an episode (a series recording overlays on it).
    pub grandparent_guid: String,
    /// `movie`, `episode`, `show`, … as PMS spells it.
    pub kind: String,
    pub title: String,
    /// The show's title for an episode (`grandparentTitle`); empty for a movie.
    pub show_title: String,
    pub season: i64,
    pub episode: i64,
    pub year: i64,
    pub summary: String,
    pub thumb: String,
    /// The program's portrait art for a guide cell: the `coverPoster` image, else the show's
    /// thumb, else the program's own. A path or an absolute URL.
    pub poster: String,
    /// The program's backdrop (`art`, else the show's `grandparentArt`). A path or an absolute URL.
    pub art: String,
    /// `TV-14`, `PG`, … as the guide spells it.
    pub content_rating: String,
    /// The guide's genre tags, in its order.
    pub genres: Vec<String>,
    pub begins_at: i64,
    pub ends_at: i64,
    pub channel_identifier: String,
    pub channel_vcn: String,
    pub channel_title: String,
    /// The channel's logo as the airing carries it (`Media.channelThumb`).
    pub channel_thumb: String,
    /// PMS's own "on now" flag. A guide decides "now" from its clock; this is a hint.
    pub on_air: bool,
    pub premiere: bool,
    /// The recording rule that covers THIS airing's program (`subscriptionID`), empty if none.
    pub subscription_id: String,
    /// The series rule that covers it (`grandparentSubscriptionID`), empty if none.
    pub show_subscription_id: String,
    pub media_id: String,
}

impl Airing {
    /// Is a recording rule (episode or series) scheduled for this airing?
    pub fn is_recording_scheduled(&self) -> bool {
        !self.subscription_id.is_empty() || !self.show_subscription_id.is_empty()
    }
    /// The rule a cancel from this airing acts on: the series rule wins, as in the official
    /// client (`grandparentSubscriptionID || subscriptionID`).
    pub fn covering_subscription(&self) -> &str {
        if self.show_subscription_id.is_empty() { &self.subscription_id } else { &self.show_subscription_id }
    }
    /// Does `[begins_at, ends_at)` contain `now` (epoch seconds)?
    pub fn airs_at(&self, now: i64) -> bool {
        self.begins_at <= now && now < self.ends_at
    }
}

/// What a recording rule records, the hint PMS keeps beside it (`Video` or `Directory`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SubscriptionHint {
    pub kind: String,
    pub title: String,
    pub show_title: String,
    pub year: i64,
    pub thumb: String,
    pub guid: String,
}

/// How a [`Setting`] is edited.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SettingKind {
    Bool,
    Int,
    #[default]
    Text,
}

/// One allowed value of an enumerated [`Setting`]: `value` is what goes on the wire, `label`
/// what a row shows (PMS percent-encodes it in `enumValues`; this is the decoded text).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SettingOption {
    pub value: String,
    pub label: String,
}

/// One recording preference of a rule or template (`Setting`). Its `id` is the name of the
/// `prefs[...]` query key that carries it back.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Setting {
    pub id: String,
    pub label: String,
    pub summary: String,
    pub kind: SettingKind,
    pub default: String,
    /// The current value, as the wire spells it (`"1"`, `"720"`, `"true"`). Empty when PMS gave
    /// none — such a setting is shown, never sent.
    pub value: String,
    /// Never shown, always sent back with its value.
    pub hidden: bool,
    pub advanced: bool,
    pub group: String,
    pub options: Vec<SettingOption>,
}

impl Setting {
    /// The label of the current value for an enumerated setting, else the raw value.
    pub fn value_label(&self) -> &str {
        self.options.iter().find(|o| o.value == self.value).map_or(self.value.as_str(), |o| o.label.as_str())
    }
    /// A bool setting's current value (`1`/`true`).
    pub fn is_on(&self) -> bool {
        matches!(self.value.trim(), "1" | "true" | "True")
    }
}

/// One choice the record flow offers for an airing — "This Episode", "All Episodes" — a
/// `SubscriptionTemplate > MediaSubscription` of `GET /media/subscriptions/template`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TemplateChoice {
    pub title: String,
    pub selected: bool,
    /// The metadata type the rule records (`type`, 1 movie / 2 show / 4 episode), sent back on create.
    pub kind: i64,
    /// The pre-encoded query string PMS wants back verbatim on create (`hints[...]`, `params[...]`).
    pub parameters: String,
    pub target_section: String,
    pub target_location: String,
    pub section_title: String,
    pub location_path: String,
    pub airings_type: String,
    pub settings: Vec<Setting>,
    pub hint: SubscriptionHint,
}

/// A recording rule (`MediaSubscription` of `GET /media/subscriptions`). The list's order is the
/// rule priority: earlier wins a tuner conflict.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Subscription {
    pub id: String,
    pub kind: i64,
    pub title: String,
    pub target_section: String,
    pub target_location: String,
    pub section_title: String,
    pub location_path: String,
    pub airings_type: String,
    pub settings: Vec<Setting>,
    pub hint: SubscriptionHint,
    pub grabs: Vec<Grab>,
}

/// Where one recording operation stands (`MediaGrabOperation.status`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum GrabStatus {
    Inactive,
    #[default]
    Scheduled,
    InProgress,
    Complete,
    Cancelled,
    Error,
    PostProcessing,
    Paused,
    Other(String),
}

impl GrabStatus {
    pub fn of(wire: &str) -> Self {
        match wire.trim().to_ascii_lowercase().as_str() {
            "inactive" => Self::Inactive,
            "scheduled" | "" => Self::Scheduled,
            "inprogress" | "in_progress" | "recording" => Self::InProgress,
            "complete" | "completed" => Self::Complete,
            "cancelled" | "canceled" => Self::Cancelled,
            "error" | "failed" => Self::Error,
            "postprocessing" => Self::PostProcessing,
            "paused" => Self::Paused,
            other => Self::Other(other.to_string()),
        }
    }
    /// Is the tuner recording it right now?
    pub fn is_recording(&self) -> bool {
        matches!(self, Self::InProgress)
    }
    /// Can the grab still be stopped or cancelled?
    pub fn is_pending_or_active(&self) -> bool {
        matches!(self, Self::Scheduled | Self::InProgress | Self::Paused | Self::Inactive)
    }
}

/// One recording operation — an airing a rule will record or is recording
/// (`MediaGrabOperation` of `GET /media/subscriptions/scheduled`, or nested in a rule).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Grab {
    pub id: String,
    /// `/media/grabbers/operations/{id}` — what a DELETE cancels.
    pub key: String,
    pub subscription_id: String,
    pub status: GrabStatus,
    pub percent: f64,
    pub airing: Airing,
}

/// A recording that already holds the tuner a tune or a new rule wants
/// (`Conflicts > MediaGrabOperation`). `rolling` marks another viewer's live session rather than
/// a recording.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Conflict {
    pub grab_key: String,
    pub subscription_id: String,
    pub rolling: bool,
    pub airing: Airing,
}

/// The tuner's capture window, from a `TranscodeSession`: `time_stamp` is the window's origin
/// (epoch seconds) and the offsets bound what can be played, in seconds from it. The units are
/// inferred from how clients use them (`docs/pms-api.md` §10) and unmeasured.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CaptureWindow {
    pub time_stamp: f64,
    pub min_offset: f64,
    pub max_offset: f64,
}

impl CaptureWindow {
    /// The live edge, as an epoch second.
    pub fn live_edge(&self) -> f64 {
        self.time_stamp + self.max_offset
    }
    /// How many seconds behind the live edge the oldest playable moment is.
    pub fn depth(&self) -> f64 {
        (self.max_offset - self.min_offset).max(0.0)
    }
}

/// The item a tune hands back to play: the grab's `Video`/`Metadata`. Its `key` is the path the
/// universal transcoder plays (`path=`), and `session` is `Media.uuid`, the live session id.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TunedItem {
    pub key: String,
    pub rating_key: String,
    pub kind: String,
    pub title: String,
    pub show_title: String,
    pub session: String,
    pub begins_at: i64,
    pub ends_at: i64,
    pub channel_identifier: String,
    pub channel_title: String,
    pub channel_vcn: String,
    pub width: i64,
    pub height: i64,
    pub video_codec: String,
    pub audio_codec: String,
}

/// A tune's answer.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TuneResult {
    /// The live subscription the tune created (`MediaSubscription.key`) — what a refresh reads
    /// and a release deletes. Empty when PMS answered without one (the bare `Metadata` shape);
    /// such a tune is released by stopping its transcode alone.
    pub subscription_id: String,
    pub item: TunedItem,
    pub capture: Option<CaptureWindow>,
    pub conflicts: Vec<Conflict>,
    /// The programs around now on the tuned channel (`MediaSubscription > Timeline`).
    pub programs: Vec<Airing>,
}

impl TuneResult {
    /// Did the tuner refuse because every tuner is busy?
    pub fn has_conflicts(&self) -> bool {
        !self.conflicts.is_empty()
    }
    /// Is there something to play?
    pub fn playable(&self) -> bool {
        !self.item.key.is_empty() && self.conflicts.is_empty()
    }
}

/// `POST /media/subscriptions`'s answer: the new rule's id, and the recordings it collides with.
/// A rule created with conflicts EXISTS on the server: "Prefer this" raises it, "Cancel this"
/// deletes it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CreateResult {
    pub subscription_id: String,
    pub conflicts: Vec<Conflict>,
}

/// What a record request sends (`POST /media/subscriptions`): the chosen template entry's
/// `parameters` and `type` verbatim, the target library, and one value per setting.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CreateSubscription {
    pub kind: i64,
    pub parameters: String,
    pub target_section: String,
    /// Sent only when non-empty (a library with one location needs none).
    pub target_location: String,
    /// `(setting id, value)` in send order. Hidden settings are included by the caller.
    pub prefs: Vec<(String, String)>,
}

impl CreateSubscription {
    /// The request a choice makes with every setting at its current value — what "Record" sends
    /// when nothing was edited. Settings without a value are left out (PMS gave none to echo).
    pub fn from_choice(choice: &TemplateChoice) -> Self {
        Self {
            kind: choice.kind,
            parameters: choice.parameters.clone(),
            target_section: choice.target_section.clone(),
            target_location: choice.target_location.clone(),
            prefs: choice.settings.iter().filter(|s| !s.value.is_empty()).map(|s| (s.id.clone(), s.value.clone())).collect(),
        }
    }
}

/// What a rule edit sends (`PUT /media/subscriptions/{id}`): no `type`, no `parameters`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EditSubscription {
    pub target_section: String,
    pub target_location: String,
    pub prefs: Vec<(String, String)>,
}

impl EditSubscription {
    pub fn from_subscription(sub: &Subscription) -> Self {
        Self {
            target_section: sub.target_section.clone(),
            target_location: sub.target_location.clone(),
            prefs: sub.settings.iter().filter(|s| !s.value.is_empty()).map(|s| (s.id.clone(), s.value.clone())).collect(),
        }
    }
}

/// Why a Live TV request produced nothing usable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LiveError {
    /// Nothing answered.
    Transport,
    /// A non-2xx status.
    Status(i32),
    /// A 2xx whose container carried PMS's own error (`status` + `message`).
    Server { status: i64, message: String },
    /// A 2xx whose body is not the expected shape.
    Unreadable,
    /// A debug build refused to write to a real server (`live_tv::guard`).
    Refused,
    /// The caller passed an id or key that cannot name the resource (never sent).
    Invalid,
    /// A tune the server answered with conflicts: every tuner is in use.
    Busy,
}

impl LiveError {
    /// A short, user-safe reason (no ids, no paths).
    pub fn reason(&self) -> String {
        match self {
            Self::Transport => "the server did not answer".into(),
            Self::Status(403) => "this account cannot use the DVR on this server".into(),
            Self::Status(404) => "the server no longer has it".into(),
            Self::Status(409) => "a recording like this already exists".into(),
            Self::Status(s) => format!("the server answered HTTP {s}"),
            Self::Server { message, .. } if !message.is_empty() => message.clone(),
            Self::Server { status, .. } => format!("the server refused ({status})"),
            Self::Unreadable => "the server's answer could not be read".into(),
            Self::Refused => "debug build: writes to a real server are off".into(),
            Self::Invalid => "invalid request".into(),
            Self::Busy => "every tuner is busy".into(),
        }
    }
}

pub type LiveResult<T> = Result<T, LiveError>;
