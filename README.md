# VRAM Block Device

A Rust application that uses OpenCL to allocate GPU memory and exposes it as a block device using either an NBD server or a ublk userspace block device (libublk).

---

## Installation

### From Source

```bash
git clone https://github.com/theblazehen/vramblk.git
cd vramblk
cargo build --release
```

The executable will be at `target/release/vramblk`.

### From Crates.io

```bash
cargo install vramblk
```

---

## Requirements

- Rust toolchain (cargo, rustc)
- OpenCL runtime and development libraries
- A compatible GPU with OpenCL support
- For NBD: `nbd-client` utility (for connecting the kernel NBD module to the server)
- For ublk: Linux kernel 6.0+ with the ublk driver (module: `ublk_drv`) available
  - Load the driver if needed: `sudo modprobe ublk_drv`
  - Access to `/dev/ublk-control` (root or suitable udev rules for unprivileged mode). See libublk README for example udev rules.

### On Ubuntu/Debian:

```bash
sudo apt update
sudo apt install ocl-icd-opencl-dev opencl-headers nbd-client build-essential
```

### On Fedora/CentOS:

```bash
sudo dnf install ocl-icd-devel opencl-headers nbd-client gcc make
```

---

## Usage

The application does not strictly require root privileges. However, if you intend to use VRAM as swap, running as root (or with the appropriate capabilities) is strongly recommended to allow locking memory with `mlockall(2)`, and for `nbd-client` operations. If you do not run as root, you can grant the necessary capability with:

```bash
sudo setcap cap_ipc_lock=eip ./target/release/vramblk
```

This allows the process to lock memory without full root privileges.

### List Available OpenCL Devices

```bash
./target/release/vramblk --list-devices
```

### Start the Server

```bash
sudo ./target/release/vramblk [OPTIONS]
```

The server will attempt to lock its memory using `mlockall` and then run in the foreground, listening on the specified address. Locking memory with `mlockall` ensures the server process is never swapped out, which is critical for swap usage. Check the log output for success or failure of `mlockall`.

### Connect the NBD Device (in another terminal)

You need the `nbd-client` utility for this step.

```bash
# Example connecting /dev/nbd0 to the server running on localhost:10809
sudo nbd-client localhost 10809 /dev/nbd0 -N vram
```

Replace `localhost:10809` with the listen address if you changed it, `/dev/nbd0` with the desired device, and `vram` with the export name if changed.

---

## Using as Swap


1. **Start the server as root, or grant `CAP_IPC_LOCK` capability:**

   ```bash
   # Option 1: Run as root
   sudo ./target/release/vramblk --size 2G

   # Option 2: Grant CAP_IPC_LOCK and run as regular user
   sudo setcap cap_ipc_lock=eip ./target/release/vramblk
   ./target/release/vramblk --size 2G
   ```

2. **Connect the NBD device for swap:**

   ```bash
   sudo nbd-client localhost 10809 /dev/nbd0 -N vram -swap -C 1
   ```

   - `-swap` tells `nbd-client` to optimize for swap usage.
   - `-C 1` sets the number of connections to 1 (recommended for swap).

3. **Mark the device as swap and enable:**

   ```bash
   sudo mkswap /dev/nbd0
   sudo swapon /dev/nbd0
   ```

4. **Check swap status:**

   ```bash
   swapon --show
   ```

5. **To disable swap and disconnect:**

   ```bash
   sudo swapoff /dev/nbd0
   sudo nbd-client -d /dev/nbd0
   ```

**Important:**  
- The server process must not be swapped out. If `mlockall` fails, swap usage is unsafe. `mlockall` locks the server's memory into RAM, preventing it from being swapped out, which is essential for swap reliability.
- The `nbd-client` process itself should also be protected from swapping (consider running it as a systemd service with `MemoryDenyWriteExecute=no` and `LimitMEMLOCK=infinity`).
- Data in GPU VRAM is volatile and will be lost if the server or GPU resets.

---

## Options

- `-s, --size <SIZE>`: Size of the block device (accepts suffixes: e.g., `512M`, `2G`, default: `2048M`)
- `-d, --device <DEVICE>`: GPU device index to use (default: 0)
- `-p, --platform <PLATFORM>`: OpenCL platform index (default: 0)
- `-l, --listen-addr <LISTEN_ADDR>`: Listen address for the NBD server (default: "127.0.0.1:10809")
- `-e, --export-name <EXPORT_NAME>`: Export name advertised over NBD (default: "vram")
- `-v, --verbose`: Enable verbose logging
- `--list-devices`: List available OpenCL platforms and devices and exit
- `--driver <DRIVER>`: Frontend driver to use: `nbd` or `ublk` (default: `nbd`)
- `-h, --help`: Print help information
- `-V, --version`: Print version information

---

### Example

```bash
# Start server for 4GB device on GPU 0, listening on default address
sudo ./target/release/vramblk --size 4G --device 0

# In another terminal: Connect nbd-client
sudo nbd-client localhost 10809 /dev/nbd0 -N vram
```

---

## How It Works

1.  The `vramblk` executable parses arguments and initializes logging.
2.  It calls `mlockall(MCL_CURRENT | MCL_FUTURE)` to lock its current and future memory pages into RAM, preventing swap-out.
3.  It initializes OpenCL and allocates a buffer in GPU memory (`VRamBuffer`).
4.  If `--driver nbd` (default):
    *   Start a Tokio TCP listener and accept clients.
    *   Perform the NBD handshake using `nbd::server::handshake`.
    *   Wrap `VRamBuffer` in a `VramSeeker` implementing `std::io::{Read, Write, Seek}`.
    *   Run the NBD transmission loop using `nbd::server::transmission`.
5.  If `--driver ublk`:
    *   Create a ublk device with libublk, set parameters (capacity from `VRamBuffer::size()`, logical block size default 4096).
    *   Run per-queue io_uring loop and map requests:
        - READ: copy into libublk IO buffer from `VRamBuffer::read()`
        - WRITE: copy from libublk IO buffer via `VRamBuffer::write()`
        - FLUSH: succeed (VRAM is volatile)
        - DISCARD/WRITE_ZEROES: currently EOPNOTSUPP
6.  The server runs until `Ctrl+C` is received.

---

## Limitations

- Performance is limited by PCI-Express bandwidth, OpenCL overhead, and the NBD/TCP stack.
- Maximum size is limited by available GPU memory.
- Not recommended for critical data (no persistence).
- Requires `nbd-client` to be installed separately.
- Requires root privileges for the server (`mlockall`, OpenCL) and `nbd-client`.
- `mlockall` might fail if limits (`ulimit -l`) are too low or user lacks privileges.
- Preventing `nbd-client` from swapping is not handled by this application.

---

## License

MIT

---

## Contributing

Contributions are welcome! Please feel free to submit a Pull Request.