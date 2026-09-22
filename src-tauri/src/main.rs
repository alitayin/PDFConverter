#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tauri::Emitter;
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_opener::OpenerExt;
use thiserror::Error;
use uuid::Uuid;

mod docx;
mod electron_bridge;
mod html_pdf;
mod image_pdf;
mod markdown;
mod markdown_pdf;
mod office;
mod pdf;
mod pdf_writer;
#[cfg(test)]
mod pptx;
#[cfg(test)]
mod presentation_pdf;
mod render;
mod svg_pdf;
mod worker;

use crate::pdf::PageSelection;

const MAX_INPUT_BYTES: u64 = 250 * 1024 * 1024;
const JOB_TIMEOUT: Duration = Duration::from_secs(15 * 60);

static JOB_CANCELLATION: OnceLock<Mutex<HashMap<String, Arc<AtomicBool>>>> = OnceLock::new();
static JOB_LIMITER: OnceLock<JobLimiter> = OnceLock::new();
static OUTPUT_MOVE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn cancellation_registry() -> &'static Mutex<HashMap<String, Arc<AtomicBool>>> {
    JOB_CANCELLATION.get_or_init(|| Mutex::new(HashMap::new()))
}

fn insert_job(job_id: &str) -> Arc<AtomicBool> {
    let cancellation = Arc::new(AtomicBool::new(false));
    let mut registry = cancellation_registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    registry.insert(job_id.to_owned(), Arc::clone(&cancellation));
    cancellation
}

fn remove_job(job_id: &str) {
    let mut registry = cancellation_registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    registry.remove(job_id);
}

#[derive(Debug, Clone, Copy)]
enum JobLane {
    TextAndImage,
    Document,
    #[cfg(target_os = "windows")]
    NativeRender,
}

impl JobLane {
    fn for_kind(kind: &ConversionKind) -> Self {
        match kind {
            ConversionKind::PdfToImage => {
                #[cfg(target_os = "windows")]
                {
                    Self::NativeRender
                }
                #[cfg(not(target_os = "windows"))]
                {
                    Self::TextAndImage
                }
            }
            ConversionKind::PdfToTxt
            | ConversionKind::PdfToMarkdown
            | ConversionKind::ImageToPdf
            | ConversionKind::SvgToPdf => Self::TextAndImage,
            ConversionKind::PdfToDoc
            | ConversionKind::PdfToDocx
            | ConversionKind::PdfToOdt
            | ConversionKind::PdfToOdp
            | ConversionKind::PdfToPptx
            | ConversionKind::PdfToPpt
            | ConversionKind::PdfToRtf
            | ConversionKind::PdfToFlatOdtXml
            | ConversionKind::TxtToPdf
            | ConversionKind::RtfToPdf
            | ConversionKind::DocxToPdf
            | ConversionKind::DocToPdf
            | ConversionKind::OdtToPdf
            | ConversionKind::PptxToPdf
            | ConversionKind::PptToPdf
            | ConversionKind::OdpToPdf
            | ConversionKind::XlsxToPdf
            | ConversionKind::OdsToPdf => Self::Document,
            ConversionKind::HtmlToPdf | ConversionKind::MarkdownToPdf => Self::Document,
        }
    }

    fn limit(self) -> usize {
        match self {
            Self::TextAndImage => 2,
            Self::Document => 1,
            #[cfg(target_os = "windows")]
            Self::NativeRender => 1,
        }
    }
}

#[derive(Debug, Default)]
struct JobLaneCounts {
    text_and_image: usize,
    document: usize,
    #[cfg(target_os = "windows")]
    native_render: usize,
}

struct JobLimiter {
    counts: Mutex<JobLaneCounts>,
    wake: Condvar,
}

struct JobPermit {
    limiter: &'static JobLimiter,
    lane: JobLane,
}

impl Drop for JobPermit {
    fn drop(&mut self) {
        let mut counts = self
            .limiter
            .counts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match self.lane {
            JobLane::TextAndImage => {
                counts.text_and_image = counts.text_and_image.saturating_sub(1)
            }
            JobLane::Document => counts.document = counts.document.saturating_sub(1),
            #[cfg(target_os = "windows")]
            JobLane::NativeRender => counts.native_render = counts.native_render.saturating_sub(1),
        }
        self.limiter.wake.notify_one();
    }
}

fn job_limiter() -> &'static JobLimiter {
    JOB_LIMITER.get_or_init(|| JobLimiter {
        counts: Mutex::new(JobLaneCounts::default()),
        wake: Condvar::new(),
    })
}

impl JobLimiter {
    fn acquire(
        &'static self,
        kind: &ConversionKind,
        cancellation: &AtomicBool,
        started: Instant,
    ) -> Result<JobPermit, StopReason> {
        let lane = JobLane::for_kind(kind);
        let mut counts = self
            .counts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        loop {
            if let Some(reason) = stop_reason(cancellation, started) {
                return Err(reason);
            }
            let active = match lane {
                JobLane::TextAndImage => counts.text_and_image,
                JobLane::Document => counts.document,
                #[cfg(target_os = "windows")]
                JobLane::NativeRender => counts.native_render,
            };
            if active < lane.limit() {
                match lane {
                    JobLane::TextAndImage => counts.text_and_image += 1,
                    JobLane::Document => counts.document += 1,
                    #[cfg(target_os = "windows")]
                    JobLane::NativeRender => counts.native_render += 1,
                }
                return Ok(JobPermit {
                    limiter: self,
                    lane,
                });
            }
            let (next, _) = self
                .wake
                .wait_timeout(counts, Duration::from_millis(50))
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            counts = next;
        }
    }
}

fn output_move_lock() -> &'static Mutex<()> {
    OUTPUT_MOVE_LOCK.get_or_init(|| Mutex::new(()))
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StartJobRequest {
    kind: ConversionKind,
    inputs: Vec<String>,
    output_dir: Option<String>,
    options: ConversionOptions,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum ConversionKind {
    PdfToDoc,
    PdfToDocx,
    PdfToOdt,
    PdfToOdp,
    PdfToRtf,
    PdfToFlatOdtXml,
    DocToPdf,
    DocxToPdf,
    OdtToPdf,
    PdfToImage,
    PdfToTxt,
    PdfToMarkdown,
    MarkdownToPdf,
    TxtToPdf,
    RtfToPdf,
    PdfToPpt,
    PdfToPptx,
    ImageToPdf,
    SvgToPdf,
    HtmlToPdf,
    PptToPdf,
    PptxToPdf,
    OdpToPdf,
    XlsxToPdf,
    OdsToPdf,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ConversionOptions {
    image_format: Option<ImageFormat>,
    dpi: Option<u16>,
    pages: Option<String>,
    jpg_quality: Option<u8>,
    #[serde(default)]
    merge_images: bool,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ImageFormat {
    Png,
    Jpg,
    Bmp,
    Gif,
    Webp,
    Tiff,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct InputFileInfo {
    path: String,
    name: String,
    size: u64,
}

#[derive(Debug, Error, Serialize)]
#[serde(tag = "code", content = "message", rename_all = "SCREAMING_SNAKE_CASE")]
enum AppError {
    #[error("Choose at least one input file")]
    NoInputs,
    #[error("The input path must be absolute")]
    InputPathRequired,
    #[error("The input file does not exist")]
    InputNotFound,
    #[error("The input file is too large")]
    InputTooLarge,
    #[error("Image merging supports up to 200 files and 512 MiB total")]
    ImageBatchTooLarge,
    #[error("The input file is not supported for this conversion")]
    UnsupportedFormat,
    #[error("DPI must be 150, 200, or 300")]
    InvalidDpi,
    #[error("JPG quality must be between 1 and 100")]
    InvalidJpgQuality,
    #[error("The page range is invalid")]
    InvalidPages,
    #[error("The output path must be absolute")]
    OutputPathRequired,
    #[error("The output directory does not exist or is not writable")]
    OutputDirNotFound,
    #[error("This output directory is not writable")]
    OutputPathNotAllowed,
    #[error("The task does not exist or has already ended")]
    JobNotFound,
    #[error("Repair is not supported on this platform")]
    RepairUnavailable,
}

impl AppError {
    fn code(&self) -> &'static str {
        match self {
            Self::NoInputs => "NO_INPUTS",
            Self::InputPathRequired => "INPUT_PATH_REQUIRED",
            Self::InputNotFound => "INPUT_NOT_FOUND",
            Self::InputTooLarge => "INPUT_TOO_LARGE",
            Self::ImageBatchTooLarge => "INPUT_TOO_LARGE",
            Self::UnsupportedFormat => "UNSUPPORTED_FORMAT",
            Self::InvalidDpi => "INVALID_DPI",
            Self::InvalidJpgQuality => "INVALID_JPG_QUALITY",
            Self::InvalidPages => "INVALID_PAGES",
            Self::OutputPathRequired => "OUTPUT_PATH_REQUIRED",
            Self::OutputDirNotFound => "OUTPUT_DIR_NOT_FOUND",
            Self::OutputPathNotAllowed => "OUTPUT_PATH_NOT_ALLOWED",
            Self::JobNotFound => "JOB_NOT_FOUND",
            Self::RepairUnavailable => "REPAIR_UNAVAILABLE",
        }
    }
}

impl From<AppError> for String {
    fn from(error: AppError) -> Self {
        format!("{}: {}", error.code(), error)
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SelfCheck {
    status: &'static str,
    checks: Vec<SelfCheckItem>,
}

#[derive(Debug, Serialize)]
struct SelfCheckItem {
    id: &'static str,
    status: &'static str,
    message: &'static str,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DiagnosticInfo {
    app_version: &'static str,
    os: &'static str,
    architecture: &'static str,
    os_version: String,
    self_check: &'static str,
    checks: Vec<DiagnosticCheck>,
    component_hashes: Vec<DiagnosticComponent>,
}

#[derive(Debug, Serialize)]
struct DiagnosticCheck {
    id: &'static str,
    status: &'static str,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DiagnosticComponent {
    id: &'static str,
    sha256: Option<String>,
    status: &'static str,
}

#[tauri::command]
fn get_self_check() -> SelfCheck {
    build_self_check()
}

fn build_self_check() -> SelfCheck {
    let manifest_ok = std::env::current_exe()
        .map(|path| path.is_file())
        .unwrap_or(false);
    let temp_dir_ok = std::env::temp_dir().is_dir();
    let filesystem_ok = temp_dir_ok && writable_probe();
    let engine_ok = temp_dir_ok && engine_probe();
    let office_ok = office::is_installed();
    SelfCheck {
        status: if manifest_ok && filesystem_ok && engine_ok && office_ok {
            "ready"
        } else {
            "repairable"
        },
        checks: vec![
            SelfCheckItem {
                id: "app_manifest",
                status: if manifest_ok { "passed" } else { "failed" },
                message: if manifest_ok {
                    "Application files are present"
                } else {
                    "Application files could not be located"
                },
            },
            SelfCheckItem {
                id: "worker_runtime",
                status: if engine_ok { "passed" } else { "failed" },
                message: if engine_ok {
                    "Embedded conversion worker is available"
                } else {
                    "Embedded conversion worker self-check failed"
                },
            },
            SelfCheckItem {
                id: "pdf_engines",
                status: if engine_ok { "passed" } else { "failed" },
                message: if engine_ok {
                    "PDF text, image, and document engines are available"
                } else {
                    "PDF text, image, or document engine self-check failed"
                },
            },
            SelfCheckItem {
                id: "office_runtime",
                status: if office_ok { "passed" } else { "failed" },
                message: if office_ok {
                    "Office conversion is available; check the output layout"
                } else {
                    "Office conversion is unavailable; check or repair the installation"
                },
            },
            SelfCheckItem {
                id: "filesystem",
                status: if filesystem_ok { "passed" } else { "failed" },
                message: if filesystem_ok {
                    "Temporary directory is readable and writable"
                } else {
                    "Temporary directory is unavailable or not writable"
                },
            },
        ],
    }
}

#[tauri::command]
fn get_diagnostic_info() -> Result<String, String> {
    diagnostic_info(None)
}

fn diagnostic_info(worker_path: Option<&Path>) -> Result<String, String> {
    let check = build_self_check();
    let component_hashes = diagnostic_components(worker_path);
    let info = DiagnosticInfo {
        app_version: env!("CARGO_PKG_VERSION"),
        os: std::env::consts::OS,
        architecture: std::env::consts::ARCH,
        os_version: platform_version(),
        self_check: check.status,
        checks: check
            .checks
            .iter()
            .map(|item| DiagnosticCheck {
                id: item.id,
                status: item.status,
            })
            .collect(),
        component_hashes,
    };
    serde_json::to_string_pretty(&info)
        .map_err(|error| format!("DIAGNOSTIC_SERIALIZE_FAILED: {error}"))
}

fn diagnostic_components(worker_path: Option<&Path>) -> Vec<DiagnosticComponent> {
    let executable = std::env::current_exe().ok();
    let default_worker = executable.as_ref().and_then(|path| {
        let name = if cfg!(windows) {
            "pdf_to_txt_worker.exe"
        } else {
            "pdf_to_txt_worker"
        };
        path.parent().map(|parent| parent.join(name))
    });
    let worker = worker_path.or(default_worker.as_deref());
    [
        ("app", executable.as_deref()),
        ("pdf_to_txt_worker", worker),
    ]
    .into_iter()
    .map(
        |(id, path)| match path.and_then(|path| sha256_file(path).ok()) {
            Some(sha256) => DiagnosticComponent {
                id,
                sha256: Some(sha256),
                status: "passed",
            },
            None => DiagnosticComponent {
                id,
                sha256: None,
                status: "unavailable",
            },
        },
    )
    .collect()
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = File::open(path)?;
    let mut context = ring::digest::Context::new(&ring::digest::SHA256);
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        context.update(&buffer[..count]);
    }
    let digest = context.finish();
    Ok(digest
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

#[cfg(target_os = "macos")]
fn platform_version() -> String {
    use std::ffi::CString;
    use std::os::raw::{c_char, c_int, c_void};

    #[link(name = "System")]
    unsafe extern "C" {
        fn sysctlbyname(
            name: *const c_char,
            oldp: *mut c_void,
            oldlenp: *mut usize,
            newp: *mut c_void,
            newlen: usize,
        ) -> c_int;
    }

    let Ok(name) = CString::new("kern.osproductversion") else {
        return "macOS".to_owned();
    };
    let mut length = 0usize;
    // SAFETY: `name` is a valid NUL-terminated key; the first call only asks
    // the kernel for the required buffer size and passes null output pointers.
    let result = unsafe {
        sysctlbyname(
            name.as_ptr(),
            std::ptr::null_mut(),
            &mut length,
            std::ptr::null_mut(),
            0,
        )
    };
    if result != 0 || length == 0 {
        return "macOS".to_owned();
    }
    let mut bytes = vec![0u8; length];
    // SAFETY: `bytes` has exactly the size requested by the kernel and remains
    // alive for the duration of the second query.
    let result = unsafe {
        sysctlbyname(
            name.as_ptr(),
            bytes.as_mut_ptr().cast(),
            &mut length,
            std::ptr::null_mut(),
            0,
        )
    };
    if result != 0 {
        return "macOS".to_owned();
    }
    bytes.truncate(length);
    String::from_utf8(bytes)
        .unwrap_or_else(|_| "macOS".to_owned())
        .trim_end_matches('\0')
        .to_owned()
}

#[cfg(target_os = "windows")]
fn platform_version() -> String {
    // Windows does not expose a stable safe-std API for the host version. The
    // diagnostic path is best-effort and must never prevent conversion.
    "Windows".to_owned()
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn platform_version() -> String {
    std::env::consts::OS.to_owned()
}

fn writable_probe() -> bool {
    let path = std::env::temp_dir().join(format!(
        "minimal-pdf-converter-probe-{}",
        Uuid::new_v4().simple()
    ));
    let result = std::fs::write(&path, b"probe")
        .and_then(|_| std::fs::read(&path))
        .map(|bytes| bytes == b"probe")
        .unwrap_or(false);
    let _ = std::fs::remove_file(path);
    result
}

fn engine_probe() -> bool {
    let (text_ok, docx_ok, image_ok) = engine_probe_components();
    text_ok && docx_ok && image_ok
}

fn engine_probe_components() -> (bool, bool, bool) {
    let root = std::env::temp_dir().join(format!(
        "minimal-pdf-converter-self-check-{}",
        Uuid::new_v4().simple()
    ));
    if std::fs::create_dir_all(&root).is_err() {
        return (false, false, false);
    }
    let pdf_path = root.join("probe.pdf");
    let docx_path = root.join("probe.docx");
    #[cfg(not(target_os = "windows"))]
    let image_dir = root.join("images");
    let pdf_ok = pdf_writer::write_text_pdf(&pdf_path, "self check").is_ok();
    let text_ok = pdf_ok
        && pdf::extract_text_from_path(&pdf_path, &PageSelection::All)
            .map(|result| result.text.contains("self check"))
            .unwrap_or(false);
    let docx_ok = docx::write_text_docx(&docx_path, &["self check".to_owned()]).is_ok()
        && docx::extract_text_from_path(&docx_path)
            .map(|text| text.contains("self check"))
            .unwrap_or(false);
    #[cfg(target_os = "windows")]
    let image_ok = render::windows_render::probe_image_engine(&root);
    #[cfg(not(target_os = "windows"))]
    let image_ok = pdf_ok
        && render::render_pdf(
            &pdf_path,
            &image_dir,
            "probe",
            &render::RenderOptions {
                selection: PageSelection::All,
                dpi: 150,
                format: render::ImageFormat::Png,
                jpg_quality: 85,
                max_selected_pages: pdf::MAX_PAGES,
                max_output_bytes: u64::MAX,
            },
            || false,
            |_, _, _| {},
        )
        .map(|result| !result.outputs.is_empty())
        .unwrap_or(false);
    let _ = std::fs::remove_dir_all(root);
    (text_ok, docx_ok, image_ok)
}

#[tauri::command]
fn inspect_input_files(paths: Vec<String>) -> Result<Vec<InputFileInfo>, String> {
    if paths.is_empty() {
        return Err(AppError::NoInputs.into());
    }

    paths
        .into_iter()
        .map(|input| {
            let path = Path::new(&input);
            if !path.is_absolute() {
                return Err(AppError::InputPathRequired.into());
            }
            let metadata = std::fs::symlink_metadata(path).map_err(|_| AppError::InputNotFound)?;
            if !metadata.file_type().is_file() {
                return Err(AppError::InputNotFound.into());
            }
            if metadata.len() > MAX_INPUT_BYTES {
                return Err(AppError::InputTooLarge.into());
            }
            let canonical = path.canonicalize().map_err(|_| AppError::InputNotFound)?;
            let name = canonical
                .file_name()
                .and_then(|value| value.to_str())
                .filter(|value| !value.is_empty())
                .ok_or(AppError::InputNotFound)?
                .to_owned();
            Ok(InputFileInfo {
                path: canonical.to_string_lossy().into_owned(),
                name,
                size: metadata.len(),
            })
        })
        .collect()
}

#[tauri::command]
fn start_job(app: tauri::AppHandle, request: StartJobRequest) -> Result<String, String> {
    let (job_id, _worker) = start_job_with_emitter(request, move |event| {
        let _ = app.emit("conversion://progress", event);
    })?;
    Ok(job_id)
}

fn start_job_with_emitter(
    request: StartJobRequest,
    mut emit: impl FnMut(worker::WorkerEvent) + Send + 'static,
) -> Result<(String, std::thread::JoinHandle<()>), String> {
    validate_request(&request).map_err(String::from)?;
    let job_id = format!("job_{}", Uuid::new_v4().simple());
    let output_dir = request.output_dir.as_deref().map(PathBuf::from);
    let inputs = request.inputs.iter().map(PathBuf::from).collect::<Vec<_>>();
    let kind = request.kind.clone();
    let options = request.options.clone();
    let cancellation = insert_job(&job_id);
    let thread_job_id = job_id.clone();
    let worker = std::thread::spawn(move || {
        run_job(
            thread_job_id.clone(),
            kind,
            inputs,
            output_dir,
            options,
            cancellation,
            &mut emit,
        );
        remove_job(&thread_job_id);
    });
    Ok((job_id, worker))
}

#[tauri::command]
fn cancel_job(job_id: String) -> Result<(), String> {
    let registry = cancellation_registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(cancellation) = registry.get(&job_id) else {
        return Err(AppError::JobNotFound.into());
    };
    cancellation.store(true, Ordering::Release);
    Ok(())
}

#[tauri::command]
async fn choose_output_dir(
    app: tauri::AppHandle,
    window: tauri::Window,
) -> Result<Option<String>, String> {
    let selected = tauri::async_runtime::spawn_blocking(move || {
        app.dialog()
            .file()
            .set_parent(&window)
            .blocking_pick_folder()
    })
    .await
    .map_err(|error| format!("OUTPUT_DIR_NOT_FOUND: {error}"))?;
    selected
        .map(|path| {
            path.into_path()
                .map(|path| path.to_string_lossy().into_owned())
                .map_err(|error| format!("OUTPUT_DIR_NOT_FOUND: {error}"))
        })
        .transpose()
}

#[tauri::command]
async fn choose_input_files(
    app: tauri::AppHandle,
    window: tauri::Window,
) -> Result<Vec<String>, String> {
    let selected = tauri::async_runtime::spawn_blocking(move || {
        app.dialog()
            .file()
            .set_parent(&window)
            .add_filter(
                "Supported files",
                &[
                    "pdf", "docx", "odt", "rtf", "txt", "pptx", "odp", "xlsx", "ods", "png", "jpg",
                    "jpeg", "gif", "webp", "tif", "tiff", "doc", "ppt", "md", "markdown",
                ],
            )
            .blocking_pick_files()
    })
    .await
    .map_err(|error| format!("INPUT_NOT_FOUND: {error}"))?;
    selected
        .unwrap_or_default()
        .into_iter()
        .map(|path| {
            path.into_path()
                .map(|path| path.to_string_lossy().into_owned())
                .map_err(|error| format!("INPUT_NOT_FOUND: {error}"))
        })
        .collect()
}

#[tauri::command]
fn repair_install() -> Result<(), String> {
    // All current engines are embedded in the signed application binary.
    // There is no mutable runtime bundle to repair in this build.
    Err(AppError::RepairUnavailable.into())
}

#[tauri::command]
fn open_path(app: tauri::AppHandle, path: String) -> Result<(), String> {
    let path = PathBuf::from(path);
    if !path.is_absolute() || (!path.is_file() && !path.is_dir()) {
        return Err(AppError::InputNotFound.into());
    }
    let canonical = path.canonicalize().map_err(|_| AppError::InputNotFound)?;
    let directory = if canonical.is_dir() {
        canonical.as_path()
    } else {
        canonical.parent().ok_or(AppError::OutputPathNotAllowed)?
    };
    validate_output_dir(directory).map_err(String::from)?;
    app.opener()
        .open_path(canonical.to_string_lossy().into_owned(), None::<&str>)
        .map_err(|error| format!("OUTPUT_OPEN_FAILED: {error}"))
}

fn validate_request(request: &StartJobRequest) -> Result<(), AppError> {
    if request.inputs.is_empty() {
        return Err(AppError::NoInputs);
    }
    if request.options.merge_images && !matches!(request.kind, ConversionKind::ImageToPdf) {
        return Err(AppError::UnsupportedFormat);
    }
    if request.options.merge_images && request.inputs.len() > 200 {
        return Err(AppError::ImageBatchTooLarge);
    }
    let mut batch_bytes = 0u64;
    for input in &request.inputs {
        let path = Path::new(input);
        if !path.is_absolute() {
            return Err(AppError::InputPathRequired);
        }
        let metadata = std::fs::symlink_metadata(path).map_err(|_| AppError::InputNotFound)?;
        if !metadata.file_type().is_file() {
            return Err(AppError::InputNotFound);
        }
        let size = metadata.len();
        let limit = match request.kind {
            ConversionKind::ImageToPdf => image_pdf::MAX_INPUT_BYTES,
            ConversionKind::SvgToPdf => svg_pdf::MAX_INPUT_BYTES,
            ConversionKind::HtmlToPdf => html_pdf::MAX_INPUT_BYTES,
            ConversionKind::MarkdownToPdf => markdown_pdf::MAX_INPUT_BYTES,
            _ => MAX_INPUT_BYTES,
        };
        if size > limit {
            return Err(AppError::InputTooLarge);
        }
        if request.options.merge_images {
            batch_bytes = batch_bytes
                .checked_add(size)
                .ok_or(AppError::ImageBatchTooLarge)?;
            if batch_bytes > 512 * 1024 * 1024 {
                return Err(AppError::ImageBatchTooLarge);
            }
        }
        if !supports_extension(&request.kind, path) {
            return Err(AppError::UnsupportedFormat);
        }
    }
    if matches!(request.kind, ConversionKind::PdfToImage) {
        if let Some(dpi) = request.options.dpi {
            if !matches!(dpi, 150 | 200 | 300) {
                return Err(AppError::InvalidDpi);
            }
        }
        if let Some(quality) = request.options.jpg_quality {
            if !(1..=100).contains(&quality) {
                return Err(AppError::InvalidJpgQuality);
            }
        }
    }
    if let Some(pages) = &request.options.pages {
        if matches!(
            request.kind,
            ConversionKind::HtmlToPdf
                | ConversionKind::MarkdownToPdf
                | ConversionKind::PdfToPptx
                | ConversionKind::PdfToPpt
        ) && !is_all_pages(pages)
        {
            return Err(AppError::InvalidPages);
        }
        if !is_all_pages(pages) && !valid_pages(pages) {
            return Err(AppError::InvalidPages);
        }
    }
    for input in &request.inputs {
        let input_path = Path::new(input);
        let output_path = request
            .output_dir
            .as_deref()
            .map(Path::new)
            .or_else(|| input_path.parent())
            .ok_or(AppError::OutputDirNotFound)?;
        validate_output_dir(output_path)?;
    }
    Ok(())
}

fn validate_output_dir(path: &Path) -> Result<(), AppError> {
    if !path.is_absolute() {
        return Err(AppError::OutputPathRequired);
    }
    if !path.is_dir() {
        return Err(AppError::OutputDirNotFound);
    }
    let canonical = path
        .canonicalize()
        .map_err(|_| AppError::OutputDirNotFound)?;
    if is_forbidden_output_dir(&canonical) {
        return Err(AppError::OutputPathNotAllowed);
    }
    Ok(())
}

fn is_forbidden_output_dir(path: &Path) -> bool {
    #[cfg(windows)]
    {
        let normalized = path.to_string_lossy().to_ascii_lowercase();
        normalized == "c:\\"
            || normalized == "c:\\windows"
            || normalized.starts_with("c:\\windows\\")
            || normalized == "c:\\program files"
            || normalized.starts_with("c:\\program files\\")
            || normalized == "c:\\program files (x86)"
            || normalized.starts_with("c:\\program files (x86)\\")
    }
    #[cfg(not(windows))]
    {
        const FORBIDDEN: &[&str] = &[
            "/",
            "/System",
            "/Library",
            "/Applications",
            "/usr",
            "/bin",
            "/sbin",
            "/etc",
            "/private/etc",
        ];
        FORBIDDEN.iter().any(|prefix| {
            let prefix = Path::new(prefix);
            path == prefix || (prefix != Path::new("/") && path.starts_with(prefix))
        })
    }
}

fn supports_extension(kind: &ConversionKind, path: &Path) -> bool {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match kind {
        ConversionKind::DocToPdf => extension == "doc",
        ConversionKind::DocxToPdf => extension == "docx",
        ConversionKind::TxtToPdf => extension == "txt",
        ConversionKind::RtfToPdf => extension == "rtf",
        ConversionKind::OdtToPdf => extension == "odt",
        ConversionKind::PptToPdf => extension == "ppt",
        ConversionKind::PptxToPdf => extension == "pptx",
        ConversionKind::OdpToPdf => extension == "odp",
        ConversionKind::XlsxToPdf => extension == "xlsx",
        ConversionKind::OdsToPdf => extension == "ods",
        ConversionKind::HtmlToPdf => matches!(extension.as_str(), "html" | "htm"),
        ConversionKind::MarkdownToPdf => matches!(extension.as_str(), "md" | "markdown"),
        ConversionKind::SvgToPdf => extension == "svg",
        ConversionKind::ImageToPdf => {
            matches!(
                extension.as_str(),
                "png" | "jpg" | "jpeg" | "bmp" | "gif" | "webp" | "tif" | "tiff"
            )
        }
        ConversionKind::PdfToDoc
        | ConversionKind::PdfToDocx
        | ConversionKind::PdfToOdt
        | ConversionKind::PdfToOdp
        | ConversionKind::PdfToPptx
        | ConversionKind::PdfToPpt
        | ConversionKind::PdfToRtf
        | ConversionKind::PdfToFlatOdtXml
        | ConversionKind::PdfToImage
        | ConversionKind::PdfToTxt
        | ConversionKind::PdfToMarkdown => extension == "pdf",
    }
}

fn valid_pages(value: &str) -> bool {
    if value.trim().is_empty() {
        return false;
    }
    value.split(',').all(|part| {
        let pieces: Vec<_> = part.trim().split('-').collect();
        pieces.len() <= 2
            && pieces
                .iter()
                .all(|item| item.parse::<u32>().is_ok_and(|page| page > 0))
            && pieces
                .first()
                .zip(pieces.get(1))
                .map(|(start, end)| {
                    start.parse::<u32>().unwrap_or(0) <= end.parse::<u32>().unwrap_or(0)
                })
                .unwrap_or(true)
    })
}

fn is_all_pages(value: &str) -> bool {
    value.trim().is_empty() || value.trim().eq_ignore_ascii_case("all") || value.trim() == "全部"
}

#[derive(Debug, Clone, Copy)]
enum StopReason {
    Cancelled,
    TimedOut,
}

#[derive(Debug)]
struct EngineOutput {
    paths: Vec<PathBuf>,
    message: String,
}

#[derive(Debug)]
struct EngineFailure {
    code: &'static str,
    message: String,
}

#[derive(Debug)]
enum FinalizationFailure {
    Stopped(StopReason),
    Write(String),
}

struct JobWorkspace {
    path: PathBuf,
}

impl JobWorkspace {
    fn create(path: PathBuf) -> std::io::Result<Self> {
        let parent = path.parent().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Temporary directory has no parent",
            )
        })?;
        match std::fs::create_dir(parent) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        let parent_metadata = std::fs::symlink_metadata(parent)?;
        if !parent_metadata.file_type().is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Temporary directory parent must be a real, non-symlink directory",
            ));
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            // Junctions are directories too, but can redirect staging outside the output root.
            if parent_metadata.file_attributes() & 0x400 != 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "Temporary directory parent cannot be a reparse point",
                ));
            }
        }
        std::fs::create_dir(&path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(Self { path })
    }
}

impl Drop for JobWorkspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

struct ConversionContext<'a> {
    kind: &'a ConversionKind,
    input: &'a Path,
    temp_root: &'a Path,
    stem: &'a str,
    options: &'a ConversionOptions,
    cancellation: &'a AtomicBool,
    started: Instant,
}

fn stop_reason(cancellation: &AtomicBool, started: Instant) -> Option<StopReason> {
    if cancellation.load(Ordering::Acquire) {
        Some(StopReason::Cancelled)
    } else if started.elapsed() >= JOB_TIMEOUT {
        Some(StopReason::TimedOut)
    } else {
        None
    }
}

fn run_job(
    job_id: String,
    kind: ConversionKind,
    inputs: Vec<PathBuf>,
    output_dir: Option<PathBuf>,
    options: ConversionOptions,
    cancellation: Arc<AtomicBool>,
    emit: &mut impl FnMut(worker::WorkerEvent),
) {
    let total = inputs.len();
    let started = Instant::now();
    let mut sequence = 0;
    emit_queued(emit, &job_id, &mut sequence, total);
    let _permit = match job_limiter().acquire(&kind, &cancellation, started) {
        Ok(permit) => permit,
        Err(reason) => {
            emit_stopped(emit, &job_id, &mut sequence, 0, total, reason);
            return;
        }
    };
    if matches!(kind, ConversionKind::ImageToPdf) && options.merge_images {
        run_merged_image_job(job_id, inputs, output_dir, cancellation, started, emit);
        return;
    }
    for (index, input) in inputs.into_iter().enumerate() {
        if let Some(reason) = stop_reason(&cancellation, started) {
            emit_stopped(emit, &job_id, &mut sequence, index, total, reason);
            break;
        }
        let requested_output_root = output_dir.clone().unwrap_or_else(|| {
            input
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."))
        });
        let output_root = match requested_output_root.canonicalize() {
            Ok(path) => path,
            Err(error) => {
                emit_failed(
                    emit,
                    &job_id,
                    &mut sequence,
                    index + 1,
                    total,
                    "OUTPUT_DIR_NOT_FOUND",
                    format!("Could not read the output directory: {error}"),
                );
                continue;
            }
        };
        let input = match input.canonicalize() {
            Ok(path) => path,
            Err(error) => {
                emit_failed(
                    emit,
                    &job_id,
                    &mut sequence,
                    index + 1,
                    total,
                    "INPUT_NOT_FOUND",
                    format!("Could not read the input file: {error}"),
                );
                continue;
            }
        };
        let workspace =
            match JobWorkspace::create(output_root.join(".minimal-pdf-converter").join(&job_id)) {
                Ok(workspace) => workspace,
                Err(error) => {
                    emit_failed(
                        emit,
                        &job_id,
                        &mut sequence,
                        index + 1,
                        total,
                        "OUTPUT_WRITE_FAILED",
                        format!("Could not create the temporary directory: {error}"),
                    );
                    continue;
                }
            };
        let stem = input
            .file_stem()
            .and_then(|value| value.to_str())
            .filter(|value| !value.is_empty())
            .unwrap_or("output");
        let mut report = |phase: &str, message: String| {
            sequence += 1;
            emit(worker::WorkerEvent {
                job_id: job_id.clone(),
                seq: sequence,
                state: worker::WorkerState::Running,
                completed: index,
                total,
                phase: phase.to_owned(),
                message,
                outputs: None,
                error_code: None,
            });
        };
        let context = ConversionContext {
            kind: &kind,
            input: &input,
            temp_root: &workspace.path,
            stem,
            options: &options,
            cancellation: &cancellation,
            started,
        };
        let engine_result = execute_conversion(&context, &mut report);
        let result = match engine_result {
            Ok(result) => result,
            Err(error) => {
                if let Some(reason) = stop_reason(&cancellation, started) {
                    emit_stopped(emit, &job_id, &mut sequence, index, total, reason);
                    break;
                }
                if error.code == "TIMED_OUT" {
                    emit_stopped(
                        emit,
                        &job_id,
                        &mut sequence,
                        index,
                        total,
                        StopReason::TimedOut,
                    );
                    break;
                }
                emit_failed(
                    emit,
                    &job_id,
                    &mut sequence,
                    index + 1,
                    total,
                    error.code,
                    error.message,
                );
                continue;
            }
        };
        let extension = output_extension(&kind, &options);
        let moved_outputs =
            match finalize_outputs(&output_root, stem, &extension, &result.paths, || {
                stop_reason(&cancellation, started)
            }) {
                Ok(paths) => paths,
                Err(FinalizationFailure::Stopped(reason)) => {
                    emit_stopped(emit, &job_id, &mut sequence, index, total, reason);
                    break;
                }
                Err(FinalizationFailure::Write(error)) => {
                    emit_failed(
                        emit,
                        &job_id,
                        &mut sequence,
                        index + 1,
                        total,
                        "OUTPUT_WRITE_FAILED",
                        error,
                    );
                    continue;
                }
            };
        if let Some(reason) = stop_reason(&cancellation, started) {
            for output in &moved_outputs {
                let _ = std::fs::remove_file(output);
            }
            emit_stopped(emit, &job_id, &mut sequence, index, total, reason);
            break;
        }
        {
            sequence += 1;
            emit(worker::WorkerEvent {
                job_id: job_id.clone(),
                seq: sequence,
                state: worker::WorkerState::Succeeded,
                completed: index + 1,
                total,
                phase: "done".to_owned(),
                message: result.message,
                outputs: Some(moved_outputs),
                error_code: None,
            });
        }
    }
}

fn run_merged_image_job(
    job_id: String,
    inputs: Vec<PathBuf>,
    output_dir: Option<PathBuf>,
    cancellation: Arc<AtomicBool>,
    started: Instant,
    emit: &mut impl FnMut(worker::WorkerEvent),
) {
    let total = inputs.len();
    let mut sequence = 1u64;
    let output_root = output_dir
        .or_else(|| {
            inputs
                .first()
                .and_then(|path| path.parent().map(Path::to_path_buf))
        })
        .and_then(|path| path.canonicalize().ok());
    let Some(output_root) = output_root else {
        emit_failed(
            emit,
            &job_id,
            &mut sequence,
            total,
            total,
            "OUTPUT_DIR_NOT_FOUND",
            "Could not read the output directory".to_owned(),
        );
        return;
    };
    if inputs.iter().any(|input| {
        !std::fs::symlink_metadata(input).is_ok_and(|metadata| metadata.file_type().is_file())
    }) {
        emit_failed(
            emit,
            &job_id,
            &mut sequence,
            total,
            total,
            "INPUT_NOT_FOUND",
            "The image input is missing or is not a regular file".to_owned(),
        );
        return;
    }
    let canonical_inputs = match inputs
        .iter()
        .map(|input| input.canonicalize())
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(paths) => paths,
        Err(error) => {
            emit_failed(
                emit,
                &job_id,
                &mut sequence,
                total,
                total,
                "INPUT_NOT_FOUND",
                format!("Could not read the input file: {error}"),
            );
            return;
        }
    };
    let workspace =
        match JobWorkspace::create(output_root.join(".minimal-pdf-converter").join(&job_id)) {
            Ok(workspace) => workspace,
            Err(error) => {
                emit_failed(
                    emit,
                    &job_id,
                    &mut sequence,
                    total,
                    total,
                    "OUTPUT_WRITE_FAILED",
                    format!("Could not create the temporary directory: {error}"),
                );
                return;
            }
        };
    let stem = canonical_inputs
        .first()
        .and_then(|path| path.file_stem())
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or("images");
    let stem = format!("{stem}-merged");
    let output = workspace.path.join(format!("{stem}.pdf"));
    let should_stop = || stop_reason(&cancellation, started).is_some();
    let conversion = image_pdf::write_images_pdf(
        &canonical_inputs,
        &output,
        should_stop,
        |completed, page_total| {
            sequence += 1;
            emit(worker::WorkerEvent {
                job_id: job_id.clone(),
                seq: sequence,
                state: worker::WorkerState::Running,
                completed,
                total: page_total,
                phase: "batch".to_owned(),
                message: format!("Merging image {completed} of {page_total}"),
                outputs: None,
                error_code: None,
            });
        },
    );
    match conversion {
        Ok(()) => {}
        Err(error) => {
            if let Some(reason) = stop_reason(&cancellation, started) {
                emit_stopped(emit, &job_id, &mut sequence, 0, total, reason);
            } else {
                emit_failed(
                    emit,
                    &job_id,
                    &mut sequence,
                    total,
                    total,
                    error.code(),
                    error.to_string(),
                );
            }
            return;
        }
    }
    let moved_outputs = match finalize_outputs(&output_root, &stem, "pdf", &[output], || {
        stop_reason(&cancellation, started)
    }) {
        Ok(paths) => paths,
        Err(FinalizationFailure::Stopped(reason)) => {
            emit_stopped(emit, &job_id, &mut sequence, 0, total, reason);
            return;
        }
        Err(FinalizationFailure::Write(error)) => {
            emit_failed(
                emit,
                &job_id,
                &mut sequence,
                total,
                total,
                "OUTPUT_WRITE_FAILED",
                error,
            );
            return;
        }
    };
    if let Some(reason) = stop_reason(&cancellation, started) {
        for path in &moved_outputs {
            let _ = std::fs::remove_file(path);
        }
        emit_stopped(emit, &job_id, &mut sequence, 0, total, reason);
        return;
    }
    sequence += 1;
    emit(worker::WorkerEvent {
        job_id,
        seq: sequence,
        state: worker::WorkerState::Succeeded,
        completed: total,
        total,
        phase: "batch_done".to_owned(),
        message: format!("Created a {total}-page PDF"),
        outputs: Some(moved_outputs),
        error_code: None,
    });
}

fn execute_conversion(
    context: &ConversionContext<'_>,
    report: &mut impl FnMut(&str, String),
) -> Result<EngineOutput, EngineFailure> {
    let should_stop = || stop_reason(context.cancellation, context.started).is_some();
    match context.kind {
        ConversionKind::PdfToTxt => {
            let output = context.temp_root.join(format!("{}.txt", context.stem));
            let request = worker::WorkerRequest {
                protocol: 1,
                job_id: "embedded".to_owned(),
                kind: "pdf_to_txt".to_owned(),
                input: context.input.to_owned(),
                output: output.clone(),
                options: worker::WorkerOptions {
                    pages: context.options.pages.clone(),
                },
            };
            let result = worker::execute_with_control(
                &request,
                |mut event| {
                    report(&event.phase, event.message.clone());
                    event.job_id = "embedded".to_owned();
                },
                should_stop,
            )
            .map_err(|error| EngineFailure {
                code: error.code,
                message: error.message,
            })?;
            Ok(EngineOutput {
                paths: vec![result.output],
                message: format!(
                    "Completed {} pages and {} text blocks",
                    result.page_count, result.text_object_count
                ),
            })
        }
        ConversionKind::PdfToMarkdown => {
            if context
                .options
                .pages
                .as_deref()
                .is_some_and(|pages| !is_all_pages(pages))
            {
                return Err(EngineFailure {
                    code: "UNSUPPORTED_FORMAT",
                    message: "PDF to Markdown currently supports all pages only".to_owned(),
                });
            }
            let output = context.temp_root.join(format!("{}.md", context.stem));
            report(
                "extracting",
                "Extracting the PDF text layer and creating Markdown".to_owned(),
            );
            let result = markdown::pdf_to_markdown(context.input, &output, should_stop).map_err(
                |error| EngineFailure {
                    code: error.code(),
                    message: error.to_string(),
                },
            )?;
            Ok(EngineOutput {
                paths: vec![output],
                message: format!(
                    "Exported {} pages and {} text blocks (no headings or tables inferred)",
                    result.page_count, result.text_object_count
                ),
            })
        }
        ConversionKind::PdfToDoc
        | ConversionKind::PdfToDocx
        | ConversionKind::PdfToOdt
        | ConversionKind::PdfToOdp
        | ConversionKind::PdfToPptx
        | ConversionKind::PdfToPpt
        | ConversionKind::PdfToRtf
        | ConversionKind::PdfToFlatOdtXml => {
            if context
                .options
                .pages
                .as_deref()
                .is_some_and(|pages| !is_all_pages(pages))
            {
                return Err(EngineFailure {
                    code: "UNSUPPORTED_FORMAT",
                    message: "PDF to office documents currently supports all pages only".to_owned(),
                });
            }
            let extension = output_extension(context.kind, context.options);
            let output = context
                .temp_root
                .join(format!("{}.{}", context.stem, extension));
            report(
                "converting",
                format!("Importing PDF and creating {}", extension.to_uppercase()),
            );
            let converted = match context.kind {
                ConversionKind::PdfToDoc => {
                    office::pdf_to_doc(context.input, &output, context.temp_root, should_stop)
                }
                ConversionKind::PdfToDocx => {
                    office::pdf_to_docx(context.input, &output, context.temp_root, should_stop)
                }
                ConversionKind::PdfToOdt => {
                    office::pdf_to_odt(context.input, &output, context.temp_root, should_stop)
                }
                ConversionKind::PdfToOdp => {
                    office::pdf_to_odp(context.input, &output, context.temp_root, should_stop)
                }
                ConversionKind::PdfToPptx => {
                    office::pdf_to_pptx(context.input, &output, context.temp_root, should_stop)
                }
                ConversionKind::PdfToPpt => {
                    office::pdf_to_ppt(context.input, &output, context.temp_root, should_stop)
                }
                ConversionKind::PdfToRtf => {
                    office::pdf_to_rtf(context.input, &output, context.temp_root, should_stop)
                }
                ConversionKind::PdfToFlatOdtXml => office::pdf_to_flat_odt_xml(
                    context.input,
                    &output,
                    context.temp_root,
                    should_stop,
                ),
                _ => unreachable!("only office PDF outputs enter this branch"),
            };
            converted.map_err(|error| EngineFailure {
                code: error.code(),
                message: error.to_string(),
            })?;
            Ok(EngineOutput {
                paths: vec![output],
                message: format!(
                    "Created {} (check each page's layout)",
                    extension.to_uppercase()
                ),
            })
        }
        ConversionKind::DocToPdf | ConversionKind::DocxToPdf => {
            let output = context.temp_root.join(format!("{}.pdf", context.stem));
            report("converting", "Layouting DOCX and creating PDF".to_owned());
            let converted = match context.kind {
                ConversionKind::DocToPdf => {
                    office::doc_to_pdf(context.input, &output, context.temp_root, should_stop)
                }
                _ => office::docx_to_pdf(context.input, &output, context.temp_root, should_stop),
            };
            converted.map_err(|error| EngineFailure {
                code: error.code(),
                message: error.to_string(),
            })?;
            Ok(EngineOutput {
                paths: vec![output],
                message: "Created PDF (check fonts and layout)".to_owned(),
            })
        }
        ConversionKind::TxtToPdf | ConversionKind::RtfToPdf => {
            let output = context.temp_root.join(format!("{}.pdf", context.stem));
            report(
                "converting",
                "Typesetting the text document and creating PDF".to_owned(),
            );
            let converted = match context.kind {
                ConversionKind::TxtToPdf => {
                    office::txt_to_pdf(context.input, &output, context.temp_root, should_stop)
                }
                ConversionKind::RtfToPdf => {
                    office::rtf_to_pdf(context.input, &output, context.temp_root, should_stop)
                }
                _ => unreachable!("only text document inputs enter this branch"),
            };
            converted.map_err(|error| EngineFailure {
                code: error.code(),
                message: error.to_string(),
            })?;
            Ok(EngineOutput {
                paths: vec![output],
                message: "Created PDF (check fonts, line wrapping, and page breaks)".to_owned(),
            })
        }
        ConversionKind::OdtToPdf => {
            let output = context.temp_root.join(format!("{}.pdf", context.stem));
            report("converting", "Layouting ODT and creating PDF".to_owned());
            office::odt_to_pdf(context.input, &output, context.temp_root, should_stop).map_err(
                |error| EngineFailure {
                    code: error.code(),
                    message: error.to_string(),
                },
            )?;
            Ok(EngineOutput {
                paths: vec![output],
                message: "Created PDF (check fonts and layout)".to_owned(),
            })
        }
        ConversionKind::PdfToImage => {
            report("reading", "Reading PDF pages".to_owned());
            let selection =
                PageSelection::parse(context.options.pages.as_deref()).map_err(pdf_failure)?;
            let format = match context.options.image_format.as_ref() {
                Some(ImageFormat::Jpg) => render::ImageFormat::Jpg,
                Some(ImageFormat::Bmp) => render::ImageFormat::Bmp,
                Some(ImageFormat::Gif) => render::ImageFormat::Gif,
                Some(ImageFormat::Webp) => render::ImageFormat::Webp,
                Some(ImageFormat::Tiff) => render::ImageFormat::Tiff,
                Some(ImageFormat::Png) | None => render::ImageFormat::Png,
            };
            let result = render::render_pdf(
                context.input,
                context.temp_root,
                context.stem,
                &render::RenderOptions {
                    selection,
                    dpi: context.options.dpi.unwrap_or(200),
                    format,
                    jpg_quality: context.options.jpg_quality.unwrap_or(85),
                    max_selected_pages: pdf::MAX_PAGES,
                    max_output_bytes: u64::MAX,
                },
                should_stop,
                |index, total, phase| {
                    report(phase, format!("Rendering page {} of {}", index + 1, total));
                },
            )
            .map_err(render_failure)?;
            Ok(EngineOutput {
                paths: result.outputs,
                message: format!("Created {} images", result.selected_page_count),
            })
        }
        ConversionKind::ImageToPdf => {
            report("reading", "Reading images".to_owned());
            let output = context.temp_root.join(format!("{}.pdf", context.stem));
            report("writing", "Creating PDF".to_owned());
            image_pdf::write_image_pdf(context.input, &output, should_stop)
                .map_err(image_pdf_failure)?;
            Ok(EngineOutput {
                paths: vec![output],
                message: "Created a single-page PDF".to_owned(),
            })
        }
        ConversionKind::SvgToPdf => {
            let output = context.temp_root.join(format!("{}.pdf", context.stem));
            report("reading", "Validating SVG".to_owned());
            svg_pdf::svg_to_pdf(context.input, &output, should_stop).map_err(|error| {
                EngineFailure {
                    code: error.code(),
                    message: error.to_string(),
                }
            })?;
            Ok(EngineOutput {
                paths: vec![output],
                message: "Created vector PDF".to_owned(),
            })
        }
        ConversionKind::HtmlToPdf => {
            let output = context.temp_root.join(format!("{}.pdf", context.stem));
            report("converting", "Printing HTML offline".to_owned());
            html_pdf::html_to_pdf(context.input, &output, context.temp_root, should_stop).map_err(
                |error| EngineFailure {
                    code: error.code(),
                    message: error.to_string(),
                },
            )?;
            Ok(EngineOutput {
                paths: vec![output],
                message: "Created PDF (external resources are not loaded)".to_owned(),
            })
        }
        ConversionKind::MarkdownToPdf => {
            let output = context.temp_root.join(format!("{}.pdf", context.stem));
            report(
                "converting",
                "Typesetting Markdown and creating PDF".to_owned(),
            );
            markdown_pdf::markdown_to_pdf(context.input, &output, context.temp_root, should_stop)
                .map_err(|error| EngineFailure {
                code: error.code(),
                message: error.to_string(),
            })?;
            Ok(EngineOutput {
                paths: vec![output],
                message: "Created Markdown PDF".to_owned(),
            })
        }
        ConversionKind::PptToPdf | ConversionKind::PptxToPdf => {
            let output = context.temp_root.join(format!("{}.pdf", context.stem));
            report("converting", "Layouting PPTX and creating PDF".to_owned());
            let converted = match context.kind {
                ConversionKind::PptToPdf => {
                    office::ppt_to_pdf(context.input, &output, context.temp_root, should_stop)
                }
                _ => office::pptx_to_pdf(context.input, &output, context.temp_root, should_stop),
            };
            converted.map_err(|error| EngineFailure {
                code: error.code(),
                message: error.to_string(),
            })?;
            Ok(EngineOutput {
                paths: vec![output],
                message: "Created PDF (check each page's layout)".to_owned(),
            })
        }
        ConversionKind::OdpToPdf => {
            let output = context.temp_root.join(format!("{}.pdf", context.stem));
            report("converting", "Layouting ODP and creating PDF".to_owned());
            office::odp_to_pdf(context.input, &output, context.temp_root, should_stop).map_err(
                |error| EngineFailure {
                    code: error.code(),
                    message: error.to_string(),
                },
            )?;
            Ok(EngineOutput {
                paths: vec![output],
                message: "Created PDF (check each page's layout)".to_owned(),
            })
        }
        ConversionKind::XlsxToPdf | ConversionKind::OdsToPdf => {
            let output = context.temp_root.join(format!("{}.pdf", context.stem));
            report(
                "converting",
                "Layouting the spreadsheet and creating PDF".to_owned(),
            );
            let converted = match context.kind {
                ConversionKind::XlsxToPdf => {
                    office::xlsx_to_pdf(context.input, &output, context.temp_root, should_stop)
                }
                ConversionKind::OdsToPdf => {
                    office::ods_to_pdf(context.input, &output, context.temp_root, should_stop)
                }
                _ => unreachable!("only spreadsheet inputs enter this branch"),
            };
            converted.map_err(|error| EngineFailure {
                code: error.code(),
                message: error.to_string(),
            })?;
            Ok(EngineOutput {
                paths: vec![output],
                message: "Created PDF (check page breaks and tables)".to_owned(),
            })
        }
    }
}

fn output_extension(kind: &ConversionKind, options: &ConversionOptions) -> String {
    match kind {
        ConversionKind::PdfToDoc => "doc".to_owned(),
        ConversionKind::PdfToDocx => "docx".to_owned(),
        ConversionKind::PdfToOdt => "odt".to_owned(),
        ConversionKind::PdfToOdp => "odp".to_owned(),
        ConversionKind::PdfToRtf => "rtf".to_owned(),
        ConversionKind::PdfToFlatOdtXml => "xml".to_owned(),
        ConversionKind::DocToPdf
        | ConversionKind::DocxToPdf
        | ConversionKind::TxtToPdf
        | ConversionKind::RtfToPdf
        | ConversionKind::OdtToPdf
        | ConversionKind::ImageToPdf
        | ConversionKind::SvgToPdf
        | ConversionKind::HtmlToPdf
        | ConversionKind::MarkdownToPdf
        | ConversionKind::PptToPdf
        | ConversionKind::PptxToPdf
        | ConversionKind::OdpToPdf
        | ConversionKind::XlsxToPdf
        | ConversionKind::OdsToPdf => "pdf".to_owned(),
        ConversionKind::PdfToImage => match options.image_format {
            Some(ImageFormat::Jpg) => "jpg".to_owned(),
            Some(ImageFormat::Bmp) => "bmp".to_owned(),
            Some(ImageFormat::Gif) => "gif".to_owned(),
            Some(ImageFormat::Webp) => "webp".to_owned(),
            Some(ImageFormat::Tiff) => "tiff".to_owned(),
            Some(ImageFormat::Png) | None => "png".to_owned(),
        },
        ConversionKind::PdfToTxt => "txt".to_owned(),
        ConversionKind::PdfToMarkdown => "md".to_owned(),
        ConversionKind::PdfToPptx => "pptx".to_owned(),
        ConversionKind::PdfToPpt => "ppt".to_owned(),
    }
}

fn reserve_outputs(
    output_root: &Path,
    stem: &str,
    extension: &str,
    count: usize,
) -> Result<Vec<PathBuf>, String> {
    if count == 0 {
        return Err("No output was produced".to_owned());
    }
    for index in 0..=1000 {
        let prefix = if index == 0 {
            stem.to_owned()
        } else {
            format!("{stem} ({index})")
        };
        let candidates = (0..count)
            .map(|page| {
                let name = if count == 1 {
                    format!("{prefix}.{extension}")
                } else {
                    format!("{prefix}-{:03}.{extension}", page + 1)
                };
                output_root.join(name)
            })
            .collect::<Vec<_>>();
        let mut reserved = Vec::with_capacity(candidates.len());
        let mut conflict = false;
        for candidate in &candidates {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(candidate)
            {
                Ok(_) => reserved.push(candidate.clone()),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    conflict = true;
                    break;
                }
                Err(error) => {
                    for path in &reserved {
                        let _ = std::fs::remove_file(path);
                    }
                    return Err(format!("Could not reserve the output file: {error}"));
                }
            }
        }
        if !conflict {
            return Ok(reserved);
        }
        for path in &reserved {
            let _ = std::fs::remove_file(path);
        }
    }
    Err("Too many output files have the same name".to_owned())
}

fn finalize_outputs(
    output_root: &Path,
    stem: &str,
    extension: &str,
    temporary_outputs: &[PathBuf],
    mut stop: impl FnMut() -> Option<StopReason>,
) -> Result<Vec<String>, FinalizationFailure> {
    if let Some(reason) = stop() {
        return Err(FinalizationFailure::Stopped(reason));
    }
    for temporary in temporary_outputs {
        validate_staged_output(temporary, extension, &mut stop)?;
    }
    if let Some(reason) = stop() {
        return Err(FinalizationFailure::Stopped(reason));
    }
    let _guard = output_move_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let final_outputs = reserve_outputs(output_root, stem, extension, temporary_outputs.len())
        .map_err(FinalizationFailure::Write)?;
    let mut moved_outputs = Vec::with_capacity(temporary_outputs.len());
    for (temporary, final_output) in temporary_outputs.iter().zip(final_outputs.iter()) {
        let result = if let Some(reason) = stop() {
            Err(FinalizationFailure::Stopped(reason))
        } else {
            install_output(temporary, final_output)
                .map_err(|error| {
                    FinalizationFailure::Write(format!("Could not move the output file: {error}"))
                })
                .and_then(|()| match stop() {
                    Some(reason) => Err(FinalizationFailure::Stopped(reason)),
                    None => Ok(()),
                })
        };
        if let Err(error) = result {
            for output in &final_outputs {
                let _ = std::fs::remove_file(output);
            }
            return Err(error);
        }
        moved_outputs.push(final_output.to_string_lossy().into_owned());
    }
    Ok(moved_outputs)
}

fn validate_staged_output(
    path: &Path,
    extension: &str,
    stop: &mut impl FnMut() -> Option<StopReason>,
) -> Result<(), FinalizationFailure> {
    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        FinalizationFailure::Write(format!("Could not inspect the staged output: {error}"))
    })?;
    if !metadata.file_type().is_file() || metadata.len() == 0 {
        return Err(FinalizationFailure::Write(
            "The staged output must be a non-empty regular file".to_owned(),
        ));
    }
    let mut file = File::open(path).map_err(|error| {
        FinalizationFailure::Write(format!("Could not read the staged output: {error}"))
    })?;
    if matches!(extension, "txt" | "md") {
        return validate_utf8_output(&mut file, stop);
    }
    if extension == "rtf" {
        return office::verify_rtf(path).map_err(|error| {
            FinalizationFailure::Write(format!("The staged RTF output is invalid: {error}"))
        });
    }
    if extension == "xml" {
        let mut stopped = None;
        let result = office::verify_flat_odt_xml(path, &mut || {
            if stopped.is_none() {
                stopped = stop();
            }
            stopped.is_some()
        });
        if let Some(reason) = stopped {
            return Err(FinalizationFailure::Stopped(reason));
        }
        return result.map_err(|error| {
            FinalizationFailure::Write(format!(
                "The staged Flat ODF XML output is invalid: {error}"
            ))
        });
    }
    let signature: &[u8] = match extension {
        "pdf" => b"%PDF-",
        "docx" | "odt" | "pptx" | "odp" => b"PK\x03\x04",
        "doc" | "ppt" => b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1",
        "png" => b"\x89PNG\r\n\x1a\n",
        "jpg" => b"\xff\xd8\xff",
        "bmp" => b"BM",
        "gif" => b"GIF8",
        "webp" => b"RIFF",
        "tiff" => b"II*\0",
        _ => {
            return Err(FinalizationFailure::Write(
                "Unsupported output file format".to_owned(),
            ));
        }
    };
    if let Some(reason) = stop() {
        return Err(FinalizationFailure::Stopped(reason));
    }
    let mut header = [0u8; 12];
    let header_length = if extension == "webp" {
        12
    } else {
        signature.len()
    };
    file.read_exact(&mut header[..header_length])
        .map_err(|error| {
            FinalizationFailure::Write(format!("Could not read the output file header: {error}"))
        })?;
    if &header[..signature.len()] != signature {
        return Err(FinalizationFailure::Write(
            "The staged output file format is invalid".to_owned(),
        ));
    }
    if extension == "webp" && &header[8..12] != b"WEBP" {
        return Err(FinalizationFailure::Write(
            "The staged output file format is invalid".to_owned(),
        ));
    }
    if extension == "gif" {
        file.seek(SeekFrom::Start(0)).map_err(|error| {
            FinalizationFailure::Write(format!("Could not read the GIF header: {error}"))
        })?;
        let mut gif_header = [0u8; 6];
        file.read_exact(&mut gif_header).map_err(|error| {
            FinalizationFailure::Write(format!("Could not read the GIF header: {error}"))
        })?;
        if gif_header != *b"GIF87a" && gif_header != *b"GIF89a" {
            return Err(FinalizationFailure::Write(
                "The staged output file format is invalid".to_owned(),
            ));
        }
    }
    if extension == "jpg" {
        file.seek(SeekFrom::End(-2)).map_err(|error| {
            FinalizationFailure::Write(format!("Could not read the JPG trailer: {error}"))
        })?;
        let mut trailer = [0u8; 2];
        file.read_exact(&mut trailer).map_err(|error| {
            FinalizationFailure::Write(format!("Could not read the JPG trailer: {error}"))
        })?;
        if trailer != [0xff, 0xd9] {
            return Err(FinalizationFailure::Write(
                "The staged output file format is invalid".to_owned(),
            ));
        }
    }
    Ok(())
}

fn validate_utf8_output(
    file: &mut File,
    stop: &mut impl FnMut() -> Option<StopReason>,
) -> Result<(), FinalizationFailure> {
    let mut buffer = [0u8; 8192 + 3];
    let mut pending = 0;
    loop {
        if let Some(reason) = stop() {
            return Err(FinalizationFailure::Stopped(reason));
        }
        let read = file.read(&mut buffer[pending..]).map_err(|error| {
            FinalizationFailure::Write(format!("Could not read the TXT output: {error}"))
        })?;
        if read == 0 {
            return if pending == 0 {
                Ok(())
            } else {
                Err(FinalizationFailure::Write(
                    "TXT is not valid UTF-8".to_owned(),
                ))
            };
        }
        let end = pending + read;
        match std::str::from_utf8(&buffer[..end]) {
            Ok(_) => pending = 0,
            Err(error) if error.error_len().is_none() => {
                pending = end - error.valid_up_to();
                buffer.copy_within(error.valid_up_to()..end, 0);
            }
            Err(_) => {
                return Err(FinalizationFailure::Write(
                    "TXT is not valid UTF-8".to_owned(),
                ));
            }
        }
    }
}

#[cfg(windows)]
fn install_output(temporary: &Path, final_output: &Path) -> std::io::Result<()> {
    std::fs::copy(temporary, final_output)?;
    std::fs::remove_file(temporary)
}

#[cfg(not(windows))]
fn install_output(temporary: &Path, final_output: &Path) -> std::io::Result<()> {
    std::fs::rename(temporary, final_output)
}

fn pdf_failure(error: pdf::PdfError) -> EngineFailure {
    let code = match error {
        pdf::PdfError::InvalidPdf | pdf::PdfError::CorruptedPdf => "CORRUPTED_PDF",
        pdf::PdfError::UnsupportedStructure | pdf::PdfError::UnsupportedStreamLength => {
            "UNSUPPORTED_PDF_STRUCTURE"
        }
        pdf::PdfError::StreamLimitExceeded | pdf::PdfError::CMapLimitExceeded => {
            "PDF_STREAM_TOO_LARGE"
        }
        pdf::PdfError::NoTextLayer => "NO_TEXT_LAYER",
        pdf::PdfError::InvalidPageRange => "INVALID_PAGES",
        pdf::PdfError::PageLimitExceeded => "PAGE_LIMIT_EXCEEDED",
        pdf::PdfError::UnsupportedFilter(_) | pdf::PdfError::Decompression => "CONVERSION_FAILED",
        pdf::PdfError::Cancelled => "CANCELLED",
        pdf::PdfError::Io(_) => "INPUT_READ_FAILED",
    };
    EngineFailure {
        code,
        message: error.to_string(),
    }
}

fn render_failure(error: render::RenderError) -> EngineFailure {
    match error {
        render::RenderError::Pdf(error) => pdf_failure(error),
        render::RenderError::Io(error) => EngineFailure {
            code: "OUTPUT_WRITE_FAILED",
            message: error.to_string(),
        },
        render::RenderError::PixelLimit => EngineFailure {
            code: "CONVERSION_FAILED",
            message: "The total image pixel count exceeds the safety limit".to_owned(),
        },
        render::RenderError::Encoding => EngineFailure {
            code: "OUTPUT_WRITE_FAILED",
            message: "Image encoding failed".to_owned(),
        },
        render::RenderError::OutputLimit => EngineFailure {
            code: "OUTPUT_WRITE_FAILED",
            message: error.to_string(),
        },
        #[cfg(target_os = "windows")]
        render::RenderError::NativeUnavailable => EngineFailure {
            code: "WORKER_NOT_INSTALLED",
            message: error.to_string(),
        },
        #[cfg(target_os = "windows")]
        render::RenderError::NativeFailed => EngineFailure {
            code: "CONVERSION_FAILED",
            message: error.to_string(),
        },
    }
}

fn image_pdf_failure(error: image_pdf::ImagePdfError) -> EngineFailure {
    let code = error.code();
    EngineFailure {
        code,
        message: error.to_string(),
    }
}

fn emit_queued(
    emit: &mut impl FnMut(worker::WorkerEvent),
    job_id: &str,
    sequence: &mut u64,
    total: usize,
) {
    *sequence += 1;
    emit(worker::WorkerEvent {
        job_id: job_id.to_owned(),
        seq: *sequence,
        state: worker::WorkerState::Queued,
        completed: 0,
        total,
        phase: "queued".to_owned(),
        message: "Task queued".to_owned(),
        outputs: None,
        error_code: None,
    });
}

fn emit_stopped(
    emit: &mut impl FnMut(worker::WorkerEvent),
    job_id: &str,
    sequence: &mut u64,
    completed: usize,
    total: usize,
    reason: StopReason,
) {
    *sequence += 1;
    let (state, phase, message, error_code) = match reason {
        StopReason::Cancelled => (
            worker::WorkerState::Cancelled,
            "cancelled",
            "Task cancelled",
            "CANCELLED",
        ),
        StopReason::TimedOut => (
            worker::WorkerState::TimedOut,
            "timed_out",
            "Task timed out",
            "TIMEOUT",
        ),
    };
    emit(worker::WorkerEvent {
        job_id: job_id.to_owned(),
        seq: *sequence,
        state,
        completed,
        total,
        phase: phase.to_owned(),
        message: message.to_owned(),
        outputs: None,
        error_code: Some(error_code.to_owned()),
    });
}

fn emit_failed(
    emit: &mut impl FnMut(worker::WorkerEvent),
    job_id: &str,
    sequence: &mut u64,
    completed: usize,
    total: usize,
    error_code: &str,
    message: String,
) {
    *sequence += 1;
    emit(worker::WorkerEvent {
        job_id: job_id.to_owned(),
        seq: *sequence,
        state: worker::WorkerState::Failed,
        completed,
        total,
        phase: "failed".to_owned(),
        message,
        outputs: None,
        error_code: Some(error_code.to_owned()),
    });
}

fn main() {
    #[cfg(target_os = "windows")]
    if render::windows_render::is_worker_process() {
        std::process::exit(render::windows_render::run_worker_stdio());
    }
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--electron-bridge")) {
        if let Err(error) = electron_bridge::run_stdio() {
            eprintln!("electron bridge exited: {error}");
            std::process::exit(1);
        }
        return;
    }
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            get_self_check,
            get_diagnostic_info,
            inspect_input_files,
            start_job,
            cancel_job,
            choose_input_files,
            choose_output_dir,
            open_path,
            repair_install
        ])
        .run(tauri::generate_context!())
        .expect("error while running minimal PDF converter");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn output_fixture(label: &str) -> JobWorkspace {
        JobWorkspace::create(std::env::temp_dir().join(format!(
            "minimal-pdf-converter-{label}-{}",
            Uuid::new_v4().simple()
        )))
        .expect("create fixture directory")
    }

    #[test]
    fn workspace_refuses_preexisting_job_directory_without_deleting_its_contents() {
        let root = output_fixture("workspace-existing-job");
        let job = root.path.join(".minimal-pdf-converter/job_existing");
        fs::create_dir_all(&job).expect("create preexisting job directory");
        let existing = job.join("keep.txt");
        fs::write(&existing, b"keep this file").expect("write preexisting file");

        let error = JobWorkspace::create(job)
            .err()
            .expect("preexisting job directory must be rejected");
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(
            fs::read(existing).expect("read existing file"),
            b"keep this file"
        );
    }

    #[cfg(unix)]
    #[test]
    fn workspace_refuses_symlinked_staging_parent_without_touching_target() {
        use std::os::unix::fs::symlink;

        let root = output_fixture("workspace-link-parent");
        let outside = output_fixture("workspace-link-target");
        let existing = outside.path.join("keep.txt");
        fs::write(&existing, b"keep this file").expect("write target file");
        symlink(&outside.path, root.path.join(".minimal-pdf-converter"))
            .expect("create staging parent symlink");

        let error = JobWorkspace::create(root.path.join(".minimal-pdf-converter/job_link"))
            .err()
            .expect("symlinked staging parent must be rejected");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert!(!outside.path.join("job_link").exists());
        assert_eq!(
            fs::read(existing).expect("read target file"),
            b"keep this file"
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_pdf_image_jobs_use_a_single_native_render_lane() {
        assert!(matches!(
            JobLane::for_kind(&ConversionKind::PdfToImage),
            JobLane::NativeRender
        ));
        assert!(matches!(
            JobLane::for_kind(&ConversionKind::PdfToPptx),
            JobLane::Document
        ));
        assert_eq!(JobLane::NativeRender.limit(), 1);
        assert_eq!(JobLane::for_kind(&ConversionKind::PdfToTxt).limit(), 2);
    }

    #[test]
    fn maps_unsupported_pdf_structure_to_stable_engine_code() {
        assert_eq!(
            pdf_failure(pdf::PdfError::UnsupportedStructure).code,
            "UNSUPPORTED_PDF_STRUCTURE"
        );
        assert_eq!(
            pdf_failure(pdf::PdfError::UnsupportedStreamLength).code,
            "UNSUPPORTED_PDF_STRUCTURE"
        );
        assert_eq!(
            pdf_failure(pdf::PdfError::StreamLimitExceeded).code,
            "PDF_STREAM_TOO_LARGE"
        );
        assert_eq!(
            pdf_failure(pdf::PdfError::CMapLimitExceeded).code,
            "PDF_STREAM_TOO_LARGE"
        );
    }

    #[test]
    fn accepts_only_supported_extensions() {
        assert!(supports_extension(
            &ConversionKind::PdfToTxt,
            Path::new("/tmp/file.PDF")
        ));
        assert!(!supports_extension(
            &ConversionKind::PdfToTxt,
            Path::new("/tmp/file.docx")
        ));
        assert!(supports_extension(
            &ConversionKind::DocxToPdf,
            Path::new("/tmp/file.DOCX")
        ));
        assert!(supports_extension(
            &ConversionKind::PdfToPptx,
            Path::new("/tmp/slides.PDF")
        ));
        assert!(!supports_extension(
            &ConversionKind::PdfToPptx,
            Path::new("/tmp/slides.pptx")
        ));
        assert!(supports_extension(
            &ConversionKind::PptxToPdf,
            Path::new("/tmp/slides.PPTX")
        ));
        assert!(!supports_extension(
            &ConversionKind::PptxToPdf,
            Path::new("/tmp/slides.ppt")
        ));
        assert!(supports_extension(
            &ConversionKind::ImageToPdf,
            Path::new("/tmp/photo.JPEG")
        ));
        assert!(supports_extension(
            &ConversionKind::ImageToPdf,
            Path::new("/tmp/photo.png")
        ));
        assert!(supports_extension(
            &ConversionKind::ImageToPdf,
            Path::new("/tmp/photo.GIF")
        ));
        assert!(supports_extension(
            &ConversionKind::ImageToPdf,
            Path::new("/tmp/photo.webp")
        ));
        assert!(supports_extension(
            &ConversionKind::ImageToPdf,
            Path::new("/tmp/photo.BMP")
        ));
        assert!(supports_extension(
            &ConversionKind::ImageToPdf,
            Path::new("/tmp/scan.TIF")
        ));
        assert!(supports_extension(
            &ConversionKind::ImageToPdf,
            Path::new("/tmp/scan.tiff")
        ));
        assert!(!supports_extension(
            &ConversionKind::ImageToPdf,
            Path::new("/tmp/photo.pdf")
        ));
        for (kind, accepted, rejected) in [
            (ConversionKind::PdfToOdt, "file.PDF", "file.odt"),
            (ConversionKind::PdfToOdp, "file.PDF", "file.odp"),
            (ConversionKind::PdfToRtf, "file.PDF", "file.rtf"),
            (ConversionKind::PdfToFlatOdtXml, "file.PDF", "file.xml"),
            (ConversionKind::PdfToDoc, "file.PDF", "file.doc"),
            (ConversionKind::PdfToPpt, "file.PDF", "file.ppt"),
            (ConversionKind::PdfToMarkdown, "file.PDF", "file.md"),
            (
                ConversionKind::MarkdownToPdf,
                "file.MARKDOWN",
                "file.md.exe",
            ),
            (ConversionKind::DocToPdf, "file.DOC", "file.docx"),
            (ConversionKind::PptToPdf, "file.PPT", "file.pptx"),
            (ConversionKind::TxtToPdf, "file.TXT", "file.rtf"),
            (ConversionKind::RtfToPdf, "file.RTF", "file.txt"),
            (ConversionKind::SvgToPdf, "file.SVG", "file.svg.exe"),
            (ConversionKind::OdtToPdf, "file.ODT", "file.doc"),
            (ConversionKind::OdpToPdf, "file.ODP", "file.ppt"),
            (ConversionKind::XlsxToPdf, "file.XLSX", "file.xls"),
            (ConversionKind::OdsToPdf, "file.ODS", "file.xlsx"),
        ] {
            assert!(supports_extension(&kind, Path::new(accepted)), "{accepted}");
            assert!(
                !supports_extension(&kind, Path::new(rejected)),
                "{rejected}"
            );
        }
    }

    #[test]
    fn image_to_pdf_kind_matches_frontend_ipc_name() {
        let kind: ConversionKind =
            serde_json::from_str("\"image_to_pdf\"").expect("deserialize UI kind");
        assert!(matches!(kind, ConversionKind::ImageToPdf));
        assert_eq!(
            serde_json::to_string(&kind).expect("serialize UI kind"),
            "\"image_to_pdf\""
        );
        assert_eq!(JobLane::for_kind(&kind).limit(), 2);
    }

    #[test]
    fn pptx_to_pdf_kind_matches_frontend_ipc_name() {
        let kind: ConversionKind =
            serde_json::from_str("\"pptx_to_pdf\"").expect("deserialize PPTX conversion kind");
        assert!(matches!(kind, ConversionKind::PptxToPdf));
        assert_eq!(
            serde_json::to_string(&kind).expect("serialize PPTX conversion kind"),
            "\"pptx_to_pdf\""
        );
        assert_eq!(JobLane::for_kind(&kind).limit(), 1);
    }

    #[test]
    fn new_formats_match_frontend_ipc_and_output_extensions() {
        for (name, extension) in [
            ("pdf_to_odt", "odt"),
            ("pdf_to_odp", "odp"),
            ("pdf_to_rtf", "rtf"),
            ("pdf_to_flat_odt_xml", "xml"),
            ("pdf_to_doc", "doc"),
            ("pdf_to_ppt", "ppt"),
            ("doc_to_pdf", "pdf"),
            ("ppt_to_pdf", "pdf"),
            ("pdf_to_markdown", "md"),
            ("txt_to_pdf", "pdf"),
            ("rtf_to_pdf", "pdf"),
            ("odt_to_pdf", "pdf"),
            ("odp_to_pdf", "pdf"),
            ("xlsx_to_pdf", "pdf"),
            ("ods_to_pdf", "pdf"),
        ] {
            let kind: ConversionKind =
                serde_json::from_str(&format!("\"{name}\"")).expect("deserialize new UI kind");
            assert_eq!(
                serde_json::to_string(&kind).expect("serialize kind"),
                format!("\"{name}\"")
            );
            assert_eq!(
                output_extension(&kind, &ConversionOptions::default()),
                extension
            );
        }
        assert_eq!(JobLane::for_kind(&ConversionKind::OdsToPdf).limit(), 1);
        assert_eq!(JobLane::for_kind(&ConversionKind::PdfToPptx).limit(), 1);
        assert_eq!(JobLane::for_kind(&ConversionKind::PdfToMarkdown).limit(), 2);
        for removed in [
            "pdf_to_html",
            "pdf_to_epub",
            "epub_to_pdf",
            "pdf_to_cbz",
            "comic_to_pdf",
        ] {
            assert!(serde_json::from_str::<ConversionKind>(&format!("\"{removed}\"")).is_err());
        }
        for format in ["png", "jpg", "bmp", "gif", "webp", "tiff"] {
            let format: ImageFormat =
                serde_json::from_str(&format!("\"{format}\"")).expect("parse image format");
            let options = ConversionOptions {
                image_format: Some(format),
                ..ConversionOptions::default()
            };
            assert_eq!(
                output_extension(&ConversionKind::PdfToImage, &options),
                format!("{format:?}").to_ascii_lowercase()
            );
        }
    }

    #[test]
    fn markdown_rejects_partial_page_requests_instead_of_ignoring_them() {
        let root = output_fixture("markdown-page-selection");
        let input = root.path.join("source.pdf");
        pdf_writer::write_text_pdf(&input, "page text").expect("write PDF fixture");
        let temp_root = root.path.join("staging");
        fs::create_dir(&temp_root).expect("create staging directory");
        let options = ConversionOptions {
            pages: Some("1".to_owned()),
            ..ConversionOptions::default()
        };
        let cancellation = AtomicBool::new(false);
        let context = ConversionContext {
            kind: &ConversionKind::PdfToMarkdown,
            input: &input,
            temp_root: &temp_root,
            stem: "source",
            options: &options,
            cancellation: &cancellation,
            started: Instant::now(),
        };

        let error = execute_conversion(&context, &mut |_, _| {})
            .expect_err("partial Markdown selection must fail explicitly");

        assert_eq!(error.code, "UNSUPPORTED_FORMAT");
        assert!(!temp_root.join("source.md").exists());
    }

    #[test]
    fn editable_pptx_rejects_partial_page_requests_without_starting_office() {
        let root = output_fixture("pptx-page-selection");
        let input = root.path.join("source.pdf");
        pdf_writer::write_text_pdf(&input, "page text").expect("write PDF fixture");
        let temp_root = root.path.join("staging");
        fs::create_dir(&temp_root).expect("create staging directory");
        let options = ConversionOptions {
            pages: Some("1".to_owned()),
            ..ConversionOptions::default()
        };
        let cancellation = AtomicBool::new(false);
        let context = ConversionContext {
            kind: &ConversionKind::PdfToPptx,
            input: &input,
            temp_root: &temp_root,
            stem: "source",
            options: &options,
            cancellation: &cancellation,
            started: Instant::now(),
        };

        let error = execute_conversion(&context, &mut |_, _| {})
            .expect_err("partial PDF-to-PPTX selection must fail explicitly");
        assert_eq!(error.code, "UNSUPPORTED_FORMAT");
        assert!(!temp_root.join("source.pptx").exists());
    }

    #[test]
    fn image_to_pdf_request_enforces_engine_input_limit() {
        let root = output_fixture("image-input-limit");
        let input = root.path.join("large.jpg");
        File::create(&input)
            .expect("create image fixture")
            .set_len(image_pdf::MAX_INPUT_BYTES + 1)
            .expect("set image size");
        let request = StartJobRequest {
            kind: ConversionKind::ImageToPdf,
            inputs: vec![input.to_string_lossy().into_owned()],
            output_dir: None,
            options: ConversionOptions::default(),
        };
        assert!(matches!(
            validate_request(&request),
            Err(AppError::InputTooLarge)
        ));
    }

    #[test]
    fn image_merge_option_is_typed_and_bounded_at_ipc_boundary() {
        let root = output_fixture("image-merge-ipc");
        let image = root.path.join("first.png");
        fs::write(&image, b"not decoded at request validation").expect("image path");
        let mut request: StartJobRequest = serde_json::from_value(serde_json::json!({
            "kind": "image_to_pdf",
            "inputs": [image.to_string_lossy()],
            "outputDir": root.path.to_string_lossy(),
            "options": { "mergeImages": true }
        }))
        .expect("deserialize mergeImages");
        assert!(request.options.merge_images);
        assert!(validate_request(&request).is_ok());
        request.inputs = vec![image.to_string_lossy().into_owned(); 201];
        assert!(matches!(
            validate_request(&request),
            Err(AppError::ImageBatchTooLarge)
        ));
        request.inputs.truncate(1);
        request.kind = ConversionKind::PdfToTxt;
        assert!(matches!(
            validate_request(&request),
            Err(AppError::UnsupportedFormat)
        ));
        request.kind = ConversionKind::ImageToPdf;
        let mut bad_option = serde_json::json!({
            "kind": "image_to_pdf",
            "inputs": [image.to_string_lossy()],
            "options": { "mergeImages": "yes" }
        });
        assert!(serde_json::from_value::<StartJobRequest>(bad_option.take()).is_err());
    }

    #[test]
    fn merged_image_job_produces_one_ordered_pdf_and_fails_as_a_batch() {
        let root = output_fixture("image-merge-run");
        let red = root.path.join("red.png");
        let blue = root.path.join("blue.png");
        for (path, pixels) in [(&red, &[255u8, 0, 0][..]), (&blue, &[0u8, 0, 255][..])] {
            let mut encoder = png::Encoder::new(File::create(path).expect("create PNG"), 1, 1);
            encoder.set_color(png::ColorType::Rgb);
            encoder.set_depth(png::BitDepth::Eight);
            encoder
                .write_header()
                .expect("PNG header")
                .write_image_data(pixels)
                .expect("PNG data");
        }
        let inputs = vec![red.clone(), blue.clone()];
        let mut events = Vec::new();
        run_job(
            "job_image_merge_success".to_owned(),
            ConversionKind::ImageToPdf,
            inputs.clone(),
            Some(root.path.clone()),
            ConversionOptions {
                merge_images: true,
                ..Default::default()
            },
            Arc::new(AtomicBool::new(false)),
            &mut |event| events.push(event),
        );
        let success = events.last().expect("terminal event");
        assert!(matches!(success.state, worker::WorkerState::Succeeded));
        assert_eq!((success.completed, success.total), (2, 2));
        let outputs = success.outputs.as_ref().expect("one merged output");
        assert_eq!(outputs.len(), 1);
        assert!(Path::new(&outputs[0])
            .file_name()
            .is_some_and(|name| name == "red-merged.pdf"));
        let pdf = fs::read(&outputs[0]).expect("read merged PDF");
        assert!(pdf
            .windows(b"/Count 2".len())
            .any(|chunk| chunk == b"/Count 2"));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event.state, worker::WorkerState::Running))
                .count(),
            2
        );

        let malformed = root.path.join("broken.bmp");
        fs::write(&malformed, b"BMbroken").expect("broken image");
        let mut failed = Vec::new();
        run_job(
            "job_image_merge_failure".to_owned(),
            ConversionKind::ImageToPdf,
            vec![red, malformed],
            Some(root.path.clone()),
            ConversionOptions {
                merge_images: true,
                ..Default::default()
            },
            Arc::new(AtomicBool::new(false)),
            &mut |event| failed.push(event),
        );
        let failure = failed.last().expect("failure event");
        assert!(matches!(failure.state, worker::WorkerState::Failed));
        assert_eq!((failure.completed, failure.total), (2, 2));
        assert!(failure.outputs.is_none());
        assert_eq!(
            fs::read_dir(&root.path)
                .expect("output directory")
                .filter_map(Result::ok)
                .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "pdf"))
                .count(),
            1
        );

        let mut cancelled = Vec::new();
        run_job(
            "job_image_merge_cancel".to_owned(),
            ConversionKind::ImageToPdf,
            vec![blue],
            Some(root.path.clone()),
            ConversionOptions {
                merge_images: true,
                ..Default::default()
            },
            Arc::new(AtomicBool::new(true)),
            &mut |event| cancelled.push(event),
        );
        assert!(matches!(
            cancelled.last().map(|event| &event.state),
            Some(worker::WorkerState::Cancelled)
        ));
    }

    #[test]
    fn validates_page_ranges() {
        assert!(valid_pages("1-3,8"));
        assert!(!valid_pages("0,3"));
        assert!(!valid_pages("1--3"));
    }

    #[test]
    fn validates_a_real_input_path() {
        let path = std::env::temp_dir().join("minimal-pdf-converter-test.pdf");
        fs::write(&path, b"%PDF-1.7").expect("write fixture");
        let request = StartJobRequest {
            kind: ConversionKind::PdfToTxt,
            inputs: vec![path.to_string_lossy().into_owned()],
            output_dir: None,
            options: ConversionOptions::default(),
        };
        assert!(validate_request(&request).is_ok());
        let _ = fs::remove_file(path);
    }

    #[test]
    fn rejects_system_output_directories() {
        assert!(matches!(
            validate_output_dir(Path::new("/")),
            Err(AppError::OutputPathNotAllowed)
        ));
        assert!(validate_output_dir(&std::env::temp_dir()).is_ok());
    }

    #[test]
    fn inspects_absolute_regular_files() {
        let path = std::env::temp_dir().join("minimal-pdf-converter-inspect.pdf");
        fs::write(&path, b"fixture").expect("write fixture");
        let result =
            inspect_input_files(vec![path.to_string_lossy().into_owned()]).expect("inspect file");
        assert_eq!(result[0].name, "minimal-pdf-converter-inspect.pdf");
        assert_eq!(result[0].size, 7);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn rejects_relative_inspection_paths() {
        assert_eq!(
            inspect_input_files(vec!["relative.pdf".to_owned()]).expect_err("must reject"),
            "INPUT_PATH_REQUIRED: The input path must be absolute"
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_input_before_canonicalization() {
        use std::os::unix::fs::symlink;

        let root = output_fixture("symlinked-input");
        let target = root.path.join("target.pdf");
        let input = root.path.join("linked.pdf");
        fs::write(&target, b"%PDF-1.7\n%%EOF\n").expect("write target PDF");
        symlink(&target, &input).expect("create input symlink");

        assert_eq!(
            inspect_input_files(vec![input.to_string_lossy().into_owned()])
                .expect_err("inspection must reject symlink"),
            "INPUT_NOT_FOUND: The input file does not exist"
        );
        let request = StartJobRequest {
            kind: ConversionKind::PdfToTxt,
            inputs: vec![input.to_string_lossy().into_owned()],
            output_dir: None,
            options: ConversionOptions::default(),
        };
        assert!(matches!(
            validate_request(&request),
            Err(AppError::InputNotFound)
        ));
    }

    #[test]
    fn embedded_engine_self_check_is_healthy() {
        assert_eq!(engine_probe_components(), (true, true, true));
    }

    #[test]
    fn finalization_avoids_overwriting_existing_output() {
        let root = std::env::temp_dir().join(format!(
            "minimal-pdf-converter-output-test-{}",
            Uuid::new_v4().simple()
        ));
        fs::create_dir_all(&root).expect("create output root");
        let existing = root.join("report.txt");
        fs::write(&existing, b"keep").expect("write existing output");
        let temporary = root.join("temporary.txt");
        fs::write(&temporary, b"new").expect("write temporary output");
        let outputs = finalize_outputs(&root, "report", "txt", &[temporary.clone()], || None)
            .expect("finalize output");
        assert_eq!(fs::read(&existing).expect("read existing"), b"keep");
        assert_eq!(fs::read(&outputs[0]).expect("read generated"), b"new");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn finalization_rejects_wrong_signatures_before_reserving_outputs() {
        let root = output_fixture("wrong-output-signatures");
        let workspace = JobWorkspace::create(root.path.join(".minimal-pdf-converter/job_invalid"))
            .expect("create job workspace");
        for (extension, bytes) in [
            ("pdf", b"not a PDF".as_slice()),
            ("docx", b"not a DOCX".as_slice()),
            ("odt", b"not an ODT".as_slice()),
            ("odp", b"not an ODP".as_slice()),
            ("cbz", b"not a CBZ".as_slice()),
            ("png", b"not a PNG".as_slice()),
            ("jpg", b"not a JPG".as_slice()),
            ("bmp", b"not a BMP".as_slice()),
            ("gif", b"not a GIF".as_slice()),
            ("webp", b"not a WEBP".as_slice()),
            ("tiff", b"not a TIFF".as_slice()),
        ] {
            let staged = workspace.path.join(format!("staged.{extension}"));
            fs::write(&staged, bytes).expect("write invalid staged output");
            let outcome =
                finalize_outputs(&root.path, "report", extension, &[staged.clone()], || None);
            assert!(matches!(outcome, Err(FinalizationFailure::Write(_))));
            assert_eq!(fs::read(staged).expect("read staged output"), bytes);
            assert!(!root.path.join(format!("report.{extension}")).exists());
        }
    }

    #[test]
    fn finalization_rejects_empty_staged_output_and_can_retry() {
        let root = output_fixture("empty-output-retry");
        let workspace = JobWorkspace::create(root.path.join(".minimal-pdf-converter/job_empty"))
            .expect("create job workspace");
        let staged = workspace.path.join("report.pdf");
        fs::write(&staged, []).expect("write empty staged output");
        let outcome = finalize_outputs(&root.path, "report", "pdf", &[staged.clone()], || None);
        assert!(matches!(outcome, Err(FinalizationFailure::Write(_))));
        assert!(!root.path.join("report.pdf").exists());

        pdf_writer::write_text_pdf(&staged, "retry").expect("write valid PDF on retry");
        let outputs = finalize_outputs(&root.path, "report", "pdf", &[staged], || None)
            .expect("retry succeeds");
        assert_eq!(
            outputs,
            vec![root.path.join("report.pdf").to_string_lossy()]
        );
        assert!(fs::read(&outputs[0])
            .expect("read installed PDF")
            .starts_with(b"%PDF-"));
    }

    #[cfg(unix)]
    #[test]
    fn finalization_rejects_symlinked_staged_output_without_touching_target() {
        use std::os::unix::fs::symlink;

        let root = output_fixture("symlinked-staged-output");
        let outside = output_fixture("symlinked-output-target");
        let target = outside.path.join("target.txt");
        fs::write(&target, b"keep this file").expect("write symlink target");
        let workspace = JobWorkspace::create(root.path.join(".minimal-pdf-converter/job_link"))
            .expect("create job workspace");
        let staged = workspace.path.join("report.txt");
        symlink(&target, &staged).expect("create output symlink");

        let outcome = finalize_outputs(&root.path, "report", "txt", &[staged], || None);
        assert!(matches!(outcome, Err(FinalizationFailure::Write(_))));
        assert!(!root.path.join("report.txt").exists());
        assert_eq!(
            fs::read(&target).expect("read symlink target"),
            b"keep this file"
        );
    }

    #[test]
    fn finalization_checks_utf8_across_read_chunks_and_rejects_invalid_text() {
        let root = output_fixture("utf8-output");
        let workspace = JobWorkspace::create(root.path.join(".minimal-pdf-converter/job_utf8"))
            .expect("create job workspace");
        let staged = workspace.path.join("valid.txt");
        let mut text = vec![b'a'; 8194];
        text.extend_from_slice("你好".as_bytes());
        fs::write(&staged, &text).expect("write UTF-8 across chunk boundary");
        let outputs = finalize_outputs(&root.path, "valid", "txt", &[staged], || None)
            .expect("split UTF-8 character stays valid");
        assert_eq!(fs::read(&outputs[0]).expect("read installed TXT"), text);

        let invalid = workspace.path.join("invalid.txt");
        fs::write(&invalid, [0xe4, 0xb8]).expect("write truncated UTF-8 sequence");
        let outcome = finalize_outputs(&root.path, "invalid", "txt", &[invalid], || None);
        assert!(matches!(outcome, Err(FinalizationFailure::Write(_))));
        assert!(!root.path.join("invalid.txt").exists());
    }

    #[test]
    fn finalization_accepts_rendered_jpg_and_rejects_missing_end_marker() {
        let root = output_fixture("jpg-output");
        let workspace = JobWorkspace::create(root.path.join(".minimal-pdf-converter/job_jpg"))
            .expect("create job workspace");
        let staged = workspace.path.join("valid.jpg");
        render::write_jpg(&staged, 1, 1, &[240, 20, 20], 85).expect("render fixture JPG");
        let mut no_stop = || None;
        validate_staged_output(&staged, "jpg", &mut no_stop).expect("JPG signature is valid");

        let mut truncated = fs::read(&staged).expect("read JPG fixture");
        truncated.pop();
        let damaged = workspace.path.join("damaged.jpg");
        fs::write(&damaged, truncated).expect("write truncated JPG");
        let outcome = finalize_outputs(&root.path, "damaged", "jpg", &[damaged], || None);
        assert!(matches!(outcome, Err(FinalizationFailure::Write(_))));
        assert!(!root.path.join("damaged.jpg").exists());
    }

    #[test]
    fn finalization_recognizes_active_office_and_image_signatures() {
        let root = output_fixture("new-output-signatures");
        let workspace = JobWorkspace::create(root.path.join(".minimal-pdf-converter/job_formats"))
            .expect("create job workspace");
        for (extension, bytes) in [
            ("odt", b"PK\x03\x04package".as_slice()),
            ("odp", b"PK\x03\x04package".as_slice()),
            (
                "doc",
                b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1document".as_slice(),
            ),
            ("ppt", b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1slides".as_slice()),
            ("bmp", b"BMbitmap".as_slice()),
            ("gif", b"GIF89aimage".as_slice()),
            ("webp", b"RIFF\x04\x00\x00\x00WEBPdata".as_slice()),
            ("tiff", b"II*\0image".as_slice()),
        ] {
            let staged = workspace.path.join(format!("valid.{extension}"));
            fs::write(&staged, bytes).expect("write staged output");
            let mut no_stop = || None;
            validate_staged_output(&staged, extension, &mut no_stop)
                .unwrap_or_else(|error| panic!("{extension} signature rejected: {error:?}"));
        }

        let retired = workspace.path.join("retired.cbz");
        fs::write(&retired, b"PK\x03\x04package").expect("write retired format");
        assert!(matches!(
            validate_staged_output(&retired, "cbz", &mut || None),
            Err(FinalizationFailure::Write(_))
        ));

        let fake_webp = workspace.path.join("fake.webp");
        fs::write(&fake_webp, b"RIFF\x04\x00\x00\x00NOPEdata").expect("write fake WebP");
        let mut no_stop = || None;
        assert!(matches!(
            validate_staged_output(&fake_webp, "webp", &mut no_stop),
            Err(FinalizationFailure::Write(_))
        ));
    }

    #[test]
    fn finalization_validates_rtf_and_flat_odf_xml_structures() {
        let root = output_fixture("office-text-output-signatures");
        let workspace =
            JobWorkspace::create(root.path.join(".minimal-pdf-converter/job_office_text"))
                .expect("create job workspace");
        let rtf = workspace.path.join("valid.rtf");
        fs::write(&rtf, b"{\\rtf1\\ansi validated}\r\n").expect("write valid RTF");
        let mut no_stop = || None;
        validate_staged_output(&rtf, "rtf", &mut no_stop).expect("accept valid RTF");

        let xml = workspace.path.join("valid.xml");
        fs::write(
            &xml,
            concat!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>",
                "<office:document ",
                "xmlns:office=\"urn:oasis:names:tc:opendocument:xmlns:office:1.0\" ",
                "office:mimetype=\"application/vnd.oasis.opendocument.text\" ",
                "office:version=\"1.3\">",
                "<office:body><office:text/></office:body>",
                "</office:document>"
            ),
        )
        .expect("write valid Flat ODF XML");
        validate_staged_output(&xml, "xml", &mut no_stop).expect("accept valid Flat ODF XML");

        let fake_xml = workspace.path.join("fake.xml");
        fs::write(&fake_xml, "<document>not Flat ODF</document>").expect("write non-ODF XML");
        assert!(matches!(
            validate_staged_output(&fake_xml, "xml", &mut no_stop),
            Err(FinalizationFailure::Write(_))
        ));
    }

    #[test]
    fn cancellation_before_docx_conversion_does_not_publish_output() {
        let root = output_fixture("cancel-after-write");
        let input = root.path.join("source.docx");
        docx::write_text_docx(&input, &["cancel me".to_owned()]).expect("write DOCX fixture");
        let workspace = JobWorkspace::create(root.path.join(".minimal-pdf-converter/job_cancel"))
            .expect("create job workspace");
        let cancellation = AtomicBool::new(false);
        let kind = ConversionKind::DocxToPdf;
        let options = ConversionOptions::default();
        let started = Instant::now();
        let context = ConversionContext {
            kind: &kind,
            input: &input,
            temp_root: &workspace.path,
            stem: "source",
            options: &options,
            cancellation: &cancellation,
            started,
        };
        let mut reached_conversion = false;
        let result = execute_conversion(&context, &mut |phase, _| {
            if phase == "converting" {
                reached_conversion = true;
                cancellation.store(true, Ordering::Release);
            }
        });
        assert!(reached_conversion);
        assert!(matches!(
            result,
            Err(EngineFailure {
                code: "CANCELLED",
                ..
            })
        ));
        assert!(!root.path.join("source.pdf").exists());
        assert!(!workspace.path.join("source.pdf").exists());
        drop(workspace);
        assert!(!root.path.join(".minimal-pdf-converter/job_cancel").exists());
    }

    #[test]
    fn cancellation_between_page_installs_rolls_back_all_reserved_outputs() {
        let root = output_fixture("cancel-between-installs");
        let workspace = JobWorkspace::create(root.path.join(".minimal-pdf-converter/job_cancel"))
            .expect("create job workspace");
        let first = workspace.path.join("first.txt");
        let second = workspace.path.join("second.txt");
        fs::write(&first, b"first page").expect("write first staged page");
        fs::write(&second, b"second page").expect("write second staged page");
        let outcome = finalize_outputs(
            &root.path,
            "report",
            "txt",
            &[first.clone(), second.clone()],
            || {
                (!first.exists() && root.path.join("report-001.txt").exists())
                    .then_some(StopReason::Cancelled)
            },
        );
        assert!(matches!(
            outcome,
            Err(FinalizationFailure::Stopped(StopReason::Cancelled))
        ));
        assert!(!first.exists(), "first page was moved before cancellation");
        assert!(second.exists(), "second page was not installed");
        assert!(!root.path.join("report-001.txt").exists());
        assert!(!root.path.join("report-002.txt").exists());
        drop(workspace);
        assert!(!root.path.join(".minimal-pdf-converter/job_cancel").exists());
    }

    #[test]
    fn timeout_after_last_install_removes_partial_output() {
        let root = output_fixture("timeout-after-install");
        let workspace = JobWorkspace::create(root.path.join(".minimal-pdf-converter/job_timeout"))
            .expect("create job workspace");
        let staged = workspace.path.join("report.txt");
        fs::write(&staged, b"late output").expect("write staged result");
        let outcome = finalize_outputs(&root.path, "report", "txt", &[staged.clone()], || {
            (!staged.exists() && root.path.join("report.txt").exists())
                .then_some(StopReason::TimedOut)
        });
        assert!(matches!(
            outcome,
            Err(FinalizationFailure::Stopped(StopReason::TimedOut))
        ));
        assert!(!staged.exists(), "output was moved before timeout");
        assert!(!root.path.join("report.txt").exists());
        drop(workspace);
        assert!(!root
            .path
            .join(".minimal-pdf-converter/job_timeout")
            .exists());
    }

    #[test]
    fn elapsed_timeout_prevents_reservation_before_output_install() {
        let root = output_fixture("timeout-before-install");
        let workspace = JobWorkspace::create(root.path.join(".minimal-pdf-converter/job_timeout"))
            .expect("create job workspace");
        let staged = workspace.path.join("report.txt");
        fs::write(&staged, b"late output").expect("write staged result");
        let cancellation = AtomicBool::new(false);
        let started = Instant::now() - JOB_TIMEOUT;
        let outcome = finalize_outputs(&root.path, "report", "txt", &[staged], || {
            stop_reason(&cancellation, started)
        });
        assert!(matches!(
            outcome,
            Err(FinalizationFailure::Stopped(StopReason::TimedOut))
        ));
        assert!(!root.path.join("report.txt").exists());
        drop(workspace);
        assert!(!root
            .path
            .join(".minimal-pdf-converter/job_timeout")
            .exists());
    }

    #[test]
    fn failed_install_rolls_back_partial_pages_and_retry_preserves_existing_file() {
        let root = output_fixture("install-retry");
        let existing = root.path.join("report.txt");
        fs::write(&existing, b"existing result").expect("write existing file");
        let workspace = JobWorkspace::create(root.path.join(".minimal-pdf-converter/job_failed"))
            .expect("create failed job workspace");
        let first = workspace.path.join("first.txt");
        let second = workspace.path.join("second.txt");
        fs::write(&first, b"partial result").expect("write first staged page");
        fs::write(&second, b"second page").expect("write second staged page");
        let mut removed_second = false;
        let outcome = finalize_outputs(
            &root.path,
            "report",
            "txt",
            &[first.clone(), second.clone()],
            || {
                if !removed_second && root.path.join("report-001.txt").exists() {
                    fs::remove_file(&second).expect("remove second staged page after validation");
                    removed_second = true;
                }
                None
            },
        );
        assert!(matches!(outcome, Err(FinalizationFailure::Write(_))));
        assert!(!first.exists(), "first page moved before second failed");
        assert!(!root.path.join("report-001.txt").exists());
        assert!(!root.path.join("report-002.txt").exists());
        assert_eq!(
            fs::read(&existing).expect("read existing file"),
            b"existing result"
        );
        drop(workspace);
        assert!(!root.path.join(".minimal-pdf-converter/job_failed").exists());

        let retry = JobWorkspace::create(root.path.join(".minimal-pdf-converter/job_retry"))
            .expect("create retry workspace");
        let staged = retry.path.join("report.txt");
        fs::write(&staged, b"retry result").expect("write retry result");
        let outputs = finalize_outputs(&root.path, "report", "txt", &[staged], || None)
            .expect("retry succeeds");
        assert_eq!(
            outputs,
            vec![root.path.join("report (1).txt").to_string_lossy()]
        );
        assert_eq!(
            fs::read(&outputs[0]).expect("read retry output"),
            b"retry result"
        );
        assert_eq!(
            fs::read(&existing).expect("read existing file"),
            b"existing result"
        );
        drop(retry);
        assert!(!root.path.join(".minimal-pdf-converter/job_retry").exists());
    }

    #[test]
    fn executes_available_conversion_engines_on_local_fixtures() {
        let root = std::env::temp_dir().join(format!(
            "minimal-pdf-converter-engine-smoke-{}",
            Uuid::new_v4().simple()
        ));
        fs::create_dir_all(&root).expect("create smoke root");
        let input_pdf = root.join("sample.pdf");
        pdf_writer::write_text_pdf(&input_pdf, "Hello\n你好").expect("write fixture PDF");
        let input_docx = root.join("sample.docx");
        docx::write_text_docx(&input_docx, &["Hello\n你好".to_owned()])
            .expect("write fixture DOCX");
        let input_txt = root.join("sample.txt");
        fs::write(&input_txt, "Hello UTF-8 text\n你好").expect("write fixture TXT");
        let input_rtf = root.join("sample.rtf");
        fs::write(&input_rtf, b"{\\rtf1\\ansi Hello RTF}\r\n").expect("write fixture RTF");
        let input_png = root.join("sample.png");
        {
            let mut encoder =
                png::Encoder::new(File::create(&input_png).expect("create PNG fixture"), 1, 1);
            encoder.set_color(png::ColorType::Rgb);
            encoder.set_depth(png::BitDepth::Eight);
            encoder
                .write_header()
                .expect("write PNG header")
                .write_image_data(&[0, 145, 205])
                .expect("write PNG pixel");
        }
        let input_pptx = root.join("sample.pptx");
        let slide = pptx::SlideImage::from_file(input_png.clone(), pptx::SlideImageFormat::Png)
            .expect("read PNG slide fixture");
        pptx::write_image_pptx(&input_pptx, &[slide], || false)
            .expect("write image-only PPTX fixture");
        let cancellation = AtomicBool::new(false);
        let options = ConversionOptions::default();

        let cases = [
            (ConversionKind::PdfToTxt, input_pdf.clone(), "txt"),
            (ConversionKind::PdfToDocx, input_pdf.clone(), "docx"),
            (ConversionKind::PdfToRtf, input_pdf.clone(), "rtf"),
            (ConversionKind::PdfToFlatOdtXml, input_pdf.clone(), "xml"),
            (ConversionKind::PdfToDoc, input_pdf.clone(), "doc"),
            (ConversionKind::PdfToPpt, input_pdf.clone(), "ppt"),
            (ConversionKind::PdfToMarkdown, input_pdf.clone(), "md"),
            (ConversionKind::PdfToImage, input_pdf.clone(), "png"),
            (ConversionKind::DocxToPdf, input_docx, "pdf"),
            (ConversionKind::TxtToPdf, input_txt, "pdf"),
            (ConversionKind::RtfToPdf, input_rtf, "pdf"),
            (ConversionKind::PdfToPptx, input_pdf, "pptx"),
            (ConversionKind::ImageToPdf, input_png, "pdf"),
            (ConversionKind::PptxToPdf, input_pptx, "pdf"),
        ];
        for (index, (kind, input, extension)) in cases.into_iter().enumerate() {
            if matches!(
                kind,
                ConversionKind::PdfToDocx
                    | ConversionKind::PdfToRtf
                    | ConversionKind::PdfToFlatOdtXml
                    | ConversionKind::DocxToPdf
                    | ConversionKind::TxtToPdf
                    | ConversionKind::RtfToPdf
                    | ConversionKind::PptxToPdf
            ) && !office::is_installed()
            {
                continue;
            }
            let temp_root = root.join(format!("temp-{index}"));
            fs::create_dir_all(&temp_root).expect("create engine temp root");
            let stem = input
                .file_stem()
                .and_then(|value| value.to_str())
                .expect("fixture stem");
            let context = ConversionContext {
                kind: &kind,
                input: &input,
                temp_root: &temp_root,
                stem,
                options: &options,
                cancellation: &cancellation,
                started: Instant::now(),
            };
            let result = execute_conversion(&context, &mut |_, _| {}).expect("engine conversion");
            assert!(!result.paths.is_empty());
            assert!(result.paths.iter().all(|path| path.is_file()));
            for path in &result.paths {
                validate_staged_output(path, extension, &mut || None)
                    .expect("engine output passes pre-install validation");
            }
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn preserves_unicode_through_docx_pdf_txt_round_trip() {
        let root = std::env::temp_dir().join(format!(
            "minimal-pdf-converter-unicode-roundtrip-{}",
            Uuid::new_v4().simple()
        ));
        fs::create_dir_all(&root).expect("create round-trip root");
        let input_docx = root.join("source.docx");
        docx::write_text_docx(&input_docx, &["标题：你好，世界".to_owned()])
            .expect("write source DOCX");
        let cancellation = AtomicBool::new(false);
        let options = ConversionOptions::default();
        let pdf_dir = root.join("pdf");
        fs::create_dir_all(&pdf_dir).expect("create PDF temp root");
        let docx_kind = ConversionKind::DocxToPdf;
        let docx_context = ConversionContext {
            kind: &docx_kind,
            input: &input_docx,
            temp_root: &pdf_dir,
            stem: "source",
            options: &options,
            cancellation: &cancellation,
            started: Instant::now(),
        };
        let pdf = execute_conversion(&docx_context, &mut |_, _| {})
            .expect("DOCX to PDF conversion")
            .paths
            .into_iter()
            .next()
            .expect("PDF output");

        let txt_dir = root.join("txt");
        fs::create_dir_all(&txt_dir).expect("create TXT temp root");
        let txt_kind = ConversionKind::PdfToTxt;
        let txt_context = ConversionContext {
            kind: &txt_kind,
            input: &pdf,
            temp_root: &txt_dir,
            stem: "source",
            options: &options,
            cancellation: &cancellation,
            started: Instant::now(),
        };
        let txt = execute_conversion(&txt_context, &mut |_, _| {})
            .expect("PDF to TXT conversion")
            .paths
            .into_iter()
            .next()
            .expect("TXT output");
        assert_eq!(
            fs::read_to_string(txt).expect("read TXT"),
            "标题：你好，世界"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn conversion_engine_honors_cancellation_before_work() {
        let root = std::env::temp_dir().join(format!(
            "minimal-pdf-converter-engine-cancel-{}",
            Uuid::new_v4().simple()
        ));
        fs::create_dir_all(&root).expect("create cancel root");
        let input = root.join("sample.pdf");
        pdf_writer::write_text_pdf(&input, "cancel me").expect("write fixture PDF");
        let cancellation = AtomicBool::new(true);
        let kind = ConversionKind::PdfToTxt;
        let options = ConversionOptions::default();
        let context = ConversionContext {
            kind: &kind,
            input: &input,
            temp_root: &root,
            stem: "sample",
            options: &options,
            cancellation: &cancellation,
            started: Instant::now(),
        };
        let error = execute_conversion(&context, &mut |_, _| {}).expect_err("must cancel");
        assert_eq!(error.code, "CANCELLED");
        let _ = fs::remove_dir_all(root);
    }
}
