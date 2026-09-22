use crate::{
    build_self_check, cancel_job, cancellation_registry, diagnostic_info, inspect_input_files,
    start_job_with_emitter, worker, StartJobRequest,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, Value};
use std::ffi::OsStr;
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

const MAX_REQUEST_BYTES: usize = 1024 * 1024;
const MAX_PENDING_JOBS: usize = 32;
#[derive(Debug)]
struct Config {
    worker_path: Option<PathBuf>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    id: String,
    command: String,
    #[serde(default = "empty_args")]
    args: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyArgs {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PathsArgs {
    paths: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JobArgs {
    request: StartJobRequest,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CancelArgs {
    job_id: String,
}

#[derive(Serialize)]
struct Response {
    id: String,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Serialize)]
struct Progress {
    event: &'static str,
    payload: worker::WorkerEvent,
}

fn empty_args() -> Value {
    json!({})
}

fn parse_args<T: DeserializeOwned>(value: Value) -> Result<T, String> {
    serde_json::from_value(value)
        .map_err(|_| String::from("INVALID_REQUEST: invalid parameters or unsupported fields"))
}

fn parse_config() -> Result<Config, String> {
    let mut args = std::env::args_os().skip(2);
    let mut worker_path = None;
    while let Some(flag) = args.next() {
        if flag == OsStr::new("--worker-path") && worker_path.is_none() {
            worker_path = args.next().map(PathBuf::from);
            if worker_path.is_none() {
                return Err(String::from(
                    "INVALID_BRIDGE_CONFIG: missing --worker-path value",
                ));
            }
        } else {
            return Err(String::from(
                "INVALID_BRIDGE_CONFIG: unknown or duplicate startup argument",
            ));
        }
    }
    if worker_path.as_ref().is_some_and(|path| !path.is_absolute()) {
        return Err(String::from(
            "INVALID_BRIDGE_CONFIG: worker path must be absolute",
        ));
    }
    Ok(Config { worker_path })
}

fn read_frame(reader: &mut impl BufRead, frame: &mut Vec<u8>) -> io::Result<Option<bool>> {
    frame.clear();
    let mut too_long = false;
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(if frame.is_empty() && !too_long {
                None
            } else {
                Some(!too_long)
            });
        }
        let size = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        let ends_line = available[size - 1] == b'\n';
        if !too_long && frame.len().saturating_add(size) <= MAX_REQUEST_BYTES {
            frame.extend_from_slice(&available[..size]);
        } else {
            too_long = true;
            frame.clear();
        }
        reader.consume(size);
        if ends_line {
            return Ok(Some(!too_long));
        }
    }
}

fn write_line(writer: &mut impl Write, message: &impl Serialize) -> io::Result<()> {
    serde_json::to_writer(&mut *writer, message)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

fn write_shared(
    stdout: &Arc<Mutex<BufWriter<io::Stdout>>>,
    message: &impl Serialize,
) -> io::Result<()> {
    let mut writer = stdout
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    write_line(&mut *writer, message)
}

fn cancel_all_jobs() {
    let registry = cancellation_registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    for cancellation in registry.values() {
        cancellation.store(true, Ordering::Release);
    }
}

fn dispatch(
    command: &str,
    args: Value,
    config: &Config,
    stdout: &Arc<Mutex<BufWriter<io::Stdout>>>,
    workers: &mut Vec<JoinHandle<()>>,
) -> Result<Value, String> {
    match command {
        "get_self_check" => {
            let _: EmptyArgs = parse_args(args)?;
            serde_json::to_value(build_self_check())
        }
        "get_diagnostic_info" => {
            let _: EmptyArgs = parse_args(args)?;
            let text = diagnostic_info(config.worker_path.as_deref())?;
            serde_json::to_value(text)
        }
        "inspect_input_files" => {
            let args: PathsArgs = parse_args(args)?;
            serde_json::to_value(inspect_input_files(args.paths)?)
        }
        "start_job" => {
            let args: JobArgs = parse_args(args)?;
            let mut index = 0;
            while index < workers.len() {
                if workers[index].is_finished() {
                    let worker = workers.swap_remove(index);
                    let _ = worker.join();
                } else {
                    index += 1;
                }
            }
            if workers.len() >= MAX_PENDING_JOBS {
                return Err(String::from("JOB_LIMIT_REACHED: too many queued tasks"));
            }
            let stdout = Arc::clone(stdout);
            let (job_id, worker) = start_job_with_emitter(args.request, move |event| {
                let progress = Progress {
                    event: "conversion://progress",
                    payload: event,
                };
                if write_shared(&stdout, &progress).is_err() {
                    cancel_all_jobs();
                }
            })?;
            workers.push(worker);
            serde_json::to_value(job_id)
        }
        "cancel_job" => {
            let args: CancelArgs = parse_args(args)?;
            cancel_job(args.job_id)?;
            serde_json::to_value(())
        }
        _ => return Err(String::from("UNKNOWN_COMMAND: unsupported bridge command")),
    }
    .map_err(|_| String::from("BRIDGE_SERIALIZATION_FAILED: response serialization failed"))
}

fn handle_frame(
    frame: &[u8],
    config: &Config,
    stdout: &Arc<Mutex<BufWriter<io::Stdout>>>,
    workers: &mut Vec<JoinHandle<()>>,
) -> Response {
    let request: Request = match serde_json::from_slice(frame) {
        Ok(request) => request,
        Err(_) => {
            return Response {
                id: String::from("unknown"),
                ok: false,
                result: None,
                error: Some(String::from("INVALID_REQUEST: invalid JSONL request")),
            }
        }
    };
    if request.id.is_empty() || request.id.len() > 128 {
        return Response {
            id: String::from("unknown"),
            ok: false,
            result: None,
            error: Some(String::from("INVALID_REQUEST: invalid request id")),
        };
    }
    let id = request.id;
    let outcome = dispatch(&request.command, request.args, config, stdout, workers);
    match outcome {
        Ok(result) => Response {
            id,
            ok: true,
            result: Some(result),
            error: None,
        },
        Err(error) => Response {
            id,
            ok: false,
            result: None,
            error: Some(error),
        },
    }
}

fn serve(
    config: &Config,
    stdout: &Arc<Mutex<BufWriter<io::Stdout>>>,
    workers: &mut Vec<JoinHandle<()>>,
) -> io::Result<()> {
    let stdin = io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let mut frame = Vec::with_capacity(1024);
    while let Some(within_limit) = read_frame(&mut reader, &mut frame)? {
        let response = if within_limit {
            handle_frame(&frame, config, stdout, workers)
        } else {
            Response {
                id: String::from("unknown"),
                ok: false,
                result: None,
                error: Some(String::from(
                    "INVALID_REQUEST: request exceeds the size limit",
                )),
            }
        };
        write_shared(stdout, &response)?;
    }
    Ok(())
}

pub fn run_stdio() -> Result<(), String> {
    let config = parse_config()?;
    let stdout = Arc::new(Mutex::new(BufWriter::new(io::stdout())));
    let mut workers = Vec::new();
    let result = serve(&config, &stdout, &mut workers);
    cancel_all_jobs();
    for worker in workers {
        let _ = worker.join();
    }
    result.map_err(|error| format!("BRIDGE_IO_FAILED: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{pdf_writer, run_job, ConversionKind, ConversionOptions};
    use std::sync::atomic::AtomicBool;
    use uuid::Uuid;

    #[test]
    fn bounded_jsonl_reader_recovers_after_oversized_frame() {
        let input = format!("{}\n{{\"id\":\"next\"}}\n", "x".repeat(MAX_REQUEST_BYTES));
        let mut reader = BufReader::new(input.as_bytes());
        let mut frame = Vec::new();
        assert_eq!(read_frame(&mut reader, &mut frame).unwrap(), Some(false));
        assert_eq!(read_frame(&mut reader, &mut frame).unwrap(), Some(true));
        assert_eq!(frame, b"{\"id\":\"next\"}\n");
        assert_eq!(read_frame(&mut reader, &mut frame).unwrap(), None);
    }

    #[test]
    fn rejects_unknown_job_options_instead_of_silently_using_defaults() {
        let request = json!({
            "kind": "pdf_to_txt",
            "inputs": ["/tmp/example.pdf"],
            "options": { "dpp": 300 }
        });
        assert!(serde_json::from_value::<StartJobRequest>(request).is_err());
    }

    #[test]
    fn response_and_event_use_distinct_jsonl_shapes() {
        let mut bytes = Vec::new();
        write_line(
            &mut bytes,
            &Response {
                id: String::from("r1"),
                ok: true,
                result: Some(json!("job_1")),
                error: None,
            },
        )
        .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&bytes).unwrap(),
            json!({"id":"r1", "ok":true, "result":"job_1"})
        );
        bytes.clear();
        write_line(
            &mut bytes,
            &Progress {
                event: "conversion://progress",
                payload: worker::WorkerEvent {
                    job_id: String::from("job_1"),
                    seq: 1,
                    state: worker::WorkerState::Queued,
                    completed: 0,
                    total: 1,
                    phase: String::from("queued"),
                    message: String::from("Task queued"),
                    outputs: None,
                    error_code: None,
                },
            },
        )
        .unwrap();
        let event: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(event["event"], "conversion://progress");
        assert_eq!(event["payload"]["job_id"], "job_1");
        assert_eq!(event["payload"]["state"], "queued");
    }

    #[test]
    fn bridge_job_uses_existing_engine_and_emits_success() {
        let root = std::env::temp_dir().join(format!(
            "minimal-pdf-electron-bridge-{}",
            Uuid::new_v4().simple()
        ));
        std::fs::create_dir(&root).expect("create isolated output directory");
        let input = root.join("input.pdf");
        pdf_writer::write_text_pdf(&input, "bridge fixture").expect("write fixture PDF");

        let events = Arc::new(Mutex::new(Vec::new()));
        let shared = Arc::clone(&events);
        let (job_id, worker) = start_job_with_emitter(
            StartJobRequest {
                kind: ConversionKind::PdfToTxt,
                inputs: vec![input.to_string_lossy().into_owned()],
                output_dir: Some(root.to_string_lossy().into_owned()),
                options: ConversionOptions::default(),
            },
            move |event| shared.lock().expect("event lock").push(event),
        )
        .expect("start local conversion");
        worker.join().expect("conversion thread completes");

        let events = events.lock().expect("event lock");
        assert!(matches!(
            events.first().map(|event| &event.state),
            Some(worker::WorkerState::Queued)
        ));
        let success = events
            .iter()
            .find(|event| matches!(event.state, worker::WorkerState::Succeeded))
            .expect("completion event");
        assert_eq!(success.job_id, job_id);
        assert_eq!(success.completed, 1);
        let output = success
            .outputs
            .as_ref()
            .and_then(|paths| paths.first())
            .expect("output path");
        assert!(std::fs::read_to_string(output)
            .expect("read TXT")
            .contains("bridge fixture"));
        drop(events);
        std::fs::remove_dir_all(root).expect("clean fixture");
    }

    #[test]
    fn cancelled_bridge_job_emits_terminal_event_without_output() {
        let root = std::env::temp_dir().join(format!(
            "minimal-pdf-electron-cancel-{}",
            Uuid::new_v4().simple()
        ));
        std::fs::create_dir(&root).expect("create isolated output directory");
        let cancelled = Arc::new(AtomicBool::new(true));
        let mut events = Vec::new();
        run_job(
            String::from("job_cancelled"),
            ConversionKind::PdfToTxt,
            vec![root.join("unused.pdf")],
            Some(root.clone()),
            ConversionOptions::default(),
            cancelled,
            &mut |event| events.push(event),
        );
        assert!(matches!(
            events.first().map(|event| &event.state),
            Some(worker::WorkerState::Queued)
        ));
        assert!(matches!(
            events.last().map(|event| &event.state),
            Some(worker::WorkerState::Cancelled)
        ));
        assert_eq!(
            events.last().and_then(|event| event.error_code.as_deref()),
            Some("CANCELLED")
        );
        assert!(!root.join("unused.txt").exists());
        std::fs::remove_dir_all(root).expect("clean fixture");
    }
}
