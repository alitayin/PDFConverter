use crate::pdf::{extract_text_from_path_with_control, PageSelection, PdfError};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct WorkerRequest {
    pub protocol: u8,
    pub job_id: String,
    pub kind: String,
    pub input: PathBuf,
    pub output: PathBuf,
    #[serde(default)]
    pub options: WorkerOptions,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct WorkerOptions {
    pub pages: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct WorkerEvent {
    pub job_id: String,
    pub seq: u64,
    pub state: WorkerState,
    pub completed: usize,
    pub total: usize,
    pub phase: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outputs: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)]
pub enum WorkerState {
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    TimedOut,
}

#[derive(Debug, Clone)]
pub struct WorkerResult {
    pub output: PathBuf,
    pub page_count: usize,
    pub text_object_count: usize,
}

#[derive(Debug, Clone)]
pub struct WorkerFailure {
    pub code: &'static str,
    pub message: String,
}

pub fn execute(
    request: &WorkerRequest,
    mut emit: impl FnMut(WorkerEvent),
) -> Result<WorkerResult, WorkerFailure> {
    execute_with_control(request, &mut emit, || false)
}

pub fn execute_with_control(
    request: &WorkerRequest,
    mut emit: impl FnMut(WorkerEvent),
    should_stop: impl Fn() -> bool,
) -> Result<WorkerResult, WorkerFailure> {
    if request.protocol != 1 {
        return Err(WorkerFailure {
            code: "WORKER_PROTOCOL_UNSUPPORTED",
            message: "Unsupported worker protocol version".to_owned(),
        });
    }
    if request.kind != "pdf_to_txt" {
        return Err(WorkerFailure {
            code: "UNSUPPORTED_FORMAT",
            message: "This worker currently supports PDF to TXT only".to_owned(),
        });
    }
    if !request.input.is_absolute() || !request.output.is_absolute() {
        return Err(WorkerFailure {
            code: "PATH_NOT_ALLOWED",
            message: "Input and output paths must be absolute".to_owned(),
        });
    }
    if !request.input.is_file() {
        return Err(WorkerFailure {
            code: "INPUT_NOT_FOUND",
            message: "The input file does not exist".to_owned(),
        });
    }

    if should_stop() {
        return Err(WorkerFailure {
            code: "CANCELLED",
            message: "Task cancelled".to_owned(),
        });
    }
    emit(event(
        request,
        1,
        WorkerState::Running,
        0,
        1,
        "reading",
        "Reading PDF",
    ));
    let selection =
        PageSelection::parse(request.options.pages.as_deref()).map_err(failure_from_pdf)?;
    emit(event(
        request,
        2,
        WorkerState::Running,
        0,
        1,
        "extracting",
        "Extracting the text layer",
    ));
    let result = extract_text_from_path_with_control(&request.input, &selection, &should_stop)
        .map_err(failure_from_pdf)?;

    if should_stop() {
        return Err(WorkerFailure {
            code: "CANCELLED",
            message: "Task cancelled".to_owned(),
        });
    }

    if let Some(parent) = request.output.parent() {
        fs::create_dir_all(parent).map_err(|error| WorkerFailure {
            code: "OUTPUT_WRITE_FAILED",
            message: format!("Could not create the output directory: {error}"),
        })?;
    }
    emit(event(
        request,
        3,
        WorkerState::Running,
        0,
        1,
        "writing",
        "Writing TXT",
    ));
    fs::write(&request.output, result.text.as_bytes()).map_err(|error| WorkerFailure {
        code: "OUTPUT_WRITE_FAILED",
        message: format!("Could not write TXT: {error}"),
    })?;

    Ok(WorkerResult {
        output: request.output.clone(),
        page_count: result.page_count,
        text_object_count: result.text_object_count,
    })
}

#[allow(dead_code)]
pub fn run_stdio() -> io::Result<()> {
    let stdin = io::stdin();
    let mut stdout = io::BufWriter::new(io::stdout().lock());
    let mut line = String::new();
    stdin.lock().read_line(&mut line)?;
    let request: WorkerRequest = match serde_json::from_str(line.trim()) {
        Ok(request) => request,
        Err(error) => {
            write_event(
                &mut stdout,
                WorkerEvent {
                    job_id: "unknown".to_owned(),
                    seq: 1,
                    state: WorkerState::Failed,
                    completed: 0,
                    total: 1,
                    phase: "validation".to_owned(),
                    message: format!("Invalid worker request: {error}"),
                    outputs: None,
                    error_code: Some("INVALID_WORKER_REQUEST".to_owned()),
                },
            )?;
            return Ok(());
        }
    };

    let mut last_seq = 0;
    let mut emit_to_stdout = |event: WorkerEvent| {
        last_seq = last_seq.max(event.seq);
        let _ = write_event(&mut stdout, event);
    };
    match execute(&request, &mut emit_to_stdout) {
        Ok(result) => write_event(
            &mut stdout,
            WorkerEvent {
                job_id: request.job_id.clone(),
                seq: last_seq + 1,
                state: WorkerState::Succeeded,
                completed: 1,
                total: 1,
                phase: "done".to_owned(),
                message: format!(
                    "Wrote {} pages and {} text blocks",
                    result.page_count, result.text_object_count
                ),
                outputs: Some(vec![result.output.to_string_lossy().into_owned()]),
                error_code: None,
            },
        )?,
        Err(error) => write_event(
            &mut stdout,
            WorkerEvent {
                job_id: request.job_id,
                seq: last_seq + 1,
                state: WorkerState::Failed,
                completed: 0,
                total: 1,
                phase: "failed".to_owned(),
                message: error.message,
                outputs: None,
                error_code: Some(error.code.to_owned()),
            },
        )?,
    }
    stdout.flush()
}

#[allow(dead_code)]
fn write_event(writer: &mut impl Write, event: WorkerEvent) -> io::Result<()> {
    serde_json::to_writer(&mut *writer, &event)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

fn event(
    request: &WorkerRequest,
    seq: u64,
    state: WorkerState,
    completed: usize,
    total: usize,
    phase: &str,
    message: &str,
) -> WorkerEvent {
    WorkerEvent {
        job_id: request.job_id.clone(),
        seq,
        state,
        completed,
        total,
        phase: phase.to_owned(),
        message: message.to_owned(),
        outputs: None,
        error_code: None,
    }
}

fn failure_from_pdf(error: PdfError) -> WorkerFailure {
    let code = match error {
        PdfError::InvalidPdf | PdfError::CorruptedPdf => "CORRUPTED_PDF",
        PdfError::UnsupportedStructure | PdfError::UnsupportedStreamLength => {
            "UNSUPPORTED_PDF_STRUCTURE"
        }
        PdfError::StreamLimitExceeded | PdfError::CMapLimitExceeded => "PDF_STREAM_TOO_LARGE",
        PdfError::NoTextLayer => "NO_TEXT_LAYER",
        PdfError::InvalidPageRange => "INVALID_PAGES",
        PdfError::PageLimitExceeded => "PAGE_LIMIT_EXCEEDED",
        PdfError::UnsupportedFilter(_) | PdfError::Decompression => "CONVERSION_FAILED",
        PdfError::Cancelled => "CANCELLED",
        PdfError::Io(_) => "INPUT_READ_FAILED",
    };
    WorkerFailure {
        code,
        message: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    #[test]
    fn rejects_non_pdf_worker_kind() {
        let request = WorkerRequest {
            protocol: 1,
            job_id: "j_test".to_owned(),
            kind: "pdf_to_image".to_owned(),
            input: Path::new("/tmp/input.pdf").to_owned(),
            output: Path::new("/tmp/output.txt").to_owned(),
            options: WorkerOptions::default(),
        };
        let result = execute(&request, |_| {});
        assert_eq!(result.expect_err("must reject").code, "UNSUPPORTED_FORMAT");
    }

    #[test]
    fn maps_unsupported_pdf_structure_to_stable_worker_code() {
        assert_eq!(
            failure_from_pdf(PdfError::UnsupportedStructure).code,
            "UNSUPPORTED_PDF_STRUCTURE"
        );
        assert_eq!(
            failure_from_pdf(PdfError::UnsupportedStreamLength).code,
            "UNSUPPORTED_PDF_STRUCTURE"
        );
        assert_eq!(
            failure_from_pdf(PdfError::StreamLimitExceeded).code,
            "PDF_STREAM_TOO_LARGE"
        );
        assert_eq!(
            failure_from_pdf(PdfError::CMapLimitExceeded).code,
            "PDF_STREAM_TOO_LARGE"
        );
    }

    #[test]
    fn emits_ordered_progress_for_a_valid_fixture() {
        let input = std::env::temp_dir().join("minimal-pdf-worker-input.pdf");
        let output = std::env::temp_dir().join("minimal-pdf-worker-output.txt");
        fs::write(
            &input,
            b"%PDF-1.7\n1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj\n2 0 obj << /Type /Pages /Kids [3 0 R] /Count 1 >> endobj\n3 0 obj << /Type /Page /Parent 2 0 R /Contents 4 0 R >> endobj\n4 0 obj << /Length 22 >>\nstream\nBT (worker test) Tj ET\nendstream\nendobj\ntrailer << /Root 1 0 R >>\n%%EOF\n",
        )
        .expect("write fixture");
        let request = WorkerRequest {
            protocol: 1,
            job_id: "j_test".to_owned(),
            kind: "pdf_to_txt".to_owned(),
            input,
            output: output.clone(),
            options: WorkerOptions::default(),
        };
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&events);
        let result = execute(&request, |event| captured.lock().expect("lock").push(event));
        assert!(result.is_ok());
        let events = events.lock().expect("lock");
        assert_eq!(
            events.iter().map(|event| event.seq).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(
            fs::read_to_string(output).expect("read output"),
            "worker test"
        );
        let _ = fs::remove_file(request.input);
        let _ = fs::remove_file(request.output);
    }
}
