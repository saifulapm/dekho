//! dekho — search a movie or series and stream it straight into mpv.
//!
//! The binary in `main.rs` is the only intended consumer; these modules are
//! public so integration tests can drive the streaming chain directly (see
//! `tests/stream_smoke.rs`), which is the one part unit tests cannot cover.

pub mod browse;
pub mod engine;
pub mod pick;
pub mod player;
pub mod tmdb;
pub mod torrentio;
