# ipad-push

Fast file transfer to iPad over USB, bypassing Finder's overhead. Built in Rust using the [idevice](https://crates.io/crates/idevice) crate for direct AFC protocol access.

Achieves ~35-38 MB/s on USB 2.0 (Lightning/older USB-C iPads), compared to Finder's typical 15-25 MB/s.

## Build

```bash
cargo build --release
```

Binary will be at `./target/release/ipad-push`.

## Requirements

- macOS (uses usbmuxd socket at `/var/run/usbmuxd`)
- iPad connected via USB and trusted ("Trust This Computer" accepted on iPad)

## Quick Start

```bash
# Push a file to iPad's Downloads
ipad-push push video.mp4

# Push directly into an app's Documents folder
ipad-push push --app net.hexler.TouchViZ video.mp4

# Check device info and free space
ipad-push info

# List files
ipad-push ls /Downloads

# Disk usage breakdown
ipad-push du
```

## Commands

### push

Transfer a file to the iPad.

```bash
ipad-push push [OPTIONS] <FILE>
```

| Option | Description |
|--------|-------------|
| `-d, --dest <PATH>` | Destination path on iPad (default: `/Downloads/<filename>`) |
| `-a, --app <BUNDLE_ID>` | Push into an app's Documents folder |
| `-n, --streams <N>` | Parallel stream count (default: 4) |
| `-c, --chunk-mb <MB>` | Write chunk size in MB (default: 8) |

### ls

List files on the iPad.

```bash
ipad-push ls [OPTIONS] [PATH]
```

| Option | Description |
|--------|-------------|
| `-a, --app <BUNDLE_ID>` | Browse an app's container |
| `-r, --recursive` | Recursive listing |

**Note:** When using `--app`, the root `/` is the app container. Files are typically under `/Documents/`.

### rm

Delete files from the iPad.

```bash
ipad-push rm [OPTIONS] <PATHS>...
```

| Option | Description |
|--------|-------------|
| `-a, --app <BUNDLE_ID>` | Delete from an app's container |
| `-r, --recursive` | Recursive delete (for directories) |

### info

Show device model and free space.

```bash
ipad-push info
```

### apps

List installed apps and their file-sharing capabilities.

```bash
ipad-push apps [OPTIONS]
```

| Option | Description |
|--------|-------------|
| `-f, --filter <TYPE>` | Filter: "User", "System", or "Any" (default: User) |
| `-s, --search <QUERY>` | Search by app name or bundle ID |

When searching, shows file-sharing flags: `[file-sharing]` and/or `[doc-browser]`.

### du

ncdu-style disk usage breakdown of the iPad's media partition.

```bash
ipad-push du [OPTIONS] [PATH]
```

| Option | Description |
|--------|-------------|
| `-d, --depth <N>` | Max display depth (default: 2) |

## TouchViZ

[TouchViZ](https://hexler.net/touchviz) (bundle ID: `net.hexler.TouchViZ`) supports file sharing. Push media files directly into its Documents folder:

```bash
# Push a video into TouchViZ
ipad-push push --app net.hexler.TouchViZ video.mp4

# List TouchViZ's files
ipad-push ls --app net.hexler.TouchViZ /Documents

# Delete a file from TouchViZ
ipad-push rm --app net.hexler.TouchViZ "/Documents/old_video.mp4"
```

Files pushed this way appear immediately in TouchViZ's media browser.

## How It Works

- Connects to the iPad via usbmuxd (the system daemon that multiplexes USB connections)
- Uses the AFC (Apple File Conduit) protocol to read/write files on the media partition
- For app-specific access, uses the HouseArrest service to access app sandboxes
- Parallel mode opens multiple AFC connections through usbmuxd, each writing a different chunk of the file at a different offset (similar to how [axel](https://github.com/axel-download-accelerator/axel) accelerates downloads)

## Performance Notes

- USB 2.0 iPads (Lightning, iPad mini 6, base iPad 10): ~35-38 MB/s is the hardware ceiling
- USB 3.x iPads (iPad Pro, iPad Air M1+): higher throughput possible, parallel streams more beneficial
- Chunk size has negligible impact on throughput (USB bandwidth is the bottleneck, not protocol overhead)
- Parallel streams give ~8% improvement on USB 2.0; more significant gains expected on USB 3.x
- The main speed advantage over Finder comes from bypassing UI overhead, Spotlight indexing, and thumbnail generation

## Limitations

- **AFC media partition only**: The `du` command and non-app file operations can only see the media partition (DCIM, Downloads, Music, etc.), not apps or system storage
- **App access requires file sharing**: Apps must have `UIFileSharingEnabled` in their Info.plist. Use `ipad-push apps --search <name>` to check
- **App container paths**: When using `--app`, paths are relative to the app container root, not the Documents folder. Use `/Documents/` prefix for the Documents directory
- **macOS only**: Relies on the usbmuxd Unix socket
