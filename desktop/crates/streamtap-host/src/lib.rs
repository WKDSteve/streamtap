//! The streamtap native messaging host as a library: the message schema
//! (`proto`), the two framings (`framing`), the handshake of the bridge
//! (`auth`) and the relay between the browser and the app (`relay`). The
//! binary (`main.rs`) only wires them to the process's standard streams.

pub mod auth;
pub mod framing;
pub mod proto;
pub mod relay;
