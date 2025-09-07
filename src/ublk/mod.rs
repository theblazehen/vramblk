// ublk frontend module
// Feature-gated implementation placeholder. When the "ublk" feature is enabled
// and the libublk-based implementation is provided, server.rs will contain the
// real start_ublk_server. For now we provide a stub that compiles and returns
// a clear error at runtime if selected without the feature.

mod server;

pub use server::{start_ublk_server, UblkConfig};