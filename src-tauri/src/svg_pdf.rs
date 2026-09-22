//! Converts a bounded, offline SVG subset into one vector PDF page.

use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;
use quick_xml::XmlVersion;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::Path;
use svg2pdf::usvg;
use thiserror::Error;

pub(crate) const MAX_INPUT_BYTES: u64 = 2 * 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = 32 * 1024 * 1024;
const MAX_NODES: usize = 10_000;
const MAX_DEPTH: usize = 32;
const MAX_ATTRIBUTE_BYTES: usize = 128 * 1024;
const MAX_PAGE_POINTS: f32 = 14_400.0;

#[derive(Debug, Error)]
pub(crate) enum SvgPdfError {
    #[error("The input must be a regular SVG file at an absolute path and cannot be a symlink")]
    InvalidInput,
    #[error("PDF output must be an absolute path in an existing directory")]
    InvalidOutputPath,
    #[error("The SVG file is invalid or corrupted")]
    InvalidSvg,
    #[error("The SVG contains unsupported elements, attributes, or external resources")]
    UnsupportedSvg,
    #[error("The SVG file, complexity, or page size exceeds the limit")]
    TooLarge,
    #[error("The generated PDF exceeds the size limit")]
    OutputTooLarge,
    #[error("SVG to PDF conversion cancelled")]
    Cancelled,
    #[error("Could not read the SVG: {0}")]
    InputIo(#[source] io::Error),
    #[error("Could not write the PDF: {0}")]
    OutputIo(#[source] io::Error),
}

impl SvgPdfError {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::InvalidInput => "PATH_NOT_ALLOWED",
            Self::InvalidOutputPath => "OUTPUT_PATH_NOT_ALLOWED",
            Self::InvalidSvg => "CORRUPTED_SVG",
            Self::UnsupportedSvg => "UNSUPPORTED_FORMAT",
            Self::TooLarge => "INPUT_TOO_LARGE",
            Self::OutputTooLarge => "OUTPUT_SIZE_EXCEEDED",
            Self::Cancelled => "CANCELLED",
            Self::InputIo(_) => "INPUT_READ_FAILED",
            Self::OutputIo(_) => "OUTPUT_WRITE_FAILED",
        }
    }
}

/// Writes a single-page vector PDF from a local static SVG.
///
/// SVG references to disk/network resources, raster images, filters, scripts
/// and unsupported features fail explicitly. Text is outlined using the host's
/// installed fonts and is not selectable in the resulting PDF.
///
/// # Errors
/// Rejects invalid paths, unsupported or oversized SVGs, cancellation and I/O
/// errors. Output is never overwritten and failed output is removed.
pub(crate) fn svg_to_pdf(
    input: &Path,
    output: &Path,
    should_stop: impl Fn() -> bool,
) -> Result<(), SvgPdfError> {
    if !input.is_absolute()
        || input
            .extension()
            .and_then(|extension| extension.to_str())
            .is_none_or(|extension| !extension.eq_ignore_ascii_case("svg"))
    {
        return Err(SvgPdfError::InvalidInput);
    }
    if !output.is_absolute()
        || output
            .extension()
            .and_then(|extension| extension.to_str())
            .is_none_or(|extension| !extension.eq_ignore_ascii_case("pdf"))
        || !output.parent().is_some_and(Path::is_dir)
    {
        return Err(SvgPdfError::InvalidOutputPath);
    }
    if should_stop() {
        return Err(SvgPdfError::Cancelled);
    }
    let metadata = fs::symlink_metadata(input).map_err(SvgPdfError::InputIo)?;
    if !metadata.file_type().is_file() || metadata.len() == 0 {
        return Err(SvgPdfError::InvalidInput);
    }
    if metadata.len() > MAX_INPUT_BYTES {
        return Err(SvgPdfError::TooLarge);
    }
    let mut file = File::open(input).map_err(SvgPdfError::InputIo)?;
    let opened = file.metadata().map_err(SvgPdfError::InputIo)?;
    if !opened.is_file() || opened.len() != metadata.len() {
        return Err(SvgPdfError::InvalidInput);
    }
    let mut data = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or_default());
    let mut chunk = [0u8; 16 * 1024];
    loop {
        if should_stop() {
            return Err(SvgPdfError::Cancelled);
        }
        let count = file.read(&mut chunk).map_err(SvgPdfError::InputIo)?;
        if count == 0 {
            break;
        }
        if data.len().saturating_add(count) > MAX_INPUT_BYTES as usize {
            return Err(SvgPdfError::TooLarge);
        }
        data.extend_from_slice(&chunk[..count]);
    }
    if data.len() != usize::try_from(metadata.len()).map_err(|_| SvgPdfError::TooLarge)? {
        return Err(SvgPdfError::InvalidInput);
    }
    let xml = std::str::from_utf8(&data)
        .map_err(|_| SvgPdfError::InvalidSvg)?
        .trim_start_matches('\u{feff}');
    let contains_text = preflight_svg(xml, &should_stop)?;
    if should_stop() {
        return Err(SvgPdfError::Cancelled);
    }
    let mut options = usvg::Options {
        resources_dir: None,
        default_size: usvg::Size::from_wh(794.0, 1123.0).ok_or(SvgPdfError::InvalidSvg)?,
        image_href_resolver: usvg::ImageHrefResolver {
            resolve_data: Box::new(|_, _, _| None),
            resolve_string: Box::new(|_, _| None),
        },
        ..Default::default()
    };
    if contains_text {
        options.fontdb_mut().load_system_fonts();
        if options.fontdb.faces().next().is_none() {
            return Err(SvgPdfError::UnsupportedSvg);
        }
    }
    if should_stop() {
        return Err(SvgPdfError::Cancelled);
    }
    let tree = usvg::Tree::from_str(xml, &options).map_err(|_| SvgPdfError::InvalidSvg)?;
    let size = tree.size();
    if !size.width().is_finite()
        || !size.height().is_finite()
        || size.width() <= 0.0
        || size.height() <= 0.0
        || size.width() * 72.0 / 96.0 > MAX_PAGE_POINTS
        || size.height() * 72.0 / 96.0 > MAX_PAGE_POINTS
    {
        return Err(SvgPdfError::TooLarge);
    }
    if should_stop() {
        return Err(SvgPdfError::Cancelled);
    }
    let pdf = svg2pdf::to_pdf(
        &tree,
        svg2pdf::ConversionOptions {
            embed_text: false,
            ..Default::default()
        },
        svg2pdf::PageOptions { dpi: 96.0 },
    )
    .map_err(|_| SvgPdfError::UnsupportedSvg)?;
    if pdf.len() > MAX_OUTPUT_BYTES {
        return Err(SvgPdfError::OutputTooLarge);
    }
    if !pdf.starts_with(b"%PDF-")
        || pdf
            .windows(b"/Subtype /Image".len())
            .any(|bytes| bytes == b"/Subtype /Image")
    {
        return Err(SvgPdfError::UnsupportedSvg);
    }
    if should_stop() {
        return Err(SvgPdfError::Cancelled);
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)
        .map_err(SvgPdfError::OutputIo)?;
    let mut guard = OutputGuard {
        path: output,
        keep: false,
    };
    file.write_all(&pdf).map_err(SvgPdfError::OutputIo)?;
    file.sync_all().map_err(SvgPdfError::OutputIo)?;
    if should_stop() {
        return Err(SvgPdfError::Cancelled);
    }
    guard.keep = true;
    Ok(())
}

struct OutputGuard<'a> {
    path: &'a Path,
    keep: bool,
}

impl Drop for OutputGuard<'_> {
    fn drop(&mut self) {
        if !self.keep {
            let _ = fs::remove_file(self.path);
        }
    }
}

fn preflight_svg(xml: &str, should_stop: &impl Fn() -> bool) -> Result<bool, SvgPdfError> {
    let mut reader = Reader::from_str(xml);
    let mut depth = 0usize;
    let mut nodes = 0usize;
    let mut saw_root = false;
    let mut has_text = false;
    loop {
        if should_stop() {
            return Err(SvgPdfError::Cancelled);
        }
        match reader.read_event().map_err(|_| SvgPdfError::InvalidSvg)? {
            Event::Start(tag) => {
                has_text |= validate_tag(&reader, &tag, depth, &mut nodes, &mut saw_root)?;
                depth += 1;
                if depth > MAX_DEPTH {
                    return Err(SvgPdfError::TooLarge);
                }
            }
            Event::Empty(tag) => {
                has_text |= validate_tag(&reader, &tag, depth, &mut nodes, &mut saw_root)?;
            }
            Event::End(_) => {
                if depth == 0 {
                    return Err(SvgPdfError::InvalidSvg);
                }
                depth -= 1;
            }
            Event::Text(text) if depth == 0 && !text.xml10_content().trim().is_empty() => {
                return Err(SvgPdfError::InvalidSvg);
            }
            Event::CData(_) | Event::DocType(_) | Event::PI(_) => {
                return Err(SvgPdfError::UnsupportedSvg);
            }
            Event::GeneralRef(reference) => {
                let allowed = reference
                    .resolve_char_ref()
                    .map_err(|_| SvgPdfError::InvalidSvg)?
                    .is_some()
                    || quick_xml::escape::resolve_predefined_entity(reference.as_ref()).is_some();
                if !allowed {
                    return Err(SvgPdfError::UnsupportedSvg);
                }
            }
            Event::Decl(declaration) => {
                if let Some(encoding) = declaration.encoding() {
                    let encoding = encoding.map_err(|_| SvgPdfError::InvalidSvg)?;
                    if !encoding.as_ref().eq_ignore_ascii_case("utf-8") {
                        return Err(SvgPdfError::UnsupportedSvg);
                    }
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if !saw_root || depth != 0 {
        return Err(SvgPdfError::InvalidSvg);
    }
    Ok(has_text)
}

fn validate_tag(
    reader: &Reader<&[u8]>,
    tag: &BytesStart<'_>,
    depth: usize,
    nodes: &mut usize,
    saw_root: &mut bool,
) -> Result<bool, SvgPdfError> {
    let tag_name = tag.name();
    let name: &str = tag_name.as_ref();
    if depth == 0 {
        if *saw_root || name != "svg" {
            return Err(SvgPdfError::InvalidSvg);
        }
        *saw_root = true;
    }
    if !allowed_element(name) {
        return Err(SvgPdfError::UnsupportedSvg);
    }
    *nodes += 1;
    if *nodes > MAX_NODES {
        return Err(SvgPdfError::TooLarge);
    }
    validate_attributes(reader, tag, depth == 0)?;
    Ok(matches!(name, "text" | "tspan"))
}

fn allowed_element(name: &str) -> bool {
    matches!(
        name,
        "svg"
            | "g"
            | "defs"
            | "title"
            | "desc"
            | "rect"
            | "circle"
            | "ellipse"
            | "line"
            | "polyline"
            | "polygon"
            | "path"
            | "linearGradient"
            | "radialGradient"
            | "stop"
            | "clipPath"
            | "text"
            | "tspan"
    )
}

fn allowed_attribute(name: &str) -> bool {
    matches!(
        name,
        "id" | "version"
            | "width"
            | "height"
            | "viewBox"
            | "preserveAspectRatio"
            | "x"
            | "y"
            | "x1"
            | "y1"
            | "x2"
            | "y2"
            | "cx"
            | "cy"
            | "r"
            | "rx"
            | "ry"
            | "d"
            | "points"
            | "transform"
            | "fill"
            | "fill-opacity"
            | "fill-rule"
            | "stroke"
            | "stroke-width"
            | "stroke-linecap"
            | "stroke-linejoin"
            | "stroke-miterlimit"
            | "stroke-dasharray"
            | "stroke-dashoffset"
            | "stroke-opacity"
            | "opacity"
            | "color"
            | "clip-path"
            | "clip-rule"
            | "display"
            | "visibility"
            | "font-family"
            | "font-size"
            | "font-weight"
            | "font-style"
            | "text-anchor"
            | "dominant-baseline"
            | "letter-spacing"
            | "dx"
            | "dy"
            | "offset"
            | "stop-color"
            | "stop-opacity"
            | "gradientUnits"
            | "gradientTransform"
            | "spreadMethod"
            | "xml:space"
    )
}

fn validate_attributes(
    _reader: &Reader<&[u8]>,
    tag: &BytesStart<'_>,
    root: bool,
) -> Result<(), SvgPdfError> {
    for item in tag.attributes() {
        let attribute = item.map_err(|_| SvgPdfError::InvalidSvg)?;
        let name = attribute.key.as_ref();
        if attribute.value.len() > MAX_ATTRIBUTE_BYTES {
            return Err(SvgPdfError::TooLarge);
        }
        let value = attribute
            .normalized_value(XmlVersion::Implicit1_0)
            .map_err(|_| SvgPdfError::InvalidSvg)?;
        if name == "xmlns" && root && value == "http://www.w3.org/2000/svg" {
            continue;
        }
        if name == "xmlns:xlink" && root && value == "http://www.w3.org/1999/xlink" {
            continue;
        }
        if !allowed_attribute(name) || value.len() > MAX_ATTRIBUTE_BYTES {
            return Err(SvgPdfError::UnsupportedSvg);
        }
        let lowercase = value.to_ascii_lowercase();
        if lowercase.contains("://")
            || lowercase.contains("file:")
            || lowercase.contains("data:")
            || lowercase.contains("javascript:")
        {
            return Err(SvgPdfError::UnsupportedSvg);
        }
        if lowercase.contains("url(")
            && (!matches!(name, "fill" | "stroke" | "clip-path") || !matches_local_resource(&value))
        {
            return Err(SvgPdfError::UnsupportedSvg);
        }
    }
    Ok(())
}

fn matches_local_resource(value: &str) -> bool {
    let Some(id) = value
        .strip_prefix("url(#")
        .and_then(|value| value.strip_suffix(')'))
    else {
        return false;
    };
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("minimalpdf-svg-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&path).expect("fixture directory");
            Self(path)
        }

        fn input(&self, xml: &str) -> PathBuf {
            let input = self.0.join("shape.svg");
            fs::write(&input, xml).expect("fixture SVG");
            input
        }

        fn output(&self) -> PathBuf {
            self.0.join("shape.pdf")
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn shapes_and_gradient_stay_vector_with_css_pixel_page_size() {
        let fixture = Fixture::new();
        let input = fixture.input(r##"<svg xmlns="http://www.w3.org/2000/svg" width="96" height="48"><defs><linearGradient id="paint"><stop offset="0" stop-color="#f00"/><stop offset="1" stop-color="#0f0"/></linearGradient></defs><rect x="0" y="0" width="96" height="48" fill="url(#paint)"/><path d="M2 2 L30 20" stroke="#00f"/></svg>"##);
        let output = fixture.output();
        svg_to_pdf(&input, &output, || false).expect("vector SVG conversion");
        let pdf = fs::read(output).expect("read vector PDF");
        assert!(pdf.starts_with(b"%PDF-"));
        assert!(!pdf
            .windows(b"/Subtype /Image".len())
            .any(|bytes| bytes == b"/Subtype /Image"));
        assert!(pdf
            .windows(b"/MediaBox [0 0 72 36]".len())
            .any(|bytes| bytes == b"/MediaBox [0 0 72 36]"));
    }

    #[test]
    fn local_system_font_text_is_outlined_without_embedding_raster_assets() {
        let fixture = Fixture::new();
        let input = fixture.input(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"140\" height=\"48\"><text x=\"8\" y=\"30\" font-family=\"sans-serif\" font-size=\"22\">Vector</text></svg>",
        );
        let output = fixture.output();
        svg_to_pdf(&input, &output, || false).expect("outline text using installed fonts");
        let pdf = fs::read(output).expect("read outlined PDF");
        assert!(pdf.starts_with(b"%PDF-"));
        assert!(!pdf
            .windows(b"/Subtype /Image".len())
            .any(|bytes| bytes == b"/Subtype /Image"));
        assert!(!pdf
            .windows(b"/FontFile".len())
            .any(|bytes| bytes == b"/FontFile"));
    }

    #[test]
    fn active_content_external_assets_and_raster_fallback_are_rejected() {
        let fixture = Fixture::new();
        for node in [
            "<script>alert(1)</script>",
            "<foreignObject><div>unsafe</div></foreignObject>",
            "<image href=\"file:///private/data.png\"/>",
            "<image href=\"https://example.invalid/image.png\"/>",
            "<filter id=\"blur\"/>",
            "<style>@import url(https://example.invalid/x.css)</style>",
            "<use href=\"#shape\"/>",
        ] {
            let input = fixture.input(&format!("<svg>{node}</svg>"));
            assert!(
                matches!(
                    svg_to_pdf(&input, &fixture.output(), || false),
                    Err(SvgPdfError::UnsupportedSvg)
                ),
                "accepted {node}"
            );
            assert!(!fixture.output().exists());
        }
        for attr in [
            "fill=\"url(file:///etc/passwd)\"",
            "fill=\"url(https://example.invalid/x)\"",
            "style=\"fill:url(#paint)\"",
            "onload=\"alert(1)\"",
            "xml:base=\"file:///etc/\"",
            "href=\"../../outside.svg\"",
        ] {
            let input = fixture.input(&format!(
                "<svg><rect {attr} width=\"10\" height=\"10\"/></svg>"
            ));
            assert!(
                matches!(
                    svg_to_pdf(&input, &fixture.output(), || false),
                    Err(SvgPdfError::UnsupportedSvg)
                ),
                "accepted {attr}"
            );
            assert!(!fixture.output().exists());
        }
        let input =
            fixture.input("<!DOCTYPE svg [<!ENTITY leak SYSTEM 'file:///etc/passwd'>]><svg/>");
        assert!(matches!(
            svg_to_pdf(&input, &fixture.output(), || false),
            Err(SvgPdfError::UnsupportedSvg)
        ));
    }

    #[test]
    fn limits_cancellation_and_retry_remove_no_existing_output() {
        let fixture = Fixture::new();
        let input = fixture
            .input("<svg width=\"20\" height=\"10\"><circle cx=\"10\" cy=\"5\" r=\"5\"/></svg>");
        let output = fixture.output();
        assert!(matches!(
            svg_to_pdf(&input, &output, || true),
            Err(SvgPdfError::Cancelled)
        ));
        assert!(!output.exists());
        fs::write(&output, b"existing").expect("existing output");
        assert!(matches!(
            svg_to_pdf(&input, &output, || false),
            Err(SvgPdfError::OutputIo(_))
        ));
        assert_eq!(
            fs::read(&output).expect("existing output preserved"),
            b"existing"
        );
        fs::remove_file(&output).expect("remove known test output");
        svg_to_pdf(&input, &output, || false).expect("retry conversion");
        assert!(fs::read(&output).expect("retry PDF").starts_with(b"%PDF-"));
        fs::remove_file(&output).expect("remove known test output");
        let input = fixture.input(
            "<svg width=\"20000\" height=\"20\"><rect width=\"20000\" height=\"20\"/></svg>",
        );
        assert!(matches!(
            svg_to_pdf(&input, &output, || false),
            Err(SvgPdfError::TooLarge)
        ));
        assert!(!output.exists());
        File::create(&input)
            .expect("resize fixture")
            .set_len(MAX_INPUT_BYTES + 1)
            .expect("oversize fixture");
        assert!(matches!(
            svg_to_pdf(&input, &output, || false),
            Err(SvgPdfError::TooLarge)
        ));
        assert!(!output.exists());
    }
}
