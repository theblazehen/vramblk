//! NBD server implementation using the `nbd` crate v0.3.1.

use crate::opencl::VRamBuffer;
use anyhow::{Context, Result};
use nbd;
use nbd::Export;
use std::io::{Error as IoError, ErrorKind, Read, Result as IoResult, Seek, SeekFrom, Write};
use std::net::{SocketAddr, TcpStream as StdTcpStream};
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::signal;
use tokio::task;

/// Configuration for the NBD server
#[derive(Debug, Clone)]
pub struct NbdConfig {
    /// Socket address to listen on (e.g., "127.0.0.1:10809")
    pub listen_addr: String,
    /// Export name advertised to clients (used during handshake)
    pub export_name: String,
}

impl Default for NbdConfig {
    fn default() -> Self {
        Self {
            listen_addr: "127.0.0.1:10809".to_string(),
            export_name: "vram".to_string(),
        }
    }
}

// --- Wrapper struct implementing Read/Write/Seek for VRamBuffer ---
struct VramSeeker {
    buffer: Arc<VRamBuffer>,
    pos: u64,
    size: u64,
}

impl VramSeeker {
    fn new(buffer: Arc<VRamBuffer>) -> Self {
        let size = buffer.size() as u64;
        VramSeeker {
            buffer,
            pos: 0,
            size,
        }
    }
}

impl Read for VramSeeker {
    fn read(&mut self, buf: &mut [u8]) -> IoResult<usize> {
        let remaining = self.size.saturating_sub(self.pos);
        if remaining == 0 {
            return Ok(0);
        }

        let read_len = std::cmp::min(buf.len() as u64, remaining) as usize;
        let read_buf = &mut buf[..read_len];

        match self.buffer.read(self.pos as usize, read_buf) {
            Ok(_) => {
                self.pos += read_len as u64;
                log::trace!("VramSeeker read {} bytes, new pos {}", read_len, self.pos);
                Ok(read_len)
            }
            Err(e) => {
                log::error!("VRAM read error during NBD Read: {}", e);
                Err(IoError::new(ErrorKind::Other, "VRAM read failed"))
            }
        }
    }
}

impl Write for VramSeeker {
    fn write(&mut self, buf: &[u8]) -> IoResult<usize> {
        let remaining = self.size.saturating_sub(self.pos);
        if remaining == 0 {
            return Err(IoError::new(
                ErrorKind::WriteZero,
                "Write past end of VRAM buffer",
            ));
        }

        let write_len = std::cmp::min(buf.len() as u64, remaining) as usize;
        if write_len == 0 {
            return Ok(0);
        }
        let write_buf = &buf[..write_len];

        match self.buffer.write(self.pos as usize, write_buf) {
            Ok(_) => {
                self.pos += write_len as u64;
                log::trace!("VramSeeker wrote {} bytes, new pos {}", write_len, self.pos);
                Ok(write_len)
            }
            Err(e) => {
                log::error!("VRAM write error during NBD Write: {}", e);
                Err(IoError::new(ErrorKind::Other, "VRAM write failed"))
            }
        }
    }

    fn flush(&mut self) -> IoResult<()> {
        log::trace!("VramSeeker flush");
        Ok(())
    }
}

impl Seek for VramSeeker {
    fn seek(&mut self, style: SeekFrom) -> IoResult<u64> {
        let (base_pos, offset) = match style {
            SeekFrom::Start(n) => {
                self.pos = n;
                log::trace!("VramSeeker seek to Start({}), new pos {}", n, self.pos);
                return Ok(n);
            }
            SeekFrom::End(n) => (self.size, n),
            SeekFrom::Current(n) => (self.pos, n),
        };
        let new_pos = if offset >= 0 {
            base_pos.checked_add(offset as u64)
        } else {
            base_pos.checked_sub((offset.wrapping_neg()) as u64)
        };
        match new_pos {
            Some(n) => {
                self.pos = n;
                log::trace!("VramSeeker seek relative({}), new pos {}", offset, self.pos);
                Ok(self.pos)
            }
            None => Err(IoError::new(
                ErrorKind::InvalidInput,
                "invalid seek to a negative or overflowing position",
            )),
        }
    }
}

pub async fn start_nbd_server(buffer: Arc<VRamBuffer>, config: &NbdConfig) -> Result<()> {
    let addr: SocketAddr = config
        .listen_addr
        .parse()
        .with_context(|| format!("Invalid listen address: {}", config.listen_addr))?;

    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("Failed to bind TCP listener to {}", addr))?;

    log::info!("NBD server listening on {}", addr);
    log::info!(
        "Waiting for connections for export '{}' (size: {} bytes)",
        config.export_name,
        buffer.size()
    );

    loop {
        tokio::select! {
            Ok((stream, client_addr)) = listener.accept() => {
                log::info!("NBD client connected: {}", client_addr);

                let buffer_clone = buffer.clone();
                let config_clone = config.clone();

                // Spawn a blocking task to handle the synchronous nbd crate logic
                task::spawn_blocking(move || {
                    match stream.into_std() {
                        Ok(std_stream) => {
                             if let Err(e) = std_stream.set_nonblocking(false) {
                                 log::error!("Failed to set stream to blocking for {}: {}", client_addr, e);
                                 return;
                             }
                             log::info!("Handling client {} in blocking task...", client_addr);
                             if let Err(e) = handle_connection(std_stream, buffer_clone, config_clone) {
                                 if e.downcast_ref::<IoError>().map_or(true, |ioe| ioe.kind() != ErrorKind::BrokenPipe) {
                                     log::error!("Client {} error: {:?}", client_addr, e);
                                 }
                             }
                             log::info!("Client {} disconnected.", client_addr);
                        }
                        Err(e) => {
                            log::error!("Failed to convert Tokio stream to std stream for {}: {}", client_addr, e);
                        }
                    }
                });
            }
            _ = signal::ctrl_c() => {
                log::info!("Ctrl-C received, shutting down NBD server.");
                break;
            }
            else => {
                 log::error!("NBD listener accept error.");
                 break;
            }
        }
    }

    log::info!("NBD server loop finished.");
    Ok(())
}

fn handle_connection(
    mut stream: StdTcpStream,
    buffer: Arc<VRamBuffer>,
    config: NbdConfig,
) -> Result<()> {
    let _export_data = nbd::server::handshake(&mut stream, |name| {
        if name == config.export_name {
            Ok(Export {
                size: buffer.size() as u64,
                readonly: false,
                send_flush: true,
                resizeable: false,
                rotational: false,
                send_trim: false,
                data: (),
            })
        } else {
            log::warn!("Client requested unknown export: {}", name);
            Err(IoError::new(ErrorKind::NotFound, "Export not found"))
        }
    })
    .context("NBD handshake failed")?;

    log::info!("Handshake successful for export '{}'", config.export_name);

    let vram_seeker = VramSeeker::new(buffer);
    nbd::server::transmission(&mut stream, vram_seeker).context("NBD transmission phase failed")?;

    Ok(())
}
