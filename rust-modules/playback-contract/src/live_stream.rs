//! **What kind of live stream is playing**, as the route (`plx_media::route::live`) installs it
//! and the Feed's HLS cursor (`plx_play_feed::ff`) reads it once when a cursor opens. The route
//! and the Feed reach this crate and not each other, so the two facts live here.
//!
//! - [`set_live_stream`]: the stream is a live channel. Its playlist never ends, so the cursor
//!   tolerates what a library transcode should not see (a refresh that fails a few times, a window
//!   that slid past the cursor) and treats a playlist that stopped growing as the end of the tune.
//! - [`set_provider_stream`]: the live stream is a cloud provider's (Plex's own channels): its
//!   playlists are read as the provider shape and it starts near the live edge.
use std::sync::atomic::{AtomicBool, Ordering};

static LIVE_STREAM: AtomicBool = AtomicBool::new(false);
static PROVIDER_STREAM: AtomicBool = AtomicBool::new(false);

/// See the module doc. Set by the live route's install, cleared by its release.
pub fn set_live_stream(live: bool) {
    LIVE_STREAM.store(live, Ordering::Release);
}

/// Is the stream now playing a live channel?
pub fn live_stream() -> bool {
    LIVE_STREAM.load(Ordering::Acquire)
}

/// See the module doc. Set and cleared with [`set_live_stream`] by the live route.
pub fn set_provider_stream(provider: bool) {
    PROVIDER_STREAM.store(provider, Ordering::Release);
}

/// Is the live stream now playing a cloud provider's?
pub fn provider_stream() -> bool {
    PROVIDER_STREAM.load(Ordering::Acquire)
}
