#[cfg(not(windows))]
compile_error!("diskcpy currently supports Windows only.");

use clap::Parser;
use std::error::Error;
use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE, GetDiskFreeSpaceExW};
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::System::Ioctl::{GET_LENGTH_INFORMATION, IOCTL_DISK_GET_LENGTH_INFO};
use windows::core::PCWSTR;

type AppResult<T> = Result<T, Box<dyn Error>>;

#[derive(Debug, Parser)]
#[command(
    author,
    version,
    about = "Copy between files and raw Windows disks",
    long_about = "A utility to clone raw eMMC storage devices. To identify physical drives, run this command in PowerShell:\n\n  Get-CimInstance Win32_DiskDrive | Select-Object DeviceID, Model, Size\n\nTo target a physical drive on Windows, use the \\\\.\\PhysicalDriveX format.",
    after_help = "EXAMPLES:\n  Backup Drive:      diskcpy.exe \\\\.\\PhysicalDrive2 backup-emmc.img\n  Manual Truncate:   diskcpy.exe \\\\.\\PhysicalDrive2 backup.img --count 8gb\n  Flash Image Back:  diskcpy.exe backup-emmc.img \\\\.\\PhysicalDrive2"
)]
struct Args {
    /// Source file path or raw device path (e.g. \\\\.\\PhysicalDrive2)
    source: PathBuf,

    /// Destination file path or raw device path
    destination: PathBuf,

    #[arg(long, default_value = "512kb", value_parser = parse_block_size)]
    blocksize: u64,

    /// Stop copying after reaching this limit (e.g. 8gb, or an exact byte count)
    #[arg(long, value_parser = parse_count)]
    count: Option<u64>,

    /// Skip this many bytes from the start of the source before copying
    #[arg(long, default_value = "0", value_parser = parse_skip)]
    skip: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EndpointKind {
    File,
    Device,
}

#[derive(Debug)]
struct Endpoint {
    path: PathBuf,
    kind: EndpointKind,
}

fn main() -> AppResult<()> {
    let args = Args::parse();
    let source = Endpoint::from_path(args.source);
    let destination = Endpoint::from_path(args.destination);

    if args.blocksize == 0 {
        return Err("blocksize must be greater than zero".into());
    }

    if (source.is_device() || destination.is_device()) && args.blocksize % 512 != 0 {
        return Err("blocksize must be a multiple of 512 bytes when reading from or writing to a raw device".into());
    }

    if source.is_device() && args.skip % 512 != 0 {
        return Err("skip must be a multiple of 512 bytes when reading from a raw device".into());
    }

    let source_size = source.size()?;
    let total_bytes = copy_size_after_skip(source_size, args.skip, args.count)?;

    validate_destination_capacity(&destination, total_bytes)?;

    if destination.is_device() && total_bytes % 512 != 0 {
        return Err(
            "source size must be a multiple of 512 bytes when writing to a raw device".into(),
        );
    }

    let block_size =
        usize::try_from(args.blocksize).map_err(|_| "blocksize is too large for this platform")?;

    let mut reader = source.open_for_read()?;
    reader.seek(SeekFrom::Start(args.skip))?;

    let mut writer = destination.open_for_write()?;
    copy_with_progress(&mut reader, &mut writer, total_bytes, block_size)?;
    println!("Copied {}.", format_bytes(total_bytes));

    Ok(())
}

impl Endpoint {
    fn from_path(path: PathBuf) -> Self {
        let kind = if is_device_path(&path) {
            EndpointKind::Device
        } else {
            EndpointKind::File
        };

        Self { path, kind }
    }

    fn is_device(&self) -> bool {
        self.kind == EndpointKind::Device
    }

    fn size(&self) -> AppResult<u64> {
        match self.kind {
            EndpointKind::File => {
                let metadata = std::fs::metadata(&self.path)?;
                if metadata.is_dir() {
                    return Err(
                        format!("{} is a directory, not a file", self.path.display()).into(),
                    );
                }
                Ok(metadata.len())
            }
            EndpointKind::Device => query_device_size(&self.path),
        }
    }

    fn open_for_read(&self) -> io::Result<File> {
        match self.kind {
            EndpointKind::File => OpenOptions::new().read(true).open(&self.path),
            EndpointKind::Device => OpenOptions::new()
                .read(true)
                .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE).0)
                .open(&self.path),
        }
    }

    fn open_for_write(&self) -> io::Result<File> {
        match self.kind {
            EndpointKind::File => {
                if self.path.is_dir() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("{} is a directory, not a file", self.path.display()),
                    ));
                }

                OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(&self.path)
            }
            EndpointKind::Device => OpenOptions::new()
                .read(true)
                .write(true)
                .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE).0)
                .open(&self.path),
        }
    }
}

fn parse_block_size(value: &str) -> Result<u64, String> {
    parse_human_readable_size(value)
        .map_err(|message| format!("failed to parse block size: {message}"))
}

fn parse_count(value: &str) -> Result<u64, String> {
    parse_human_readable_size(value).map_err(|message| format!("failed to parse count: {message}"))
}

fn parse_skip(value: &str) -> Result<u64, String> {
    parse_human_readable_size(value).map_err(|message| format!("failed to parse skip: {message}"))
}

fn parse_human_readable_size(value: &str) -> Result<u64, String> {
    let normalized = value.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        return Err("cannot be empty".to_string());
    }

    let digits_len = normalized
        .chars()
        .take_while(|character| character.is_ascii_digit())
        .count();

    if digits_len == 0 {
        return Err(format!("invalid value: {value}"));
    }

    let number: u64 = normalized[..digits_len]
        .parse()
        .map_err(|_| format!("invalid value: {value}"))?;
    let suffix = normalized[digits_len..].trim();

    let multiplier = match suffix {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1024,
        "m" | "mb" | "mib" => 1024_u64.pow(2),
        "g" | "gb" | "gib" => 1024_u64.pow(3),
        "t" | "tb" | "tib" => 1024_u64.pow(4),
        _ => return Err(format!("unsupported suffix in {value}")),
    };

    number
        .checked_mul(multiplier)
        .ok_or_else(|| format!("value is too large: {value}"))
}

fn validate_destination_capacity(destination: &Endpoint, source_size: u64) -> AppResult<()> {
    match destination.kind {
        EndpointKind::Device => {
            let destination_size = destination.size()?;
            if destination_size < source_size {
                return Err(format!(
                    "destination device is too small: {} available, {} required",
                    format_bytes(destination_size),
                    format_bytes(source_size)
                )
                .into());
            }
        }
        EndpointKind::File => {
            let existing_length = existing_file_length(&destination.path)?;
            let required_additional = source_size.saturating_sub(existing_length);
            let available_space = available_bytes_for_path(&destination.path)?;

            if available_space < required_additional {
                return Err(format!(
                    "destination volume does not have enough free space: {} free, {} required",
                    format_bytes(available_space),
                    format_bytes(required_additional)
                )
                .into());
            }
        }
    }

    Ok(())
}

fn copy_size_after_skip(source_size: u64, skip: u64, count: Option<u64>) -> AppResult<u64> {
    if skip > source_size {
        return Err(format!(
            "requested skip ({}) is larger than the source size ({})",
            format_bytes(skip),
            format_bytes(source_size)
        )
        .into());
    }

    let Some(count) = count else {
        return Ok(source_size - skip);
    };

    if count == 0 {
        return Err("count must be greater than zero".into());
    }

    let copy_end = skip.checked_add(count).ok_or_else(|| {
        format!(
            "requested skip ({}) plus count ({}) is too large",
            format_bytes(skip),
            format_bytes(count)
        )
    })?;

    if copy_end > source_size {
        return Err(format!(
            "requested skip ({}) plus count ({}) exceeds the source size ({})",
            format_bytes(skip),
            format_bytes(count),
            format_bytes(source_size)
        )
        .into());
    }

    Ok(count)
}

fn existing_file_length(path: &Path) -> io::Result<u64> {
    match std::fs::metadata(path) {
        Ok(metadata) => {
            if metadata.is_dir() {
                Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{} is a directory, not a file", path.display()),
                ))
            } else {
                Ok(metadata.len())
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error),
    }
}

fn available_bytes_for_path(path: &Path) -> AppResult<u64> {
    let directory = nearest_existing_directory(path)?;
    query_free_space(&directory)
}

fn nearest_existing_directory(path: &Path) -> AppResult<PathBuf> {
    let mut candidate = if path.exists() {
        if path.is_dir() {
            path.to_path_buf()
        } else {
            path.parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .map(Path::to_path_buf)
                .unwrap_or(std::env::current_dir()?)
        }
    } else {
        path.parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .unwrap_or(std::env::current_dir()?)
    };

    if candidate.is_relative() {
        candidate = std::env::current_dir()?.join(candidate);
    }

    for ancestor in candidate.ancestors() {
        if ancestor.exists() && ancestor.is_dir() {
            return Ok(ancestor.to_path_buf());
        }
    }

    Err(format!("no existing parent directory found for {}", path.display()).into())
}

fn query_free_space(path: &Path) -> AppResult<u64> {
    let wide = wide_null(path.as_os_str());
    let mut free_bytes = 0u64;

    unsafe {
        GetDiskFreeSpaceExW(PCWSTR(wide.as_ptr()), Some(&mut free_bytes), None, None)?;
    }

    Ok(free_bytes)
}

fn query_device_size(path: &Path) -> AppResult<u64> {
    let file = OpenOptions::new()
        .read(true)
        .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE).0)
        .open(path)?;

    let mut length = GET_LENGTH_INFORMATION::default();
    let mut bytes_returned = 0u32;

    unsafe {
        DeviceIoControl(
            HANDLE(file.as_raw_handle()),
            IOCTL_DISK_GET_LENGTH_INFO,
            None,
            0,
            Some((&mut length as *mut GET_LENGTH_INFORMATION).cast()),
            size_of::<GET_LENGTH_INFORMATION>() as u32,
            Some(&mut bytes_returned),
            None,
        )?;
    }

    u64::try_from(length.Length)
        .map_err(|_| format!("device reported a negative length for {}", path.display()).into())
}

fn copy_with_progress(
    reader: &mut File,
    writer: &mut File,
    total_bytes: u64,
    block_size: usize,
) -> io::Result<()> {
    let mut buffer = vec![0u8; block_size];
    let mut written = 0u64;
    let started_at = Instant::now();

    print_progress(0, total_bytes, started_at)?;

    while written < total_bytes {
        let remaining = total_bytes - written;
        let chunk_len = usize::try_from(remaining.min(block_size as u64)).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "remaining byte count does not fit in memory",
            )
        })?;
        let read = reader.read(&mut buffer[..chunk_len])?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "source ended before the reported size was fully copied",
            ));
        }

        writer.write_all(&buffer[..read])?;
        written += read as u64;
        print_progress(written, total_bytes, started_at)?;
    }

    writer.flush()?;
    println!();

    Ok(())
}

fn print_progress(written: u64, total: u64, started_at: Instant) -> io::Result<()> {
    let percentage = if total == 0 {
        100.0
    } else {
        (written as f64 / total as f64) * 100.0
    };

    let eta = if written == 0 || total <= written {
        None
    } else {
        let elapsed = started_at.elapsed();
        if elapsed.is_zero() {
            None
        } else {
            let bytes_per_second = written as f64 / elapsed.as_secs_f64();
            if bytes_per_second > 0.0 {
                let remaining_seconds = ((total - written) as f64 / bytes_per_second).ceil();
                Some(Duration::from_secs_f64(remaining_seconds))
            } else {
                None
            }
        }
    };

    let eta_display = eta
        .map(format_eta)
        .unwrap_or_else(|| "--:--:--".to_string());

    print!(
        "\r{}/{} {:.1}% ETA {}      ",
        format_bytes(written),
        format_bytes(total),
        percentage,
        eta_display
    );
    io::stdout().flush()
}

fn format_eta(duration: Duration) -> String {
    let total_seconds = duration.as_secs();
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;

    format!("{hours:02}:{minutes:02}:{seconds:02}")
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];

    if bytes < 1024 {
        return format!("{bytes} B");
    }

    let mut value = bytes as f64;
    let mut unit_index = 0usize;
    while value >= 1024.0 && unit_index < UNITS.len() - 1 {
        value /= 1024.0;
        unit_index += 1;
    }

    format!("{value:.2} {}", UNITS[unit_index])
}

fn is_device_path(path: &Path) -> bool {
    path.as_os_str().to_string_lossy().starts_with(r"\\.\")
}

fn wide_null(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::{copy_size_after_skip, parse_human_readable_size, parse_skip};

    #[test]
    fn parses_raw_byte_count() {
        assert_eq!(parse_human_readable_size("512").unwrap(), 512);
    }

    #[test]
    fn parses_kilobyte_suffixes() {
        assert_eq!(parse_human_readable_size("512kb").unwrap(), 512 * 1024);
        assert_eq!(parse_human_readable_size("512kib").unwrap(), 512 * 1024);
    }

    #[test]
    fn parses_megabyte_suffixes_case_insensitively() {
        assert_eq!(parse_human_readable_size("5mb").unwrap(), 5 * 1024 * 1024);
        assert_eq!(parse_human_readable_size("5mib").unwrap(), 5 * 1024 * 1024);
        assert_eq!(parse_human_readable_size("5MiB").unwrap(), 5 * 1024 * 1024);
    }

    #[test]
    fn rejects_unsupported_suffixes() {
        assert!(parse_human_readable_size("5xb").is_err());
    }

    #[test]
    fn parses_skip_with_human_readable_size() {
        assert_eq!(parse_skip("2mb").unwrap(), 2 * 1024 * 1024);
    }

    #[test]
    fn count_is_output_size_after_skip() {
        assert_eq!(copy_size_after_skip(12, 4, Some(8)).unwrap(), 8);
    }

    #[test]
    fn copies_remaining_source_when_count_is_omitted() {
        assert_eq!(copy_size_after_skip(12, 4, None).unwrap(), 8);
    }

    #[test]
    fn rejects_skip_plus_count_past_source_size() {
        assert!(copy_size_after_skip(12, 4, Some(9)).is_err());
    }
}
