//! Stable Markdown export from a PDF's existing text layer.

use crate::pdf::{self, PageSelection, PdfError, TextPage};
use std::fs::{self, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use thiserror::Error;

const MAX_INPUT_BYTES: u64 = 250 * 1024 * 1024;
const MAX_OUTPUT_BYTES: u64 = 64 * 1024 * 1024;
const ESCAPE_CHUNK_BYTES: usize = 8 * 1024;

/// Failures while extracting PDF text or writing the Markdown document.
#[derive(Debug, Error)]
pub(crate) enum MarkdownError {
    #[error("The PDF input must be a local regular file and cannot be a symlink")]
    InvalidInputPath,
    #[error("The PDF input exceeds the Markdown conversion safety limit")]
    InputTooLarge,
    #[error("Markdown output must be a new .md file in an existing directory")]
    InvalidOutputPath,
    #[error("Markdown output exceeds the 64 MiB safety limit")]
    OutputTooLarge,
    #[error("PDF to Markdown conversion cancelled")]
    Cancelled,
    #[error("Could not read the PDF text layer: {0}")]
    Pdf(#[from] PdfError),
    #[error("Could not write Markdown: {0}")]
    OutputIo(#[source] io::Error),
}

impl MarkdownError {
    /// Returns the stable worker error code for this failure class.
    #[allow(dead_code)]
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::InvalidInputPath => "PATH_NOT_ALLOWED",
            Self::InputTooLarge => "INPUT_TOO_LARGE",
            Self::InvalidOutputPath => "OUTPUT_PATH_NOT_ALLOWED",
            Self::OutputTooLarge => "OUTPUT_SIZE_EXCEEDED",
            Self::Cancelled | Self::Pdf(PdfError::Cancelled) => "CANCELLED",
            Self::Pdf(PdfError::NoTextLayer) => "NO_TEXT_LAYER",
            Self::Pdf(PdfError::PageLimitExceeded) => "PAGE_LIMIT_EXCEEDED",
            Self::Pdf(PdfError::InvalidPageRange) => "INVALID_PAGES",
            Self::Pdf(PdfError::Io(_)) => "INPUT_READ_FAILED",
            Self::Pdf(_) => "CONVERSION_FAILED",
            Self::OutputIo(_) => "OUTPUT_WRITE_FAILED",
        }
    }
}

/// Observable facts about a completed Markdown export.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MarkdownStats {
    pub(crate) page_count: usize,
    pub(crate) text_object_count: usize,
    pub(crate) output_bytes: u64,
}

struct OutputGuard {
    path: PathBuf,
    keep: bool,
}

impl Drop for OutputGuard {
    fn drop(&mut self) {
        if !self.keep {
            let _ = fs::remove_file(&self.path);
        }
    }
}

struct BoundedWriter<W> {
    inner: W,
    written: u64,
    limit: u64,
}

impl<W: Write> BoundedWriter<W> {
    fn new(inner: W, limit: u64) -> Self {
        Self {
            inner,
            written: 0,
            limit,
        }
    }

    fn write_chunk(&mut self, bytes: &[u8]) -> Result<(), MarkdownError> {
        let chunk_bytes = u64::try_from(bytes.len()).map_err(|_| MarkdownError::OutputTooLarge)?;
        let next_size = self
            .written
            .checked_add(chunk_bytes)
            .filter(|size| *size <= self.limit)
            .ok_or(MarkdownError::OutputTooLarge)?;
        self.inner
            .write_all(bytes)
            .map_err(MarkdownError::OutputIo)?;
        self.written = next_size;
        Ok(())
    }

    fn flush(&mut self) -> Result<(), MarkdownError> {
        self.inner.flush().map_err(MarkdownError::OutputIo)
    }
}

/// Writes deterministic Markdown from every PDF page in extraction order.
///
/// The exporter preserves page boundaries with `<!-- page: N -->` comments and
/// escapes text that could otherwise become Markdown syntax. It deliberately
/// does not infer headings, lists, tables, or other document structure.
/// Existing output is never overwritten.
///
/// # Errors
///
/// Returns an error for unsafe paths, unreadable or unsupported PDFs, PDFs
/// without a text layer, cancellation, output above the fixed safety limit,
/// or any output creation, write, or flush failure. Any output created by this
/// call is removed unless the complete document was flushed successfully.
#[allow(dead_code)]
pub(crate) fn pdf_to_markdown(
    input: &Path,
    output: &Path,
    should_stop: impl Fn() -> bool,
) -> Result<MarkdownStats, MarkdownError> {
    pdf_to_markdown_with_limit(input, output, MAX_OUTPUT_BYTES, &should_stop)
}

fn pdf_to_markdown_with_limit(
    input: &Path,
    output: &Path,
    output_limit: u64,
    should_stop: &impl Fn() -> bool,
) -> Result<MarkdownStats, MarkdownError> {
    validate_paths(input, output)?;
    if should_stop() {
        return Err(MarkdownError::Cancelled);
    }

    let extracted =
        pdf::extract_text_from_path_with_control(input, &PageSelection::All, should_stop)?;
    if should_stop() {
        return Err(MarkdownError::Cancelled);
    }

    let output_bytes = write_document(output, &extracted.pages, output_limit, should_stop)?;
    Ok(MarkdownStats {
        page_count: extracted.page_count,
        text_object_count: extracted.text_object_count,
        output_bytes,
    })
}

fn validate_paths(input: &Path, output: &Path) -> Result<(), MarkdownError> {
    if !input.is_absolute()
        || input
            .extension()
            .and_then(|value| value.to_str())
            .is_none_or(|value| !value.eq_ignore_ascii_case("pdf"))
    {
        return Err(MarkdownError::InvalidInputPath);
    }
    let input_metadata = fs::symlink_metadata(input).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            MarkdownError::InvalidInputPath
        } else {
            MarkdownError::Pdf(PdfError::Io(error))
        }
    })?;
    if !input_metadata.file_type().is_file()
        || input_metadata.file_type().is_symlink()
        || input_metadata.len() == 0
    {
        return Err(MarkdownError::InvalidInputPath);
    }
    if input_metadata.len() > MAX_INPUT_BYTES {
        return Err(MarkdownError::InputTooLarge);
    }

    let Some(parent) = output.parent() else {
        return Err(MarkdownError::InvalidOutputPath);
    };
    let parent_is_safe_directory = fs::symlink_metadata(parent)
        .map(|metadata| metadata.file_type().is_dir() && !metadata.file_type().is_symlink())
        .unwrap_or(false);
    let output_is_valid = output.is_absolute()
        && output
            .extension()
            .and_then(|value| value.to_str())
            .is_some_and(|value| value.eq_ignore_ascii_case("md"))
        && parent_is_safe_directory;
    if !output_is_valid {
        return Err(MarkdownError::InvalidOutputPath);
    }
    Ok(())
}

fn write_document(
    output: &Path,
    pages: &[TextPage],
    output_limit: u64,
    should_stop: &impl Fn() -> bool,
) -> Result<u64, MarkdownError> {
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)
        .map_err(|error| {
            if error.kind() == io::ErrorKind::AlreadyExists {
                MarkdownError::InvalidOutputPath
            } else {
                MarkdownError::OutputIo(error)
            }
        })?;
    let mut guard = OutputGuard {
        path: output.to_path_buf(),
        keep: false,
    };
    let mut writer = BoundedWriter::new(BufWriter::new(file), output_limit);

    for (index, page) in pages.iter().enumerate() {
        if should_stop() {
            return Err(MarkdownError::Cancelled);
        }
        if index > 0 {
            writer.write_chunk(b"\n\n")?;
        }
        let page_number = index.checked_add(1).ok_or(MarkdownError::OutputTooLarge)?;
        let marker = format!("<!-- page: {page_number} -->");
        writer.write_chunk(marker.as_bytes())?;
        if !page.text.is_empty() {
            writer.write_chunk(b"\n\n")?;
            write_escaped_text(&mut writer, &page.text, should_stop)?;
        }
    }

    if should_stop() {
        return Err(MarkdownError::Cancelled);
    }
    writer.write_chunk(b"\n")?;
    writer.flush()?;
    let output_bytes = writer.written;
    drop(writer);
    guard.keep = true;
    Ok(output_bytes)
}

fn write_escaped_text<W: Write>(
    writer: &mut BoundedWriter<W>,
    text: &str,
    should_stop: &impl Fn() -> bool,
) -> Result<(), MarkdownError> {
    if should_stop() {
        return Err(MarkdownError::Cancelled);
    }
    let mut chunk = String::with_capacity(ESCAPE_CHUNK_BYTES);
    for segment in text.split_inclusive('\n') {
        let (line, has_newline) = segment
            .strip_suffix('\n')
            .map_or((segment, false), |line| (line, true));
        write_escaped_line(writer, &mut chunk, line, should_stop)?;
        if has_newline {
            chunk.push('\n');
        }
        if chunk.len() >= ESCAPE_CHUNK_BYTES {
            flush_escape_chunk(writer, &mut chunk, should_stop)?;
        }
    }
    flush_escape_chunk(writer, &mut chunk, should_stop)
}

fn write_escaped_line<W: Write>(
    writer: &mut BoundedWriter<W>,
    chunk: &mut String,
    line: &str,
    should_stop: &impl Fn() -> bool,
) -> Result<(), MarkdownError> {
    let syntax_start = line
        .as_bytes()
        .iter()
        .take_while(|byte| **byte == b' ')
        .count();
    let syntax_start = (syntax_start <= 3).then_some(syntax_start);

    for (byte_index, character) in line.char_indices() {
        let escape = is_inline_control(character)
            || (character == '!' && line[byte_index + character.len_utf8()..].starts_with('['))
            || syntax_start.is_some_and(|start| {
                is_atx_heading_marker(line, byte_index, start, character)
                    || is_list_or_rule_marker(line, byte_index, start, character)
                    || is_ordered_list_period(line, byte_index, start, character)
            });
        if escape {
            chunk.push('\\');
        }
        chunk.push(character);
        if chunk.len() >= ESCAPE_CHUNK_BYTES {
            flush_escape_chunk(writer, chunk, should_stop)?;
        }
    }
    Ok(())
}

fn flush_escape_chunk<W: Write>(
    writer: &mut BoundedWriter<W>,
    chunk: &mut String,
    should_stop: &impl Fn() -> bool,
) -> Result<(), MarkdownError> {
    if should_stop() {
        return Err(MarkdownError::Cancelled);
    }
    if !chunk.is_empty() {
        writer.write_chunk(chunk.as_bytes())?;
        chunk.clear();
    }
    Ok(())
}

fn is_inline_control(character: char) -> bool {
    matches!(
        character,
        '\\' | '`' | '*' | '_' | '{' | '}' | '[' | ']' | '<' | '>' | '|'
    )
}

fn is_atx_heading_marker(line: &str, index: usize, start: usize, character: char) -> bool {
    if character != '#' || index != start {
        return false;
    }
    let marker_count = line[index..]
        .chars()
        .take_while(|value| *value == '#')
        .count();
    if !(1..=6).contains(&marker_count) {
        return false;
    }
    line[index + marker_count..]
        .chars()
        .next()
        .is_none_or(char::is_whitespace)
}

fn is_list_or_rule_marker(line: &str, index: usize, start: usize, character: char) -> bool {
    if index != start || !matches!(character, '+' | '-') {
        return false;
    }
    let following = &line[index + character.len_utf8()..];
    if following.chars().next().is_none_or(char::is_whitespace) {
        return true;
    }
    character == '-'
        && line[index..]
            .chars()
            .all(|value| value == '-' || value == ' ' || value == '\t')
        && line[index..].chars().filter(|value| *value == '-').count() >= 3
}

fn is_ordered_list_period(line: &str, index: usize, start: usize, character: char) -> bool {
    if character != '.' || index <= start {
        return false;
    }
    let digits = &line[start..index];
    (1..=9).contains(&digits.len())
        && digits.bytes().all(|byte| byte.is_ascii_digit())
        && line[index + 1..]
            .chars()
            .next()
            .is_none_or(char::is_whitespace)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let unique = NEXT.fetch_add(1, Ordering::Relaxed);
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "minimalpdf-markdown-{label}-{}-{now}-{unique}",
                std::process::id()
            ));
            fs::create_dir(&path).expect("create isolated test directory");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn utf16_hex(value: &str) -> String {
        let mut output = String::from("FEFF");
        for unit in value.encode_utf16() {
            output.push_str(&format!("{unit:04X}"));
        }
        output
    }

    fn pdf_fixture(pages: &[&[&str]]) -> Vec<u8> {
        assert!(!pages.is_empty());
        let mut objects = Vec::new();
        objects.push(b"<< /Type /Catalog /Pages 2 0 R >>".to_vec());

        let kids = (0..pages.len())
            .map(|index| format!("{} 0 R", 3 + index * 2))
            .collect::<Vec<_>>()
            .join(" ");
        objects
            .push(format!("<< /Type /Pages /Kids [{kids}] /Count {} >>", pages.len()).into_bytes());

        for (index, lines) in pages.iter().enumerate() {
            let page_object = 3 + index * 2;
            let stream_object = page_object + 1;
            objects.push(
                format!(
                    "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents {stream_object} 0 R >>"
                )
                .into_bytes(),
            );
            let content = if lines.is_empty() {
                String::from("q Q")
            } else {
                let operations = lines
                    .iter()
                    .enumerate()
                    .map(|(line_index, line)| {
                        let move_line = if line_index == 0 { "" } else { " T*" };
                        format!("{move_line} <{}> Tj", utf16_hex(line))
                    })
                    .collect::<String>();
                format!("BT /F1 12 Tf 72 720 Td{operations} ET")
            };
            let mut stream = format!("<< /Length {} >>\nstream\n", content.len()).into_bytes();
            stream.extend_from_slice(content.as_bytes());
            stream.extend_from_slice(b"\nendstream");
            objects.push(stream);
        }

        let mut pdf = b"%PDF-1.7\n".to_vec();
        let mut offsets = Vec::with_capacity(objects.len());
        for (index, object) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.extend_from_slice(format!("{} 0 obj\n", index + 1).as_bytes());
            pdf.extend_from_slice(object);
            pdf.extend_from_slice(b"\nendobj\n");
        }
        let xref_offset = pdf.len();
        pdf.extend_from_slice(
            format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
        );
        for offset in offsets {
            pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(
            format!(
                "trailer << /Root 1 0 R /Size {} >>\nstartxref\n{xref_offset}\n%%EOF\n",
                objects.len() + 1
            )
            .as_bytes(),
        );
        pdf
    }

    fn write_pdf(directory: &TestDir, name: &str, pages: &[&[&str]]) -> PathBuf {
        let input = directory.path().join(name);
        fs::write(&input, pdf_fixture(pages)).expect("write PDF fixture");
        input
    }

    #[test]
    fn preserves_multi_page_order_boundaries_utf8_and_empty_pages() {
        let directory = TestDir::new("multi-page");
        let input = write_pdf(
            &directory,
            "source.pdf",
            &[&["第一页", "second line"], &[], &["最后一页"]],
        );
        let output = directory.path().join("result.md");

        let stats = pdf_to_markdown(&input, &output, || false).expect("convert PDF");
        let markdown = fs::read_to_string(&output).expect("read Markdown");

        assert_eq!(
            markdown,
            "<!-- page: 1 -->\n\n第一页\nsecond line\n\n<!-- page: 2 -->\n\n<!-- page: 3 -->\n\n最后一页\n"
        );
        assert_eq!(stats.page_count, 3);
        assert_eq!(stats.text_object_count, 3);
        assert_eq!(
            stats.output_bytes,
            u64::try_from(markdown.len()).expect("Markdown length")
        );
    }

    #[test]
    fn escapes_syntax_only_where_plain_punctuation_could_be_reinterpreted() {
        let directory = TestDir::new("escaping");
        let input = write_pdf(
            &directory,
            "syntax.pdf",
            &[&[
                "# heading",
                "- item",
                "+ item",
                "1. ordered",
                "---",
                "normal # hash + plus - dash. Version 1.2!",
                "![alt] <tag> {value} a|b *em* _em_ `code` C:\\tmp",
                "! ordinary",
            ]],
        );
        let output = directory.path().join("syntax.md");

        pdf_to_markdown(&input, &output, || false).expect("convert syntax PDF");
        let markdown = fs::read_to_string(output).expect("read Markdown");

        assert_eq!(
            markdown,
            concat!(
                "<!-- page: 1 -->\n\n",
                "\\# heading\n",
                "\\- item\n",
                "\\+ item\n",
                "1\\. ordered\n",
                "\\---\n",
                "normal # hash + plus - dash. Version 1.2!\n",
                "\\!\\[alt\\] \\<tag\\> \\{value\\} a\\|b \\*em\\* \\_em\\_ \\`code\\` C:\\\\tmp\n",
                "! ordinary\n"
            )
        );
    }

    #[test]
    fn produces_identical_bytes_for_identical_input() {
        let directory = TestDir::new("deterministic");
        let input = write_pdf(&directory, "source.pdf", &[&["same", "content"]]);
        let first = directory.path().join("first.md");
        let second = directory.path().join("second.md");

        pdf_to_markdown(&input, &first, || false).expect("first conversion");
        pdf_to_markdown(&input, &second, || false).expect("second conversion");

        assert_eq!(
            fs::read(first).expect("first"),
            fs::read(second).expect("second")
        );
    }

    #[test]
    fn cancellation_before_extraction_creates_no_output() {
        let directory = TestDir::new("cancel-before");
        let input = write_pdf(&directory, "source.pdf", &[&["content"]]);
        let output = directory.path().join("cancelled.md");

        let error = pdf_to_markdown(&input, &output, || true).expect_err("must cancel");

        assert_eq!(error.code(), "CANCELLED");
        assert!(!output.exists());
    }

    #[test]
    fn cancellation_while_writing_removes_partial_output() {
        let directory = TestDir::new("cancel-write");
        let long_line = "text ".repeat(4_000);
        let input = write_pdf(&directory, "source.pdf", &[&[&long_line]]);
        let output = directory.path().join("cancelled.md");
        let write_checks = Cell::new(0_usize);

        let error = pdf_to_markdown(&input, &output, || {
            if output.exists() {
                let next = write_checks.get() + 1;
                write_checks.set(next);
                next >= 4
            } else {
                false
            }
        })
        .expect_err("must cancel during write");

        assert_eq!(error.code(), "CANCELLED");
        assert!(write_checks.get() >= 4);
        assert!(!output.exists());
    }

    #[test]
    fn output_limit_uses_checked_accounting_and_removes_partial_output() {
        let directory = TestDir::new("output-limit");
        let input = write_pdf(&directory, "source.pdf", &[&["too much content"]]);
        let output = directory.path().join("limited.md");

        let error = pdf_to_markdown_with_limit(&input, &output, 24, &|| false)
            .expect_err("must exceed output limit");

        assert_eq!(error.code(), "OUTPUT_SIZE_EXCEEDED");
        assert!(!output.exists());
    }

    #[test]
    fn existing_output_is_not_overwritten() {
        let directory = TestDir::new("existing");
        let input = write_pdf(&directory, "source.pdf", &[&["content"]]);
        let output = directory.path().join("existing.md");
        fs::write(&output, b"keep me").expect("write sentinel");

        let error = pdf_to_markdown(&input, &output, || false).expect_err("must not overwrite");

        assert_eq!(error.code(), "OUTPUT_PATH_NOT_ALLOWED");
        assert_eq!(fs::read(output).expect("read sentinel"), b"keep me");
    }

    #[test]
    fn extraction_failure_leaves_no_output() {
        let directory = TestDir::new("invalid-pdf");
        let input = directory.path().join("broken.pdf");
        fs::write(&input, b"%PDF-1.7\nbroken").expect("write broken PDF");
        let output = directory.path().join("broken.md");

        let error = pdf_to_markdown(&input, &output, || false).expect_err("must reject PDF");

        assert_eq!(error.code(), "CONVERSION_FAILED");
        assert!(!output.exists());
    }

    #[test]
    fn document_without_text_layer_is_rejected_without_output() {
        let directory = TestDir::new("no-text");
        let input = write_pdf(&directory, "image-only.pdf", &[&[], &[]]);
        let output = directory.path().join("image-only.md");

        let error = pdf_to_markdown(&input, &output, || false).expect_err("must reject no text");

        assert_eq!(error.code(), "NO_TEXT_LAYER");
        assert!(!output.exists());
    }
}
