//! VRAM Block Device - Expose GPU memory as a block device
//!
//! This application uses OpenCL to allocate memory on a GPU and exposes
//! it to userspace via a  NBD server implementation.
//! It attempts to lock its memory to prevent being swapped out.

mod nbd;
mod opencl;

use crate::nbd::{start_nbd_server, NbdConfig};
use crate::opencl::{VRamBuffer, VRamBufferConfig};

use anyhow::{bail, Context, Result};
use clap::Parser;
use opencl3::{
    device::{get_device_ids, Device, CL_DEVICE_TYPE_GPU},
    platform::get_platforms,
};
use std::sync::Arc;
// Correct import name: MlockAllFlags
use nix::sys::mman::{mlockall, MlockAllFlags};

/// Command line arguments for the VRAM Block Device
#[derive(Parser, Debug)]
#[clap(
    name = "vramblk",
    about = "Expose GPU memory as a block device using a NBD server. Locks memory using mlockall.",
    version
)]
struct Args {
    /// Size of the block device (e.g., 512M, 2G, 1024). Defaults to MB if no suffix.
    #[clap(short, long, value_parser = parse_size_string, default_value = "2048M")]
    size: u64, // Store size in bytes

    /// GPU device index to use (0 for first GPU)
    #[clap(short, long, default_value = "0")]
    device: usize,

    /// OpenCL platform index
    #[clap(short, long, default_value = "0")]
    platform: usize,

    /// Listen address for the NBD server (e.g., 127.0.0.1:10809 or [::1]:10809)
    #[clap(short, long, default_value = "127.0.0.1:10809")]
    listen_addr: String,

    /// Export name advertised over NBD
    #[clap(short, long, default_value = "vram")]
    export_name: String,

    /// Enable verbose logging
    #[clap(short, long)]
    verbose: bool,

    /// List available OpenCL platforms and devices and exit
    #[clap(long)]
    list_devices: bool,
}

/// Parses a size string (e.g., "512M", "2G") into bytes.
pub(crate) fn parse_size_string(size_str: &str) -> Result<u64> {
    let size_str = size_str.trim().to_uppercase();
    let (num_part, suffix) = size_str.split_at(
        size_str
            .find(|c: char| !c.is_digit(10))
            .unwrap_or(size_str.len()),
    );

    let num: u64 = num_part.parse().context("Invalid size number")?;

    match suffix {
        "" | "M" | "MB" => Ok(num * 1024 * 1024),
        "G" | "GB" => Ok(num * 1024 * 1024 * 1024),
        _ => bail!("Invalid size suffix: '{}'. Use M/MB or G/GB.", suffix),
    }
}

/// Lists available OpenCL devices.
fn list_opencl_devices() -> Result<()> {
    println!("Available OpenCL Platforms and Devices:");
    let platforms = get_platforms().context("Failed to get OpenCL platforms")?;
    if platforms.is_empty() {
        println!("  No OpenCL platforms found.");
        return Ok(());
    }

    for (plat_idx, platform) in platforms.iter().enumerate() {
        let plat_name = platform
            .name()
            .unwrap_or_else(|_| "Unknown Platform".to_string());
        println!("\nPlatform {}: {}", plat_idx, plat_name);

        match get_device_ids(platform.id(), CL_DEVICE_TYPE_GPU) {
            Ok(device_ids) => {
                if device_ids.is_empty() {
                    println!("  No GPU devices found on this platform.");
                } else {
                    for (dev_idx, device_id) in device_ids.iter().enumerate() {
                        let device = Device::new(*device_id);
                        let dev_name = device
                            .name()
                            .unwrap_or_else(|_| "Unknown Device".to_string());
                        let dev_vendor = device
                            .vendor()
                            .unwrap_or_else(|_| "Unknown Vendor".to_string());
                        let dev_mem = device.global_mem_size().unwrap_or(0);
                        println!(
                            "  Device {}: {} ({}) - Memory: {} MB",
                            dev_idx,
                            dev_name,
                            dev_vendor,
                            dev_mem / (1024 * 1024)
                        );
                    }
                }
            }
            Err(e) => {
                println!("  Error getting devices for this platform: {}", e);
            }
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    if args.list_devices {
        return list_opencl_devices();
    }

    if args.verbose {
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("debug")).init();
    } else {
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    }

    log::info!("Starting VRAM Block Device (NBD Server)");

    // --- Lock process memory ---
    log::info!("Attempting to lock process memory using mlockall()...");
    // Use correct flag names from the MlockAllFlags type
    match mlockall(MlockAllFlags::MCL_CURRENT | MlockAllFlags::MCL_FUTURE) {
        Ok(_) => log::info!("Successfully locked process memory."),
        Err(e) => {
            log::warn!(
                "Failed to lock process memory (requires root or CAP_IPC_LOCK): {}",
                e
            );
        }
    }
    // -------------------------

    // Size is already parsed into bytes
    log::info!(
        "Allocating {} bytes ({} MB) on GPU device {} (Platform {})",
        args.size,
        args.size / (1024 * 1024), // Log MB for readability
        args.device,
        args.platform
    );

    let buffer_config = VRamBufferConfig {
        size: args.size as usize, // VRamBufferConfig expects usize
        device_index: args.device,
        platform_index: args.platform,
    };

    let buffer =
        Arc::new(VRamBuffer::new(&buffer_config).context("Failed to allocate GPU memory")?);

    log::info!(
        "Successfully allocated {} bytes ({} MB) on {}",
        args.size,
        args.size / (1024 * 1024), // Log MB for readability
        buffer.device_name()
    );

    let nbd_config = NbdConfig {
        listen_addr: args.listen_addr.clone(),
        export_name: args.export_name.clone(),
    };

    // Start the NBD server (this function now runs until shutdown)
    start_nbd_server(buffer, &nbd_config).await?;

    log::info!("VRAM Block Device server has shut down.");
    Ok(())
}
