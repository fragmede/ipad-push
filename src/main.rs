use std::io::SeekFrom;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use clap::Parser;
use indicatif::{ProgressBar, ProgressStyle};
use idevice::IdeviceService;
use idevice::afc::opcode::AfcFopenMode;
use idevice::afc::AfcClient;
use idevice::house_arrest::HouseArrestClient;
use idevice::installation_proxy::InstallationProxyClient;
use idevice::lockdown::LockdownClient;
use idevice::provider::{IdeviceProvider, UsbmuxdProvider};
use idevice::usbmuxd::{UsbmuxdAddr, UsbmuxdConnection};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};

/// Fast file transfer and management for iPad over USB
#[derive(Parser)]
#[command(name = "ipad-push", about = "Fast file transfer to iPad via USB")]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Subcommand)]
enum Command {
    /// Push a file to the iPad
    Push {
        /// Local file to transfer
        file: PathBuf,
        /// Destination path on iPad (default: /Downloads/<filename>)
        #[arg(short, long)]
        dest: Option<String>,
        /// App bundle ID to push into (e.g. com.example.app)
        #[arg(short, long)]
        app: Option<String>,
        /// Number of parallel streams (axel-style chunked transfer)
        #[arg(short = 'n', long, default_value = "4")]
        streams: usize,
        /// Chunk size in MB for each write operation
        #[arg(short, long, default_value = "8")]
        chunk_mb: usize,
    },
    /// List files on the iPad
    Ls {
        /// Path to list (default: /)
        #[arg(default_value = "/")]
        path: String,
        /// App bundle ID to browse (e.g. com.example.app)
        #[arg(short, long)]
        app: Option<String>,
        /// Recursive listing
        #[arg(short, long)]
        recursive: bool,
    },
    /// Delete files on the iPad
    Rm {
        /// Paths to delete
        paths: Vec<String>,
        /// App bundle ID
        #[arg(short, long)]
        app: Option<String>,
        /// Recursive delete
        #[arg(short, long)]
        recursive: bool,
    },
    /// Show device info and free space
    Info,
    /// List installed apps
    Apps {
        /// Filter: "User", "System", or "Any"
        #[arg(short, long, default_value = "User")]
        filter: String,
        /// Search for apps matching a string
        #[arg(short, long)]
        search: Option<String>,
    },
    /// Disk usage breakdown (ncdu-style)
    Du {
        /// Path to analyze (default: /)
        #[arg(default_value = "/")]
        path: String,
        /// Max depth to display
        #[arg(short, long, default_value = "2")]
        depth: usize,
    },
}

/// Connect to AFC service on the first USB device found
async fn connect_afc(provider: &UsbmuxdProvider) -> Result<AfcClient> {
    let mut lockdown = LockdownClient::connect(provider).await?;
    let pairing = provider.get_pairing_file().await?;
    lockdown.start_session(&pairing).await?;
    let (port, ssl) = lockdown.start_service("com.apple.afc").await?;
    let mut idevice = provider.connect(port).await?;
    if ssl {
        idevice.start_session(&pairing, false).await?;
    }
    Ok(AfcClient::new(idevice))
}

/// Connect to AFC service scoped to an app's Documents directory
async fn connect_afc_app(provider: &UsbmuxdProvider, bundle_id: &str) -> Result<AfcClient> {
    let mut lockdown = LockdownClient::connect(provider).await?;
    let pairing = provider.get_pairing_file().await?;
    lockdown.start_session(&pairing).await?;
    let (port, ssl) = lockdown.start_service("com.apple.mobile.house_arrest").await?;
    let mut idevice = provider.connect(port).await?;
    if ssl {
        idevice.start_session(&pairing, false).await?;
    }
    let ha = HouseArrestClient::new(idevice);
    let afc = ha.vend_documents(bundle_id).await
        .with_context(|| format!("Failed to access app documents for {bundle_id}"))?;
    Ok(afc)
}

/// Connect to AFC — either global or app-scoped
async fn get_afc(provider: &UsbmuxdProvider, app: Option<&str>) -> Result<AfcClient> {
    match app {
        Some(bundle_id) => connect_afc_app(provider, bundle_id).await,
        None => connect_afc(provider).await,
    }
}

/// Ensure parent directories exist on device
async fn ensure_dir(afc: &mut AfcClient, path: &str) -> Result<()> {
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    let mut current = String::new();
    for part in &parts[..parts.len().saturating_sub(1)] {
        current.push('/');
        current.push_str(part);
        let _ = afc.mk_dir(&current).await;
    }
    Ok(())
}

/// Single-stream transfer with optional app scope
async fn transfer_single_app(
    provider: &UsbmuxdProvider,
    app: Option<&str>,
    data: &[u8],
    dest: &str,
    chunk_size: usize,
    pb: &ProgressBar,
) -> Result<()> {
    let mut afc = get_afc(provider, app).await?;
    ensure_dir(&mut afc, dest).await?;

    let mut file = afc.open(dest, AfcFopenMode::WrOnly).await?;

    for chunk in data.chunks(chunk_size) {
        file.write_all(chunk).await?;
        pb.inc(chunk.len() as u64);
    }

    file.flush().await?;
    file.close().await?;
    Ok(())
}

/// Multi-stream parallel transfer (axel-style)
async fn transfer_parallel(
    provider: &UsbmuxdProvider,
    data: &[u8],
    dest: &str,
    streams: usize,
    chunk_size: usize,
    pb: &ProgressBar,
) -> Result<()> {
    let total = data.len();
    let stream_chunk = total / streams;

    // Stream 0 creates the file
    {
        let mut afc = connect_afc(provider).await?;
        ensure_dir(&mut afc, dest).await?;
        let file = afc.open(dest, AfcFopenMode::WrOnly).await?;
        file.close().await?;
    }

    let bytes_written = Arc::new(AtomicU64::new(0));

    let mut handles = Vec::new();
    for i in 0..streams {
        let offset = i * stream_chunk;
        let end = if i == streams - 1 { total } else { (i + 1) * stream_chunk };
        let len = end - offset;

        let provider = provider.clone();
        let dest = dest.to_string();
        let bytes_written = bytes_written.clone();
        let chunk_data = data[offset..end].to_vec();

        let handle = tokio::spawn(async move {
            let mut afc = connect_afc(&provider).await?;
            let mut file = afc.open(&dest, AfcFopenMode::Rw).await?;

            file.seek(SeekFrom::Start(offset as u64)).await?;

            let mut written = 0;
            while written < len {
                let sub_end = (written + chunk_size).min(len);
                let sub_chunk = &chunk_data[written..sub_end];
                file.write_all(sub_chunk).await?;
                written += sub_chunk.len();
                bytes_written.fetch_add(sub_chunk.len() as u64, Ordering::Relaxed);
            }

            file.flush().await?;
            file.close().await?;
            Ok::<_, anyhow::Error>(())
        });
        handles.push(handle);
    }

    // Progress updater
    let pb_clone = pb.clone();
    let bytes_written_clone = bytes_written.clone();
    let total_u64 = total as u64;
    let progress_handle = tokio::spawn(async move {
        loop {
            let current = bytes_written_clone.load(Ordering::Relaxed);
            pb_clone.set_position(current);
            if current >= total_u64 {
                break;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        }
    });

    let mut errors = Vec::new();
    for (i, handle) in handles.into_iter().enumerate() {
        match handle.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => errors.push(format!("stream {i}: {e}")),
            Err(e) => errors.push(format!("stream {i} panicked: {e}")),
        }
    }

    progress_handle.abort();
    pb.set_position(total as u64);

    if !errors.is_empty() {
        bail!("Transfer errors:\n{}", errors.join("\n"));
    }

    Ok(())
}

async fn get_provider() -> Result<UsbmuxdProvider> {
    let addr = UsbmuxdAddr::default();
    let mut conn = UsbmuxdConnection::default().await
        .context("Cannot connect to usbmuxd. Is the iPad plugged in?")?;
    let devices = conn.get_devices().await?;
    let device = devices
        .into_iter()
        .find(|d| d.connection_type == idevice::usbmuxd::Connection::Usb)
        .context("No USB device found. Is the iPad plugged in and trusted?")?;
    Ok(device.to_provider(addr, "ipad-push"))
}

fn format_size(bytes: usize) -> String {
    if bytes >= 1_073_741_824 {
        format!("{:.1} GB", bytes as f64 / 1_073_741_824.0)
    } else if bytes >= 1_048_576 {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{} B", bytes)
    }
}

async fn cmd_ls(provider: &UsbmuxdProvider, path: &str, app: Option<&str>, recursive: bool) -> Result<()> {
    let mut afc = get_afc(provider, app).await?;
    ls_inner(&mut afc, path, recursive, 0).await
}

#[async_recursion::async_recursion]
async fn ls_inner(afc: &mut AfcClient, path: &str, recursive: bool, depth: usize) -> Result<()> {
    let entries = afc.list_dir(path).await?;
    for entry in &entries {
        if entry == "." || entry == ".." {
            continue;
        }
        let full_path = if path == "/" {
            format!("/{entry}")
        } else {
            format!("{path}/{entry}")
        };
        match afc.get_file_info(&full_path).await {
            Ok(info) => {
                let indent = "  ".repeat(depth);
                if info.st_ifmt == "S_IFDIR" {
                    println!("{indent}{entry}/");
                    if recursive {
                        ls_inner(afc, &full_path, true, depth + 1).await?;
                    }
                } else {
                    println!("{indent}{entry}  ({})", format_size(info.size));
                }
            }
            Err(_) => {
                let indent = "  ".repeat(depth);
                println!("{indent}{entry}  [error reading info]");
            }
        }
    }
    Ok(())
}

async fn cmd_rm(provider: &UsbmuxdProvider, paths: &[String], app: Option<&str>, recursive: bool) -> Result<()> {
    let mut afc = get_afc(provider, app).await?;
    for path in paths {
        if recursive {
            afc.remove_all(path).await
                .with_context(|| format!("Failed to remove {path}"))?;
        } else {
            afc.remove(path).await
                .with_context(|| format!("Failed to remove {path}"))?;
        }
        eprintln!("Deleted: {path}");
    }
    Ok(())
}

async fn cmd_apps(provider: &UsbmuxdProvider, filter: &str, search: Option<&str>) -> Result<()> {
    let mut lockdown = LockdownClient::connect(provider).await?;
    let pairing = provider.get_pairing_file().await?;
    lockdown.start_session(&pairing).await?;
    let (port, ssl) = lockdown.start_service("com.apple.mobile.installation_proxy").await?;
    let mut idevice = provider.connect(port).await?;
    if ssl {
        idevice.start_session(&pairing, false).await?;
    }
    let mut client = InstallationProxyClient::new(idevice);
    let apps = client.get_apps(Some(filter), None).await?;

    let mut entries: Vec<(String, String)> = apps
        .iter()
        .filter_map(|(bundle_id, info)| {
            let name = info
                .as_dictionary()
                .and_then(|d| d.get("CFBundleDisplayName").or(d.get("CFBundleName")))
                .and_then(|v| v.as_string())
                .unwrap_or("?")
                .to_string();
            if let Some(q) = search {
                let q_lower = q.to_lowercase();
                if !name.to_lowercase().contains(&q_lower)
                    && !bundle_id.to_lowercase().contains(&q_lower)
                {
                    return None;
                }
            }
            Some((name, bundle_id.clone()))
        })
        .collect();

    entries.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));

    for (name, bundle_id) in &entries {
        // Show file sharing status if searching
        if search.is_some() {
            if let Some(info) = apps.get(bundle_id).and_then(|v| v.as_dictionary()) {
                let sharing = info
                    .get("UIFileSharingEnabled")
                    .and_then(|v| v.as_boolean())
                    .unwrap_or(false);
                let doc_browser = info
                    .get("UISupportsDocumentBrowser")
                    .and_then(|v| v.as_boolean())
                    .unwrap_or(false);
                let flags = match (sharing, doc_browser) {
                    (true, true) => " [file-sharing, doc-browser]",
                    (true, false) => " [file-sharing]",
                    (false, true) => " [doc-browser]",
                    (false, false) => "",
                };
                println!("{:<30} {}{}", name, bundle_id, flags);
            } else {
                println!("{:<30} {}", name, bundle_id);
            }
        } else {
            println!("{:<30} {}", name, bundle_id);
        }
    }
    eprintln!("\n{} apps", entries.len());
    Ok(())
}

async fn cmd_info(provider: &UsbmuxdProvider) -> Result<()> {
    let mut afc = connect_afc(provider).await?;
    let info = afc.get_device_info().await?;
    println!("Model:      {}", info.model);
    println!("Total:      {}", format_size(info.total_bytes));
    println!("Free:       {}", format_size(info.free_bytes));
    println!("Block size: {}", format_size(info.block_size));
    Ok(())
}

struct DuEntry {
    path: String,
    size: u64,
}

#[async_recursion::async_recursion]
async fn du_walk(afc: &mut AfcClient, path: &str) -> Result<u64> {
    let entries = match afc.list_dir(path).await {
        Ok(e) => e,
        Err(_) => return Ok(0),
    };
    let mut total: u64 = 0;
    for entry in &entries {
        if entry == "." || entry == ".." {
            continue;
        }
        let full_path = if path == "/" {
            format!("/{entry}")
        } else {
            format!("{path}/{entry}")
        };
        match afc.get_file_info(&full_path).await {
            Ok(info) => {
                if info.st_ifmt == "S_IFDIR" {
                    total += du_walk(afc, &full_path).await?;
                } else {
                    total += info.size as u64;
                }
            }
            Err(_) => {}
        }
    }
    Ok(total)
}

async fn cmd_du(provider: &UsbmuxdProvider, path: &str, max_depth: usize) -> Result<()> {
    let mut afc = connect_afc(provider).await?;
    let info = afc.get_device_info().await?;
    let total_bytes = info.total_bytes as u64;
    let free_bytes = info.free_bytes as u64;
    let used_bytes = total_bytes - free_bytes;

    eprintln!("Scanning {}...", path);

    let entries = afc.list_dir(path).await?;
    let mut results: Vec<DuEntry> = Vec::new();

    for entry in &entries {
        if entry == "." || entry == ".." {
            continue;
        }
        let full_path = if path == "/" {
            format!("/{entry}")
        } else {
            format!("{path}/{entry}")
        };
        match afc.get_file_info(&full_path).await {
            Ok(info) => {
                let size = if info.st_ifmt == "S_IFDIR" {
                    du_walk(&mut afc, &full_path).await?
                } else {
                    info.size as u64
                };
                results.push(DuEntry { path: full_path, size });
            }
            Err(_) => {}
        }
    }

    results.sort_by(|a, b| b.size.cmp(&a.size));

    // Print header
    println!("{:>10}  {:<6}  {}", "SIZE", "%USED", "PATH");
    println!("{:>10}  {:<6}  {}", "----", "-----", "----");

    for entry in &results {
        let pct = if used_bytes > 0 {
            (entry.size as f64 / used_bytes as f64) * 100.0
        } else {
            0.0
        };
        let bar_width = 20;
        let filled = ((pct / 100.0) * bar_width as f64) as usize;
        let bar: String = "#".repeat(filled) + &"-".repeat(bar_width - filled);
        println!(
            "{:>10}  {:>5.1}%  [{}] {}",
            format_size(entry.size as usize),
            pct,
            bar,
            entry.path,
        );
    }

    println!();
    println!("Total used: {} / {} ({:.1}% full, {} free)",
        format_size(used_bytes as usize),
        format_size(total_bytes as usize),
        (used_bytes as f64 / total_bytes as f64) * 100.0,
        format_size(free_bytes as usize),
    );

    // Recurse into top dirs if depth > 1
    if max_depth > 1 {
        for entry in &results {
            if entry.size == 0 {
                continue;
            }
            // Check if it's a dir
            if let Ok(info) = afc.get_file_info(&entry.path).await {
                if info.st_ifmt == "S_IFDIR" {
                    println!("\n--- {} ({}) ---", entry.path, format_size(entry.size as usize));
                    let sub_entries = match afc.list_dir(&entry.path).await {
                        Ok(e) => e,
                        Err(_) => continue,
                    };
                    let mut sub_results: Vec<DuEntry> = Vec::new();
                    for sub in &sub_entries {
                        if sub == "." || sub == ".." {
                            continue;
                        }
                        let sub_path = format!("{}/{sub}", entry.path);
                        match afc.get_file_info(&sub_path).await {
                            Ok(si) => {
                                let size = if si.st_ifmt == "S_IFDIR" {
                                    du_walk(&mut afc, &sub_path).await?
                                } else {
                                    si.size as u64
                                };
                                if size > 0 {
                                    sub_results.push(DuEntry { path: sub_path, size });
                                }
                            }
                            Err(_) => {}
                        }
                    }
                    sub_results.sort_by(|a, b| b.size.cmp(&a.size));
                    for sub in &sub_results {
                        println!("  {:>10}  {}", format_size(sub.size as usize), sub.path);
                    }
                }
            }
        }
    }

    Ok(())
}

async fn cmd_push(
    provider: &UsbmuxdProvider,
    file: &PathBuf,
    dest: Option<&String>,
    app: Option<&str>,
    streams: usize,
    chunk_mb: usize,
) -> Result<()> {
    let data = std::fs::read(file)
        .with_context(|| format!("Failed to read {}", file.display()))?;
    let file_size = data.len() as u64;
    let filename = file.file_name().context("No filename")?.to_string_lossy();

    let dest = match dest {
        Some(d) => d.clone(),
        None if app.is_some() => format!("/Documents/{}", filename),
        None => format!("/Downloads/{}", filename),
    };

    let chunk_size = chunk_mb * 1024 * 1024;

    {
        // Space check uses global AFC (app-scoped doesn't support get_device_info)
        let mut afc = connect_afc(provider).await?;
        let info = afc.get_device_info().await?;
        let target = if let Some(a) = app {
            format!("{} ({})", info.model, a)
        } else {
            info.model.clone()
        };
        eprintln!(
            "Device: {} | Free: {:.1} GB",
            target,
            info.free_bytes as f64 / 1_073_741_824.0
        );
        if file_size > info.free_bytes as u64 {
            bail!(
                "Not enough space: file is {:.1} MB but only {:.1} MB free",
                file_size as f64 / 1_048_576.0,
                info.free_bytes as f64 / 1_048_576.0
            );
        }
    }

    let pb = ProgressBar::new(file_size);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("{spinner:.green} [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}) ETA {eta}")
            .unwrap()
            .progress_chars("=>-"),
    );

    eprintln!(
        "Pushing {} ({:.1} MB) -> {} [{}x streams, {} MB chunks]",
        filename,
        file_size as f64 / 1_048_576.0,
        dest,
        streams,
        chunk_mb,
    );

    let start = Instant::now();

    // For app-scoped transfers, use single stream (each house_arrest connection is separate)
    if app.is_some() || streams <= 1 {
        transfer_single_app(provider, app, &data, &dest, chunk_size, &pb).await?;
    } else {
        transfer_parallel(provider, &data, &dest, streams, chunk_size, &pb).await?;
    }

    pb.finish_and_clear();
    let elapsed = start.elapsed();
    let mb_per_sec = (file_size as f64 / 1_048_576.0) / elapsed.as_secs_f64();
    eprintln!(
        "Done: {:.1} MB in {:.1}s ({:.1} MB/s)",
        file_size as f64 / 1_048_576.0,
        elapsed.as_secs_f64(),
        mb_per_sec,
    );

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let provider = get_provider().await?;

    match &args.command {
        Command::Push { file, dest, app, streams, chunk_mb } => {
            cmd_push(&provider, file, dest.as_ref(), app.as_deref(), *streams, *chunk_mb).await
        }
        Command::Ls { path, app, recursive } => {
            cmd_ls(&provider, path, app.as_deref(), *recursive).await
        }
        Command::Rm { paths, app, recursive } => {
            cmd_rm(&provider, paths, app.as_deref(), *recursive).await
        }
        Command::Info => {
            cmd_info(&provider).await
        }
        Command::Apps { filter, search } => {
            cmd_apps(&provider, filter, search.as_deref()).await
        }
        Command::Du { path, depth } => {
            cmd_du(&provider, path, *depth).await
        }
    }
}
