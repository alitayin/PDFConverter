//! Bounded local Markdown to PDF conversion through the offline HTML printer.
//!
//! Markdown is parsed with `pulldown-cmark`, rendered to a self-contained HTML
//! document, and handed to [`crate::html_pdf`] for the existing Chromium worker.
//! Raw HTML and all non-inline image references are rejected before rendering.

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use pulldown_cmark::{html, Event, LinkType, Options, Parser, Tag};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use thiserror::Error;

pub(crate) const MAX_INPUT_BYTES: u64 = 4 * 1024 * 1024;
const MAX_OUTPUT_BYTES: u64 = 4 * 1024 * 1024;
const MAX_INLINE_IMAGE_BYTES: usize = 1024 * 1024;
const MAX_INLINE_IMAGE_TOTAL_BYTES: usize = 2 * 1024 * 1024;
const MAX_INLINE_IMAGE_PIXELS: u64 = 12_000_000;
const MAX_MARKDOWN_EVENTS: usize = 100_000;

const STYLE: &str = r#"
  @page { size: A4; margin: 18mm 16mm 18mm; }
  :root { color: #202124; background: #fff; font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; }
  body { margin: 0; line-height: 1.55; font-size: 11pt; overflow-wrap: anywhere; }
  h1, h2, h3, h4, h5, h6 { color: #111; line-height: 1.2; margin: 1.2em 0 0.45em; break-after: avoid; }
  h1 { font-size: 24pt; } h2 { font-size: 18pt; } h3 { font-size: 14pt; }
  p, ul, ol, blockquote, pre, table { margin: 0.65em 0; }
  blockquote { border-left: 3px solid #c7cbd1; color: #5f6368; padding: 0.1em 1em; }
  code { background: #f1f3f4; border-radius: 3px; padding: 0.08em 0.25em; font-family: ui-monospace, SFMono-Regular, Menlo, monospace; }
  pre { background: #f6f8fa; border: 1px solid #e1e4e8; border-radius: 4px; padding: 0.8em; white-space: pre-wrap; break-inside: avoid; }
  pre code { background: transparent; padding: 0; }
  table { border-collapse: collapse; width: 100%; font-size: 10pt; break-inside: avoid; }
  th, td { border: 1px solid #c7cbd1; padding: 0.35em 0.55em; text-align: left; vertical-align: top; }
  th { background: #f1f3f4; }
  img { max-width: 100%; max-height: 220mm; object-fit: contain; }
  a { color: #1558a6; text-decoration: underline; }
  hr { border: 0; border-top: 1px solid #c7cbd1; margin: 1.2em 0; }
  .task-list-item { list-style: none; }
  .task-list-item input { margin-right: 0.4em; }
"#;

#[derive(Debug, Error)]
pub(crate) enum MarkdownPdfError {
    #[error("Markdown input must be a regular UTF-8 .md/.markdown file at an absolute path")]
    InvalidInput,
    #[error("Markdown input exceeds the 4 MiB safety limit")]
    InputTooLarge,
    #[error("Markdown output must be a new .pdf file in an existing directory")]
    InvalidOutput,
    #[error("Markdown contains unsupported raw HTML, external images, or local image references")]
    UnsupportedContent,
    #[error("Embedded Markdown images exceed the safety limit")]
    InlineImageTooLarge,
    #[error("Markdown HTML output exceeds the safety limit")]
    OutputTooLarge,
    #[error("Markdown to PDF conversion cancelled")]
    Cancelled,
    #[error("Could not read Markdown: {0}")]
    InputIo(#[source] io::Error),
    #[error("Could not write Markdown HTML: {0}")]
    OutputIo(#[source] io::Error),
    #[error("HTML to PDF conversion failed: {0}")]
    Html(#[from] crate::html_pdf::HtmlPdfError),
}

impl MarkdownPdfError {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::InvalidInput => "PATH_NOT_ALLOWED",
            Self::InputTooLarge | Self::InlineImageTooLarge | Self::OutputTooLarge => {
                "INPUT_TOO_LARGE"
            }
            Self::InvalidOutput => "OUTPUT_PATH_NOT_ALLOWED",
            Self::UnsupportedContent => "UNSUPPORTED_FORMAT",
            Self::Cancelled | Self::Html(crate::html_pdf::HtmlPdfError::Cancelled) => "CANCELLED",
            Self::Html(error) => error.code(),
            Self::InputIo(_) => "INPUT_READ_FAILED",
            Self::OutputIo(_) => "OUTPUT_WRITE_FAILED",
        }
    }
}

struct RemoveOnDrop {
    path: PathBuf,
    keep: bool,
}

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        if !self.keep {
            let _ = fs::remove_file(&self.path);
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

fn validate_output(output: &Path) -> Result<(), MarkdownPdfError> {
    let parent = output.parent().ok_or(MarkdownPdfError::InvalidOutput)?;
    let parent_meta = fs::symlink_metadata(parent).map_err(|_| MarkdownPdfError::InvalidOutput)?;
    if !output.is_absolute()
        || fs::symlink_metadata(output).is_ok()
        || !parent_meta.file_type().is_dir()
        || parent_meta.file_type().is_symlink()
        || !output
            .extension()
            .and_then(|value| value.to_str())
            .is_some_and(|value| value.eq_ignore_ascii_case("pdf"))
    {
        return Err(MarkdownPdfError::InvalidOutput);
    }
    Ok(())
}

fn read_input(input: &Path, should_stop: &impl Fn() -> bool) -> Result<String, MarkdownPdfError> {
    if should_stop() {
        return Err(MarkdownPdfError::Cancelled);
    }
    if !input.is_absolute()
        || !input
            .extension()
            .and_then(|value| value.to_str())
            .is_some_and(|value| matches!(value.to_ascii_lowercase().as_str(), "md" | "markdown"))
    {
        return Err(MarkdownPdfError::InvalidInput);
    }
    let before = fs::symlink_metadata(input).map_err(|_| MarkdownPdfError::InvalidInput)?;
    if !before.file_type().is_file() || before.file_type().is_symlink() || before.len() == 0 {
        return Err(MarkdownPdfError::InvalidInput);
    }
    if before.len() > MAX_INPUT_BYTES {
        return Err(MarkdownPdfError::InputTooLarge);
    }
    let mut open = OpenOptions::new();
    open.read(true);
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        open.custom_flags(0x0000_0100);
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        open.custom_flags(0x0002_0000);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        open.custom_flags(0x0020_0000).share_mode(0x0000_0001);
    }
    let mut file = open
        .open(input)
        .map_err(|_| MarkdownPdfError::InvalidInput)?;
    let opened = file.metadata().map_err(MarkdownPdfError::InputIo)?;
    if !opened.is_file() || !same_identity(&before, &opened) {
        return Err(MarkdownPdfError::InvalidInput);
    }
    let mut bytes = Vec::with_capacity(before.len() as usize);
    (&mut file)
        .take(MAX_INPUT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(MarkdownPdfError::InputIo)?;
    if should_stop() {
        return Err(MarkdownPdfError::Cancelled);
    }
    let after = fs::symlink_metadata(input).map_err(|_| MarkdownPdfError::InvalidInput)?;
    let opened_after = file.metadata().map_err(MarkdownPdfError::InputIo)?;
    if bytes.is_empty()
        || bytes.len() as u64 != before.len()
        || bytes.len() as u64 > MAX_INPUT_BYTES
        || !same_identity(&before, &after)
        || !same_identity(&opened, &opened_after)
        || before.modified().ok() != after.modified().ok()
        || opened.modified().ok() != opened_after.modified().ok()
        || bytes.contains(&0)
    {
        return Err(MarkdownPdfError::InvalidInput);
    }
    String::from_utf8(bytes).map_err(|_| MarkdownPdfError::InvalidInput)
}

fn safe_link(destination: &str) -> bool {
    if destination.is_empty()
        || destination
            .chars()
            .any(|ch| ch.is_control() || ch.is_whitespace())
    {
        return false;
    }
    if destination.starts_with('#') && destination.len() > 1 {
        return true;
    }
    let Ok(url) = url::Url::parse(destination) else {
        return false;
    };
    matches!(url.scheme(), "http" | "https" | "mailto")
}

fn validate_image(
    destination: &str,
    total_image_bytes: &mut usize,
) -> Result<(), MarkdownPdfError> {
    let Some((header, encoded)) = destination.split_once(',') else {
        return Err(MarkdownPdfError::UnsupportedContent);
    };
    let mime = header
        .strip_prefix("data:image/")
        .and_then(|value| value.strip_suffix(";base64"))
        .map(str::to_ascii_lowercase)
        .ok_or(MarkdownPdfError::UnsupportedContent)?;
    if !matches!(mime.as_str(), "png" | "jpeg") || encoded.is_empty() {
        return Err(MarkdownPdfError::UnsupportedContent);
    }
    if encoded.len() > MAX_INLINE_IMAGE_BYTES.div_ceil(3) * 4 + 4 {
        return Err(MarkdownPdfError::InlineImageTooLarge);
    }
    let decoded = BASE64
        .decode(encoded)
        .map_err(|_| MarkdownPdfError::UnsupportedContent)?;
    let decoded_len = decoded.len();
    if decoded_len > MAX_INLINE_IMAGE_BYTES {
        return Err(MarkdownPdfError::InlineImageTooLarge);
    }
    *total_image_bytes = total_image_bytes
        .checked_add(decoded_len)
        .filter(|size| *size <= MAX_INLINE_IMAGE_TOTAL_BYTES)
        .ok_or(MarkdownPdfError::InlineImageTooLarge)?;
    let dimensions = match mime.as_str() {
        "png" => {
            if !decoded.starts_with(b"\x89PNG\r\n\x1a\n") {
                return Err(MarkdownPdfError::UnsupportedContent);
            }
            let mut decoder = png::Decoder::new(io::Cursor::new(&decoded));
            decoder.set_ignore_text_chunk(true);
            decoder.set_limits(png::Limits {
                bytes: 64 * 1024 * 1024,
            });
            let mut reader = decoder
                .read_info()
                .map_err(|_| MarkdownPdfError::UnsupportedContent)?;
            let dimensions = (reader.info().width, reader.info().height);
            check_dimensions(dimensions)?;
            let length = reader.output_buffer_size();
            if length > 64 * 1024 * 1024 {
                return Err(MarkdownPdfError::InlineImageTooLarge);
            }
            let mut pixels = vec![0; length];
            reader
                .next_frame(&mut pixels)
                .map_err(|_| MarkdownPdfError::UnsupportedContent)?;
            reader
                .finish()
                .map_err(|_| MarkdownPdfError::UnsupportedContent)?;
            dimensions
        }
        "jpeg" => {
            if !decoded.starts_with(b"\xff\xd8\xff") {
                return Err(MarkdownPdfError::UnsupportedContent);
            }
            let mut reader = jpeg_decoder::Decoder::new(io::Cursor::new(&decoded));
            reader
                .read_info()
                .map_err(|_| MarkdownPdfError::UnsupportedContent)?;
            let info = reader.info().ok_or(MarkdownPdfError::UnsupportedContent)?;
            let dimensions = (u32::from(info.width), u32::from(info.height));
            check_dimensions(dimensions)?;
            reader
                .decode()
                .map_err(|_| MarkdownPdfError::UnsupportedContent)?;
            dimensions
        }
        _ => return Err(MarkdownPdfError::UnsupportedContent),
    };
    check_dimensions(dimensions)
}

fn check_dimensions(dimensions: (u32, u32)) -> Result<(), MarkdownPdfError> {
    if dimensions.0 == 0
        || dimensions.1 == 0
        || u64::from(dimensions.0) * u64::from(dimensions.1) > MAX_INLINE_IMAGE_PIXELS
    {
        return Err(MarkdownPdfError::InlineImageTooLarge);
    }
    Ok(())
}

fn render_html(
    markdown: &str,
    should_stop: &impl Fn() -> bool,
) -> Result<String, MarkdownPdfError> {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_FOOTNOTES);
    let parser = Parser::new_ext(markdown, options);
    let mut events = Vec::new();
    let mut image_bytes = 0_usize;
    for (index, event) in parser.enumerate() {
        if index >= MAX_MARKDOWN_EVENTS {
            return Err(MarkdownPdfError::OutputTooLarge);
        }
        if index % 128 == 0 && should_stop() {
            return Err(MarkdownPdfError::Cancelled);
        }
        match &event {
            Event::Html(_) | Event::InlineHtml(_) => {
                return Err(MarkdownPdfError::UnsupportedContent)
            }
            Event::Start(Tag::HtmlBlock) => return Err(MarkdownPdfError::UnsupportedContent),
            Event::Start(Tag::Image { dest_url, .. }) => {
                validate_image(dest_url, &mut image_bytes)?
            }
            Event::Start(Tag::Link {
                link_type: LinkType::Email,
                dest_url,
                ..
            }) if !dest_url.contains('@')
                || dest_url
                    .chars()
                    .any(|ch| ch.is_whitespace() || ch.is_control()) =>
            {
                return Err(MarkdownPdfError::UnsupportedContent)
            }
            Event::Start(Tag::Link {
                dest_url,
                link_type,
                ..
            }) if !matches!(link_type, LinkType::Email) && !safe_link(dest_url) => {
                return Err(MarkdownPdfError::UnsupportedContent)
            }
            Event::InlineMath(_) | Event::DisplayMath(_) => {
                return Err(MarkdownPdfError::UnsupportedContent)
            }
            _ => {}
        }
        events.push(event);
    }
    if should_stop() {
        return Err(MarkdownPdfError::Cancelled);
    }
    let mut body = String::new();
    html::push_html(&mut body, events.into_iter());
    let html = format!("<!doctype html><html><head><meta charset=\"utf-8\"><style>{STYLE}</style></head><body>{body}</body></html>");
    if html.len() as u64 > MAX_OUTPUT_BYTES {
        return Err(MarkdownPdfError::OutputTooLarge);
    }
    Ok(html)
}

fn write_html(path: &Path, html: &str) -> Result<RemoveOnDrop, MarkdownPdfError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(MarkdownPdfError::OutputIo)?;
    let guard = RemoveOnDrop {
        path: path.to_path_buf(),
        keep: false,
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(MarkdownPdfError::OutputIo)?;
    }
    file.write_all(html.as_bytes())
        .map_err(MarkdownPdfError::OutputIo)?;
    file.flush().map_err(MarkdownPdfError::OutputIo)?;
    Ok(guard)
}

/// Converts a local `.md` or `.markdown` file to PDF with the controlled HTML printer.
///
/// The generated HTML is self-contained. Raw HTML, scripts, styles, local image paths,
/// remote image URLs, SVG/data images, and unsupported mathematical blocks are rejected.
/// Textual links are retained only for HTTP(S), mailto, and document fragments.
///
/// # Errors
///
/// Returns a classified error for unsafe paths, malformed/oversized Markdown, unsupported
/// content, cancellation, worker failure, or output I/O. Temporary HTML and failed PDFs are
/// removed before returning an error.
pub(crate) fn markdown_to_pdf(
    input: &Path,
    output: &Path,
    workspace: &Path,
    should_stop: impl Fn() -> bool,
) -> Result<(), MarkdownPdfError> {
    markdown_to_pdf_with_renderer(
        input,
        output,
        workspace,
        should_stop,
        |html, pdf, dir, stop| crate::html_pdf::html_to_pdf(html, pdf, dir, stop),
    )
}

fn markdown_to_pdf_with_renderer(
    input: &Path,
    output: &Path,
    workspace: &Path,
    should_stop: impl Fn() -> bool,
    render: impl FnOnce(
        &Path,
        &Path,
        &Path,
        &dyn Fn() -> bool,
    ) -> Result<(), crate::html_pdf::HtmlPdfError>,
) -> Result<(), MarkdownPdfError> {
    validate_output(output)?;
    let markdown = read_input(input, &should_stop)?;
    let html = render_html(&markdown, &should_stop)?;
    if should_stop() {
        return Err(MarkdownPdfError::Cancelled);
    }
    if !workspace.is_absolute()
        || output.parent() != Some(workspace)
        || !fs::symlink_metadata(workspace).is_ok_and(|meta| meta.file_type().is_dir())
    {
        return Err(MarkdownPdfError::InvalidOutput);
    }
    let html_path = workspace.join("markdown-input.html");
    let _html_guard = write_html(&html_path, &html)?;
    let mut output_guard = RemoveOnDrop {
        path: output.to_path_buf(),
        keep: false,
    };
    render(&html_path, output, workspace, &should_stop)?;
    if should_stop() {
        return Err(MarkdownPdfError::Cancelled);
    }
    output_guard.keep = true;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use uuid::Uuid;

    fn temp_dir() -> PathBuf {
        let path = std::env::temp_dir().join(format!("markdown-pdf-{}", Uuid::new_v4()));
        fs::create_dir(&path).expect("create test directory");
        path
    }

    #[test]
    fn parser_preserves_structure_and_rejects_active_content() {
        let markdown = "# Heading\n\n| A | B |\n| --- | --- |\n| 1 | 2 |\n\n```rust\nlet x = 1;\n```\n\n[docs](https://example.test/docs)";
        let html = render_html(markdown, &|| false).expect("render markdown");
        assert!(html.contains("<h1>Heading</h1>"));
        assert!(html.contains("<table>"));
        assert!(html.contains("<pre><code class=\"language-rust\">"));
        assert!(html.contains("href=\"https://example.test/docs\""));
        assert!(matches!(
            render_html("<script>alert(1)</script>", &|| false),
            Err(MarkdownPdfError::UnsupportedContent)
        ));
    }

    #[test]
    fn rejects_remote_and_local_images_but_allows_bounded_inline_png() {
        assert!(matches!(
            render_html("![remote](https://example.test/a.png)", &|| false),
            Err(MarkdownPdfError::UnsupportedContent)
        ));
        assert!(matches!(
            render_html("![local](./a.png)", &|| false),
            Err(MarkdownPdfError::UnsupportedContent)
        ));
        let mut pixels = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut pixels, 1, 1);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().expect("PNG header");
            writer
                .write_image_data(&[255, 0, 0, 255])
                .expect("PNG pixel");
        }
        let inline = format!("![inline](data:image/png;base64,{})", BASE64.encode(pixels));
        render_html(&inline, &|| false).expect("render inline PNG");
        let oversized = format!("data:image/png;base64,{}", "A".repeat(1_400_000));
        assert!(matches!(
            validate_image(&oversized, &mut 0),
            Err(MarkdownPdfError::InlineImageTooLarge)
        ));
        assert!(matches!(
            render_html("[bad](javascript:alert(1))", &|| false),
            Err(MarkdownPdfError::UnsupportedContent)
        ));
    }

    #[test]
    fn cancellation_and_input_limit_leave_no_output() {
        let directory = temp_dir();
        let input = directory.join("note.md");
        let output = directory.join("note.pdf");
        fs::write(&input, "# note").expect("write input");
        assert!(matches!(
            markdown_to_pdf(&input, &output, &directory, || true),
            Err(MarkdownPdfError::Cancelled)
        ));
        assert!(!output.exists());
        let oversized = directory.join("large.md");
        let file = vec![b'a'; MAX_INPUT_BYTES as usize + 1];
        fs::write(&oversized, file).expect("write large input");
        assert!(matches!(
            markdown_to_pdf(&oversized, &output, &directory, || false),
            Err(MarkdownPdfError::InputTooLarge)
        ));
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn markdown_extension_accepts_only_strict_utf8_regular_files() {
        let directory = temp_dir();
        let input = directory.join("note.MARKDOWN");
        fs::write(&input, "# UTF-8 测试").expect("write markdown input");
        assert_eq!(
            read_input(&input, &|| false).expect("read .markdown input"),
            "# UTF-8 测试"
        );
        let wrong_extension = directory.join("note.markdown.txt");
        fs::write(&wrong_extension, "# note").expect("write unsupported extension");
        assert!(matches!(
            read_input(&wrong_extension, &|| false),
            Err(MarkdownPdfError::InvalidInput)
        ));
        fs::write(&input, [b'#', b' ', 0xff]).expect("write malformed UTF-8");
        assert!(matches!(
            read_input(&input, &|| false),
            Err(MarkdownPdfError::InvalidInput)
        ));
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn worker_failure_cleans_generated_html_and_pdf() {
        let directory = temp_dir();
        let input = directory.join("note.md");
        let output = directory.join("note.pdf");
        fs::write(&input, "# note").expect("write input");
        let result = markdown_to_pdf_with_renderer(
            &input,
            &output,
            &directory,
            || false,
            |html, pdf, _, _| {
                assert!(html.exists());
                fs::write(pdf, b"not a PDF").expect("simulate failed renderer");
                Err(crate::html_pdf::HtmlPdfError::PrintFailed)
            },
        );
        assert!(matches!(result, Err(MarkdownPdfError::Html(_))));
        assert!(!directory.join("markdown-input.html").exists());
        assert!(!output.exists());
        let stop = AtomicBool::new(false);
        let cancelled = markdown_to_pdf_with_renderer(
            &input,
            &output,
            &directory,
            || stop.load(Ordering::Relaxed),
            |_, pdf, _, _| {
                fs::write(pdf, b"%PDF-1.4\n").expect("simulate late output");
                stop.store(true, Ordering::Relaxed);
                Ok(())
            },
        );
        assert!(matches!(cancelled, Err(MarkdownPdfError::Cancelled)));
        assert!(!directory.join("markdown-input.html").exists());
        assert!(!output.exists());
        markdown_to_pdf_with_renderer(
            &input,
            &output,
            &directory,
            || false,
            |_, pdf, _, _| {
                fs::write(pdf, b"%PDF-1.4\n%%EOF\n").expect("simulate successful retry");
                Ok(())
            },
        )
        .expect("retry after failure");
        assert!(output.exists());
        assert!(!directory.join("markdown-input.html").exists());
        fs::remove_dir_all(directory).expect("remove test directory");
    }
}
