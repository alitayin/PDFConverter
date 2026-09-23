use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use quick_xml::events::{BytesStart, Event};
use quick_xml::reader::Reader;
use quick_xml::XmlVersion;
use std::cell::RefCell;
use std::collections::HashSet;
use thiserror::Error;
use zip::ZipArchive;

const RUNTIME_PROBE_TIMEOUT: Duration = Duration::from_secs(3);
const POLL_INTERVAL: Duration = Duration::from_millis(50);
const MAX_OFFICE_INPUT_BYTES: u64 = 250 * 1024 * 1024;
const MAX_ZIP_ENTRIES: usize = 4096;
const MAX_RELATIONSHIPS_BYTES: u64 = 4 * 1024 * 1024;
const MAX_ALL_RELATIONSHIPS_BYTES: u64 = 32 * 1024 * 1024;
const MAX_TOTAL_UNCOMPRESSED_BYTES: u64 = 768 * 1024 * 1024;
const MAX_ODF_XML_BYTES: u64 = 64 * 1024 * 1024;
const MAX_PDF_FIDELITY_TEXT_BYTES: usize = 64 * 1024 * 1024;
const OFFICE_SCAN_BUFFER_BYTES: usize = 16 * 1024;
const OFFICE_COPY_BUFFER_BYTES: usize = 64 * 1024;
const MAX_RTF_DEPTH: usize = 256;
const MAX_RTF_CONTROL_WORD_BYTES: usize = 32;
const FLAT_ODT_MEDIA_TYPE: &str = "application/vnd.oasis.opendocument.text";
const ODF_OFFICE_NAMESPACE: &str = "urn:oasis:names:tc:opendocument:xmlns:office:1.0";

#[derive(Debug, Error)]
pub(crate) enum OfficeError {
    #[error("The local office conversion runtime is unavailable; check the installation")]
    NotInstalled,
    #[error("The bundled office conversion runtime is missing or damaged; reinstall the app")]
    BundledRuntimeMissing,
    #[error("Unsupported Office input, output, or path")]
    InvalidInput,
    #[error("The Office file contains macros, active content, or relationships that may load remote resources")]
    UnsafeContent,
    #[error("Could not create an isolated office conversion workspace: {0}")]
    Workspace(#[source] io::Error),
    #[error("Could not start the office conversion runtime: {0}")]
    Spawn(#[source] io::Error),
    #[error("Office conversion failed with exit status: {0}")]
    ConversionFailed(String),
    #[error("Office conversion did not produce a valid {0}")]
    InvalidOutput(&'static str),
    #[error("The PDF has no extractable text layer or contains unmappable characters; DOCX creation stopped to avoid data loss")]
    PdfTextFidelityUnsafe,
    #[error("Conversion cancelled")]
    Cancelled,
}

impl OfficeError {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::NotInstalled | Self::BundledRuntimeMissing => "WORKER_NOT_INSTALLED",
            Self::InvalidInput | Self::UnsafeContent => "UNSUPPORTED_FORMAT",
            Self::PdfTextFidelityUnsafe => "PDF_TEXT_FIDELITY_UNSAFE",
            Self::Cancelled => "CANCELLED",
            Self::Workspace(_) | Self::InvalidOutput(_) => "OUTPUT_WRITE_FAILED",
            Self::Spawn(_) | Self::ConversionFailed(_) => "CONVERSION_FAILED",
        }
    }
}

#[derive(Clone, Copy)]
enum OfficeConversion {
    PdfToDoc,
    PdfToDocx,
    PdfToOdt,
    PdfToOdp,
    PdfToPpt,
    PdfToPptx,
    PdfToRtf,
    PdfToFlatOdtXml,
    TxtToPdf,
    RtfToPdf,
    DocToPdf,
    DocxToPdf,
    OdtToPdf,
    PptToPdf,
    PptxToPdf,
    OdpToPdf,
    XlsxToPdf,
    OdsToPdf,
}

impl OfficeConversion {
    fn input_extension(self) -> &'static str {
        match self {
            Self::PdfToDoc
            | Self::PdfToDocx
            | Self::PdfToOdt
            | Self::PdfToOdp
            | Self::PdfToPptx
            | Self::PdfToPpt
            | Self::PdfToRtf
            | Self::PdfToFlatOdtXml => "pdf",
            Self::TxtToPdf => "txt",
            Self::RtfToPdf => "rtf",
            Self::DocToPdf => "doc",
            Self::DocxToPdf => "docx",
            Self::OdtToPdf => "odt",
            Self::PptToPdf => "ppt",
            Self::PptxToPdf => "pptx",
            Self::OdpToPdf => "odp",
            Self::XlsxToPdf => "xlsx",
            Self::OdsToPdf => "ods",
        }
    }

    fn output_extension(self) -> &'static str {
        match self {
            Self::PdfToDoc => "doc",
            Self::PdfToDocx => "docx",
            Self::PdfToOdt => "odt",
            Self::PdfToOdp => "odp",
            Self::PdfToPpt => "ppt",
            Self::PdfToPptx => "pptx",
            Self::PdfToRtf => "rtf",
            Self::PdfToFlatOdtXml => "xml",
            Self::TxtToPdf
            | Self::RtfToPdf
            | Self::DocToPdf
            | Self::DocxToPdf
            | Self::OdtToPdf
            | Self::PptToPdf
            | Self::PptxToPdf
            | Self::OdpToPdf
            | Self::XlsxToPdf
            | Self::OdsToPdf => "pdf",
        }
    }

    fn export_filter(self) -> &'static str {
        match self {
            Self::PdfToDoc => "doc:MS Word 97",
            Self::PdfToDocx => "docx",
            Self::PdfToOdt => "odt:writer8",
            Self::PdfToOdp => "odp:impress8",
            Self::PdfToPpt => "ppt:MS PowerPoint 97",
            Self::PdfToPptx => "pptx:Impress MS PowerPoint 2007 XML",
            Self::PdfToRtf => "rtf:Rich Text Format",
            Self::PdfToFlatOdtXml => "xml:OpenDocument Text Flat XML",
            Self::TxtToPdf | Self::RtfToPdf | Self::DocToPdf | Self::DocxToPdf => {
                "pdf:writer_pdf_Export"
            }
            Self::OdtToPdf => "pdf:writer_pdf_Export",
            Self::PptToPdf | Self::PptxToPdf => "pdf:impress_pdf_Export",
            Self::OdpToPdf => "pdf:impress_pdf_Export",
            Self::XlsxToPdf | Self::OdsToPdf => "pdf:calc_pdf_Export",
        }
    }

    fn import_filter(self) -> Option<&'static str> {
        match self {
            Self::PdfToDoc
            | Self::PdfToDocx
            | Self::PdfToOdt
            | Self::PdfToRtf
            | Self::PdfToFlatOdtXml => Some("writer_pdf_import"),
            Self::PdfToOdp | Self::PdfToPpt | Self::PdfToPptx => Some("impress_pdf_import"),
            Self::TxtToPdf => Some("Text (encoded):UTF8,LF,,"),
            Self::RtfToPdf => Some("Rich Text Format"),
            _ => None,
        }
    }
}

fn is_regular_nonempty_file(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|metadata| {
            metadata.file_type().is_file()
                && metadata.len() > 0
                && !is_windows_reparse_point(&metadata)
        })
        .unwrap_or(false)
}

#[cfg(windows)]
fn is_windows_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    metadata.file_attributes() & 0x400 != 0
}

#[cfg(not(windows))]
fn is_windows_reparse_point(_metadata: &fs::Metadata) -> bool {
    false
}

fn is_real_directory(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_dir() && !is_windows_reparse_point(&metadata))
        .unwrap_or(false)
}

#[derive(Clone, Copy)]
enum OfficeRuntimeMode {
    Host,
    Bundled,
    Invalid,
}

impl OfficeRuntimeMode {
    fn from_value(value: Option<&OsStr>) -> Self {
        match value {
            None => Self::Host,
            Some(value) if value == "bundled" => Self::Bundled,
            Some(_) => Self::Invalid,
        }
    }
}

fn bundled_executable(current_exe: &Path, supplied: Option<&OsStr>) -> Option<PathBuf> {
    let supplied = PathBuf::from(supplied?);
    let resources = current_exe.parent()?.parent()?;
    #[cfg(target_os = "macos")]
    let components = ["office", "LibreOffice.app", "Contents", "MacOS", "soffice"];
    #[cfg(target_os = "windows")]
    let components = ["office", "LibreOffice", "program", "soffice.exe"];
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let components = ["office", "libreoffice", "program", "soffice"];
    let expected = components
        .iter()
        .fold(resources.to_path_buf(), |path, part| path.join(part));
    if supplied != expected || !is_real_directory(resources) {
        return None;
    }
    let mut component_path = resources.to_path_buf();
    for component in &components[..components.len() - 1] {
        component_path.push(component);
        if !is_real_directory(&component_path) {
            return None;
        }
    }
    is_regular_nonempty_file(&expected).then_some(expected)
}

fn office_executable() -> Option<PathBuf> {
    let mode_value: Option<OsString> = std::env::var_os("MINIMALPDF_OFFICE_MODE");
    let supplied = std::env::var_os("MINIMALPDF_OFFICE_EXECUTABLE");
    let mode = OfficeRuntimeMode::from_value(mode_value.as_deref());
    let current_exe = std::env::current_exe().ok()?;
    office_executable_for(mode, &current_exe, supplied.as_deref())
}

fn office_executable_for(
    mode: OfficeRuntimeMode,
    current_exe: &Path,
    supplied: Option<&OsStr>,
) -> Option<PathBuf> {
    match mode {
        OfficeRuntimeMode::Bundled => return bundled_executable(current_exe, supplied),
        OfficeRuntimeMode::Invalid => return None,
        OfficeRuntimeMode::Host => {}
    }
    #[cfg(target_os = "macos")]
    {
        let path = PathBuf::from("/Applications/LibreOffice.app/Contents/MacOS/soffice");
        is_regular_nonempty_file(&path).then_some(path)
    }
    #[cfg(target_os = "windows")]
    {
        return ["ProgramFiles", "ProgramFiles(x86)"]
            .into_iter()
            .filter_map(std::env::var_os)
            .map(|root| PathBuf::from(root).join("LibreOffice/program/soffice.exe"))
            .find(|path| is_regular_nonempty_file(path));
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let path = PathBuf::from("/usr/bin/libreoffice");
        is_regular_nonempty_file(&path).then_some(path)
    }
}

pub(crate) fn is_installed() -> bool {
    let mode = OfficeRuntimeMode::from_value(std::env::var_os("MINIMALPDF_OFFICE_MODE").as_deref());
    let Some(executable) = office_executable() else {
        return false;
    };
    // The bundled executable has already passed the exact resource-layout and
    // regular-file checks in `office_executable`.  On Windows, the LibreOffice
    // launcher may keep `--version` alive while it hands off to the real
    // process, so probing it here can report a false negative.  Conversion
    // itself is the authoritative runtime check and always uses an isolated,
    // headless profile.
    if matches!(mode, OfficeRuntimeMode::Bundled) {
        return true;
    }
    let mut command = Command::new(executable);
    command
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let Ok(mut child) = OfficeChild::spawn(&mut command) else {
        return false;
    };
    let started = Instant::now();
    loop {
        match child.child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if started.elapsed() < RUNTIME_PROBE_TIMEOUT => thread::sleep(POLL_INTERVAL),
            Ok(None) | Err(_) => return false,
        }
    }
}

struct OfficeChild {
    child: Child,
    #[cfg(windows)]
    job: windows_job::Job,
}

impl OfficeChild {
    fn spawn(command: &mut Command) -> io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        #[cfg(windows)]
        let job = windows_job::Job::new()?;
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        #[allow(unused_mut)]
        let mut child = command.spawn()?;
        #[cfg(windows)]
        if let Err(error) = job.assign(&child) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
        Ok(Self {
            child,
            #[cfg(windows)]
            job,
        })
    }
}

impl Drop for OfficeChild {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            unsafe extern "C" {
                fn kill(pid: i32, signal: i32) -> i32;
            }
            if let Ok(pid) = i32::try_from(self.child.id()) {
                // SAFETY: this process was started in its own process group; the
                // negative PID addresses only that group and its descendants.
                unsafe { kill(-pid, 9) };
            }
        }
        #[cfg(windows)]
        self.job.terminate();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(windows)]
pub(crate) mod windows_job {
    use std::ffi::c_void;
    use std::io;
    use std::os::windows::io::AsRawHandle;
    use std::process::Child;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateJobObjectW(attributes: *const c_void, name: *const u16) -> *mut c_void;
        fn AssignProcessToJobObject(job: *mut c_void, process: *mut c_void) -> i32;
        fn TerminateJobObject(job: *mut c_void, exit_code: u32) -> i32;
        fn CloseHandle(handle: *mut c_void) -> i32;
    }

    pub(crate) struct Job(*mut c_void);

    impl Job {
        pub(crate) fn new() -> io::Result<Self> {
            // SAFETY: null security attributes and name request a private unnamed Job Object.
            let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if handle.is_null() {
                Err(io::Error::last_os_error())
            } else {
                Ok(Self(handle))
            }
        }

        pub(crate) fn assign(&self, child: &Child) -> io::Result<()> {
            // SAFETY: the child handle remains live while the Job Object is live.
            let success = unsafe { AssignProcessToJobObject(self.0, child.as_raw_handle()) };
            if success == 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        }

        pub(crate) fn terminate(&self) {
            // SAFETY: this handle belongs to the private Job Object created above.
            let _ = unsafe { TerminateJobObject(self.0, 1) };
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            self.terminate();
            // SAFETY: Job owns this handle and closes it once when dropped.
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
}

struct OfficeRun {
    path: PathBuf,
}

impl OfficeRun {
    fn new(workspace: &Path) -> Result<Self, OfficeError> {
        let path = workspace.join(format!("office-{}", uuid::Uuid::new_v4().simple()));
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&path).map_err(OfficeError::Workspace)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Err(error) = fs::set_permissions(&path, fs::Permissions::from_mode(0o700)) {
                let _ = fs::remove_dir(&path);
                return Err(OfficeError::Workspace(error));
            }
        }
        Ok(Self { path })
    }
}

impl Drop for OfficeRun {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

struct OfficeInputSnapshot {
    path: PathBuf,
}

impl OfficeInputSnapshot {
    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for OfficeInputSnapshot {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn valid_office_input_metadata(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_file()
        && !is_windows_reparse_point(metadata)
        && metadata.len() > 0
        && metadata.len() <= MAX_OFFICE_INPUT_BYTES
}

#[cfg(unix)]
fn same_file_identity(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(windows)]
fn same_file_identity(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    left.creation_time() == right.creation_time()
        && left.file_size() == right.file_size()
        && left.file_attributes() == right.file_attributes()
}

#[cfg(not(any(unix, windows)))]
fn same_file_identity(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.len() == right.len() && left.file_type() == right.file_type()
}

fn open_office_input(input: &Path) -> Result<(File, fs::Metadata), OfficeError> {
    let path_metadata = fs::symlink_metadata(input).map_err(|_| OfficeError::InvalidInput)?;
    if !valid_office_input_metadata(&path_metadata) {
        return Err(OfficeError::InvalidInput);
    }

    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(0x0000_0100); // O_NOFOLLOW
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(0x0002_0000); // O_NOFOLLOW
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options
            .custom_flags(0x0020_0000) // FILE_FLAG_OPEN_REPARSE_POINT
            .share_mode(0x0000_0001); // FILE_SHARE_READ
    }
    let file = options.open(input).map_err(|_| OfficeError::InvalidInput)?;
    let opened_metadata = file.metadata().map_err(|_| OfficeError::InvalidInput)?;
    if !valid_office_input_metadata(&opened_metadata)
        || !same_file_identity(&path_metadata, &opened_metadata)
    {
        return Err(OfficeError::InvalidInput);
    }
    Ok((file, opened_metadata))
}

fn source_version_is_unchanged(before: &fs::Metadata, after: &fs::Metadata) -> bool {
    same_file_identity(before, after)
        && before.len() == after.len()
        && match (before.modified(), after.modified()) {
            (Ok(before), Ok(after)) => before == after,
            _ => true,
        }
}

fn snapshot_office_input(
    conversion: OfficeConversion,
    input: &Path,
    run: &OfficeRun,
    should_stop: &mut impl FnMut() -> bool,
) -> Result<OfficeInputSnapshot, OfficeError> {
    if should_stop() {
        return Err(OfficeError::Cancelled);
    }
    let (mut source, source_metadata) = open_office_input(input)?;
    let snapshot_path = run
        .path
        .join("office-input")
        .with_extension(conversion.input_extension());
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut destination = options
        .open(&snapshot_path)
        .map_err(OfficeError::Workspace)?;
    let snapshot = OfficeInputSnapshot {
        path: snapshot_path,
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        destination
            .set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(OfficeError::Workspace)?;
    }

    let mut copied = 0_u64;
    let mut buffer = [0_u8; OFFICE_COPY_BUFFER_BYTES];
    loop {
        if should_stop() {
            return Err(OfficeError::Cancelled);
        }
        let read = source
            .read(&mut buffer)
            .map_err(|_| OfficeError::InvalidInput)?;
        if read == 0 {
            break;
        }
        copied = copied
            .checked_add(u64::try_from(read).map_err(|_| OfficeError::InvalidInput)?)
            .filter(|bytes| *bytes <= MAX_OFFICE_INPUT_BYTES)
            .ok_or(OfficeError::InvalidInput)?;
        destination
            .write_all(&buffer[..read])
            .map_err(OfficeError::Workspace)?;
    }
    destination.flush().map_err(OfficeError::Workspace)?;
    if should_stop() {
        return Err(OfficeError::Cancelled);
    }
    let final_source_metadata = source.metadata().map_err(|_| OfficeError::InvalidInput)?;
    let current_path_metadata =
        fs::symlink_metadata(input).map_err(|_| OfficeError::InvalidInput)?;
    if copied != source_metadata.len()
        || !source_version_is_unchanged(&source_metadata, &final_source_metadata)
        || !valid_office_input_metadata(&current_path_metadata)
        || !same_file_identity(&source_metadata, &current_path_metadata)
    {
        return Err(OfficeError::InvalidInput);
    }
    let snapshot_metadata =
        fs::symlink_metadata(snapshot.path()).map_err(OfficeError::Workspace)?;
    if !valid_office_input_metadata(&snapshot_metadata) || snapshot_metadata.len() != copied {
        return Err(OfficeError::InvalidInput);
    }
    drop(destination);
    Ok(snapshot)
}

fn preflight_office_input(
    conversion: OfficeConversion,
    input: &Path,
    should_stop: &mut impl FnMut() -> bool,
) -> Result<(), OfficeError> {
    match conversion {
        OfficeConversion::DocToPdf | OfficeConversion::PptToPdf => {
            preflight_legacy_office(conversion, input, should_stop)
        }
        OfficeConversion::DocxToPdf | OfficeConversion::PptxToPdf | OfficeConversion::XlsxToPdf => {
            preflight_ooxml(conversion, input, should_stop)
        }
        OfficeConversion::OdtToPdf | OfficeConversion::OdpToPdf | OfficeConversion::OdsToPdf => {
            preflight_odf(
                input,
                odf_media_type(conversion).ok_or(OfficeError::InvalidInput)?,
                should_stop,
            )
        }
        OfficeConversion::TxtToPdf => preflight_txt(input, should_stop),
        OfficeConversion::RtfToPdf => preflight_rtf(input, should_stop),
        _ => Ok(()),
    }
}

fn preflight_legacy_office(
    conversion: OfficeConversion,
    input: &Path,
    should_stop: &mut impl FnMut() -> bool,
) -> Result<(), OfficeError> {
    let required = match conversion {
        OfficeConversion::DocToPdf | OfficeConversion::PdfToDoc => "worddocument",
        OfficeConversion::PptToPdf | OfficeConversion::PdfToPpt => "powerpoint document",
        _ => return Err(OfficeError::InvalidInput),
    };
    let mut source = File::open(input).map_err(|_| OfficeError::InvalidInput)?;
    let mut magic = [0u8; 8];
    source
        .read_exact(&mut magic)
        .map_err(|_| OfficeError::InvalidInput)?;
    if magic != [0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1] {
        return Err(OfficeError::InvalidInput);
    }
    source.rewind().map_err(|_| OfficeError::InvalidInput)?;
    let mut compound = cfb::CompoundFile::open(source).map_err(|_| OfficeError::InvalidInput)?;
    let entries: Vec<_> = compound
        .walk()
        .map(|entry| {
            (
                entry.path().to_owned(),
                entry.name().to_ascii_lowercase(),
                entry.is_stream(),
                entry.len(),
            )
        })
        .collect();
    if entries.len() > MAX_ZIP_ENTRIES {
        return Err(OfficeError::UnsafeContent);
    }
    let mut found = false;
    let mut total = 0_u64;
    let mut buffer = [0_u8; OFFICE_SCAN_BUFFER_BYTES];
    for (path, name, is_stream, len) in entries {
        if should_stop() {
            return Err(OfficeError::Cancelled);
        }
        if [
            "vba",
            "macros",
            "ole10native",
            "_vba_project_cur",
            "_vba_project",
            "activex",
            "linkinfo",
        ]
        .iter()
        .any(|forbidden| name.contains(forbidden))
            || (name == "objectpool" && is_stream)
            || path.ancestors().skip(1).any(|parent| {
                parent.file_name().is_some_and(|component| {
                    component
                        .to_string_lossy()
                        .eq_ignore_ascii_case("objectpool")
                })
            })
        {
            return Err(OfficeError::UnsafeContent);
        }
        if !is_stream {
            continue;
        }
        found |= name == required && len > 0;
        total = total
            .checked_add(len)
            .filter(|bytes| *bytes <= MAX_TOTAL_UNCOMPRESSED_BYTES)
            .ok_or(OfficeError::UnsafeContent)?;
        let mut stream = compound
            .open_stream(&path)
            .map_err(|_| OfficeError::InvalidInput)?;
        let mut consumed = 0_u64;
        loop {
            if should_stop() {
                return Err(OfficeError::Cancelled);
            }
            let read = stream
                .read(&mut buffer)
                .map_err(|_| OfficeError::InvalidInput)?;
            if read == 0 {
                break;
            }
            consumed += read as u64;
            if consumed > len {
                return Err(OfficeError::InvalidInput);
            }
        }
        if consumed != len {
            return Err(OfficeError::InvalidInput);
        }
    }
    if found {
        Ok(())
    } else {
        Err(OfficeError::InvalidInput)
    }
}

fn prepare_office_input(
    conversion: OfficeConversion,
    input: &Path,
    run: &OfficeRun,
    should_stop: &mut impl FnMut() -> bool,
) -> Result<OfficeInputSnapshot, OfficeError> {
    let snapshot = snapshot_office_input(conversion, input, run, should_stop)?;
    preflight_office_input(conversion, snapshot.path(), should_stop)?;
    Ok(snapshot)
}

fn read_bounded(
    input: &mut impl Read,
    limit: usize,
    should_stop: &mut impl FnMut() -> bool,
) -> Result<Vec<u8>, OfficeError> {
    let mut output = Vec::with_capacity(limit.min(64 * 1024));
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        if should_stop() {
            return Err(OfficeError::Cancelled);
        }
        let read = input
            .read(&mut buffer)
            .map_err(|_| OfficeError::InvalidInput)?;
        if read == 0 {
            return Ok(output);
        }
        if output
            .len()
            .checked_add(read)
            .is_none_or(|length| length > limit)
        {
            return Err(OfficeError::UnsafeContent);
        }
        output.extend_from_slice(&buffer[..read]);
    }
}

fn relationship_is_safe(bytes: &[u8]) -> Result<bool, OfficeError> {
    let mut reader = Reader::from_reader(bytes);
    let mut buffer = Vec::new();
    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(Event::Empty(tag)) | Ok(Event::Start(tag))
                if tag.local_name().as_ref() == "Relationship" =>
            {
                let mut target_mode = None;
                let mut relationship_type = None;
                let mut target = None;
                for attribute in tag.attributes() {
                    let attribute = attribute.map_err(|_| OfficeError::InvalidInput)?;
                    let value = attribute
                        .normalized_value(XmlVersion::Implicit1_0)
                        .map_err(|_| OfficeError::InvalidInput)?
                        .into_owned();
                    match attribute.key.local_name().as_ref() {
                        "TargetMode" => target_mode = Some(value),
                        "Type" => relationship_type = Some(value),
                        "Target" => target = Some(value),
                        _ => {}
                    }
                }
                let target = target.ok_or(OfficeError::InvalidInput)?;
                if target_mode.is_some() {
                    if !target_mode
                        .as_deref()
                        .is_some_and(|value| value.eq_ignore_ascii_case("External"))
                    {
                        return Ok(false);
                    }
                    let is_hyperlink = relationship_type.as_deref().is_some_and(|value| {
                        matches!(value,
                            "http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink"
                            | "http://purl.oclc.org/ooxml/officeDocument/relationships/hyperlink")
                    });
                    let lower_target = target.to_ascii_lowercase();
                    if !is_hyperlink
                        || !["https://", "http://", "mailto:"]
                            .iter()
                            .any(|scheme| lower_target.starts_with(scheme))
                    {
                        return Ok(false);
                    }
                } else if target.starts_with("//")
                    || target.starts_with('\\')
                    || target.contains("://")
                    || target.to_ascii_lowercase().starts_with("file:")
                {
                    return Ok(false);
                }
            }
            Ok(Event::DocType(_)) => return Err(OfficeError::UnsafeContent),
            Ok(Event::Eof) => return Ok(true),
            Ok(_) => {}
            Err(_) => return Err(OfficeError::InvalidInput),
        }
        buffer.clear();
    }
}

fn content_types_are_safe(bytes: &[u8]) -> Result<bool, OfficeError> {
    let mut reader = Reader::from_reader(bytes);
    let mut buffer = Vec::new();
    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(Event::Empty(tag)) | Ok(Event::Start(tag)) => {
                for attribute in tag.attributes() {
                    let attribute = attribute.map_err(|_| OfficeError::InvalidInput)?;
                    if attribute.key.local_name().as_ref() == "ContentType" {
                        let content_type = attribute
                            .normalized_value(XmlVersion::Implicit1_0)
                            .map_err(|_| OfficeError::InvalidInput)?
                            .to_ascii_lowercase();
                        if content_type.contains("macroenabled")
                            || content_type.contains("activex")
                            || content_type.contains("vba")
                        {
                            return Ok(false);
                        }
                    }
                }
            }
            Ok(Event::DocType(_)) => return Err(OfficeError::UnsafeContent),
            Ok(Event::Eof) => return Ok(true),
            Ok(_) => {}
            Err(_) => return Err(OfficeError::InvalidInput),
        }
        buffer.clear();
    }
}

fn preflight_ooxml(
    conversion: OfficeConversion,
    input: &Path,
    should_stop: &mut impl FnMut() -> bool,
) -> Result<(), OfficeError> {
    let required_part = match conversion {
        OfficeConversion::DocxToPdf => "word/document.xml",
        OfficeConversion::PptxToPdf => "ppt/presentation.xml",
        OfficeConversion::XlsxToPdf => "xl/workbook.xml",
        _ => return Ok(()),
    };
    let mut file = File::open(input).map_err(|_| OfficeError::InvalidInput)?;
    if file
        .metadata()
        .map_err(|_| OfficeError::InvalidInput)?
        .len()
        > MAX_OFFICE_INPUT_BYTES
    {
        return Err(OfficeError::InvalidInput);
    }
    let mut signature = [0_u8; 4];
    file.read_exact(&mut signature)
        .map_err(|_| OfficeError::InvalidInput)?;
    if signature != *b"PK\x03\x04" {
        return Err(OfficeError::InvalidInput);
    }
    file.rewind().map_err(|_| OfficeError::InvalidInput)?;
    let mut archive = ZipArchive::new(file).map_err(|_| OfficeError::InvalidInput)?;
    if archive.is_empty() || archive.len() > MAX_ZIP_ENTRIES {
        return Err(OfficeError::UnsafeContent);
    }
    let mut names = HashSet::with_capacity(archive.len());
    let mut has_required_part = false;
    let mut has_content_types = false;
    let mut total_uncompressed = 0_u64;
    let mut total_relationships = 0_u64;
    for index in 0..archive.len() {
        if should_stop() {
            return Err(OfficeError::Cancelled);
        }
        let mut entry = archive
            .by_index(index)
            .map_err(|_| OfficeError::InvalidInput)?;
        let name = entry.name().to_owned();
        let lower_name = name.to_ascii_lowercase();
        let path_name = name.strip_suffix('/').unwrap_or(&name);
        if path_name.is_empty()
            || path_name.starts_with('/')
            || path_name.contains(['\\', ':', '\0'])
            || path_name
                .split('/')
                .any(|part| matches!(part, "" | "." | ".."))
            || !names.insert(name.clone())
            || entry.encrypted()
            || entry
                .unix_mode()
                .is_some_and(|mode| mode & 0o170000 == 0o120000)
            || !matches!(
                entry.compression(),
                zip::CompressionMethod::Stored | zip::CompressionMethod::Deflated
            )
        {
            return Err(OfficeError::UnsafeContent);
        }
        if lower_name.contains("vbaproject")
            || lower_name.contains("/activex/")
            || lower_name.contains("/embeddings/")
            || lower_name.starts_with("customui/")
        {
            return Err(OfficeError::UnsafeContent);
        }
        total_uncompressed = total_uncompressed
            .checked_add(entry.size())
            .filter(|value| *value <= MAX_TOTAL_UNCOMPRESSED_BYTES)
            .ok_or(OfficeError::UnsafeContent)?;
        has_required_part |= name == required_part;
        if name == "[Content_Types].xml" {
            has_content_types = true;
            if entry.size() > MAX_RELATIONSHIPS_BYTES {
                return Err(OfficeError::UnsafeContent);
            }
            let bytes = read_bounded(&mut entry, MAX_RELATIONSHIPS_BYTES as usize, should_stop)?;
            if !content_types_are_safe(&bytes)? {
                return Err(OfficeError::UnsafeContent);
            }
        } else if lower_name.ends_with(".rels") {
            total_relationships = total_relationships
                .checked_add(entry.size())
                .filter(|value| *value <= MAX_ALL_RELATIONSHIPS_BYTES)
                .ok_or(OfficeError::UnsafeContent)?;
            if entry.size() > MAX_RELATIONSHIPS_BYTES {
                return Err(OfficeError::UnsafeContent);
            }
            let bytes = read_bounded(&mut entry, MAX_RELATIONSHIPS_BYTES as usize, should_stop)?;
            if !relationship_is_safe(&bytes)? {
                return Err(OfficeError::UnsafeContent);
            }
        }
    }
    if has_required_part && has_content_types {
        Ok(())
    } else {
        Err(OfficeError::InvalidInput)
    }
}

fn odf_media_type(conversion: OfficeConversion) -> Option<&'static str> {
    match conversion {
        OfficeConversion::PdfToOdt | OfficeConversion::OdtToPdf => {
            Some("application/vnd.oasis.opendocument.text")
        }
        OfficeConversion::PdfToOdp | OfficeConversion::OdpToPdf => {
            Some("application/vnd.oasis.opendocument.presentation")
        }
        OfficeConversion::OdsToPdf => Some("application/vnd.oasis.opendocument.spreadsheet"),
        _ => None,
    }
}

fn odf_xml_is_safe(
    entry: impl Read,
    should_stop: &mut impl FnMut() -> bool,
) -> Result<(), OfficeError> {
    let mut reader = Reader::from_reader(BufReader::new(entry));
    let mut buffer = Vec::new();
    loop {
        if should_stop() {
            return Err(OfficeError::Cancelled);
        }
        match reader.read_event_into(&mut buffer) {
            Ok(Event::Empty(tag)) | Ok(Event::Start(tag)) => {
                let link_only = tag.name().as_ref() == "text:a";
                for attribute in tag.attributes() {
                    let attribute = attribute.map_err(|_| OfficeError::InvalidInput)?;
                    if attribute.key.as_ref() != "xlink:href" {
                        continue;
                    }
                    let value = attribute
                        .normalized_value(XmlVersion::Implicit1_0)
                        .map_err(|_| OfficeError::InvalidInput)?;
                    let target = value.as_ref();
                    let web_link = link_only
                        && ["https://", "http://", "mailto:"]
                            .iter()
                            .any(|prefix| target.to_ascii_lowercase().starts_with(prefix));
                    let safe_local = !target.is_empty()
                        && !target.starts_with(['/', '\\'])
                        && !target.contains([':', '\\', '%', '\0'])
                        && !target.split('/').any(|part| matches!(part, ".." | "."));
                    if !web_link && !safe_local {
                        return Err(OfficeError::UnsafeContent);
                    }
                }
            }
            Ok(Event::DocType(_)) => return Err(OfficeError::UnsafeContent),
            Ok(Event::Eof) => return Ok(()),
            Ok(_) => {}
            Err(_) => return Err(OfficeError::InvalidInput),
        }
        buffer.clear();
    }
}

fn preflight_odf(
    input: &Path,
    expected_media_type: &str,
    should_stop: &mut impl FnMut() -> bool,
) -> Result<(), OfficeError> {
    let mut file = File::open(input).map_err(|_| OfficeError::InvalidInput)?;
    if file
        .metadata()
        .map_err(|_| OfficeError::InvalidInput)?
        .len()
        > MAX_OFFICE_INPUT_BYTES
    {
        return Err(OfficeError::InvalidInput);
    }
    let mut signature = [0_u8; 4];
    file.read_exact(&mut signature)
        .map_err(|_| OfficeError::InvalidInput)?;
    if signature != *b"PK\x03\x04" {
        return Err(OfficeError::InvalidInput);
    }
    file.rewind().map_err(|_| OfficeError::InvalidInput)?;
    let mut archive = ZipArchive::new(file).map_err(|_| OfficeError::InvalidInput)?;
    if archive.is_empty() || archive.len() > MAX_ZIP_ENTRIES {
        return Err(OfficeError::UnsafeContent);
    }
    let mut names = HashSet::with_capacity(archive.len());
    let mut total_uncompressed = 0_u64;
    let mut media_type_matches = false;
    let mut has_content = false;
    let mut has_manifest = false;
    for index in 0..archive.len() {
        if should_stop() {
            return Err(OfficeError::Cancelled);
        }
        let mut entry = archive
            .by_index(index)
            .map_err(|_| OfficeError::InvalidInput)?;
        let name = entry.name().to_owned();
        let name_without_slash = name.strip_suffix('/').unwrap_or(&name);
        let lower = name.to_ascii_lowercase();
        if name_without_slash.is_empty()
            || name_without_slash.starts_with('/')
            || name_without_slash.contains(['\\', ':', '\0'])
            || name_without_slash
                .split('/')
                .any(|part| matches!(part, "" | "." | ".."))
            || !names.insert(name.clone())
            || entry.encrypted()
            || entry
                .unix_mode()
                .is_some_and(|mode| mode & 0o170000 == 0o120000)
            || !matches!(
                entry.compression(),
                zip::CompressionMethod::Stored | zip::CompressionMethod::Deflated
            )
            || lower.starts_with("basic/")
            || lower.starts_with("scripts/")
        {
            return Err(OfficeError::UnsafeContent);
        }
        total_uncompressed = total_uncompressed
            .checked_add(entry.size())
            .filter(|value| *value <= MAX_TOTAL_UNCOMPRESSED_BYTES)
            .ok_or(OfficeError::UnsafeContent)?;
        if name == "mimetype" {
            let bytes = read_bounded(&mut entry, 128, should_stop)?;
            media_type_matches = bytes == expected_media_type.as_bytes();
        } else if name == "content.xml" {
            has_content = true;
        } else if name == "META-INF/manifest.xml" {
            has_manifest = true;
        }
        if lower.ends_with(".xml") {
            if entry.size() > MAX_ODF_XML_BYTES {
                return Err(OfficeError::UnsafeContent);
            }
            odf_xml_is_safe(&mut entry, should_stop)?;
        }
    }
    if media_type_matches && has_content && has_manifest {
        Ok(())
    } else {
        Err(OfficeError::InvalidInput)
    }
}

fn txt_bytes_are_safe(bytes: &[u8]) -> bool {
    bytes
        .iter()
        .all(|byte| !matches!(*byte, 0x00..=0x08 | 0x0b..=0x0c | 0x0e..=0x1f | 0x7f))
}

fn preflight_txt(input: &Path, should_stop: &mut impl FnMut() -> bool) -> Result<(), OfficeError> {
    let metadata = fs::symlink_metadata(input).map_err(|_| OfficeError::InvalidInput)?;
    if !metadata.file_type().is_file()
        || is_windows_reparse_point(&metadata)
        || metadata.len() == 0
        || metadata.len() > MAX_OFFICE_INPUT_BYTES
    {
        return Err(OfficeError::InvalidInput);
    }

    let mut file = File::open(input).map_err(|_| OfficeError::InvalidInput)?;
    let mut buffer = [0_u8; OFFICE_SCAN_BUFFER_BYTES + 4];
    let mut carried = 0_usize;
    loop {
        if should_stop() {
            return Err(OfficeError::Cancelled);
        }
        let read = file
            .read(&mut buffer[carried..carried + OFFICE_SCAN_BUFFER_BYTES])
            .map_err(|_| OfficeError::InvalidInput)?;
        if read == 0 {
            return if carried == 0 {
                Ok(())
            } else {
                Err(OfficeError::InvalidInput)
            };
        }

        let length = carried + read;
        match std::str::from_utf8(&buffer[..length]) {
            Ok(_) => {
                if !txt_bytes_are_safe(&buffer[..length]) {
                    return Err(OfficeError::UnsafeContent);
                }
                carried = 0;
            }
            Err(error) => {
                let valid = error.valid_up_to();
                if !txt_bytes_are_safe(&buffer[..valid]) || error.error_len().is_some() {
                    return Err(OfficeError::InvalidInput);
                }
                let incomplete = length - valid;
                if incomplete == 0 || incomplete > 3 {
                    return Err(OfficeError::InvalidInput);
                }
                buffer.copy_within(valid..length, 0);
                carried = incomplete;
            }
        }
    }
}

struct RtfByteStream<R> {
    reader: R,
    buffer: [u8; OFFICE_SCAN_BUFFER_BYTES],
    offset: usize,
    length: usize,
    pushed_back: Vec<u8>,
}

impl<R: Read> RtfByteStream<R> {
    fn new(reader: R) -> Self {
        Self {
            reader,
            buffer: [0; OFFICE_SCAN_BUFFER_BYTES],
            offset: 0,
            length: 0,
            pushed_back: Vec::with_capacity(2),
        }
    }

    fn next_byte(
        &mut self,
        should_stop: &mut impl FnMut() -> bool,
    ) -> Result<Option<u8>, OfficeError> {
        if should_stop() {
            return Err(OfficeError::Cancelled);
        }
        if let Some(byte) = self.pushed_back.pop() {
            return Ok(Some(byte));
        }
        if self.offset == self.length {
            self.length = self
                .reader
                .read(&mut self.buffer)
                .map_err(|_| OfficeError::InvalidInput)?;
            self.offset = 0;
            if self.length == 0 {
                return Ok(None);
            }
        }
        let byte = self.buffer[self.offset];
        self.offset += 1;
        Ok(Some(byte))
    }

    fn push_back(&mut self, byte: u8) {
        self.pushed_back.push(byte);
    }
}

#[derive(Debug)]
enum RtfGroup {
    Plain,
    Field {
        shape_instruction: bool,
        result: bool,
    },
    FieldInstruction,
    FieldResult,
}

#[derive(Debug)]
struct RtfScanState {
    groups: Vec<RtfGroup>,
}

impl RtfScanState {
    fn new() -> Self {
        Self {
            groups: vec![RtfGroup::Plain],
        }
    }

    fn push_group(&mut self) {
        self.groups.push(RtfGroup::Plain);
    }

    fn pop_group(&mut self) -> Result<(), OfficeError> {
        let group = self.groups.pop().ok_or(OfficeError::InvalidInput)?;
        if let RtfGroup::Field {
            shape_instruction,
            result,
        } = group
        {
            if !shape_instruction || !result {
                return Err(OfficeError::UnsafeContent);
            }
        }
        Ok(())
    }

    fn mark_field(&mut self) -> Result<(), OfficeError> {
        match self.groups.last_mut() {
            Some(group @ RtfGroup::Plain) => {
                *group = RtfGroup::Field {
                    shape_instruction: false,
                    result: false,
                };
                Ok(())
            }
            _ => Err(OfficeError::UnsafeContent),
        }
    }

    fn mark_field_instruction(&mut self) -> Result<(), OfficeError> {
        let parent = self
            .groups
            .len()
            .checked_sub(2)
            .ok_or(OfficeError::UnsafeContent)?;
        if !matches!(self.groups.get(parent), Some(RtfGroup::Field { .. })) {
            return Err(OfficeError::UnsafeContent);
        }
        match self.groups.last_mut() {
            Some(group @ RtfGroup::Plain) => {
                *group = RtfGroup::FieldInstruction;
                Ok(())
            }
            _ => Err(OfficeError::UnsafeContent),
        }
    }

    fn mark_field_result(&mut self) -> Result<(), OfficeError> {
        let parent = self
            .groups
            .len()
            .checked_sub(2)
            .ok_or(OfficeError::UnsafeContent)?;
        if !matches!(self.groups.get(parent), Some(RtfGroup::Field { .. })) {
            return Err(OfficeError::UnsafeContent);
        }
        match self.groups.last_mut() {
            Some(group @ RtfGroup::Plain) => {
                *group = RtfGroup::FieldResult;
                if let Some(RtfGroup::Field { result, .. }) = self.groups.get_mut(parent) {
                    *result = true;
                }
                Ok(())
            }
            _ => Err(OfficeError::UnsafeContent),
        }
    }

    fn mark_shape_instruction(&mut self) -> Result<(), OfficeError> {
        if !matches!(self.groups.last(), Some(RtfGroup::FieldInstruction)) {
            return Err(OfficeError::UnsafeContent);
        }
        let parent = self
            .groups
            .len()
            .checked_sub(2)
            .ok_or(OfficeError::UnsafeContent)?;
        if let Some(RtfGroup::Field {
            shape_instruction, ..
        }) = self.groups.get_mut(parent)
        {
            *shape_instruction = true;
            Ok(())
        } else {
            Err(OfficeError::UnsafeContent)
        }
    }
}

fn rtf_instruction_whitespace(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\r' | b'\n')
}

fn consume_shape_instruction<R: Read>(
    stream: &mut RtfByteStream<R>,
    state: &mut RtfScanState,
    should_stop: &mut impl FnMut() -> bool,
) -> Result<(), OfficeError> {
    const SHAPE: &[u8] = b"shape";
    let mut position = 0_usize;
    let mut started = false;
    loop {
        let Some(byte) = stream.next_byte(should_stop)? else {
            return Err(OfficeError::InvalidInput);
        };
        if byte == b'}' {
            if started && position == SHAPE.len() {
                state.mark_shape_instruction()?;
                stream.push_back(byte);
                return Ok(());
            }
            return Err(OfficeError::UnsafeContent);
        }
        if rtf_instruction_whitespace(byte) {
            if started && position != SHAPE.len() {
                return Err(OfficeError::UnsafeContent);
            }
            continue;
        }
        started = true;
        if position >= SHAPE.len() || byte.to_ascii_lowercase() != SHAPE[position] {
            return Err(OfficeError::UnsafeContent);
        }
        position += 1;
    }
}

fn rtf_control_is_unsafe(word: &[u8]) -> bool {
    word.starts_with(b"fld")
        || word == b"object"
        || word.starts_with(b"obj")
        || word.starts_with(b"file")
        || matches!(
            word,
            b"template"
                | b"nextfile"
                | b"nextgraphic"
                | b"datastore"
                | b"datafield"
                | b"htmltag"
                | b"mhtmltag"
                | b"htmlrtf"
        )
}

fn read_rtf_parameter<R: Read>(
    stream: &mut RtfByteStream<R>,
    first_digit: u8,
    should_stop: &mut impl FnMut() -> bool,
) -> Result<(u64, Option<u8>), OfficeError> {
    let mut value = u64::from(first_digit - b'0');
    loop {
        let Some(byte) = stream.next_byte(should_stop)? else {
            return Ok((value, None));
        };
        if !byte.is_ascii_digit() {
            return Ok((value, Some(byte)));
        }
        value = value
            .checked_mul(10)
            .and_then(|number| number.checked_add(u64::from(byte - b'0')))
            .ok_or(OfficeError::InvalidInput)?;
    }
}

fn finish_rtf_control<R: Read>(
    stream: &mut RtfByteStream<R>,
    state: &mut RtfScanState,
    word: &[u8],
    parameter: Option<(bool, u64)>,
    boundary: Option<u8>,
    should_stop: &mut impl FnMut() -> bool,
) -> Result<(), OfficeError> {
    if word == b"field" {
        if parameter.is_some() {
            return Err(OfficeError::UnsafeContent);
        }
        state.mark_field()?;
        if let Some(byte) = boundary.filter(|byte| *byte != b' ') {
            stream.push_back(byte);
        }
        return Ok(());
    }
    if word == b"fldinst" {
        if parameter.is_some() || boundary != Some(b' ') {
            return Err(OfficeError::UnsafeContent);
        }
        state.mark_field_instruction()?;
        return consume_shape_instruction(stream, state, should_stop);
    }
    if word == b"fldrslt" {
        if parameter.is_some() {
            return Err(OfficeError::UnsafeContent);
        }
        state.mark_field_result()?;
        if let Some(byte) = boundary.filter(|byte| *byte != b' ') {
            stream.push_back(byte);
        }
        return Ok(());
    }
    if rtf_control_is_unsafe(word) {
        return Err(OfficeError::UnsafeContent);
    }
    if word == b"bin" {
        let Some((false, length)) = parameter else {
            return Err(OfficeError::InvalidInput);
        };
        if length > MAX_OFFICE_INPUT_BYTES {
            return Err(OfficeError::UnsafeContent);
        }
        if let Some(byte) = boundary.filter(|byte| *byte != b' ') {
            stream.push_back(byte);
        }
        for _ in 0..length {
            if stream.next_byte(should_stop)?.is_none() {
                return Err(OfficeError::InvalidInput);
            }
        }
    } else if let Some(byte) = boundary.filter(|byte| *byte != b' ') {
        stream.push_back(byte);
    }
    Ok(())
}

fn scan_rtf_control<R: Read>(
    stream: &mut RtfByteStream<R>,
    state: &mut RtfScanState,
    should_stop: &mut impl FnMut() -> bool,
) -> Result<(), OfficeError> {
    let first = stream
        .next_byte(should_stop)?
        .ok_or(OfficeError::InvalidInput)?;
    if first == b'\'' {
        for _ in 0..2 {
            if !stream
                .next_byte(should_stop)?
                .is_some_and(|byte| byte.is_ascii_hexdigit())
            {
                return Err(OfficeError::InvalidInput);
            }
        }
        return Ok(());
    }
    if !first.is_ascii_alphabetic() {
        return if first == 0 {
            Err(OfficeError::InvalidInput)
        } else {
            Ok(())
        };
    }

    let mut word = Vec::with_capacity(16);
    word.push(first.to_ascii_lowercase());
    let boundary = loop {
        let Some(byte) = stream.next_byte(should_stop)? else {
            break None;
        };
        if !byte.is_ascii_alphabetic() {
            break Some(byte);
        }
        if word.len() == MAX_RTF_CONTROL_WORD_BYTES {
            return Err(OfficeError::UnsafeContent);
        }
        word.push(byte.to_ascii_lowercase());
    };

    let (parameter, boundary) = match boundary {
        Some(byte) if byte.is_ascii_digit() => {
            let (value, boundary) = read_rtf_parameter(stream, byte, should_stop)?;
            (Some((false, value)), boundary)
        }
        Some(b'-') => match stream.next_byte(should_stop)? {
            Some(byte) if byte.is_ascii_digit() => {
                let (value, boundary) = read_rtf_parameter(stream, byte, should_stop)?;
                (Some((true, value)), boundary)
            }
            next => {
                if let Some(byte) = next {
                    stream.push_back(byte);
                }
                stream.push_back(b'-');
                (None, None)
            }
        },
        boundary => (None, boundary),
    };
    finish_rtf_control(stream, state, &word, parameter, boundary, should_stop)
}

fn preflight_rtf(input: &Path, should_stop: &mut impl FnMut() -> bool) -> Result<(), OfficeError> {
    let metadata = fs::symlink_metadata(input).map_err(|_| OfficeError::InvalidInput)?;
    if !metadata.file_type().is_file()
        || is_windows_reparse_point(&metadata)
        || metadata.len() < 7
        || metadata.len() > MAX_OFFICE_INPUT_BYTES
    {
        return Err(OfficeError::InvalidInput);
    }

    let file = File::open(input).map_err(|_| OfficeError::InvalidInput)?;
    let mut stream = RtfByteStream::new(file);
    for expected in b"{\\rtf1" {
        if stream.next_byte(should_stop)? != Some(*expected) {
            return Err(OfficeError::InvalidInput);
        }
    }

    let mut depth = 1_usize;
    let mut root_closed = false;
    let mut first_after_signature = true;
    let mut state = RtfScanState::new();
    while let Some(byte) = stream.next_byte(should_stop)? {
        if root_closed {
            if !byte.is_ascii_whitespace() {
                return Err(OfficeError::InvalidInput);
            }
            continue;
        }
        if first_after_signature {
            first_after_signature = false;
            if byte.is_ascii_digit() {
                return Err(OfficeError::InvalidInput);
            }
        }
        match byte {
            b'{' => {
                if depth == MAX_RTF_DEPTH {
                    return Err(OfficeError::UnsafeContent);
                }
                depth += 1;
                state.push_group();
            }
            b'}' => {
                depth = depth.checked_sub(1).ok_or(OfficeError::InvalidInput)?;
                root_closed = depth == 0;
                state.pop_group()?;
            }
            b'\\' => scan_rtf_control(&mut stream, &mut state, should_stop)?,
            0x00..=0x08 | 0x0b..=0x0c | 0x0e..=0x1f | 0x7f => {
                return Err(OfficeError::InvalidInput);
            }
            _ => {}
        }
    }
    if root_closed {
        Ok(())
    } else {
        Err(OfficeError::InvalidInput)
    }
}

fn validate_paths(
    conversion: OfficeConversion,
    input: &Path,
    output: &Path,
    workspace: &Path,
) -> Result<(), OfficeError> {
    if !input.is_absolute()
        || !output.is_absolute()
        || !workspace.is_absolute()
        || !is_regular_nonempty_file(input)
        || output.parent() != Some(workspace)
        || fs::symlink_metadata(output).is_ok()
        || !is_real_directory(workspace)
        || !has_extension(input, conversion.input_extension())
        || !has_extension(output, conversion.output_extension())
    {
        return Err(OfficeError::InvalidInput);
    }
    Ok(())
}

fn has_extension(path: &Path, extension: &str) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case(extension))
}

fn verify_pdf_to_docx_text_fidelity(
    input: &Path,
    should_stop: &mut impl FnMut() -> bool,
) -> Result<(), OfficeError> {
    let stop = RefCell::new(should_stop);
    let extracted = crate::pdf::extract_pages_from_path_bounded_with_control(
        input,
        &crate::pdf::PageSelection::All,
        MAX_OFFICE_INPUT_BYTES,
        MAX_PDF_FIDELITY_TEXT_BYTES,
        || (stop.borrow_mut())(),
    );
    match extracted {
        Ok(pages)
            if pages.iter().any(|page| {
                page.text
                    .chars()
                    .any(|character| matches!(character, '\0' | '\u{fffd}'))
            }) =>
        {
            Err(OfficeError::PdfTextFidelityUnsafe)
        }
        Ok(_) => Ok(()),
        Err(crate::pdf::BoundedPageExtractionError::Pdf(crate::pdf::PdfError::Cancelled)) => {
            Err(OfficeError::Cancelled)
        }
        Err(crate::pdf::BoundedPageExtractionError::Pdf(crate::pdf::PdfError::NoTextLayer)) => {
            Err(OfficeError::PdfTextFidelityUnsafe)
        }
        // This guard only rejects proven lossy mappings. PDFs outside the in-house
        // extractor's compatibility range still proceed through Writer PDF import.
        Err(_) if (stop.borrow_mut())() => Err(OfficeError::Cancelled),
        Err(_) => Ok(()),
    }
}

fn convert(
    conversion: OfficeConversion,
    input: &Path,
    output: &Path,
    workspace: &Path,
    mut should_stop: impl FnMut() -> bool,
) -> Result<(), OfficeError> {
    if should_stop() {
        return Err(OfficeError::Cancelled);
    }
    validate_paths(conversion, input, output, workspace)?;
    let run = OfficeRun::new(workspace)?;
    let prepared_input = prepare_office_input(conversion, input, &run, &mut should_stop)?;
    if matches!(
        conversion,
        OfficeConversion::PdfToDoc | OfficeConversion::PdfToDocx
    ) {
        verify_pdf_to_docx_text_fidelity(prepared_input.path(), &mut should_stop)?;
    }
    let executable = office_executable().ok_or_else(|| {
        let mode_value = std::env::var_os("MINIMALPDF_OFFICE_MODE");
        match OfficeRuntimeMode::from_value(mode_value.as_deref()) {
            OfficeRuntimeMode::Host => OfficeError::NotInstalled,
            OfficeRuntimeMode::Bundled | OfficeRuntimeMode::Invalid => {
                OfficeError::BundledRuntimeMissing
            }
        }
    })?;
    let profile = run.path.join("libreoffice-profile");
    let converted_dir = run.path.join("libreoffice-output");
    fs::create_dir(&profile).map_err(OfficeError::Workspace)?;
    fs::create_dir(&converted_dir).map_err(OfficeError::Workspace)?;
    let profile_url =
        url::Url::from_directory_path(&profile).map_err(|()| OfficeError::InvalidInput)?;

    let mut command = office_conversion_command(&executable, &profile_url);
    if let Some(filter) = conversion.import_filter() {
        command.arg(format!("--infilter={filter}"));
    }
    command
        .arg("--convert-to")
        .arg(conversion.export_filter())
        .arg("--outdir")
        .arg(&converted_dir)
        .arg(prepared_input.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = OfficeChild::spawn(&mut command).map_err(OfficeError::Spawn)?;
    loop {
        if should_stop() {
            return Err(OfficeError::Cancelled);
        }
        match child.child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(status)) => return Err(OfficeError::ConversionFailed(status.to_string())),
            Ok(None) => thread::sleep(POLL_INTERVAL),
            Err(error) => return Err(OfficeError::Spawn(error)),
        }
    }
    if should_stop() {
        return Err(OfficeError::Cancelled);
    }
    let stem = prepared_input
        .path()
        .file_stem()
        .ok_or(OfficeError::InvalidInput)?;
    let converted = converted_dir
        .join(stem)
        .with_extension(conversion.output_extension());
    match conversion {
        OfficeConversion::PdfToDoc | OfficeConversion::PdfToPpt => {
            let result = preflight_legacy_office(conversion, &converted, &mut should_stop);
            match result {
                Ok(()) => {}
                Err(OfficeError::Cancelled) => return Err(OfficeError::Cancelled),
                Err(_) => return Err(OfficeError::InvalidOutput(conversion.output_extension())),
            }
        }
        OfficeConversion::PdfToDocx => verify_docx(&converted)?,
        OfficeConversion::PdfToPptx => {
            let result = preflight_ooxml(OfficeConversion::PptxToPdf, &converted, &mut should_stop);
            match result {
                Ok(()) => {}
                Err(OfficeError::Cancelled) => return Err(OfficeError::Cancelled),
                Err(_) => return Err(OfficeError::InvalidOutput("PPTX")),
            }
        }
        OfficeConversion::PdfToRtf => verify_rtf(&converted)?,
        OfficeConversion::PdfToFlatOdtXml => {
            verify_flat_odt_xml(&converted, &mut should_stop)?;
        }
        OfficeConversion::PdfToOdt | OfficeConversion::PdfToOdp => {
            let result = preflight_odf(
                &converted,
                odf_media_type(conversion).ok_or(OfficeError::InvalidInput)?,
                &mut should_stop,
            );
            match result {
                Ok(()) => {}
                Err(OfficeError::Cancelled) => return Err(OfficeError::Cancelled),
                Err(_) => return Err(OfficeError::InvalidOutput(conversion.output_extension())),
            }
        }
        OfficeConversion::TxtToPdf
        | OfficeConversion::RtfToPdf
        | OfficeConversion::DocToPdf
        | OfficeConversion::DocxToPdf
        | OfficeConversion::OdtToPdf
        | OfficeConversion::PptToPdf
        | OfficeConversion::PptxToPdf
        | OfficeConversion::OdpToPdf
        | OfficeConversion::XlsxToPdf
        | OfficeConversion::OdsToPdf => verify_pdf(&converted)?,
    }
    if should_stop() {
        return Err(OfficeError::Cancelled);
    }
    fs::rename(converted, output).map_err(OfficeError::Workspace)
}

fn office_conversion_command(executable: &Path, profile_url: &url::Url) -> Command {
    let mut command = Command::new(executable);
    command
        .arg(format!("-env:UserInstallation={profile_url}"))
        .args([
            "--headless",
            "--invisible",
            "--nologo",
            "--norestore",
            "--nodefault",
            "--nolockcheck",
        ]);
    command
}

fn verify_docx(path: &Path) -> Result<(), OfficeError> {
    if !is_regular_nonempty_file(path) {
        return Err(OfficeError::InvalidOutput("DOCX"));
    }
    let mut signature = [0_u8; 4];
    File::open(path)
        .and_then(|mut file| file.read_exact(&mut signature))
        .map_err(|_| OfficeError::InvalidOutput("DOCX"))?;
    if signature != *b"PK\x03\x04" {
        return Err(OfficeError::InvalidOutput("DOCX"));
    }
    Ok(())
}

pub(crate) fn verify_rtf(path: &Path) -> Result<(), OfficeError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| OfficeError::InvalidOutput("RTF"))?;
    if !metadata.file_type().is_file() || is_windows_reparse_point(&metadata) || metadata.len() < 7
    {
        return Err(OfficeError::InvalidOutput("RTF"));
    }
    let mut file = File::open(path).map_err(|_| OfficeError::InvalidOutput("RTF"))?;
    let mut signature = [0_u8; 6];
    file.read_exact(&mut signature)
        .map_err(|_| OfficeError::InvalidOutput("RTF"))?;
    if signature != *b"{\\rtf1" {
        return Err(OfficeError::InvalidOutput("RTF"));
    }
    let tail_length = metadata.len().min(4096);
    let tail_length_i64 =
        i64::try_from(tail_length).map_err(|_| OfficeError::InvalidOutput("RTF"))?;
    file.seek(SeekFrom::End(-tail_length_i64))
        .map_err(|_| OfficeError::InvalidOutput("RTF"))?;
    let mut tail =
        vec![0_u8; usize::try_from(tail_length).map_err(|_| OfficeError::InvalidOutput("RTF"))?];
    file.read_exact(&mut tail)
        .map_err(|_| OfficeError::InvalidOutput("RTF"))?;
    if !tail.trim_ascii_end().ends_with(b"}") {
        return Err(OfficeError::InvalidOutput("RTF"));
    }
    Ok(())
}

fn verify_no_external_flat_odt_href(tag: &BytesStart<'_>) -> Result<(), OfficeError> {
    for attribute in tag.attributes() {
        let attribute = attribute.map_err(|_| OfficeError::InvalidOutput("Flat ODT XML"))?;
        let value = attribute
            .normalized_value(XmlVersion::Implicit1_0)
            .map_err(|_| OfficeError::InvalidOutput("Flat ODT XML"))?;
        let redefines_office_namespace =
            attribute.key.as_ref() == "xmlns:office" && value.as_ref() != ODF_OFFICE_NAMESPACE;
        let has_external_href = attribute.key.local_name().as_ref() == "href"
            && !value.is_empty()
            && !value.starts_with('#');
        if redefines_office_namespace || has_external_href {
            return Err(OfficeError::InvalidOutput("Flat ODT XML"));
        }
    }
    Ok(())
}

fn verify_flat_odt_root(tag: &BytesStart<'_>) -> Result<(), OfficeError> {
    if tag.name().as_ref() != "office:document" {
        return Err(OfficeError::InvalidOutput("Flat ODT XML"));
    }
    let mut has_office_namespace = false;
    let mut has_text_media_type = false;
    let mut has_supported_version = false;
    for attribute in tag.attributes() {
        let attribute = attribute.map_err(|_| OfficeError::InvalidOutput("Flat ODT XML"))?;
        let value = attribute
            .normalized_value(XmlVersion::Implicit1_0)
            .map_err(|_| OfficeError::InvalidOutput("Flat ODT XML"))?;
        match attribute.key.as_ref() {
            "xmlns:office" => has_office_namespace = value.as_ref() == ODF_OFFICE_NAMESPACE,
            "office:mimetype" => has_text_media_type = value.as_ref() == FLAT_ODT_MEDIA_TYPE,
            "office:version" => {
                has_supported_version = ["1.2", "1.3", "1.4"].contains(&value.as_ref());
            }
            _ => {}
        }
    }
    if has_office_namespace && has_text_media_type && has_supported_version {
        Ok(())
    } else {
        Err(OfficeError::InvalidOutput("Flat ODT XML"))
    }
}

pub(crate) fn verify_flat_odt_xml(
    path: &Path,
    should_stop: &mut impl FnMut() -> bool,
) -> Result<(), OfficeError> {
    let metadata =
        fs::symlink_metadata(path).map_err(|_| OfficeError::InvalidOutput("Flat ODT XML"))?;
    if !metadata.file_type().is_file()
        || is_windows_reparse_point(&metadata)
        || metadata.len() == 0
        || metadata.len() > MAX_ODF_XML_BYTES
    {
        return Err(OfficeError::InvalidOutput("Flat ODT XML"));
    }
    let file = File::open(path).map_err(|_| OfficeError::InvalidOutput("Flat ODT XML"))?;
    let mut reader = Reader::from_reader(BufReader::new(file));
    let mut buffer = Vec::new();
    let mut depth = 0_usize;
    let mut root_closed = false;
    let mut inside_body = false;
    let mut has_body = false;
    let mut has_text = false;
    loop {
        if should_stop() {
            return Err(OfficeError::Cancelled);
        }
        match reader.read_event_into(&mut buffer) {
            Ok(Event::Start(tag)) => {
                if root_closed {
                    return Err(OfficeError::InvalidOutput("Flat ODT XML"));
                }
                if depth == 0 {
                    verify_flat_odt_root(&tag)?;
                } else if depth == 1 && tag.name().as_ref() == "office:body" {
                    has_body = true;
                    inside_body = true;
                } else if depth == 2 && inside_body && tag.name().as_ref() == "office:text" {
                    has_text = true;
                }
                verify_no_external_flat_odt_href(&tag)?;
                depth = depth
                    .checked_add(1)
                    .ok_or(OfficeError::InvalidOutput("Flat ODT XML"))?;
            }
            Ok(Event::Empty(tag)) => {
                if depth == 0 || root_closed {
                    return Err(OfficeError::InvalidOutput("Flat ODT XML"));
                }
                if depth == 2 && inside_body && tag.name().as_ref() == "office:text" {
                    has_text = true;
                }
                verify_no_external_flat_odt_href(&tag)?;
            }
            Ok(Event::End(tag)) => {
                if depth == 0 {
                    return Err(OfficeError::InvalidOutput("Flat ODT XML"));
                }
                if depth == 2 && tag.name().as_ref() == "office:body" {
                    inside_body = false;
                }
                depth -= 1;
                root_closed |= depth == 0;
            }
            Ok(Event::DocType(_)) => {
                return Err(OfficeError::InvalidOutput("Flat ODT XML"));
            }
            Ok(Event::Text(text)) if depth == 0 && !text.as_ref().trim().is_empty() => {
                return Err(OfficeError::InvalidOutput("Flat ODT XML"));
            }
            Ok(Event::CData(text)) if depth == 0 && !text.as_ref().trim().is_empty() => {
                return Err(OfficeError::InvalidOutput("Flat ODT XML"));
            }
            Ok(Event::Eof) => {
                return if root_closed && has_body && has_text {
                    Ok(())
                } else {
                    Err(OfficeError::InvalidOutput("Flat ODT XML"))
                };
            }
            Ok(_) => {}
            Err(_) => return Err(OfficeError::InvalidOutput("Flat ODT XML")),
        }
        buffer.clear();
    }
}

fn verify_pdf(path: &Path) -> Result<(), OfficeError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| OfficeError::InvalidOutput("PDF"))?;
    if !metadata.file_type().is_file() || metadata.len() < 32 {
        return Err(OfficeError::InvalidOutput("PDF"));
    }
    let mut file = File::open(path).map_err(|_| OfficeError::InvalidOutput("PDF"))?;
    let mut header = [0_u8; 5];
    file.read_exact(&mut header)
        .map_err(|_| OfficeError::InvalidOutput("PDF"))?;
    if header != *b"%PDF-" {
        return Err(OfficeError::InvalidOutput("PDF"));
    }
    let tail_length = metadata.len().min(4096);
    let tail_length_i64 =
        i64::try_from(tail_length).map_err(|_| OfficeError::InvalidOutput("PDF"))?;
    file.seek(SeekFrom::End(-tail_length_i64))
        .map_err(|_| OfficeError::InvalidOutput("PDF"))?;
    let mut tail =
        vec![0_u8; usize::try_from(tail_length).map_err(|_| OfficeError::InvalidOutput("PDF"))?];
    file.read_exact(&mut tail)
        .map_err(|_| OfficeError::InvalidOutput("PDF"))?;
    let tail = tail.trim_ascii_end();
    if !tail.ends_with(b"%%EOF") {
        return Err(OfficeError::InvalidOutput("PDF"));
    }
    let Some(start) = tail
        .windows(b"startxref".len())
        .rposition(|part| part == b"startxref")
    else {
        return Err(OfficeError::InvalidOutput("PDF"));
    };
    let offset = tail[start + b"startxref".len()..tail.len() - b"%%EOF".len()].trim_ascii();
    if offset.is_empty() || !offset.iter().all(u8::is_ascii_digit) {
        return Err(OfficeError::InvalidOutput("PDF"));
    }
    let offset = std::str::from_utf8(offset)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|offset| *offset >= 5 && *offset < metadata.len())
        .ok_or(OfficeError::InvalidOutput("PDF"))?;
    file.seek(SeekFrom::Start(offset))
        .map_err(|_| OfficeError::InvalidOutput("PDF"))?;
    let mut xref = [0_u8; 24];
    let read = file
        .read(&mut xref)
        .map_err(|_| OfficeError::InvalidOutput("PDF"))?;
    if !xref[..read].starts_with(b"xref") && !xref[..read].windows(4).any(|part| part == b" obj") {
        return Err(OfficeError::InvalidOutput("PDF"));
    }
    Ok(())
}

pub(crate) fn pdf_to_docx(
    input: &Path,
    output: &Path,
    workspace: &Path,
    should_stop: impl FnMut() -> bool,
) -> Result<(), OfficeError> {
    convert(
        OfficeConversion::PdfToDocx,
        input,
        output,
        workspace,
        should_stop,
    )
}

pub(crate) fn pdf_to_doc(
    input: &Path,
    output: &Path,
    workspace: &Path,
    should_stop: impl FnMut() -> bool,
) -> Result<(), OfficeError> {
    convert(
        OfficeConversion::PdfToDoc,
        input,
        output,
        workspace,
        should_stop,
    )
}

pub(crate) fn pdf_to_odt(
    input: &Path,
    output: &Path,
    workspace: &Path,
    should_stop: impl FnMut() -> bool,
) -> Result<(), OfficeError> {
    convert(
        OfficeConversion::PdfToOdt,
        input,
        output,
        workspace,
        should_stop,
    )
}

pub(crate) fn pdf_to_odp(
    input: &Path,
    output: &Path,
    workspace: &Path,
    should_stop: impl FnMut() -> bool,
) -> Result<(), OfficeError> {
    convert(
        OfficeConversion::PdfToOdp,
        input,
        output,
        workspace,
        should_stop,
    )
}

pub(crate) fn pdf_to_pptx(
    input: &Path,
    output: &Path,
    workspace: &Path,
    should_stop: impl FnMut() -> bool,
) -> Result<(), OfficeError> {
    convert(
        OfficeConversion::PdfToPptx,
        input,
        output,
        workspace,
        should_stop,
    )
}

pub(crate) fn pdf_to_ppt(
    input: &Path,
    output: &Path,
    workspace: &Path,
    should_stop: impl FnMut() -> bool,
) -> Result<(), OfficeError> {
    convert(
        OfficeConversion::PdfToPpt,
        input,
        output,
        workspace,
        should_stop,
    )
}

pub(crate) fn pdf_to_rtf(
    input: &Path,
    output: &Path,
    workspace: &Path,
    should_stop: impl FnMut() -> bool,
) -> Result<(), OfficeError> {
    convert(
        OfficeConversion::PdfToRtf,
        input,
        output,
        workspace,
        should_stop,
    )
}

pub(crate) fn pdf_to_flat_odt_xml(
    input: &Path,
    output: &Path,
    workspace: &Path,
    should_stop: impl FnMut() -> bool,
) -> Result<(), OfficeError> {
    convert(
        OfficeConversion::PdfToFlatOdtXml,
        input,
        output,
        workspace,
        should_stop,
    )
}

pub(crate) fn docx_to_pdf(
    input: &Path,
    output: &Path,
    workspace: &Path,
    should_stop: impl FnMut() -> bool,
) -> Result<(), OfficeError> {
    convert(
        OfficeConversion::DocxToPdf,
        input,
        output,
        workspace,
        should_stop,
    )
}

pub(crate) fn doc_to_pdf(
    input: &Path,
    output: &Path,
    workspace: &Path,
    should_stop: impl FnMut() -> bool,
) -> Result<(), OfficeError> {
    convert(
        OfficeConversion::DocToPdf,
        input,
        output,
        workspace,
        should_stop,
    )
}

pub(crate) fn txt_to_pdf(
    input: &Path,
    output: &Path,
    workspace: &Path,
    should_stop: impl FnMut() -> bool,
) -> Result<(), OfficeError> {
    convert(
        OfficeConversion::TxtToPdf,
        input,
        output,
        workspace,
        should_stop,
    )
}

pub(crate) fn rtf_to_pdf(
    input: &Path,
    output: &Path,
    workspace: &Path,
    should_stop: impl FnMut() -> bool,
) -> Result<(), OfficeError> {
    convert(
        OfficeConversion::RtfToPdf,
        input,
        output,
        workspace,
        should_stop,
    )
}

pub(crate) fn odt_to_pdf(
    input: &Path,
    output: &Path,
    workspace: &Path,
    should_stop: impl FnMut() -> bool,
) -> Result<(), OfficeError> {
    convert(
        OfficeConversion::OdtToPdf,
        input,
        output,
        workspace,
        should_stop,
    )
}

pub(crate) fn pptx_to_pdf(
    input: &Path,
    output: &Path,
    workspace: &Path,
    should_stop: impl FnMut() -> bool,
) -> Result<(), OfficeError> {
    convert(
        OfficeConversion::PptxToPdf,
        input,
        output,
        workspace,
        should_stop,
    )
}

pub(crate) fn ppt_to_pdf(
    input: &Path,
    output: &Path,
    workspace: &Path,
    should_stop: impl FnMut() -> bool,
) -> Result<(), OfficeError> {
    convert(
        OfficeConversion::PptToPdf,
        input,
        output,
        workspace,
        should_stop,
    )
}

pub(crate) fn odp_to_pdf(
    input: &Path,
    output: &Path,
    workspace: &Path,
    should_stop: impl FnMut() -> bool,
) -> Result<(), OfficeError> {
    convert(
        OfficeConversion::OdpToPdf,
        input,
        output,
        workspace,
        should_stop,
    )
}

pub(crate) fn xlsx_to_pdf(
    input: &Path,
    output: &Path,
    workspace: &Path,
    should_stop: impl FnMut() -> bool,
) -> Result<(), OfficeError> {
    convert(
        OfficeConversion::XlsxToPdf,
        input,
        output,
        workspace,
        should_stop,
    )
}

pub(crate) fn ods_to_pdf(
    input: &Path,
    output: &Path,
    workspace: &Path,
    should_stop: impl FnMut() -> bool,
) -> Result<(), OfficeError> {
    convert(
        OfficeConversion::OdsToPdf,
        input,
        output,
        workspace,
        should_stop,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_office_real_file_probe_when_requested() {
        let Ok(path) = std::env::var("MINIMALPDF_OFFICE_PROBE") else {
            return;
        };
        let path = Path::new(&path);
        let kind = match path.extension().and_then(|extension| extension.to_str()) {
            Some("doc") => OfficeConversion::DocToPdf,
            Some("ppt") => OfficeConversion::PptToPdf,
            _ => panic!("probe requires DOC or PPT"),
        };
        assert!(preflight_legacy_office(kind, path, &mut || false).is_ok());
    }

    #[test]
    fn legacy_office_preflight_checks_container_type_active_content_and_cancellation() {
        let workspace = Workspace::new();
        for (kind, name, stream_name) in [
            (OfficeConversion::DocToPdf, "document.doc", "WordDocument"),
            (
                OfficeConversion::PptToPdf,
                "slides.ppt",
                "PowerPoint Document",
            ),
        ] {
            let path = workspace.0.join(name);
            let mut container = cfb::create(&path).expect("create CFB fixture");
            container
                .create_stream(format!("/{stream_name}"))
                .expect("create document stream")
                .write_all(b"layout data")
                .expect("write stream");
            drop(container);
            assert!(preflight_legacy_office(kind, &path, &mut || false).is_ok());
            let mut container = cfb::open_rw(&path).expect("open CFB fixture");
            container
                .create_storage("/ObjectPool")
                .expect("create empty object pool");
            drop(container);
            assert!(preflight_legacy_office(kind, &path, &mut || false).is_ok());
            assert!(matches!(
                preflight_legacy_office(kind, &path, &mut || true),
                Err(OfficeError::Cancelled)
            ));
            let mut container = cfb::open_rw(&path).expect("open CFB fixture");
            container
                .create_stream("/ObjectPool/embedded")
                .expect("add embedded object")
                .write_all(b"unsafe")
                .expect("write embedded object");
            drop(container);
            assert!(matches!(
                preflight_legacy_office(kind, &path, &mut || false),
                Err(OfficeError::UnsafeContent)
            ));
            let mut container = cfb::open_rw(&path).expect("open CFB fixture");
            container
                .create_stream("/VBA")
                .expect("add macro stream")
                .write_all(b"unsafe")
                .expect("write macro");
            drop(container);
            assert!(matches!(
                preflight_legacy_office(kind, &path, &mut || false),
                Err(OfficeError::UnsafeContent)
            ));
        }
        let forged = workspace.0.join("forged.doc");
        fs::write(&forged, b"PK\x03\x04not a DOC").expect("write forged file");
        assert!(matches!(
            preflight_legacy_office(OfficeConversion::DocToPdf, &forged, &mut || false),
            Err(OfficeError::InvalidInput)
        ));
    }

    #[test]
    fn conversion_launch_disables_office_splash_and_windows() {
        let profile = url::Url::parse("file:///tmp/isolated-office-profile")
            .expect("valid isolated profile URL");
        let command = office_conversion_command(Path::new("/tmp/soffice"), &profile);
        let args: Vec<_> = command.get_args().collect();
        for required in ["--headless", "--invisible", "--nologo", "--norestore"] {
            assert!(args.contains(&OsStr::new(required)), "missing {required}");
        }
        assert!(args.contains(&OsStr::new(
            "-env:UserInstallation=file:///tmp/isolated-office-profile"
        )));
    }

    struct Workspace(PathBuf);

    impl Workspace {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("minimalpdf-office-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&path).expect("create office workspace");
            Self(path)
        }
    }

    impl Drop for Workspace {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn packaged_office_fixture(workspace: &Workspace) -> (PathBuf, PathBuf) {
        let resources = workspace.0.join("resources");
        let bridge = resources.join("bin").join("minimal-pdf-converter");
        fs::create_dir_all(bridge.parent().expect("bridge parent"))
            .expect("create packaged bridge directory");
        fs::write(&bridge, b"bridge").expect("create packaged bridge");
        #[cfg(target_os = "macos")]
        let office = resources.join("office/LibreOffice.app/Contents/MacOS/soffice");
        #[cfg(target_os = "windows")]
        let office = resources.join("office/LibreOffice/program/soffice.exe");
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        let office = resources.join("office/libreoffice/program/soffice");
        fs::create_dir_all(office.parent().expect("Office executable parent"))
            .expect("create bundled Office directory");
        fs::write(&office, b"test binary").expect("create bundled Office executable");
        (bridge, office)
    }

    #[test]
    fn packaged_mode_requires_exact_bundled_office_even_when_host_is_installed() {
        let workspace = Workspace::new();
        let (bridge, office) = packaged_office_fixture(&workspace);
        assert!(matches!(
            OfficeRuntimeMode::from_value(Some(OsStr::new("bundled"))),
            OfficeRuntimeMode::Bundled
        ));
        assert_eq!(
            office_executable_for(
                OfficeRuntimeMode::Bundled,
                &bridge,
                Some(office.as_os_str())
            ),
            Some(office.clone())
        );

        assert!(office_executable_for(OfficeRuntimeMode::Bundled, &bridge, None).is_none());
        assert!(office_executable_for(
            OfficeRuntimeMode::Bundled,
            &bridge,
            Some(OsStr::new("/tmp/arbitrary-soffice")),
        )
        .is_none());
        assert!(office_executable_for(
            OfficeRuntimeMode::Invalid,
            &bridge,
            Some(office.as_os_str()),
        )
        .is_none());
        assert!(matches!(
            OfficeRuntimeMode::from_value(Some(OsStr::new("unrecognized"))),
            OfficeRuntimeMode::Invalid
        ));

        fs::remove_file(&office).expect("simulate missing packaged LibreOffice");
        assert!(office_executable_for(
            OfficeRuntimeMode::Bundled,
            &bridge,
            Some(office.as_os_str())
        )
        .is_none());
        assert!(
            office_executable_for(OfficeRuntimeMode::Host, &bridge, Some(office.as_os_str()))
                .is_none_or(|path| path != office)
        );
    }

    #[cfg(unix)]
    #[test]
    fn packaged_mode_rejects_symlinked_office_binary_and_directories() {
        use std::os::unix::fs::symlink;

        let workspace = Workspace::new();
        let (bridge, office) = packaged_office_fixture(&workspace);
        let external_office = workspace.0.join("alternate-soffice");
        fs::rename(&office, &external_office).expect("move executable to external path");
        symlink(&external_office, &office).expect("symlink bundled executable");
        assert!(office_executable_for(
            OfficeRuntimeMode::Bundled,
            &bridge,
            Some(office.as_os_str())
        )
        .is_none());
        fs::remove_file(&office).expect("remove executable symlink");
        fs::rename(&external_office, &office).expect("restore regular executable");

        let office_dir = bridge
            .parent()
            .expect("bridge directory")
            .parent()
            .expect("resources")
            .join("office");
        let alternate_dir = workspace.0.join("alternate-office-directory");
        fs::rename(&office_dir, &alternate_dir).expect("move bundled Office directory");
        symlink(&alternate_dir, &office_dir).expect("symlink Office directory");
        assert!(office_executable_for(
            OfficeRuntimeMode::Bundled,
            &bridge,
            Some(office.as_os_str())
        )
        .is_none());
    }

    fn docx_with_relationships(
        workspace: &Workspace,
        relationships: &str,
        content_type: &str,
    ) -> PathBuf {
        let path = workspace.0.join("untrusted.docx");
        let file = File::create(&path).expect("create OOXML test archive");
        let mut archive = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        archive
            .start_file("[Content_Types].xml", options)
            .expect("add content types");
        write!(
            archive,
            "<Types><Override ContentType=\"{content_type}\"/></Types>"
        )
        .expect("write content types");
        archive
            .start_file("word/document.xml", options)
            .expect("add document");
        archive.write_all(b"<w:document/>").expect("write document");
        archive
            .start_file("word/_rels/document.xml.rels", options)
            .expect("add relationships");
        archive
            .write_all(relationships.as_bytes())
            .expect("write relationships");
        archive.finish().expect("finish test archive");
        path
    }

    fn odf_fixture(workspace: &Workspace, extension: &str, content: &str) -> PathBuf {
        let path = workspace.0.join(format!("sample.{extension}"));
        let media_type = match extension {
            "odt" => "application/vnd.oasis.opendocument.text",
            "odp" => "application/vnd.oasis.opendocument.presentation",
            "ods" => "application/vnd.oasis.opendocument.spreadsheet",
            _ => panic!("unsupported ODF fixture"),
        };
        let file = File::create(&path).expect("create ODF fixture");
        let mut archive = zip::ZipWriter::new(file);
        let stored = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        let compressed = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        archive
            .start_file("mimetype", stored)
            .expect("add media type");
        archive
            .write_all(media_type.as_bytes())
            .expect("write media type");
        archive
            .start_file("content.xml", compressed)
            .expect("add content");
        archive
            .write_all(content.as_bytes())
            .expect("write content");
        archive
            .start_file("META-INF/manifest.xml", compressed)
            .expect("add manifest");
        archive
            .write_all(b"<manifest:manifest xmlns:manifest=\"urn:oasis:names:tc:opendocument:xmlns:manifest:1.0\"/>")
            .expect("write manifest");
        archive.finish().expect("finish ODF fixture");
        path
    }

    fn xlsx_fixture(workspace: &Workspace) -> PathBuf {
        let path = workspace.0.join("sample.xlsx");
        let file = File::create(&path).expect("create XLSX fixture");
        let mut archive = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        let parts = [
            ("[Content_Types].xml", concat!(
                "<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">",
                "<Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/>",
                "<Default Extension=\"xml\" ContentType=\"application/xml\"/>",
                "<Override PartName=\"/xl/workbook.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml\"/>",
                "<Override PartName=\"/xl/worksheets/sheet1.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml\"/>",
                "</Types>"
            )),
            ("_rels/.rels", concat!(
                "<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">",
                "<Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"xl/workbook.xml\"/>",
                "</Relationships>"
            )),
            ("xl/workbook.xml", concat!(
                "<workbook xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\">",
                "<sheets><sheet name=\"Numbers\" sheetId=\"1\" r:id=\"rId1\"/></sheets></workbook>"
            )),
            ("xl/_rels/workbook.xml.rels", concat!(
                "<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">",
                "<Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet\" Target=\"worksheets/sheet1.xml\"/>",
                "</Relationships>"
            )),
            ("xl/worksheets/sheet1.xml", concat!(
                "<worksheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\">",
                "<sheetData><row r=\"1\"><c r=\"A1\" t=\"inlineStr\"><is><t>Converted</t></is></c></row></sheetData>",
                "</worksheet>"
            )),
        ];
        for (name, data) in parts {
            archive
                .start_file(name, options)
                .expect("add spreadsheet part");
            archive
                .write_all(data.as_bytes())
                .expect("write spreadsheet part");
        }
        archive.finish().expect("finish XLSX fixture");
        path
    }

    fn generate_ods_fixture(workspace: &Workspace, xlsx: &Path) -> PathBuf {
        let profile = workspace.0.join("ods-fixture-profile");
        fs::create_dir(&profile).expect("create isolated ODS fixture profile");
        let profile_url = url::Url::from_directory_path(&profile).expect("make profile URL");
        let mut command = office_conversion_command(
            &office_executable().expect("LibreOffice was checked"),
            &profile_url,
        );
        command
            .args(["--convert-to", "ods:calc8", "--outdir"])
            .arg(&workspace.0)
            .arg(xlsx)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = OfficeChild::spawn(&mut command).expect("start ODS fixture export");
        let started = Instant::now();
        loop {
            match child.child.try_wait().expect("poll ODS fixture export") {
                Some(status) => {
                    assert!(status.success(), "ODS fixture export failed");
                    break;
                }
                None if started.elapsed() < Duration::from_secs(30) => thread::sleep(POLL_INTERVAL),
                None => panic!("ODS fixture export timed out"),
            }
        }
        let ods = xlsx.with_extension("ods");
        preflight_odf(
            &ods,
            "application/vnd.oasis.opendocument.spreadsheet",
            &mut || false,
        )
        .expect("exported ODS is safe and well formed");
        ods
    }

    #[test]
    fn open_document_preflight_rejects_wrong_type_external_images_and_scripts() {
        let workspace = Workspace::new();
        let content = "<office:document-content xmlns:office=\"urn:oasis:names:tc:opendocument:xmlns:office:1.0\" xmlns:text=\"urn:oasis:names:tc:opendocument:xmlns:text:1.0\" xmlns:xlink=\"http://www.w3.org/1999/xlink\"><text:a xlink:href=\"https://example.com/\">Link</text:a></office:document-content>";
        let input = odf_fixture(&workspace, "odt", content);
        preflight_odf(
            &input,
            "application/vnd.oasis.opendocument.text",
            &mut || false,
        )
        .expect("inert hyperlink is allowed");
        assert!(matches!(
            preflight_odf(
                &input,
                "application/vnd.oasis.opendocument.presentation",
                &mut || false
            ),
            Err(OfficeError::InvalidInput)
        ));

        let external = content
            .replace("<text:a xlink:href", "<draw:image xlink:href")
            .replace("</text:a>", "</draw:image>");
        let input = odf_fixture(&workspace, "odt", &external);
        assert!(matches!(
            preflight_odf(
                &input,
                "application/vnd.oasis.opendocument.text",
                &mut || false
            ),
            Err(OfficeError::UnsafeContent)
        ));

        let file = File::create(&input).expect("create malicious ODF");
        let mut archive = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();
        for (name, bytes) in [
            (
                "mimetype",
                b"application/vnd.oasis.opendocument.text".as_slice(),
            ),
            ("content.xml", b"<office:document-content/>".as_slice()),
            ("META-INF/manifest.xml", b"<manifest:manifest/>".as_slice()),
            ("Basic/Standard/Module1.xml", b"<script/>".as_slice()),
        ] {
            archive.start_file(name, options).expect("add ODF entry");
            archive.write_all(bytes).expect("write ODF entry");
        }
        archive.finish().expect("finish ODF archive");
        assert!(matches!(
            preflight_odf(
                &input,
                "application/vnd.oasis.opendocument.text",
                &mut || false
            ),
            Err(OfficeError::UnsafeContent)
        ));
    }

    #[test]
    fn spreadsheet_preflight_checks_package_kind_and_cancellation() {
        let workspace = Workspace::new();
        let xlsx = xlsx_fixture(&workspace);
        preflight_ooxml(OfficeConversion::XlsxToPdf, &xlsx, &mut || false)
            .expect("XLSX required parts exist");
        assert!(matches!(
            preflight_ooxml(OfficeConversion::XlsxToPdf, &xlsx, &mut || true),
            Err(OfficeError::Cancelled)
        ));
        let ods = odf_fixture(&workspace, "ods", "<office:document-content/>");
        preflight_odf(
            &ods,
            "application/vnd.oasis.opendocument.spreadsheet",
            &mut || false,
        )
        .expect("ODS required parts exist");
    }

    #[test]
    fn preflight_allows_clickable_http_hyperlink_but_not_external_image() {
        let workspace = Workspace::new();
        let hyperlinks = "<Relationships><Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink\" Target=\"https://example.org/info\" TargetMode=\"External\"/></Relationships>";
        let path = docx_with_relationships(
            &workspace,
            hyperlinks,
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml",
        );
        preflight_ooxml(OfficeConversion::DocxToPdf, &path, &mut || false)
            .expect("HTTP hyperlink is inert during conversion");
        let remote_image = "<Relationships><Relationship Id=\"rId2\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/image\" Target=\"https://example.org/remote.png\" TargetMode=\"External\"/></Relationships>";
        let path = docx_with_relationships(
            &workspace,
            remote_image,
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml",
        );
        assert!(matches!(
            preflight_ooxml(OfficeConversion::DocxToPdf, &path, &mut || false),
            Err(OfficeError::UnsafeContent)
        ));
    }

    #[test]
    fn preflight_rejects_macro_enabled_content_and_bad_zip() {
        let workspace = Workspace::new();
        let path = docx_with_relationships(
            &workspace,
            "<Relationships/>",
            "application/vnd.ms-word.document.macroEnabled.main+xml",
        );
        assert!(matches!(
            preflight_ooxml(OfficeConversion::DocxToPdf, &path, &mut || false),
            Err(OfficeError::UnsafeContent)
        ));
        let path = docx_with_relationships(
            &workspace,
            "<Relationships/>",
            "application/vnd.ms-word.document.macro&#69;nabled.main+xml",
        );
        assert!(matches!(
            preflight_ooxml(OfficeConversion::DocxToPdf, &path, &mut || false),
            Err(OfficeError::UnsafeContent)
        ));
        fs::write(&path, b"PK\x03\x04 damaged central directory").expect("damage archive");
        assert!(matches!(
            preflight_ooxml(OfficeConversion::DocxToPdf, &path, &mut || false),
            Err(OfficeError::InvalidInput)
        ));
        fs::write(&path, b"Not a ZIP, despite the DOCX extension").expect("forge DOCX");
        assert!(matches!(
            preflight_ooxml(OfficeConversion::DocxToPdf, &path, &mut || false),
            Err(OfficeError::InvalidInput)
        ));
    }

    #[test]
    fn preflight_rejects_expanded_relationship_over_limit() {
        let workspace = Workspace::new();
        let enormous = format!(
            "<Relationships>{}</Relationships>",
            " ".repeat(4 * 1024 * 1024)
        );
        let path = docx_with_relationships(
            &workspace,
            &enormous,
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml",
        );
        assert!(matches!(
            preflight_ooxml(OfficeConversion::DocxToPdf, &path, &mut || false),
            Err(OfficeError::UnsafeContent)
        ));
    }

    #[test]
    fn cancel_before_launch_does_not_require_libreoffice() {
        let result = pdf_to_docx(
            Path::new("/nonexistent/input.pdf"),
            Path::new("/nonexistent/output.docx"),
            Path::new("/nonexistent"),
            || true,
        );
        assert!(matches!(result, Err(OfficeError::Cancelled)));
        let result = pdf_to_rtf(
            Path::new("/nonexistent/input.pdf"),
            Path::new("/nonexistent/output.rtf"),
            Path::new("/nonexistent"),
            || true,
        );
        assert!(matches!(result, Err(OfficeError::Cancelled)));
        let result = pdf_to_flat_odt_xml(
            Path::new("/nonexistent/input.pdf"),
            Path::new("/nonexistent/output.xml"),
            Path::new("/nonexistent"),
            || true,
        );
        assert!(matches!(result, Err(OfficeError::Cancelled)));
        let result = txt_to_pdf(
            Path::new("/nonexistent/input.txt"),
            Path::new("/nonexistent/output.pdf"),
            Path::new("/nonexistent"),
            || true,
        );
        assert!(matches!(result, Err(OfficeError::Cancelled)));
        let result = rtf_to_pdf(
            Path::new("/nonexistent/input.rtf"),
            Path::new("/nonexistent/output.pdf"),
            Path::new("/nonexistent"),
            || true,
        );
        assert!(matches!(result, Err(OfficeError::Cancelled)));
    }

    #[test]
    fn prepared_input_keeps_preflighted_bytes_after_atomic_source_replacement() {
        let workspace = Workspace::new();
        let input = workspace.0.join("source.txt");
        let original = b"preflighted UTF-8 bytes\n";
        let replacement = b"unsafe replacement\0that must not reach LibreOffice";
        fs::write(&input, original).expect("write original input");
        let run = OfficeRun::new(&workspace.0).expect("create isolated Office run");
        let prepared =
            prepare_office_input(OfficeConversion::TxtToPdf, &input, &run, &mut || false)
                .expect("snapshot and preflight original input");

        let replacement_path = workspace.0.join("replacement.txt");
        fs::write(&replacement_path, replacement).expect("write replacement input");
        #[cfg(unix)]
        fs::rename(&replacement_path, &input).expect("atomically replace original input");
        #[cfg(windows)]
        {
            fs::remove_file(&input).expect("remove original input before replacement");
            fs::rename(&replacement_path, &input).expect("replace original input");
        }

        assert_eq!(fs::read(&input).expect("read replacement"), replacement);
        assert!(matches!(
            preflight_txt(&input, &mut || false),
            Err(OfficeError::UnsafeContent)
        ));
        assert_eq!(
            fs::read(prepared.path()).expect("read prepared command input"),
            original
        );
        assert_eq!(
            prepared.path().extension().and_then(OsStr::to_str),
            Some("txt")
        );
        preflight_txt(prepared.path(), &mut || false)
            .expect("the exact command input remains preflighted");
    }

    #[test]
    fn cancelled_snapshot_removes_partial_private_copy() {
        let workspace = Workspace::new();
        let input = workspace.0.join("large.txt");
        fs::write(&input, vec![b'a'; OFFICE_COPY_BUFFER_BYTES * 3])
            .expect("write cancellable input");
        let run = OfficeRun::new(&workspace.0).expect("create isolated Office run");
        let mut checks = 0;
        let result = snapshot_office_input(OfficeConversion::TxtToPdf, &input, &run, &mut || {
            checks += 1;
            checks >= 3
        });
        assert!(matches!(result, Err(OfficeError::Cancelled)));
        assert!(!run.path.join("office-input.txt").exists());
    }

    #[test]
    fn failed_snapshot_preflight_removes_private_copy() {
        let workspace = Workspace::new();
        let input = workspace.0.join("unsafe.txt");
        fs::write(&input, b"unsafe\0text").expect("write unsafe input");
        let run = OfficeRun::new(&workspace.0).expect("create isolated Office run");
        assert!(matches!(
            prepare_office_input(OfficeConversion::TxtToPdf, &input, &run, &mut || false),
            Err(OfficeError::UnsafeContent)
        ));
        assert!(!run.path.join("office-input.txt").exists());
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_rejects_symbolic_link_input() {
        use std::os::unix::fs::symlink;

        let workspace = Workspace::new();
        let target = workspace.0.join("target.txt");
        let input = workspace.0.join("source.txt");
        fs::write(&target, b"target bytes").expect("write symlink target");
        symlink(&target, &input).expect("create input symlink");
        let run = OfficeRun::new(&workspace.0).expect("create isolated Office run");
        assert!(matches!(
            snapshot_office_input(OfficeConversion::TxtToPdf, &input, &run, &mut || false),
            Err(OfficeError::InvalidInput)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn office_run_and_snapshot_use_private_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let workspace = Workspace::new();
        let input = workspace.0.join("source.txt");
        fs::write(&input, b"private bytes").expect("write private input");
        let run = OfficeRun::new(&workspace.0).expect("create isolated Office run");
        let snapshot =
            snapshot_office_input(OfficeConversion::TxtToPdf, &input, &run, &mut || false)
                .expect("snapshot input");
        assert_eq!(
            fs::metadata(&run.path)
                .expect("run metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(snapshot.path())
                .expect("snapshot metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn txt_preflight_streams_utf8_and_rejects_invalid_or_active_bytes() {
        let workspace = Workspace::new();
        let input = workspace.0.join("source.txt");
        let mut valid = vec![b'a'; OFFICE_SCAN_BUFFER_BYTES - 1];
        valid.extend_from_slice("界\r\n\ttext".as_bytes());
        fs::write(&input, valid).expect("write valid UTF-8 across the scan boundary");
        preflight_txt(&input, &mut || false).expect("accept bounded UTF-8 text");

        for invalid in [
            b"invalid UTF-8: \xff".as_slice(),
            b"embedded\0nul".as_slice(),
            b"raw\x1fcontrol".as_slice(),
        ] {
            fs::write(&input, invalid).expect("write rejected text fixture");
            assert!(preflight_txt(&input, &mut || false).is_err());
        }

        let mut large = vec![b'a'; OFFICE_SCAN_BUFFER_BYTES * 2];
        large.push(b'\n');
        fs::write(&input, large).expect("write cancellable text fixture");
        let mut checks = 0;
        assert!(matches!(
            preflight_txt(&input, &mut || {
                checks += 1;
                checks > 1
            }),
            Err(OfficeError::Cancelled)
        ));
    }

    #[test]
    fn rtf_preflight_handles_escapes_binary_data_and_plain_urls() {
        let workspace = Workspace::new();
        let input = workspace.0.join("source.rtf");
        for valid in [
            br"{\rtf1\ansi plain https://example.test/path}".as_slice(),
            br"{\rtf1\ansi Caf\'e9 and escaped \{braces\}}".as_slice(),
            br"{\rtf1\ansi before \bin4 {\}X after}".as_slice(),
            br"{\rtf1{\field{\*\fldinst SHAPE }{\fldrslt{\shp{\*\shpinst{\sp{\sn shapeType}{\sv 202}}}}}}}".as_slice(),
        ] {
            fs::write(&input, valid).expect("write valid RTF fixture");
            preflight_rtf(&input, &mut || false).expect("accept self-contained RTF");
        }
    }

    #[test]
    fn rtf_preflight_rejects_malformed_deep_or_dangerous_content() {
        let workspace = Workspace::new();
        let input = workspace.0.join("source.rtf");
        for malformed in [
            br"{\rtf1\ansi truncated".as_slice(),
            br"{\rtf1} trailing".as_slice(),
            br"{\rtf12}".as_slice(),
            br"{\rtf1 invalid \'xz}".as_slice(),
            br"{\rtf1\bin8 short}".as_slice(),
        ] {
            fs::write(&input, malformed).expect("write malformed RTF fixture");
            assert!(matches!(
                preflight_rtf(&input, &mut || false),
                Err(OfficeError::InvalidInput)
            ));
        }

        let mut too_deep = br"{\rtf1".to_vec();
        too_deep.extend(std::iter::repeat_n(b'{', MAX_RTF_DEPTH));
        too_deep.extend(std::iter::repeat_n(b'}', MAX_RTF_DEPTH + 1));
        fs::write(&input, too_deep).expect("write deeply nested RTF fixture");
        assert!(matches!(
            preflight_rtf(&input, &mut || false),
            Err(OfficeError::UnsafeContent)
        ));

        for dangerous in [
            br#"{\rtf1{\field{\*\fldinst HYPERLINK "https://example.test"}}}"#.as_slice(),
            br"{\rtf1{\*\FLDINST INCLUDEPICTURE file:///tmp/image.png}}".as_slice(),
            br"{\rtf1{\field{\*\fldinst SHAPE HYPERLINK}{\fldrslt safe}}}".as_slice(),
            br"{\rtf1{\field{\*\fldinst SHAPE }}}".as_slice(),
            br"{\rtf1{\field{\*\fldinst SHAPE }{\fldrslt{\object\objdata 0102}}}}".as_slice(),
            br"{\rtf1{\object\objdata 0102}}".as_slice(),
            br"{\rtf1{\filetbl ignored}}".as_slice(),
            br"{\rtf1\TeMpLaTe file:///tmp/template.dot}".as_slice(),
            br"{\rtf1\nextgraphic https://example.test/image.png}".as_slice(),
            br"{\rtf1\datastore hidden}".as_slice(),
            br"{\rtf1\htmltag external}".as_slice(),
        ] {
            fs::write(&input, dangerous).expect("write unsafe RTF fixture");
            assert!(matches!(
                preflight_rtf(&input, &mut || false),
                Err(OfficeError::UnsafeContent)
            ));
        }

        fs::write(&input, br"{\rtf1 cancellable}").expect("write cancellable RTF fixture");
        let mut checks = 0;
        assert!(matches!(
            preflight_rtf(&input, &mut || {
                checks += 1;
                checks >= 4
            }),
            Err(OfficeError::Cancelled)
        ));
    }

    #[test]
    fn text_to_pdf_contracts_enforce_extensions_and_workspace_output() {
        assert_eq!(OfficeConversion::TxtToPdf.input_extension(), "txt");
        assert_eq!(OfficeConversion::RtfToPdf.input_extension(), "rtf");
        assert_eq!(OfficeConversion::TxtToPdf.output_extension(), "pdf");
        assert_eq!(OfficeConversion::RtfToPdf.output_extension(), "pdf");
        assert_eq!(
            OfficeConversion::TxtToPdf.import_filter(),
            Some("Text (encoded):UTF8,LF,,")
        );
        assert_eq!(
            OfficeConversion::RtfToPdf.import_filter(),
            Some("Rich Text Format")
        );
        assert_eq!(
            OfficeConversion::TxtToPdf.export_filter(),
            "pdf:writer_pdf_Export"
        );
        assert_eq!(
            OfficeConversion::RtfToPdf.export_filter(),
            "pdf:writer_pdf_Export"
        );

        let workspace = Workspace::new();
        let txt = workspace.0.join("input.txt");
        let rtf = workspace.0.join("input.rtf");
        fs::write(&txt, b"text").expect("write TXT input");
        fs::write(&rtf, br"{\rtf1 text}").expect("write RTF input");
        let pdf = workspace.0.join("output.pdf");
        validate_paths(OfficeConversion::TxtToPdf, &txt, &pdf, &workspace.0)
            .expect("accept TXT to PDF paths");
        validate_paths(OfficeConversion::RtfToPdf, &rtf, &pdf, &workspace.0)
            .expect("accept RTF to PDF paths");

        let wrong_input = workspace.0.join("input.md");
        fs::write(&wrong_input, b"text").expect("write wrong extension input");
        assert!(matches!(
            validate_paths(OfficeConversion::TxtToPdf, &wrong_input, &pdf, &workspace.0),
            Err(OfficeError::InvalidInput)
        ));
        let wrong_output = workspace.0.join("output.png");
        assert!(matches!(
            validate_paths(
                OfficeConversion::RtfToPdf,
                &rtf,
                &wrong_output,
                &workspace.0
            ),
            Err(OfficeError::InvalidInput)
        ));
        let escaped_output = workspace
            .0
            .parent()
            .expect("workspace parent")
            .join("escaped-text-output.pdf");
        assert!(matches!(
            validate_paths(
                OfficeConversion::TxtToPdf,
                &txt,
                &escaped_output,
                &workspace.0
            ),
            Err(OfficeError::InvalidInput)
        ));
    }

    #[test]
    fn rejects_forged_legacy_office_inputs_and_output_outside_workspace() {
        let workspace = Workspace::new();
        for extension in ["doc", "ppt"] {
            let input = workspace.0.join(format!("legacy.{extension}"));
            fs::write(&input, b"dummy").expect("write legacy input");
            let output = workspace.0.join("converted.pdf");
            let result = if extension == "doc" {
                doc_to_pdf(&input, &output, &workspace.0, || false)
            } else {
                ppt_to_pdf(&input, &output, &workspace.0, || false)
            };
            assert!(matches!(result, Err(OfficeError::InvalidInput)));
        }
        let input = workspace.0.join("input.docx");
        fs::write(&input, b"PK\x03\x04").expect("write input");
        let output = workspace
            .0
            .parent()
            .expect("workspace parent")
            .join("escaped.pdf");
        assert!(matches!(
            docx_to_pdf(&input, &output, &workspace.0, || false),
            Err(OfficeError::InvalidInput)
        ));
        assert!(!output.exists());
    }

    #[test]
    fn pdf_text_exports_use_explicit_rtf_and_flat_odt_xml_contracts() {
        assert_eq!(
            OfficeConversion::PdfToRtf.export_filter(),
            "rtf:Rich Text Format"
        );
        assert_eq!(OfficeConversion::PdfToRtf.output_extension(), "rtf");
        assert_eq!(
            OfficeConversion::PdfToFlatOdtXml.export_filter(),
            "xml:OpenDocument Text Flat XML"
        );
        assert_eq!(OfficeConversion::PdfToFlatOdtXml.output_extension(), "xml");
    }

    #[test]
    fn rtf_validation_requires_signature_and_terminated_document_group() {
        let workspace = Workspace::new();
        let output = workspace.0.join("sample.rtf");
        fs::write(&output, b"{\\rtf1\\ansi validated}\r\n").expect("write valid RTF");
        verify_rtf(&output).expect("accept an RTF document with a complete root group");

        for invalid in [
            b"plain text}".as_slice(),
            b"{\\rtf1\\ansi truncated".as_slice(),
            b"{\\rtf1\\ansi closed}\ntrailing".as_slice(),
        ] {
            fs::write(&output, invalid).expect("write invalid RTF");
            assert!(matches!(
                verify_rtf(&output),
                Err(OfficeError::InvalidOutput("RTF"))
            ));
        }
    }

    fn flat_odt_xml(body: &str, version: &str) -> String {
        format!(
            concat!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>",
                "<office:document ",
                "xmlns:office=\"urn:oasis:names:tc:opendocument:xmlns:office:1.0\" ",
                "xmlns:text=\"urn:oasis:names:tc:opendocument:xmlns:text:1.0\" ",
                "xmlns:xlink=\"http://www.w3.org/1999/xlink\" ",
                "office:version=\"{}\" ",
                "office:mimetype=\"application/vnd.oasis.opendocument.text\">",
                "<office:body><office:text>{}</office:text></office:body>",
                "</office:document>"
            ),
            version, body
        )
    }

    #[test]
    fn flat_odt_xml_validation_requires_exact_dialect_and_self_contained_content() {
        let workspace = Workspace::new();
        let output = workspace.0.join("sample.xml");
        fs::write(&output, flat_odt_xml("<text:p>validated</text:p>", "1.4"))
            .expect("write valid Flat ODT XML");
        verify_flat_odt_xml(&output, &mut || false).expect("accept self-contained Flat ODT XML");
        assert!(matches!(
            verify_flat_odt_xml(&output, &mut || true),
            Err(OfficeError::Cancelled)
        ));

        for invalid in [
            String::from("<?xml version=\"1.0\"?><document><body/></document>"),
            flat_odt_xml("<text:p>future dialect</text:p>", "1.5"),
            format!(
                "<!DOCTYPE office:document SYSTEM \"https://example.invalid/odf.dtd\">{}",
                flat_odt_xml("<text:p>DTD</text:p>", "1.4")
            ),
            flat_odt_xml(
                "<text:a xlink:href=\"https://example.invalid/\">remote</text:a>",
                "1.4",
            ),
            flat_odt_xml(
                "<text:a xmlns:alternate=\"http://www.w3.org/1999/xlink\" alternate:href=\"file:///tmp/external\">remote alias</text:a>",
                "1.4",
            ),
        ] {
            fs::write(&output, invalid).expect("write invalid XML");
            assert!(matches!(
                verify_flat_odt_xml(&output, &mut || false),
                Err(OfficeError::InvalidOutput("Flat ODT XML"))
            ));
        }
    }

    #[test]
    fn refuses_truncated_or_forged_pdf_output() {
        let workspace = Workspace::new();
        let output = workspace.0.join("wrong.pdf");
        fs::write(&output, b"%PDF-1.7\nnot a document").expect("write fake PDF");
        assert!(matches!(
            verify_pdf(&output),
            Err(OfficeError::InvalidOutput("PDF"))
        ));

        crate::pdf_writer::write_pages_pdf(&output, &["valid PDF".to_owned()])
            .expect("write real PDF");
        verify_pdf(&output).expect("accept valid PDF structure");
        let mut bytes = fs::read(&output).expect("read valid PDF");
        bytes.extend_from_slice(b"trailing garbage");
        fs::write(&output, bytes).expect("write damaged PDF");
        assert!(matches!(
            verify_pdf(&output),
            Err(OfficeError::InvalidOutput("PDF"))
        ));
    }

    #[test]
    fn pdf_to_docx_fidelity_guard_rejects_unmapped_text_units() {
        let workspace = Workspace::new();
        let input = workspace.0.join("unmapped-text.pdf");
        let output = workspace.0.join("rejected.docx");
        crate::pdf_writer::write_pages_pdf(&input, &["before\0after".to_owned()])
            .expect("write fixture PDF");

        let error = pdf_to_docx(&input, &output, &workspace.0, || false)
            .expect_err("unmapped text must not be sent to Writer as a successful DOCX");
        assert!(matches!(error, OfficeError::PdfTextFidelityUnsafe));
        assert_eq!(error.code(), "PDF_TEXT_FIDELITY_UNSAFE");
        assert!(!output.exists(), "rejected DOCX must not be published");
    }

    #[test]
    fn pdf_to_docx_fidelity_guard_rejects_image_only_pdf() {
        let workspace = Workspace::new();
        let input = workspace.0.join("no-text-layer.pdf");
        let output = workspace.0.join("rejected.docx");
        crate::pdf_writer::write_pages_pdf(&input, &[String::new()])
            .expect("write textless fixture PDF");

        let error = pdf_to_docx(&input, &output, &workspace.0, || false)
            .expect_err("image-only PDF must not become a supposedly editable DOCX");
        assert!(matches!(error, OfficeError::PdfTextFidelityUnsafe));
        assert!(!output.exists(), "rejected DOCX must not be published");
    }

    #[test]
    fn pdf_to_docx_fidelity_guard_allows_reliably_mapped_text() {
        let workspace = Workspace::new();
        let input = workspace.0.join("mapped-text.pdf");
        crate::pdf_writer::write_pages_pdf(&input, &["before after".to_owned()])
            .expect("write fixture PDF");

        verify_pdf_to_docx_text_fidelity(&input, &mut || false)
            .expect("ordinary mapped text remains eligible for Writer import");
    }

    #[test]
    #[ignore = "requires a separately authorized real PDF set in MINIMALPDF_REAL_PDF"]
    fn real_pdf_with_unmapped_text_never_publishes_docx() {
        let input = PathBuf::from(
            std::env::var_os("MINIMALPDF_REAL_PDF")
                .expect("set MINIMALPDF_REAL_PDF to an authorized external PDF"),
        );
        let workspace = Workspace::new();
        let output = workspace.0.join("rejected.docx");
        let error = pdf_to_docx(&input, &output, &workspace.0, || false)
            .expect_err("lossy real PDF must not produce an apparently successful DOCX");
        assert_eq!(error.code(), "PDF_TEXT_FIDELITY_UNSAFE");
        assert!(!output.exists(), "rejected DOCX must not be published");
    }

    #[test]
    fn converts_real_pdf_when_libreoffice_is_installed() {
        if !is_installed() {
            return;
        }
        let workspace = Workspace::new();
        let input = workspace.0.join("document with spaces.pdf");
        let output = workspace.0.join("converted.docx");
        crate::pdf_writer::write_pages_pdf(&input, &["Hello world".to_owned()])
            .expect("create test PDF");
        pdf_to_docx(&input, &output, &workspace.0, || false).expect("convert PDF");
        assert!(fs::metadata(output).expect("output exists").len() > 4);
    }

    #[test]
    fn pdf_to_pptx_keeps_text_as_editable_slide_shapes() {
        if !is_installed() {
            return;
        }
        let workspace = Workspace::new();
        let input = workspace.0.join("editable.pdf");
        let output = workspace.0.join("editable.pptx");
        crate::pdf_writer::write_pages_pdf(&input, &["Editable slide text".to_owned()])
            .expect("create text PDF");

        pdf_to_pptx(&input, &output, &workspace.0, || false).expect("convert through Impress");
        let mut archive =
            ZipArchive::new(File::open(&output).expect("open PPTX")).expect("valid OOXML ZIP");
        let mut slide = String::new();
        archive
            .by_name("ppt/slides/slide1.xml")
            .expect("first slide")
            .read_to_string(&mut slide)
            .expect("slide XML");
        assert!(slide.contains("<p:sp>"), "expected editable slide shapes");
        assert!(slide.contains("<a:t>"), "expected native editable text");
        assert!(slide.contains("Editable"), "slide text missing");
    }

    #[test]
    fn converts_real_pdf_to_rtf_and_flat_odt_xml_when_libreoffice_is_installed() {
        if !is_installed() {
            return;
        }
        let workspace = Workspace::new();
        let input = workspace.0.join("portable text source.pdf");
        crate::pdf_writer::write_pages_pdf(&input, &["RTF and Flat ODT XML".to_owned()])
            .expect("create source PDF");

        let rtf = workspace.0.join("portable text result.rtf");
        pdf_to_rtf(&input, &rtf, &workspace.0, || false).expect("convert PDF to RTF");
        verify_rtf(&rtf).expect("RTF result has a valid envelope");
        preflight_rtf(&rtf, &mut || false).expect("generated RTF passes safe re-import preflight");
        let rtf_pdf = workspace.0.join("portable text roundtrip.pdf");
        rtf_to_pdf(&rtf, &rtf_pdf, &workspace.0, || false).expect("convert generated RTF to PDF");
        verify_pdf(&rtf_pdf).expect("RTF roundtrip result is a structurally valid PDF");

        let xml = workspace.0.join("portable text result.xml");
        pdf_to_flat_odt_xml(&input, &xml, &workspace.0, || false)
            .expect("convert PDF to Flat ODT XML");
        verify_flat_odt_xml(&xml, &mut || false)
            .expect("Flat ODT XML result has the promised dialect");
    }

    #[test]
    fn converts_real_txt_and_rtf_to_pdf_after_cancelled_attempt() {
        if !is_installed() {
            return;
        }
        let workspace = Workspace::new();

        let txt = workspace.0.join("utf8 source.txt");
        let txt_pdf = workspace.0.join("utf8 result.pdf");
        fs::write(&txt, "第一行\nSecond line\n").expect("write UTF-8 TXT source");
        let mut txt_checks = 0;
        let cancelled = txt_to_pdf(&txt, &txt_pdf, &workspace.0, || {
            txt_checks += 1;
            txt_checks > 1
        });
        assert!(matches!(cancelled, Err(OfficeError::Cancelled)));
        assert!(!txt_pdf.exists());
        txt_to_pdf(&txt, &txt_pdf, &workspace.0, || false).expect("convert TXT to PDF");
        verify_pdf(&txt_pdf).expect("TXT result is a structurally valid PDF");

        let rtf = workspace.0.join("rich text source.rtf");
        let rtf_pdf = workspace.0.join("rich text result.pdf");
        fs::write(
            &rtf,
            br"{\rtf1\ansi\deff0 {\fonttbl {\f0 Arial;}}\f0\fs24 Rich text output}",
        )
        .expect("write RTF source");
        let mut rtf_checks = 0;
        let cancelled = rtf_to_pdf(&rtf, &rtf_pdf, &workspace.0, || {
            rtf_checks += 1;
            rtf_checks >= 4
        });
        assert!(matches!(cancelled, Err(OfficeError::Cancelled)));
        assert!(!rtf_pdf.exists());
        rtf_to_pdf(&rtf, &rtf_pdf, &workspace.0, || false).expect("convert RTF to PDF");
        verify_pdf(&rtf_pdf).expect("RTF result is a structurally valid PDF");
    }

    #[test]
    fn converts_real_docx_and_pptx_when_libreoffice_is_installed() {
        if !is_installed() {
            return;
        }
        let workspace = Workspace::new();
        let input_docx = workspace.0.join("document with spaces.docx");
        let output_docx = workspace.0.join("converted-document.pdf");
        crate::docx::write_text_docx(&input_docx, &["PDF from Word".to_owned()])
            .expect("create test DOCX");
        docx_to_pdf(&input_docx, &output_docx, &workspace.0, || false).expect("convert DOCX");
        verify_pdf(&output_docx).expect("DOCX PDF is structurally valid");

        let input_png = workspace.0.join("slide.png");
        crate::render::write_png(&input_png, 1, 1, &[14, 135, 220]).expect("create slide image");
        let input_pptx = workspace.0.join("presentation with spaces.pptx");
        let slide =
            crate::pptx::SlideImage::from_file(input_png, crate::pptx::SlideImageFormat::Png);
        let slide = slide.expect("read slide image");
        crate::pptx::write_image_pptx(&input_pptx, &[slide], || false).expect("create test PPTX");
        let output_pptx = workspace.0.join("converted-presentation.pdf");
        pptx_to_pdf(&input_pptx, &output_pptx, &workspace.0, || false).expect("convert PPTX");
        verify_pdf(&output_pptx).expect("PPTX PDF is structurally valid");
    }

    #[test]
    fn converts_open_documents_and_spreadsheets_when_libreoffice_is_installed() {
        if !is_installed() {
            return;
        }
        let workspace = Workspace::new();
        let pdf = workspace.0.join("source.pdf");
        crate::pdf_writer::write_pages_pdf(&pdf, &["Open document sample".to_owned()])
            .expect("create source PDF");
        let odt = workspace.0.join("document.odt");
        pdf_to_odt(&pdf, &odt, &workspace.0, || false).expect("convert PDF to ODT");
        let odt_pdf = workspace.0.join("document.pdf");
        odt_to_pdf(&odt, &odt_pdf, &workspace.0, || false).expect("convert ODT to PDF");
        verify_pdf(&odt_pdf).expect("ODT result is a PDF");

        let odp = workspace.0.join("presentation.odp");
        pdf_to_odp(&pdf, &odp, &workspace.0, || false).expect("convert PDF to ODP");
        let odp_pdf = workspace.0.join("presentation.pdf");
        odp_to_pdf(&odp, &odp_pdf, &workspace.0, || false).expect("convert ODP to PDF");
        verify_pdf(&odp_pdf).expect("ODP result is a PDF");

        let xlsx = xlsx_fixture(&workspace);
        let xlsx_pdf = workspace.0.join("xlsx-result.pdf");
        xlsx_to_pdf(&xlsx, &xlsx_pdf, &workspace.0, || false).expect("convert XLSX to PDF");
        verify_pdf(&xlsx_pdf).expect("XLSX result is a PDF");

        let ods = generate_ods_fixture(&workspace, &xlsx);
        let ods_pdf = workspace.0.join("ods-result.pdf");
        ods_to_pdf(&ods, &ods_pdf, &workspace.0, || false).expect("convert ODS to PDF");
        verify_pdf(&ods_pdf).expect("ODS result is a PDF");
    }

    #[test]
    fn cancellation_and_retry_never_publish_failed_output() {
        if !is_installed() {
            return;
        }
        let workspace = Workspace::new();
        let input = workspace.0.join("sample.docx");
        let output = workspace.0.join("sample.pdf");
        crate::docx::write_text_docx(&input, &["Retry".to_owned()]).expect("create test DOCX");
        let started = Instant::now();
        let result = docx_to_pdf(&input, &output, &workspace.0, || {
            started.elapsed() >= Duration::from_millis(200)
        });
        assert!(matches!(result, Err(OfficeError::Cancelled)));
        assert!(!output.exists());
        docx_to_pdf(&input, &output, &workspace.0, || false).expect("retry after cancellation");
        verify_pdf(&output).expect("retry publishes valid PDF");
    }
}
