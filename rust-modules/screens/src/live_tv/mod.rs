//! **The Live TV page** (`AppArg::LiveTv`; `docs/pms-api.md` §10 is the protocol).
//!
//! The modules:
//!
//! * [`model`]: the page's pure model — element keys, the guide's geometry, focus moves, and the
//!   actions an airing, a recording or a rule offers, with the store write each one asks for.
//! * [`page`]: [`page::LiveTvScreen`], the screen, and its output alphabet [`page::LiveTvOut`].
//!
//! No sibling screen is named here: watching, recording and the tabs leave as outputs the
//! application routes.
pub mod model;
pub mod page;

pub use page::LiveTvOut;

#[cfg(test)]
mod model_tests;
