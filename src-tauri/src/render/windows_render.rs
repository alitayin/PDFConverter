use super::{
    check_image_dimensions, next_output_bytes, write_image, ImageFormat, RenderError,
    RenderOptions, RenderResult, MAX_OUTPUT_BYTES,
};
use crate::pdf::{PageSelection, PdfError, MAX_PAGES};
use libloading::Library;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::ffi::{c_char, c_int, c_ulong, c_void, OsStr};
use std::fs;
use std::io::{self, Read, Write};
#[cfg(not(test))]
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
#[cfg(not(test))]
use std::process::{Child, Command, Stdio};
#[cfg(not(test))]
use std::thread;
#[cfg(not(test))]
use std::time::Duration;
use uuid::Uuid;

const WORKER_ARG: &str = "--pdfium-render-worker";
const MAX_INPUT_BYTES: u64 = 250 * 1024 * 1024;
const MAX_REQUEST_BYTES: u64 = 128 * 1024;
const PDFIUM_VERSION: &str = "155.0.8057.0";
const PDFIUM_SOURCE_SHA256: &str =
    "55e7ebef29a1ec9523d1adb8b260a73e7dfb0f64d3f0285121d20ecd6148ef18";
const PDFIUM_SIGNED_SHA256: Option<&str> = option_env!("MPC_PDFIUM_SIGNED_SHA256");
#[cfg(not(test))]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const FPDF_BITMAP_BGRX: c_int = 3;
const FPDF_ANNOT: c_int = 1;
const FPDF_RENDER_LIMITED_IMAGE_CACHE: c_int = 0x200;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkerRequest {
    input: PathBuf,
    output_dir: PathBuf,
    stem: String,
    pages: Option<Vec<usize>>,
    dpi: u16,
    format: String,
    jpg_quality: u8,
    max_selected_pages: usize,
    max_output_bytes: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "state", content = "value", rename_all = "snake_case")]
enum WorkerResult {
    Succeeded(usize),
    Failed(FailureCode),
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum FailureCode {
    CorruptedPdf,
    InvalidPages,
    PageLimit,
    PixelLimit,
    InputTooLarge,
    InputRead,
    OutputWrite,
    Encoding,
    OutputLimit,
    MissingRuntime,
}

impl FailureCode {
    fn into_render_error(self) -> RenderError {
        match self {
            Self::CorruptedPdf => RenderError::Pdf(PdfError::CorruptedPdf),
            Self::InvalidPages => RenderError::Pdf(PdfError::InvalidPageRange),
            Self::PageLimit => RenderError::Pdf(PdfError::PageLimitExceeded),
            Self::PixelLimit => RenderError::PixelLimit,
            Self::InputTooLarge | Self::InputRead => RenderError::Pdf(PdfError::InvalidPdf),
            Self::OutputWrite => RenderError::Io(io::Error::other("PDFium page output failed")),
            Self::Encoding => RenderError::Encoding,
            Self::OutputLimit => RenderError::OutputLimit,
            Self::MissingRuntime => RenderError::NativeUnavailable,
        }
    }
}

struct WorkerDirectory {
    path: PathBuf,
    keep: bool,
}

#[cfg(not(test))]
struct WorkerChild(Child);

#[cfg(not(test))]
impl Drop for WorkerChild {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

impl Drop for WorkerDirectory {
    fn drop(&mut self) {
        if !self.keep {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

pub(crate) fn is_worker_process() -> bool {
    let mut args = std::env::args_os();
    let _executable = args.next();
    args.next().as_deref() == Some(OsStr::new(WORKER_ARG)) && args.next().is_none()
}

pub(crate) fn run_worker_stdio() -> i32 {
    let stdin = io::stdin();
    let request: WorkerRequest =
        match serde_json::from_reader(stdin.lock().take(MAX_REQUEST_BYTES + 1)) {
            Ok(request) => request,
            Err(_) => return 2,
        };
    if !request.input.is_absolute()
        || !request.output_dir.is_absolute()
        || !valid_stem(&request.stem)
    {
        return 2;
    }
    let result = match render_in_worker(&request) {
        Ok(count) => WorkerResult::Succeeded(count),
        Err(code) => WorkerResult::Failed(code),
    };
    let serialized = match serde_json::to_vec(&result) {
        Ok(bytes) => bytes,
        Err(_) => return 2,
    };
    if fs::write(request.output_dir.join("result.json"), serialized).is_err() {
        return 2;
    }
    0
}

pub(crate) fn render_pdf_native(
    input: &Path,
    output_dir: &Path,
    stem: &str,
    options: &RenderOptions,
    should_stop: impl Fn() -> bool,
    mut progress: impl FnMut(usize, usize, &str),
) -> Result<RenderResult, RenderError> {
    if !matches!(options.dpi, 150 | 200 | 300) || !valid_stem(stem) {
        return Err(RenderError::PixelLimit);
    }
    if should_stop() {
        return Err(RenderError::Pdf(PdfError::Cancelled));
    }
    fs::create_dir_all(output_dir)?;
    let directory = output_dir.join(format!("pdfium-{}", Uuid::new_v4().simple()));
    fs::create_dir(&directory)?;
    let mut work = WorkerDirectory {
        path: directory,
        keep: false,
    };
    let request = WorkerRequest {
        input: input.to_path_buf(),
        output_dir: work.path.clone(),
        stem: stem.to_owned(),
        pages: match &options.selection {
            PageSelection::All => None,
            PageSelection::Pages(pages) => Some(pages.iter().copied().collect()),
        },
        dpi: options.dpi,
        format: options.format.extension().to_owned(),
        jpg_quality: options.jpg_quality,
        max_selected_pages: options.max_selected_pages,
        max_output_bytes: options.max_output_bytes.min(MAX_OUTPUT_BYTES),
    };
    // libtest binaries do not run the application's --pdfium-render-worker entrypoint.
    // Installed-app smoke tests separately exercise the real child-process protocol.
    #[cfg(test)]
    let count = render_in_worker(&request).map_err(FailureCode::into_render_error)?;
    #[cfg(not(test))]
    let count = {
        let serialized = serde_json::to_vec(&request).map_err(|_| RenderError::NativeFailed)?;
        if serialized.len() as u64 > MAX_REQUEST_BYTES {
            return Err(RenderError::Pdf(PdfError::InvalidPageRange));
        }
        let executable = std::env::current_exe()?;
        let mut child = WorkerChild(
            Command::new(executable)
                .arg(WORKER_ARG)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .creation_flags(CREATE_NO_WINDOW)
                .spawn()?,
        );
        let mut stdin = child.0.stdin.take().ok_or(RenderError::NativeFailed)?;
        stdin.write_all(&serialized)?;
        drop(stdin);
        let mut reported = 0;
        let status = loop {
            if should_stop() {
                return Err(RenderError::Pdf(PdfError::Cancelled));
            }
            if let Some(status) = child.0.try_wait()? {
                break status;
            }
            if let Ok(text) = fs::read_to_string(work.path.join("progress.txt")) {
                replay_progress(&text, &mut reported, &mut progress);
            }
            thread::sleep(Duration::from_millis(40));
        };
        if let Ok(text) = fs::read_to_string(work.path.join("progress.txt")) {
            replay_progress(&text, &mut reported, &mut progress);
        }
        if !status.success() {
            return Err(RenderError::NativeFailed);
        }
        let metadata = fs::metadata(work.path.join("result.json"))?;
        if metadata.len() > 256 {
            return Err(RenderError::NativeFailed);
        }
        let bytes = fs::read(work.path.join("result.json"))?;
        match serde_json::from_slice::<WorkerResult>(&bytes)
            .map_err(|_| RenderError::NativeFailed)?
        {
            WorkerResult::Failed(code) => return Err(code.into_render_error()),
            WorkerResult::Succeeded(count) => count,
        }
    };
    if count == 0 || count > options.max_selected_pages {
        return Err(RenderError::NativeFailed);
    }
    #[cfg(test)]
    if let Ok(text) = fs::read_to_string(work.path.join("progress.txt")) {
        replay_progress(&text, &mut 0, &mut progress);
    }
    let extension = options.format.extension();
    let outputs = (1..=count)
        .map(|index| work.path.join(format!("{stem}-{index:03}.{extension}")))
        .collect::<Vec<_>>();
    if outputs.iter().any(|file| !file.is_file()) {
        return Err(RenderError::NativeFailed);
    }
    work.keep = true;
    Ok(RenderResult {
        outputs,
        selected_page_count: count,
    })
}

pub(crate) fn probe_image_engine(root: &Path) -> bool {
    let input = root.join("pdfium-vector-probe.pdf");
    let output_dir = root.join("vector-probe-images");
    if fs::write(&input, vector_probe_pdf()).is_err() {
        return false;
    }
    let rendered = render_pdf_native(
        &input,
        &output_dir,
        "vector-probe",
        &RenderOptions {
            selection: PageSelection::All,
            dpi: 150,
            format: ImageFormat::Png,
            jpg_quality: 85,
            max_selected_pages: MAX_PAGES,
            max_output_bytes: u64::MAX,
        },
        || false,
        |_, _, _| {},
    );
    let Ok(result) = rendered else { return false };
    let Some(first) = result.outputs.first() else {
        return false;
    };
    let Ok(file) = fs::File::open(first) else {
        return false;
    };
    let Ok(mut reader) = png::Decoder::new(file).read_info() else {
        return false;
    };
    let mut buffer = vec![0u8; reader.output_buffer_size()];
    let Ok(frame) = reader.next_frame(&mut buffer) else {
        return false;
    };
    if frame.color_type != png::ColorType::Rgb || frame.width < 20 || frame.height < 20 {
        return false;
    }
    let width = frame.width as usize;
    let height = frame.height as usize;
    let color = |y: usize| {
        let offset = (y * width + width / 2) * 3;
        buffer.get(offset..offset + 3)
    };
    matches!(color(height / 8), Some(pixel) if pixel[0] > 210 && pixel[1] < 45 && pixel[2] < 45)
        && matches!(color(height * 7 / 8), Some(pixel) if pixel[0] < 45 && pixel[1] < 45 && pixel[2] > 210)
}

fn vector_probe_pdf() -> Vec<u8> {
    let content = "q\n1 0 0 rg\n0 40 96 40 re\nf\n0 0 1 rg\n0 0 96 40 re\nf\nQ\n";
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_owned(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 96 80] /Contents 4 0 R >>".to_owned(),
        format!(
            "<< /Length {} >>\nstream\n{content}endstream",
            content.len()
        ),
    ];
    let mut output = b"%PDF-1.4\n".to_vec();
    let mut offsets = vec![0];
    for (index, object) in objects.iter().enumerate() {
        offsets.push(output.len());
        output.extend_from_slice(format!("{} 0 obj\n{object}\nendobj\n", index + 1).as_bytes());
    }
    let xref = output.len();
    output
        .extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", offsets.len()).as_bytes());
    for offset in offsets.iter().skip(1) {
        output.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    output.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            offsets.len()
        )
        .as_bytes(),
    );
    output
}

fn valid_stem(stem: &str) -> bool {
    !stem.is_empty()
        && stem != "."
        && stem != ".."
        && !stem
            .chars()
            .any(|character| matches!(character, '/' | '\\' | ':' | '\0' | '\n' | '\r'))
}

fn image_format(value: &str) -> Option<ImageFormat> {
    match value {
        "png" => Some(ImageFormat::Png),
        "jpg" => Some(ImageFormat::Jpg),
        "bmp" => Some(ImageFormat::Bmp),
        "gif" => Some(ImageFormat::Gif),
        "webp" => Some(ImageFormat::Webp),
        "tiff" => Some(ImageFormat::Tiff),
        _ => None,
    }
}

fn parse_progress(value: &str) -> Option<(usize, usize)> {
    let (completed, total) = value.trim().split_once(' ')?;
    let completed = completed.parse::<usize>().ok()?;
    let total = total.parse::<usize>().ok()?;
    (total > 0 && total <= MAX_PAGES && completed <= total).then_some((completed, total))
}

fn replay_progress(
    value: &str,
    reported: &mut usize,
    progress: &mut impl FnMut(usize, usize, &str),
) {
    for line in value.lines() {
        let Some((completed, total)) = parse_progress(line) else {
            continue;
        };
        if completed == reported.saturating_add(1) {
            progress(completed - 1, total, "rendering");
            *reported = completed;
        }
    }
}

fn render_in_worker(request: &WorkerRequest) -> Result<usize, FailureCode> {
    if !matches!(request.dpi, 150 | 200 | 300)
        || !valid_stem(&request.stem)
        || image_format(&request.format).is_none()
        || !(1..=100).contains(&request.jpg_quality)
        || request.max_selected_pages == 0
        || request.max_selected_pages > MAX_PAGES
    {
        return Err(FailureCode::PixelLimit);
    }
    if request.max_output_bytes == 0 {
        return Err(FailureCode::OutputLimit);
    }
    let format = image_format(&request.format).ok_or(FailureCode::PixelLimit)?;
    let input_size = fs::metadata(&request.input)
        .map_err(|_| FailureCode::InputRead)?
        .len();
    if input_size > MAX_INPUT_BYTES {
        return Err(FailureCode::InputTooLarge);
    }
    let bytes = fs::read(&request.input).map_err(|_| FailureCode::InputRead)?;
    let api = Pdfium::load()?;
    // SAFETY: PDFium retains `bytes` for the document lifetime; the Vec is not
    // mutated and remains alive until all pages and the document are dropped.
    let document =
        unsafe { (api.load_document)(bytes.as_ptr().cast(), bytes.len(), std::ptr::null()) };
    if document.is_null() {
        return Err(FailureCode::CorruptedPdf);
    }
    let document = Document {
        api: &api,
        raw: document,
    };
    // SAFETY: the document is alive throughout this call.
    let count = unsafe { (api.page_count)(document.raw) };
    if count <= 0 {
        return Err(FailureCode::CorruptedPdf);
    }
    let count = usize::try_from(count).map_err(|_| FailureCode::PageLimit)?;
    if count > MAX_PAGES {
        return Err(FailureCode::PageLimit);
    }
    let selected = selected_pages(request.pages.as_deref(), count)?;
    if selected.len() > request.max_selected_pages {
        return Err(FailureCode::PageLimit);
    }
    fs::write(
        request.output_dir.join("progress.txt"),
        format!("0 {}\n", selected.len()),
    )
    .map_err(|_| FailureCode::OutputWrite)?;
    let mut output_bytes = 0u64;
    for (index, page_number) in selected.iter().copied().enumerate() {
        let page_index = c_int::try_from(page_number - 1).map_err(|_| FailureCode::PageLimit)?;
        // SAFETY: page_index was validated against PDFium's page count.
        let raw_page = unsafe { (api.load_page)(document.raw, page_index) };
        if raw_page.is_null() {
            return Err(FailureCode::CorruptedPdf);
        }
        let page = Page {
            api: &api,
            raw: raw_page,
        };
        let (width, height, rgb) = render_page_rgb(&api, page.raw, request.dpi, format)?;
        let filename = format!("{}-{:03}.{}", request.stem, index + 1, request.format);
        let output = request.output_dir.join(filename);
        write_image(&output, width, height, &rgb, format, request.jpg_quality)
            .map_err(|_| FailureCode::Encoding)?;
        output_bytes = next_output_bytes(
            output_bytes,
            fs::metadata(&output)
                .map_err(|_| FailureCode::OutputWrite)?
                .len(),
            request.max_output_bytes,
        )
        .map_err(|_| FailureCode::OutputLimit)?;
        let mut progress = fs::OpenOptions::new()
            .append(true)
            .open(request.output_dir.join("progress.txt"))
            .map_err(|_| FailureCode::OutputWrite)?;
        writeln!(progress, "{} {}", index + 1, selected.len())
            .map_err(|_| FailureCode::OutputWrite)?;
    }
    Ok(selected.len())
}

fn selected_pages(pages: Option<&[usize]>, page_count: usize) -> Result<Vec<usize>, FailureCode> {
    match pages {
        None => Ok((1..=page_count).collect()),
        Some(pages) => {
            if pages.is_empty()
                || pages.len() > MAX_PAGES
                || pages.iter().any(|&page| page == 0 || page > page_count)
            {
                return Err(FailureCode::InvalidPages);
            }
            Ok(pages
                .iter()
                .copied()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect())
        }
    }
}

fn image_dimensions(
    width: f32,
    height: f32,
    dpi: u16,
    format: ImageFormat,
) -> Result<(u32, u32), FailureCode> {
    let dimension = |points: f32| {
        let scaled = f64::from(points) * f64::from(dpi) / 72.0;
        if !scaled.is_finite() || scaled <= 0.0 || scaled.ceil() > f64::from(i32::MAX) {
            return Err(FailureCode::PixelLimit);
        }
        Ok(scaled.ceil() as u32)
    };
    let width = dimension(width)?;
    let height = dimension(height)?;
    check_image_dimensions(width, height, format).map_err(|_| FailureCode::PixelLimit)?;
    Ok((width, height))
}

fn render_page_rgb(
    api: &Pdfium,
    page: *mut c_void,
    dpi: u16,
    format: ImageFormat,
) -> Result<(u32, u32, Vec<u8>), FailureCode> {
    // SAFETY: the page is held by the caller and the API is live.
    let (width, height) =
        unsafe { image_dimensions((api.page_width)(page), (api.page_height)(page), dpi, format)? };
    let w = c_int::try_from(width).map_err(|_| FailureCode::PixelLimit)?;
    let h = c_int::try_from(height).map_err(|_| FailureCode::PixelLimit)?;
    // SAFETY: dimensions were validated and PDFium owns and frees this buffer.
    let raw_bitmap =
        unsafe { (api.bitmap_create)(w, h, FPDF_BITMAP_BGRX, std::ptr::null_mut(), 0) };
    if raw_bitmap.is_null() {
        return Err(FailureCode::Encoding);
    }
    let bitmap = Bitmap {
        api,
        raw: raw_bitmap,
    };
    // SAFETY: the bitmap is alive and its coordinates fit the allocation.
    if unsafe { (api.bitmap_fill)(bitmap.raw, 0, 0, w, h, 0xFFFF_FFFF) } == 0 {
        return Err(FailureCode::Encoding);
    }
    // SAFETY: the bitmap and the page remain live for the complete native call.
    unsafe {
        (api.render_bitmap)(
            bitmap.raw,
            page,
            0,
            0,
            w,
            h,
            0,
            FPDF_ANNOT | FPDF_RENDER_LIMITED_IMAGE_CACHE,
        )
    };
    // SAFETY: PDFium returns the row stride and buffer of this live bitmap.
    let stride = unsafe { (api.bitmap_stride)(bitmap.raw) };
    if stride < w.saturating_mul(4) || stride > w.saturating_mul(8) {
        return Err(FailureCode::Encoding);
    }
    let byte_len = usize::try_from(stride)
        .ok()
        .and_then(|s| s.checked_mul(height as usize))
        .ok_or(FailureCode::Encoding)?;
    // SAFETY: the buffer is valid for at least stride*height bytes by PDFium's
    // bitmap contract; the checked dimensions bound this length to 320 MiB.
    let pointer = unsafe { (api.bitmap_buffer)(bitmap.raw) }.cast::<u8>();
    if pointer.is_null() {
        return Err(FailureCode::Encoding);
    }
    let bgra = unsafe { std::slice::from_raw_parts(pointer, byte_len) };
    let capacity = (u64::from(width) * u64::from(height) * 3) as usize;
    let mut rgb = Vec::with_capacity(capacity);
    for row in bgra.chunks_exact(stride as usize) {
        for pixel in row[..width as usize * 4].chunks_exact(4) {
            rgb.extend_from_slice(&[pixel[2], pixel[1], pixel[0]]);
        }
    }
    Ok((width, height, rgb))
}

struct Pdfium {
    _library: Library,
    destroy: unsafe extern "C" fn(),
    load_document: unsafe extern "C" fn(*const c_void, usize, *const c_char) -> *mut c_void,
    close_document: unsafe extern "C" fn(*mut c_void),
    page_count: unsafe extern "C" fn(*mut c_void) -> c_int,
    load_page: unsafe extern "C" fn(*mut c_void, c_int) -> *mut c_void,
    close_page: unsafe extern "C" fn(*mut c_void),
    page_width: unsafe extern "C" fn(*mut c_void) -> f32,
    page_height: unsafe extern "C" fn(*mut c_void) -> f32,
    bitmap_create: unsafe extern "C" fn(c_int, c_int, c_int, *mut c_void, c_int) -> *mut c_void,
    bitmap_destroy: unsafe extern "C" fn(*mut c_void),
    bitmap_fill: unsafe extern "C" fn(*mut c_void, c_int, c_int, c_int, c_int, c_ulong) -> c_int,
    bitmap_stride: unsafe extern "C" fn(*mut c_void) -> c_int,
    bitmap_buffer: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    render_bitmap:
        unsafe extern "C" fn(*mut c_void, *mut c_void, c_int, c_int, c_int, c_int, c_int, c_int),
}

impl Pdfium {
    fn load() -> Result<Self, FailureCode> {
        #[cfg(test)]
        let root = std::env::var_os("PDFIUM_RUNTIME_DIR")
            .map(PathBuf::from)
            .ok_or(FailureCode::MissingRuntime)?;
        #[cfg(not(test))]
        let root = {
            let executable = std::env::current_exe().map_err(|_| FailureCode::MissingRuntime)?;
            executable
                .parent()
                .ok_or(FailureCode::MissingRuntime)?
                .join("pdfium-runtime")
        };
        let dll = root.join("pdfium.dll");
        for path in [&root, &dll] {
            if fs::symlink_metadata(path)
                .map_err(|_| FailureCode::MissingRuntime)?
                .file_type()
                .is_symlink()
            {
                return Err(FailureCode::MissingRuntime);
            }
        }
        let version = fs::read_to_string(root.join("RUNTIME_VERSION"))
            .map_err(|_| FailureCode::MissingRuntime)?;
        let origin = fs::read_to_string(root.join("RUNTIME_DLL_SHA256_SOURCE"))
            .map_err(|_| FailureCode::MissingRuntime)?;
        if version.trim() != PDFIUM_VERSION || origin.trim() != PDFIUM_SOURCE_SHA256 {
            return Err(FailureCode::MissingRuntime);
        }
        let digest = ring::digest::digest(
            &ring::digest::SHA256,
            &fs::read(&dll).map_err(|_| FailureCode::MissingRuntime)?,
        );
        let hex = digest
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        if !approved_runtime_hash(&hex, PDFIUM_SIGNED_SHA256) {
            return Err(FailureCode::MissingRuntime);
        }
        // SAFETY: the absolute path points to the hash-checked app-local DLL,
        // never PATH or a user-controlled working directory.
        let library =
            unsafe { Library::new(dll.as_os_str()) }.map_err(|_| FailureCode::MissingRuntime)?;
        // SAFETY: each signature matches the named stable PDFium C API export.
        unsafe {
            let api = Self {
                destroy: symbol(&library, b"FPDF_DestroyLibrary\0")?,
                load_document: symbol(&library, b"FPDF_LoadMemDocument64\0")?,
                close_document: symbol(&library, b"FPDF_CloseDocument\0")?,
                page_count: symbol(&library, b"FPDF_GetPageCount\0")?,
                load_page: symbol(&library, b"FPDF_LoadPage\0")?,
                close_page: symbol(&library, b"FPDF_ClosePage\0")?,
                page_width: symbol(&library, b"FPDF_GetPageWidthF\0")?,
                page_height: symbol(&library, b"FPDF_GetPageHeightF\0")?,
                bitmap_create: symbol(&library, b"FPDFBitmap_CreateEx\0")?,
                bitmap_destroy: symbol(&library, b"FPDFBitmap_Destroy\0")?,
                bitmap_fill: symbol(&library, b"FPDFBitmap_FillRect\0")?,
                bitmap_stride: symbol(&library, b"FPDFBitmap_GetStride\0")?,
                bitmap_buffer: symbol(&library, b"FPDFBitmap_GetBuffer\0")?,
                render_bitmap: symbol(&library, b"FPDF_RenderPageBitmap\0")?,
                _library: library,
            };
            let init: unsafe extern "C" fn() = symbol(&api._library, b"FPDF_InitLibrary\0")?;
            init();
            Ok(api)
        }
    }
}

fn approved_runtime_hash(actual: &str, signed_sha256: Option<&str>) -> bool {
    match signed_sha256 {
        Some(signed) => actual == signed,
        None => actual == PDFIUM_SOURCE_SHA256,
    }
}

unsafe fn symbol<T: Copy>(library: &Library, name: &[u8]) -> Result<T, FailureCode> {
    // SAFETY: `T` is the known signature of the requested PDFium C API.
    unsafe { library.get::<T>(name) }
        .map(|symbol| *symbol)
        .map_err(|_| FailureCode::MissingRuntime)
}

impl Drop for Pdfium {
    fn drop(&mut self) {
        // SAFETY: every Document, Page and Bitmap is dropped before this API.
        unsafe { (self.destroy)() };
    }
}

struct Document<'a> {
    api: &'a Pdfium,
    raw: *mut c_void,
}
impl Drop for Document<'_> {
    fn drop(&mut self) {
        // SAFETY: the document was loaded by the live API and is closed once.
        unsafe { (self.api.close_document)(self.raw) };
    }
}

struct Page<'a> {
    api: &'a Pdfium,
    raw: *mut c_void,
}
impl Drop for Page<'_> {
    fn drop(&mut self) {
        // SAFETY: the page was loaded by the live API and is closed once.
        unsafe { (self.api.close_page)(self.raw) };
    }
}

struct Bitmap<'a> {
    api: &'a Pdfium,
    raw: *mut c_void,
}
impl Drop for Bitmap<'_> {
    fn drop(&mut self) {
        // SAFETY: the bitmap was allocated by the live API and is destroyed once.
        unsafe { (self.api.bitmap_destroy)(self.raw) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dimensions_are_checked_before_bitmap_allocation() {
        assert_eq!(
            image_dimensions(612.0, 792.0, 300, ImageFormat::Png).expect("letter"),
            (2550, 3300)
        );
        assert!(image_dimensions(100_000.0, 100_000.0, 300, ImageFormat::Png).is_err());
        assert!(image_dimensions(f32::NAN, 300.0, 150, ImageFormat::Png).is_err());
        assert!(image_dimensions(0.0, 300.0, 150, ImageFormat::Png).is_err());
        assert_eq!(
            image_dimensions(17_000.0, 12.0, 300, ImageFormat::Png),
            Ok((70_834, 50))
        );
        assert!(image_dimensions(17_000.0, 12.0, 300, ImageFormat::Jpg).is_err());
        assert!(image_dimensions(17_000.0, 12.0, 300, ImageFormat::Gif).is_err());
        assert!(image_dimensions(4_000.0, 12.0, 300, ImageFormat::Webp).is_err());
    }

    #[test]
    fn selection_and_worker_progress_reject_bad_bounds() {
        assert_eq!(
            selected_pages(Some(&[3, 1, 1]), 3).expect("pages"),
            vec![1, 3]
        );
        assert!(selected_pages(Some(&[4]), 3).is_err());
        assert!(selected_pages(Some(&[]), 3).is_err());
        assert_eq!(parse_progress("1 3"), Some((1, 3)));
        assert_eq!(parse_progress("4 3"), None);

        let mut events = Vec::new();
        let mut reported = 0;
        let progress_log = "0 3\n1 3\n2 3\n3 3\n";
        replay_progress(progress_log, &mut reported, &mut |index, total, phase| {
            events.push((index, total, phase.to_owned()));
        });
        replay_progress(progress_log, &mut reported, &mut |index, total, phase| {
            events.push((index, total, phase.to_owned()));
        });
        assert_eq!(
            events,
            vec![
                (0, 3, "rendering".to_owned()),
                (1, 3, "rendering".to_owned()),
                (2, 3, "rendering".to_owned())
            ]
        );
        assert_eq!(reported, 3);
        assert_eq!(image_format("bmp"), Some(ImageFormat::Bmp));
        assert_eq!(image_format("gif"), Some(ImageFormat::Gif));
        assert_eq!(image_format("webp"), Some(ImageFormat::Webp));
        assert_eq!(image_format("tiff"), Some(ImageFormat::Tiff));
        assert_eq!(image_format("gifx"), None);
    }

    #[test]
    fn failed_worker_drops_partial_image_directory() {
        let root = std::env::temp_dir().join(format!(
            "minimal-pdf-windows-worker-{}",
            Uuid::new_v4().simple()
        ));
        fs::create_dir(&root).expect("create work directory");
        let output = root.join("page-001.webp");
        fs::write(&output, b"partial").expect("create partial image");
        drop(WorkerDirectory {
            path: root.clone(),
            keep: false,
        });
        assert!(!root.exists());
    }

    #[test]
    fn runtime_hash_separates_development_and_signed_release() {
        let signed = "1111111111111111111111111111111111111111111111111111111111111111";
        let unknown = "0000000000000000000000000000000000000000000000000000000000000000";
        assert!(approved_runtime_hash(PDFIUM_SOURCE_SHA256, None));
        assert!(!approved_runtime_hash(signed, None));
        assert!(!approved_runtime_hash(unknown, None));
        assert!(approved_runtime_hash(signed, Some(signed)));
        assert!(!approved_runtime_hash(PDFIUM_SOURCE_SHA256, Some(signed)));
        assert!(!approved_runtime_hash(unknown, Some(signed)));
    }
}
