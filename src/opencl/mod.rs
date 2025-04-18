//! OpenCL module for GPU memory allocation and management
//!
//! This module handles interaction with the GPU via OpenCL,
//! including device selection, memory allocation, and data transfer.

mod memory;

pub use memory::{VRamBuffer, VRamBufferConfig};
