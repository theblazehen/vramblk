use anyhow::{Context, Result};
use std::sync::Arc;

use crate::backend::BlockBackend;

use libublk::{
    ctrl::{UblkCtrl, UblkCtrlBuilder},
    io::{UblkDev, UblkIOCtx, UblkQueue},
    sys, UblkError, UblkFlags, UblkIORes,
};
use std::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Configuration for the ublk frontend
#[derive(Debug, Clone)]
pub struct UblkConfig {
    /// Logical block size in bytes (e.g., 4096)
    pub logical_block_size: u32,
}

/// Start the ublk frontend server using libublk.
///
/// Blocks the current task until device shutdown (Ctrl-C or SIGTERM).
/// Shutdown is coordinated via a CancellationToken; on cancellation we call
/// UblkCtrl::kill_dev() to stop the device and let run_target unwind cleanly.
pub async fn start_ublk_server<B>(
    backend: Arc<B>,
    cfg: UblkConfig,
    cancel: CancellationToken,
) -> Result<()>
where
    B: BlockBackend + 'static,
{
    let capacity = backend.size();
    if cfg.logical_block_size == 0 || (cfg.logical_block_size & (cfg.logical_block_size - 1)) != 0 {
        anyhow::bail!("logical_block_size must be a non-zero power of two");
    }
    let lbs_shift: u8 = cfg.logical_block_size.trailing_zeros() as u8;

    // Cooperative shutdown: forward CancellationToken into blocking thread via mpsc
    let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>();
    {
        let token = cancel.clone();
        let tx = shutdown_tx.clone();
        tokio::spawn(async move {
            token.cancelled().await;
            let _ = tx.send(());
        });
    }

    // Run libublk control/IO path on a blocking thread
    tokio::task::spawn_blocking(move || -> Result<()> {
        // 1) Create control device
        let nrq: u16 = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .min(8) as u16;
        log::info!("ublk: using {} queue(s)", nrq);

        let ctrl = std::sync::Arc::new(
            UblkCtrlBuilder::default()
                .name("vram")
                .nr_queues(nrq)
                .dev_flags(UblkFlags::UBLK_DEV_F_ADD_DEV)
                .build()
                .context("failed to build UblkCtrl")?,
        );

        // Shutdown waiter: on cancel, kill device (preferred; avoids deadlocks) and return
        let ctrl_shutdown = ctrl.clone();
        let shutdown_thread = std::thread::spawn(move || {
            let _ = shutdown_rx.recv();
            log::info!("ublk: shutdown requested, killing ublk device");
            if let Err(e) = ctrl_shutdown.kill_dev() {
                log::warn!("ublk: kill_dev failed: {:?}", e);
            } else {
                log::info!("ublk: kill_dev issued");
            }
            // Do not call std::process::exit(0); allow run_target to unwind cleanly
        });

        // 2) Start the ublk target with init, per-queue IO handler, and post-start dump
        let backend_arc = backend.clone();

        ctrl.run_target(
            // Init: set device params (size and logical block size)
            move |dev: &mut UblkDev| {
                dev.set_default_params(capacity);
                // Set capacity explicitly in sectors (512B)
                dev.tgt.params.basic.dev_sectors = (capacity >> 9) as u64;
                // Override logical/physical/io hints to requested block size
                dev.tgt.params.basic.logical_bs_shift = lbs_shift;
                dev.tgt.params.basic.physical_bs_shift = lbs_shift.max(12); // 4K or higher
                dev.tgt.params.basic.io_min_shift = lbs_shift;
                dev.tgt.params.basic.io_opt_shift = lbs_shift;
                Ok(())
            },
            // Per-queue IO handler
            move |qid: u16, dev: &UblkDev| {
                // Each queue runs in its own thread context
                let q = UblkQueue::new(qid, dev).expect("Failed to create UblkQueue");
                // Allocate one IoBuf per tag (depth)
                let bufs = dev.alloc_queue_io_bufs();

                // Register buffers when not using AUTO_BUF_REG and submit initial FETCH commands
                let q = q.regiser_io_bufs(Some(&bufs)).submit_fetch_commands(Some(&bufs));

                // Share state with closure
                let backend = backend_arc.clone();

                // IO loop: handle incoming CQEs
                q.wait_and_handle_io(|q: &UblkQueue, tag: u16, _ctx: &UblkIOCtx| {
                    let iod = q.get_iod(tag);
                    let op = (iod.op_flags & 0xff) as u32; // op code is low bits
                    let offset = (iod.start_sector as u64) << 9;
                    let mut len = (iod.nr_sectors as usize) << 9;

                    // Bound by device capacity
                    let cap = backend.size();
                    if offset > cap {
                        // Past-end request: fail
                        q.complete_io_cmd(tag, std::ptr::null_mut(), Err(UblkError::OtherError(-libc::EINVAL)));
                        return;
                    }
                    if offset + len as u64 > cap {
                        len = (cap - offset) as usize;
                    }

                    // Bound by IO buffer size
                    let max_io_buf = q.dev.dev_info.max_io_buf_bytes as usize;
                    if len > max_io_buf {
                        len = max_io_buf;
                    }

                    log::debug!(
                        "ublk io: tag={} op=0x{:x} start_sector={} nr_sectors={} offset={} len={} cap={} max_io_buf={}",
                        tag, op, iod.start_sector, iod.nr_sectors, offset, len, cap, max_io_buf
                    );

                    let buf = &bufs[tag as usize];
                    match op {
                        // READ: fill buffer from backend, then complete OK(len)
                        x if x == sys::UBLK_IO_OP_READ => {
                            let dst = unsafe { std::slice::from_raw_parts_mut(buf.as_mut_ptr(), len) };
                            match backend.read_at(offset, dst) {
                                Ok(()) => {
                                    q.complete_io_cmd(tag, buf.as_mut_ptr(), Ok(UblkIORes::Result(len as i32)));
                                }
                                Err(_) => {
                                    q.complete_io_cmd(tag, buf.as_mut_ptr(), Err(UblkError::OtherError(-libc::EIO)));
                                }
                            }
                        }
                        // WRITE: write from buffer into backend, then complete OK(len)
                        x if x == sys::UBLK_IO_OP_WRITE => {
                            let src = unsafe { std::slice::from_raw_parts(buf.as_mut_ptr(), len) };
                            match backend.write_at(offset, src) {
                                Ok(()) => {
                                    q.complete_io_cmd(tag, buf.as_mut_ptr(), Ok(UblkIORes::Result(len as i32)));
                                }
                                Err(_) => {
                                    q.complete_io_cmd(tag, buf.as_mut_ptr(), Err(UblkError::OtherError(-libc::EIO)));
                                }
                            }
                        }
                        // FLUSH: volatile backend; report success
                        x if x == sys::UBLK_IO_OP_FLUSH => {
                            q.complete_io_cmd(tag, buf.as_mut_ptr(), Ok(UblkIORes::Result(0)));
                        }
                        // Unsupported ops for now
                        x if x == sys::UBLK_IO_OP_DISCARD
                            || x == sys::UBLK_IO_OP_WRITE_ZEROES
                            || x == sys::UBLK_IO_OP_WRITE_SAME =>
                        {
                            q.complete_io_cmd(tag, buf.as_mut_ptr(), Err(UblkError::OtherError(-libc::EOPNOTSUPP)));
                        }
                        // Unknown op
                        _ => {
                            q.complete_io_cmd(tag, buf.as_mut_ptr(), Err(UblkError::OtherError(-libc::EOPNOTSUPP)));
                        }
                    }
                });
            },
            // After device started: optional post-start hook (no-op)
            |_ctrl: &UblkCtrl| {},
        )
        .context("libublk run_target failed")?;

        // Wait for shutdown waiter to finish
        let _ = shutdown_thread.join();
        Ok(())
    })
    .await
    .context("ublk blocking task failed to join")??;

    Ok(())
}
