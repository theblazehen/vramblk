//! NBD (Network Block Device) module
//!
//! This module handles the NBD server implementation using the `nbd` crate,
//! exposing the GPU memory buffer over the network.

mod server;

pub use server::{NbdConfig, start_nbd_server};
