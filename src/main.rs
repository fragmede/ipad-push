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
use idevice::lockdown::LockdownClient;
use idevice::provider::{IdeviceProvider, UsbmuxdProvider};
use idevice::usbmuxd::{UsbmuxdAddr, UsbmuxdConnection};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};

/// Turbo file transfer to iPad over USB
#[derive(Parser)]
#[command(name = "ipad-push", about = "Fast file transfer to iPad via USB")]
struct Args {
    /// Local file to transfer
    file: PathBuf,

    /// Destination path on iPad (default: /Downloads/<filename>)
    #[arg(short, long)]
    dest: Option<String>,

    /// Number of parallel streams (axel-style chunked transfer)
    #[arg(short = 'n', long, default_value = "4")]
    streams: usize,

    /// Chunk size in MB for each write operation
    #[arg(short, long, default_value = "8")]
    chunk_mb: usize,
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

/// Single-stream transfer
async fn transfer_single(
    provider: &UsbmuxdProvider,
    data: &[u8],
    dest: &str,
    chunk_size: usize,
    pb: &ProgressBar,
) -> Result<()> {
    let mut afc = connect_afc(provider).await?;
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

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let data = std::fs::read(&args.file)
        .with_context(|| format!("Failed to read {}", args.file.display()))?;
    let file_size = data.len() as u64;
    let filename = args
        .file
        .file_name()
        .context("No filename")?
        .to_string_lossy();

    let dest = args
        .dest
        .unwrap_or_else(|| format!("/Downloads/{}", filename));

    let chunk_size = args.chunk_mb * 1024 * 1024;

    // Find device
    let addr = UsbmuxdAddr::default();
    let mut conn = UsbmuxdConnection::default().await
        .context("Cannot connect to usbmuxd. Is the iPad plugged in?")?;
    let devices = conn.get_devices().await?;
    let device = devices
        .into_iter()
        .find(|d| d.connection_type == idevice::usbmuxd::Connection::Usb)
        .context("No USB device found. Is the iPad plugged in and trusted?")?;

    let provider = device.to_provider(addr, "ipad-push");

    // Show device info
    {
        let mut afc = connect_afc(&provider).await?;
        let info = afc.get_device_info().await?;
        eprintln!(
            "Device: {} | Free: {:.1} GB",
            info.model,
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
        args.streams,
        args.chunk_mb,
    );

    let start = Instant::now();

    if args.streams <= 1 {
        transfer_single(&provider, &data, &dest, chunk_size, &pb).await?;
    } else {
        transfer_parallel(&provider, &data, &dest, args.streams, chunk_size, &pb).await?;
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
