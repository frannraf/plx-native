//! play_movie route selection (direct-play vs transcode) + the stream URL, transcode session, and
//! HUD strings, split in phase 9 (spec §9) into two modules by ONE rule: [`plan`] holds functions
//! of their arguments alone — no `static mut`, no `PlayerControl`, no `plex::Client` held across a
//! call, no `task::spawn*` — plus the plain data types they need, which is what lets
//! [`plan::build_stream`] run on the resolve worker; [`decision`] holds everything else — the
//! main-thread `Session`, the synchronized `PlayerControl`, and every PMS/native effect. Every
//! item this module re-exports keeps its old `route::` path, so nothing outside `route/` had to
//! change. `ci/check-deps.sh`'s `wall` gate holds `plan.rs` (and `plan_memory.rs`, its child) to zero
//! `Instant::now`/`SystemTime::now`/`.elapsed()`; `decision.rs` is not scanned by that gate (a
//! network/adapter effect is allowed to read wall time) but as of this split carries none either.

mod decision;
mod flight;
mod live;
mod plan;
mod prefs;
mod track_memory;

pub use decision::*;
pub use flight::{
    discard_stale_flight_landing, engineless_flight_outstanding, flight_is_current, flight_outstanding, recovery_flight_offset_ns, recovery_flight_outstanding, resume_flight_outstanding, retire_superseded_encoder, take_ready_flight,
    take_ready_recovery_flight, ReadyFlight, RecoveryLanding, RecoveryVerdict, SeekVerdict,
};
pub use live::{
    capture_span, drain_live_releases, install_live, is_live, is_recordable_live, live_airing, live_can_shift, live_lease_held, live_releasing, live_shift_state,
    live_shift_clock, live_shift_url, note_live_paused, note_live_shift, record_asked, release_live, release_live_tuner, restore_live_shift_clock,
    roll_live_program, set_live_context, set_record_asked, shift_target, LiveInfo, LiveRoute, ShiftClock, KEEPALIVE_S, LIVE_SNAP_S,
};
pub use plan::*;
pub use prefs::*;
pub use track_memory::{forced_only_language, forced_only_pick, memo_of_audio, memo_of_subtitle, subtitle_pick_changed, SubMode, TrackContext};
