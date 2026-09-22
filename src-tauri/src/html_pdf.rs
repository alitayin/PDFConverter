use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use thiserror::Error;

pub(crate) const MAX_INPUT_BYTES: u64 = 4 * 1024 * 1024;
const MAX_OUTPUT_BYTES: u64 = 64 * 1024 * 1024;
const PRINT_TIMEOUT: Duration = Duration::from_secs(95);

#[derive(Debug, Error)]
pub(crate) enum HtmlPdfError {
    #[error("HTML input must be a non-empty UTF-8 regular file no larger than 4 MiB")]
    InvalidInput,
    #[error("The Electron print worker is unavailable")]
    WorkerUnavailable,
    #[error("HTML printing failed or did not produce a valid PDF")]
    PrintFailed,
    #[error("HTML printing cancelled")]
    Cancelled,
    #[error("HTML printing timed out")]
    TimedOut,
    #[error("HTML print workspace read/write failed: {0}")]
    Io(#[from] std::io::Error),
}

impl HtmlPdfError {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::InvalidInput => "UNSUPPORTED_FORMAT",
            Self::WorkerUnavailable => "WORKER_NOT_INSTALLED",
            Self::PrintFailed => "CONVERSION_FAILED",
            Self::Cancelled => "CANCELLED",
            Self::TimedOut => "TIMED_OUT",
            Self::Io(_) => "OUTPUT_WRITE_FAILED",
        }
    }
}

fn same_identity(before: &fs::Metadata, after: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        before.dev() == after.dev() && before.ino() == after.ino()
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        before.creation_time() == after.creation_time()
            && before.file_size() == after.file_size()
            && before.file_attributes() == after.file_attributes()
    }
}

fn snapshot(
    input: &Path,
    workspace: &Path,
    should_stop: &impl Fn() -> bool,
) -> Result<PathBuf, HtmlPdfError> {
    if should_stop() {
        return Err(HtmlPdfError::Cancelled);
    }
    let before = fs::symlink_metadata(input).map_err(|_| HtmlPdfError::InvalidInput)?;
    if !before.file_type().is_file() || before.len() == 0 || before.len() > MAX_INPUT_BYTES {
        return Err(HtmlPdfError::InvalidInput);
    }
    let mut open = OpenOptions::new();
    open.read(true);
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        open.custom_flags(0x0000_0100); // O_NOFOLLOW
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        open.custom_flags(0x0002_0000); // O_NOFOLLOW
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        open.custom_flags(0x0020_0000).share_mode(0x0000_0001);
    }
    let mut source = open.open(input).map_err(|_| HtmlPdfError::InvalidInput)?;
    let opened = source.metadata().map_err(|_| HtmlPdfError::InvalidInput)?;
    if !opened.is_file() || !same_identity(&before, &opened) {
        return Err(HtmlPdfError::InvalidInput);
    }
    let mut bytes = Vec::with_capacity(before.len() as usize);
    (&mut source)
        .take(MAX_INPUT_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if should_stop() {
        return Err(HtmlPdfError::Cancelled);
    }
    let after = fs::symlink_metadata(input).map_err(|_| HtmlPdfError::InvalidInput)?;
    let opened_after = source.metadata().map_err(|_| HtmlPdfError::InvalidInput)?;
    if bytes.is_empty()
        || bytes.len() as u64 != before.len()
        || !same_identity(&opened, &opened_after)
        || opened.modified().ok() != opened_after.modified().ok()
        || !same_identity(&before, &after)
        || before.modified().ok() != after.modified().ok()
        || bytes.contains(&0)
        || std::str::from_utf8(&bytes).is_err()
    {
        return Err(HtmlPdfError::InvalidInput);
    }
    let path = workspace.join("html-input.html");
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        output.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    output.write_all(&bytes)?;
    output.flush()?;
    Ok(path)
}

#[cfg(unix)]
fn terminate_group(child: &mut Child) {
    unsafe extern "C" {
        fn kill(pid: i32, signal: i32) -> i32;
    }
    // SAFETY: process_group(0) assigned the child a private group; a negative PID targets it.
    let _ = unsafe { kill(-(child.id() as i32), 9) };
    let _ = child.wait();
}

#[cfg(windows)]
fn terminate_group(child: &mut Child, job: &crate::office::windows_job::Job) {
    job.terminate();
    let _ = child.wait();
}

pub(crate) fn html_to_pdf(
    input: &Path,
    output: &Path,
    workspace: &Path,
    should_stop: impl Fn() -> bool,
) -> Result<(), HtmlPdfError> {
    let executable = std::env::var_os("MINIMALPDF_ELECTRON_EXECUTABLE")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute() && path.is_file())
        .ok_or(HtmlPdfError::WorkerUnavailable)?;
    let app_root =
        std::env::var_os("MINIMALPDF_ELECTRON_APP_ROOT").ok_or(HtmlPdfError::WorkerUnavailable)?;
    let snapshot = snapshot(input, workspace, &should_stop)?;
    if !output.is_absolute() || !workspace.is_absolute() || output.exists() {
        return Err(HtmlPdfError::InvalidInput);
    }
    let mut command = Command::new(executable);
    if !app_root.is_empty() {
        let root = PathBuf::from(app_root);
        if !root.is_absolute() || !root.join("electron/main.cjs").is_file() {
            return Err(HtmlPdfError::WorkerUnavailable);
        }
        command.arg(root);
    }
    command.args([
        "--html-print-worker",
        snapshot.to_str().ok_or(HtmlPdfError::InvalidInput)?,
        output.to_str().ok_or(HtmlPdfError::InvalidInput)?,
    ]);
    command
        .env_remove("ELECTRON_RUN_AS_NODE")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    let job = crate::office::windows_job::Job::new()?;
    let mut child = command
        .spawn()
        .map_err(|_| HtmlPdfError::WorkerUnavailable)?;
    #[cfg(windows)]
    if job.assign(&child).is_err() {
        let _ = child.kill();
        let _ = child.wait();
        return Err(HtmlPdfError::WorkerUnavailable);
    }
    let started = Instant::now();
    loop {
        if should_stop() || started.elapsed() >= PRINT_TIMEOUT {
            #[cfg(unix)]
            terminate_group(&mut child);
            #[cfg(windows)]
            terminate_group(&mut child, &job);
            return Err(if should_stop() {
                HtmlPdfError::Cancelled
            } else {
                HtmlPdfError::TimedOut
            });
        }
        let state = match child.try_wait() {
            Ok(state) => state,
            Err(error) => {
                #[cfg(unix)]
                terminate_group(&mut child);
                #[cfg(windows)]
                terminate_group(&mut child, &job);
                return Err(HtmlPdfError::Io(error));
            }
        };
        match state {
            Some(status) => {
                #[cfg(unix)]
                terminate_group(&mut child);
                if !status.success() {
                    return Err(HtmlPdfError::PrintFailed);
                }
                break;
            }
            None => thread::sleep(Duration::from_millis(50)),
        }
    }
    if should_stop() {
        return Err(HtmlPdfError::Cancelled);
    }
    let size = fs::metadata(output)
        .map_err(|_| HtmlPdfError::PrintFailed)?
        .len();
    if !(8..=MAX_OUTPUT_BYTES).contains(&size) {
        return Err(HtmlPdfError::PrintFailed);
    }
    let mut pdf = File::open(output)?;
    let mut header = [0u8; 5];
    pdf.read_exact(&mut header)?;
    if &header != b"%PDF-" {
        return Err(HtmlPdfError::PrintFailed);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn snapshot_rejects_invalid_html_and_honors_cancellation() {
        let workspace = std::env::temp_dir().join(format!("html-snapshot-{}", Uuid::new_v4()));
        fs::create_dir(&workspace).unwrap();
        let input = workspace.join("input.html");
        fs::write(&input, "<h1>text</h1>").unwrap();
        assert!(matches!(
            snapshot(&input, &workspace, &|| true),
            Err(HtmlPdfError::Cancelled)
        ));
        fs::write(&input, [0xff, 0xfe]).unwrap();
        assert!(matches!(
            snapshot(&input, &workspace, &|| false),
            Err(HtmlPdfError::InvalidInput)
        ));
        fs::remove_dir_all(workspace).unwrap();
    }
}
