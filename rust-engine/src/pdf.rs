use flate2::read::ZlibDecoder;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;
use thiserror::Error;

const EPSILON: f64 = 0.001;
const MAX_SELECTED_PAGES: usize = 10_000;
const MAX_STREAM_BYTES: usize = 64 * 1024 * 1024;
const MAX_XREF_ENTRIES: usize = 100_000;
const MAX_XREF_REVISIONS: usize = 32;
const MAX_OBJECT_STREAM_OBJECTS: usize = 10_000;
const MAX_VALUE_DEPTH: usize = 64;
const MAX_FORM_INVOCATIONS: usize = 10_000;
const MAX_CMAP_ENTRIES: usize = 65_536;
const MAX_CMAP_RANGE_ENTRIES: usize = 4_096;
const MAX_CMAP_MAPPING_BYTES: usize = 4 * 1024 * 1024;
const MAX_CMAP_TOKEN_BYTES: usize = 4 * 1024;
const MAX_CMAP_TOKENS: usize = MAX_CMAP_ENTRIES * 3;
const DECODE_BUFFER_BYTES: usize = 64 * 1024;
const FILE_READ_BUFFER_BYTES: usize = 64 * 1024;
pub(crate) const MAX_PAGES: usize = MAX_SELECTED_PAGES;

#[derive(Debug, Error)]
pub enum PdfError {
    #[error("PDF structure is invalid")]
    InvalidPdf,
    #[error("The PDF is corrupted")]
    CorruptedPdf,
    #[error("The PDF uses an unsupported cross-reference structure")]
    UnsupportedStructure,
    #[error("The PDF uses an unsupported indirect stream length")]
    UnsupportedStreamLength,
    #[error("The PDF content stream exceeds the 64 MiB safety limit")]
    StreamLimitExceeded,
    #[error("The PDF font map exceeds the safety limit")]
    CMapLimitExceeded,
    #[error("The PDF has no extractable text layer")]
    NoTextLayer,
    #[error("The page range is invalid")]
    InvalidPageRange,
    #[error("The PDF page count exceeds the safety limit")]
    PageLimitExceeded,
    #[error("The PDF uses an unsupported content stream filter: {0}")]
    UnsupportedFilter(String),
    #[error("PDF content stream decompression failed")]
    Decompression,
    #[error("PDF conversion cancelled")]
    Cancelled,
    #[error("Could not read the PDF: {0}")]
    Io(#[from] io::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct PdfRef {
    object: u32,
    generation: u16,
}

#[derive(Debug, Clone)]
struct PdfObject {
    value: PdfValue,
    stream: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum XrefLocation {
    Missing,
    Table(usize),
    Stream(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum XrefEntry {
    Free,
    Direct { offset: usize, generation: u16 },
    Compressed { stream_number: u32, index: usize },
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
enum PdfValue {
    Null,
    Bool(bool),
    Number(f64),
    Name(String),
    String(Vec<u8>),
    HexString(Vec<u8>),
    Array(Vec<PdfValue>),
    Dict(BTreeMap<String, PdfValue>),
    Ref(PdfRef),
    Keyword(String),
}

type XrefTableRevision = (BTreeMap<u32, XrefEntry>, BTreeMap<String, PdfValue>);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PageSelection {
    All,
    Pages(BTreeSet<usize>),
}

impl PageSelection {
    pub fn parse(value: Option<&str>) -> Result<Self, PdfError> {
        let Some(value) = value else {
            return Ok(Self::All);
        };
        if value.trim().is_empty()
            || value.trim().eq_ignore_ascii_case("all")
            || value.trim() == "全部"
        {
            return Ok(Self::All);
        }

        let mut pages = BTreeSet::new();
        for part in value.split(',') {
            let part = part.trim();
            if part.is_empty() {
                return Err(PdfError::InvalidPageRange);
            }
            let mut bounds = part.split('-');
            let start = bounds
                .next()
                .and_then(|item| item.trim().parse::<usize>().ok())
                .filter(|page| *page > 0)
                .ok_or(PdfError::InvalidPageRange)?;
            let end = match bounds.next() {
                Some(item) => item
                    .trim()
                    .parse::<usize>()
                    .ok()
                    .filter(|page| *page >= start)
                    .ok_or(PdfError::InvalidPageRange)?,
                None => start,
            };
            if bounds.next().is_some() {
                return Err(PdfError::InvalidPageRange);
            }
            let range_size = end
                .checked_sub(start)
                .and_then(|size| size.checked_add(1))
                .ok_or(PdfError::InvalidPageRange)?;
            if range_size > MAX_SELECTED_PAGES {
                return Err(PdfError::InvalidPageRange);
            }
            pages.extend(start..=end);
            if pages.len() > MAX_SELECTED_PAGES {
                return Err(PdfError::InvalidPageRange);
            }
        }
        Ok(Self::Pages(pages))
    }

    fn includes(&self, page_number: usize) -> bool {
        match self {
            Self::All => true,
            Self::Pages(pages) => pages.contains(&page_number),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractionResult {
    pub text: String,
    pub page_count: usize,
    pub selected_page_count: usize,
    pub text_object_count: usize,
    pub pages: Vec<TextPage>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextPage {
    pub text: String,
    pub width_points: u32,
    pub height_points: u32,
    pub text_object_count: usize,
}

#[derive(Debug)]
#[allow(dead_code)] // This module is also compiled into the TXT worker without EPUB support.
pub(crate) enum BoundedPageExtractionError {
    Pdf(PdfError),
    InputLimitExceeded,
    TextLimitExceeded,
}

impl From<PdfError> for BoundedPageExtractionError {
    fn from(error: PdfError) -> Self {
        Self::Pdf(error)
    }
}

#[derive(Debug)]
#[allow(dead_code)] // `text_bytes` is consumed only by the desktop EPUB path.
struct ExtractedPages {
    page_count: usize,
    selected_page_count: usize,
    text_object_count: usize,
    text_bytes: usize,
    pages: Vec<TextPage>,
}

enum ControlledReadError {
    Pdf(PdfError),
    LimitExceeded,
}

fn read_path_with_control(
    input: &Path,
    max_bytes: Option<u64>,
    should_stop: &impl Fn() -> bool,
) -> Result<Vec<u8>, ControlledReadError> {
    if should_stop() {
        return Err(ControlledReadError::Pdf(PdfError::Cancelled));
    }
    let mut file = File::open(input)
        .map_err(PdfError::Io)
        .map_err(ControlledReadError::Pdf)?;
    let metadata = file
        .metadata()
        .map_err(PdfError::Io)
        .map_err(ControlledReadError::Pdf)?;
    if max_bytes.is_some_and(|limit| metadata.len() > limit) {
        return Err(ControlledReadError::LimitExceeded);
    }

    let initial_capacity = usize::try_from(metadata.len())
        .unwrap_or(FILE_READ_BUFFER_BYTES)
        .min(FILE_READ_BUFFER_BYTES);
    let mut bytes = Vec::with_capacity(initial_capacity);
    let mut buffer = [0_u8; FILE_READ_BUFFER_BYTES];
    loop {
        if should_stop() {
            return Err(ControlledReadError::Pdf(PdfError::Cancelled));
        }
        let read = file
            .read(&mut buffer)
            .map_err(PdfError::Io)
            .map_err(ControlledReadError::Pdf)?;
        if read == 0 {
            return Ok(bytes);
        }
        let next_length = bytes
            .len()
            .checked_add(read)
            .ok_or(ControlledReadError::LimitExceeded)?;
        if max_bytes.is_some_and(|limit| next_length as u64 > limit) {
            return Err(ControlledReadError::LimitExceeded);
        }
        bytes.extend_from_slice(&buffer[..read]);
    }
}

#[allow(dead_code)]
pub fn extract_text_from_path(
    input: impl AsRef<Path>,
    selection: &PageSelection,
) -> Result<ExtractionResult, PdfError> {
    extract_text_from_path_with_control(input, selection, || false)
}

pub fn extract_text_from_path_with_control(
    input: impl AsRef<Path>,
    selection: &PageSelection,
    should_stop: impl Fn() -> bool,
) -> Result<ExtractionResult, PdfError> {
    let bytes =
        read_path_with_control(input.as_ref(), None, &should_stop).map_err(
            |error| match error {
                ControlledReadError::Pdf(error) => error,
                ControlledReadError::LimitExceeded => PdfError::StreamLimitExceeded,
            },
        )?;
    extract_text_with_control(&bytes, selection, should_stop)
}

#[allow(dead_code)]
pub fn extract_text(bytes: &[u8], selection: &PageSelection) -> Result<ExtractionResult, PdfError> {
    extract_text_with_control(bytes, selection, || false)
}

pub fn extract_text_with_control(
    bytes: &[u8],
    selection: &PageSelection,
    should_stop: impl Fn() -> bool,
) -> Result<ExtractionResult, PdfError> {
    let extracted = extract_selected_pages(bytes, selection, None, &should_stop).map_err(
        |error| match error {
            BoundedPageExtractionError::Pdf(error) => error,
            BoundedPageExtractionError::InputLimitExceeded
            | BoundedPageExtractionError::TextLimitExceeded => PdfError::StreamLimitExceeded,
        },
    )?;
    if extracted.text_object_count == 0 {
        return Err(PdfError::NoTextLayer);
    }
    let text = extracted
        .pages
        .iter()
        .map(|page| page.text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    Ok(ExtractionResult {
        text,
        page_count: extracted.page_count,
        selected_page_count: extracted.selected_page_count,
        text_object_count: extracted.text_object_count,
        pages: extracted.pages,
    })
}

fn extract_selected_pages(
    bytes: &[u8],
    selection: &PageSelection,
    max_text_bytes: Option<usize>,
    should_stop: &impl Fn() -> bool,
) -> Result<ExtractedPages, BoundedPageExtractionError> {
    if should_stop() {
        return Err(PdfError::Cancelled.into());
    }
    if !bytes.starts_with(b"%PDF-") {
        return Err(PdfError::InvalidPdf.into());
    }
    let (document, root) = parse_document(bytes, should_stop)?;
    if should_stop() {
        return Err(PdfError::Cancelled.into());
    }
    let pages = ordered_pages(&document, root)?;
    if pages.is_empty() {
        return Err(PdfError::CorruptedPdf.into());
    }
    if pages.len() > MAX_PAGES {
        return Err(PdfError::PageLimitExceeded.into());
    }
    if let PageSelection::Pages(selected) = selection {
        if selected.iter().any(|page| *page > pages.len()) {
            return Err(PdfError::InvalidPageRange.into());
        }
    }

    let mut selected_page_count = 0_usize;
    let mut text_object_count = 0_usize;
    let mut text_bytes = 0_usize;
    let mut page_texts = Vec::new();
    for (index, page_ref) in pages.iter().enumerate() {
        if should_stop() {
            return Err(PdfError::Cancelled.into());
        }
        let page_number = index + 1;
        if !selection.includes(page_number) {
            continue;
        }
        selected_page_count += 1;
        let extracted = extract_page_content(&document, *page_ref, &should_stop)?;
        text_bytes = text_bytes
            .checked_add(extracted.text.len())
            .filter(|total| max_text_bytes.is_none_or(|limit| *total <= limit))
            .ok_or(BoundedPageExtractionError::TextLimitExceeded)?;
        text_object_count = text_object_count
            .checked_add(extracted.text_object_count)
            .ok_or(PdfError::CorruptedPdf)?;
        let (width_points, height_points) = page_size(&document, *page_ref);
        page_texts.push(TextPage {
            text: extracted.text,
            width_points,
            height_points,
            text_object_count: extracted.text_object_count,
        });
    }
    Ok(ExtractedPages {
        page_count: pages.len(),
        selected_page_count,
        text_object_count,
        text_bytes,
        pages: page_texts,
    })
}

#[allow(dead_code)]
pub fn extract_pages_from_path_with_control(
    input: impl AsRef<Path>,
    selection: &PageSelection,
    should_stop: impl Fn() -> bool,
) -> Result<Vec<TextPage>, PdfError> {
    let bytes =
        read_path_with_control(input.as_ref(), None, &should_stop).map_err(
            |error| match error {
                ControlledReadError::Pdf(error) => error,
                ControlledReadError::LimitExceeded => PdfError::StreamLimitExceeded,
            },
        )?;
    extract_selected_pages(&bytes, selection, None, &should_stop)
        .map(|extracted| extracted.pages)
        .map_err(|error| match error {
            BoundedPageExtractionError::Pdf(error) => error,
            BoundedPageExtractionError::InputLimitExceeded
            | BoundedPageExtractionError::TextLimitExceeded => PdfError::StreamLimitExceeded,
        })
}

#[allow(dead_code)] // The standalone TXT worker does not expose EPUB conversion.
pub(crate) fn extract_pages_from_path_bounded_with_control(
    input: impl AsRef<Path>,
    selection: &PageSelection,
    max_input_bytes: u64,
    max_text_bytes: usize,
    should_stop: impl Fn() -> bool,
) -> Result<Vec<TextPage>, BoundedPageExtractionError> {
    let bytes = read_path_with_control(input.as_ref(), Some(max_input_bytes), &should_stop)
        .map_err(|error| match error {
            ControlledReadError::Pdf(error) => BoundedPageExtractionError::Pdf(error),
            ControlledReadError::LimitExceeded => BoundedPageExtractionError::InputLimitExceeded,
        })?;
    let extracted = extract_selected_pages(&bytes, selection, Some(max_text_bytes), &should_stop)?;
    if extracted.text_object_count == 0 || extracted.text_bytes == 0 {
        return Err(BoundedPageExtractionError::Pdf(PdfError::NoTextLayer));
    }
    Ok(extracted.pages)
}

fn append_page_text(page: &mut String, text: &str) {
    if text.is_empty() {
        return;
    }
    if !page.is_empty() && !page.ends_with('\n') {
        page.push('\n');
    }
    page.push_str(text);
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Number(f64),
    Name(String),
    LiteralString(Vec<u8>),
    HexString(Vec<u8>),
    Keyword(String),
    StartDictionary,
    EndDictionary,
    StartArray,
    EndArray,
}

#[derive(Clone)]
struct Tokenizer<'a> {
    bytes: &'a [u8],
    position: usize,
    lookahead: Option<Token>,
}

impl<'a> Tokenizer<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            position: 0,
            lookahead: None,
        }
    }

    fn next(&mut self) -> Option<Token> {
        if let Some(token) = self.lookahead.take() {
            return Some(token);
        }
        self.skip_space_and_comments();
        let byte = *self.bytes.get(self.position)?;
        match byte {
            b'<' if self.bytes.get(self.position + 1) == Some(&b'<') => {
                self.position += 2;
                Some(Token::StartDictionary)
            }
            b'>' if self.bytes.get(self.position + 1) == Some(&b'>') => {
                self.position += 2;
                Some(Token::EndDictionary)
            }
            b'[' => {
                self.position += 1;
                Some(Token::StartArray)
            }
            b']' => {
                self.position += 1;
                Some(Token::EndArray)
            }
            b'(' => self.read_literal_string().ok().map(Token::LiteralString),
            b'<' => self.read_hex_string().ok().map(Token::HexString),
            b'/' => self.read_name().ok().map(Token::Name),
            _ if is_number_start(byte) => self.read_number_or_keyword(),
            _ => self.read_keyword().ok().map(Token::Keyword),
        }
    }

    fn peek(&mut self) -> Option<Token> {
        if self.lookahead.is_none() {
            self.lookahead = self.next();
        }
        self.lookahead.clone()
    }

    fn set_position(&mut self, position: usize) {
        self.position = position.min(self.bytes.len());
        self.lookahead = None;
    }

    fn raw_position(&self) -> usize {
        self.position
    }

    fn skip_space_and_comments(&mut self) {
        loop {
            while self
                .bytes
                .get(self.position)
                .is_some_and(|byte| byte.is_ascii_whitespace())
            {
                self.position += 1;
            }
            if self.bytes.get(self.position) != Some(&b'%') {
                return;
            }
            while self
                .bytes
                .get(self.position)
                .is_some_and(|byte| *byte != b'\n' && *byte != b'\r')
            {
                self.position += 1;
            }
        }
    }

    fn read_literal_string(&mut self) -> Result<Vec<u8>, PdfError> {
        self.position += 1;
        let mut output = Vec::new();
        let mut depth = 1usize;
        while let Some(&byte) = self.bytes.get(self.position) {
            self.position += 1;
            match byte {
                b'(' => {
                    depth += 1;
                    output.push(byte);
                }
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Ok(output);
                    }
                    output.push(byte);
                }
                b'\\' => {
                    let Some(&escaped) = self.bytes.get(self.position) else {
                        return Err(PdfError::CorruptedPdf);
                    };
                    self.position += 1;
                    match escaped {
                        b'n' => output.push(b'\n'),
                        b'r' => output.push(b'\r'),
                        b't' => output.push(b'\t'),
                        b'b' => output.push(0x08),
                        b'f' => output.push(0x0c),
                        b'\n' => {}
                        b'\r' => {
                            if self.bytes.get(self.position) == Some(&b'\n') {
                                self.position += 1;
                            }
                        }
                        b'0'..=b'7' => {
                            let mut value = escaped - b'0';
                            for _ in 0..2 {
                                let Some(next) = self.bytes.get(self.position).copied() else {
                                    break;
                                };
                                if !(b'0'..=b'7').contains(&next) {
                                    break;
                                }
                                self.position += 1;
                                value = value.saturating_mul(8).saturating_add(next - b'0');
                            }
                            output.push(value);
                        }
                        other => output.push(other),
                    }
                }
                other => output.push(other),
            }
        }
        Err(PdfError::CorruptedPdf)
    }

    fn read_hex_string(&mut self) -> Result<Vec<u8>, PdfError> {
        self.position += 1;
        let mut nibbles = Vec::new();
        while let Some(&byte) = self.bytes.get(self.position) {
            self.position += 1;
            if byte == b'>' {
                break;
            }
            if byte.is_ascii_whitespace() {
                continue;
            }
            let nibble = hex_nibble(byte).ok_or(PdfError::CorruptedPdf)?;
            nibbles.push(nibble);
        }
        if self.bytes.get(self.position.saturating_sub(1)) != Some(&b'>') {
            return Err(PdfError::CorruptedPdf);
        }
        if nibbles.len() % 2 == 1 {
            nibbles.push(0);
        }
        Ok(nibbles
            .chunks_exact(2)
            .map(|pair| pair[0] << 4 | pair[1])
            .collect())
    }

    fn read_name(&mut self) -> Result<String, PdfError> {
        self.position += 1;
        let start = self.position;
        while self
            .bytes
            .get(self.position)
            .is_some_and(|byte| !byte.is_ascii_whitespace() && !is_delimiter(*byte))
        {
            self.position += 1;
        }
        Ok(decode_name(&self.bytes[start..self.position]))
    }

    fn read_number_or_keyword(&mut self) -> Option<Token> {
        let start = self.position;
        while self
            .bytes
            .get(self.position)
            .is_some_and(|byte| !byte.is_ascii_whitespace() && !is_delimiter(*byte))
        {
            self.position += 1;
        }
        let value = std::str::from_utf8(&self.bytes[start..self.position]).ok()?;
        match value.parse::<f64>() {
            Ok(number) => Some(Token::Number(number)),
            Err(_) => Some(Token::Keyword(value.to_owned())),
        }
    }

    fn read_keyword(&mut self) -> Result<String, PdfError> {
        let start = self.position;
        while self
            .bytes
            .get(self.position)
            .is_some_and(|byte| !byte.is_ascii_whitespace() && !is_delimiter(*byte))
        {
            self.position += 1;
        }
        if start == self.position {
            return Err(PdfError::InvalidPdf);
        }
        Ok(String::from_utf8_lossy(&self.bytes[start..self.position]).into_owned())
    }
}

fn is_number_start(byte: u8) -> bool {
    byte.is_ascii_digit() || matches!(byte, b'+' | b'-' | b'.')
}

fn is_delimiter(byte: u8) -> bool {
    matches!(
        byte,
        b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/'
    )
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn decode_name(bytes: &[u8]) -> String {
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'#' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) =
                (hex_nibble(bytes[index + 1]), hex_nibble(bytes[index + 2]))
            {
                output.push(high << 4 | low);
                index += 3;
                continue;
            }
        }
        output.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&output).into_owned()
}

fn parse_document<F: Fn() -> bool>(
    bytes: &[u8],
    should_stop: &F,
) -> Result<(BTreeMap<u32, PdfObject>, Option<PdfRef>), PdfError> {
    if should_stop() {
        return Err(PdfError::Cancelled);
    }
    let xref_offset = match verify_xref_structure(bytes)? {
        XrefLocation::Missing => None,
        XrefLocation::Table(offset) => {
            let latest = read_xref_table_revision(bytes, offset)?;
            if latest.1.contains_key("XRefStm") || latest.1.contains_key("Encrypt") {
                return Err(PdfError::UnsupportedStructure);
            }
            if latest.1.contains_key("Prev") {
                let (entries, root) = read_xref_table_chain(bytes, offset, latest)?;
                return parse_indexed_objects(bytes, entries, root, should_stop);
            }
            Some(offset)
        }
        XrefLocation::Stream(offset) => {
            return parse_indexed_document(bytes, offset, should_stop);
        }
    };
    let mut tokenizer = Tokenizer::new(bytes);
    let mut objects = BTreeMap::new();
    let mut token_count = 0_usize;
    while let Some(token) = tokenizer.next() {
        token_count = token_count.checked_add(1).ok_or(PdfError::CorruptedPdf)?;
        if token_count % 1_024 == 1 && should_stop() {
            return Err(PdfError::Cancelled);
        }
        if token == Token::Keyword("trailer".to_owned()) {
            let mut probe = tokenizer.clone();
            if probe.peek() == Some(Token::StartDictionary) {
                let trailer = parse_value(&mut probe)?;
                let dict = dictionary(&trailer).ok_or(PdfError::CorruptedPdf)?;
                if dict.contains_key("XRefStm") || dict.contains_key("Prev") {
                    return Err(PdfError::UnsupportedStructure);
                }
            }
            continue;
        }
        let Token::Number(object_number) = token else {
            continue;
        };
        let Some(Token::Number(generation)) = tokenizer.next() else {
            continue;
        };
        if tokenizer.next() != Some(Token::Keyword("obj".to_owned())) {
            continue;
        }
        let object_number = number_from_u32(object_number)?;
        bounded_pdf_integer(generation, u16::MAX as usize).ok_or(PdfError::CorruptedPdf)?;
        let value = parse_value(&mut tokenizer)?;
        if matches!(
            dictionary(&value).and_then(|dict| name_value(dict.get("Type"))),
            Some("ObjStm" | "XRef")
        ) {
            return Err(PdfError::UnsupportedStructure);
        }
        let stream = if tokenizer.peek() == Some(Token::Keyword("stream".to_owned())) {
            tokenizer.next();
            Some(read_stream(&mut tokenizer, &value, xref_offset, None)?)
        } else {
            None
        };
        objects.insert(object_number, PdfObject { value, stream });
        while let Some(next) = tokenizer.next() {
            if next == Token::Keyword("endobj".to_owned()) {
                break;
            }
        }
    }
    if objects.is_empty() {
        return Err(PdfError::InvalidPdf);
    }
    Ok((objects, None))
}

fn verify_xref_structure(bytes: &[u8]) -> Result<XrefLocation, PdfError> {
    let Some(eof) = bytes
        .windows(b"%%EOF".len())
        .rposition(|part| part == b"%%EOF")
    else {
        return Ok(XrefLocation::Missing);
    };
    let footer = bytes[..eof].trim_ascii_end();
    let digits_start = footer
        .iter()
        .rposition(|byte| !byte.is_ascii_digit())
        .map_or(0, |position| position + 1);
    let (prefix, offset_bytes) = footer.split_at(digits_start);
    let marker = b"startxref";
    let Some(before_marker) = prefix.trim_ascii_end().strip_suffix(marker) else {
        return Ok(XrefLocation::Missing);
    };
    if offset_bytes.is_empty()
        || before_marker
            .last()
            .is_some_and(|byte| !byte.is_ascii_whitespace())
    {
        return Ok(XrefLocation::Missing);
    }

    let offset = std::str::from_utf8(offset_bytes)
        .ok()
        .and_then(|digits| digits.parse::<usize>().ok())
        .ok_or(PdfError::CorruptedPdf)?;
    let target = bytes.get(offset..).ok_or(PdfError::CorruptedPdf)?;
    if target.starts_with(b"xref")
        && target
            .get(b"xref".len())
            .is_none_or(|byte| byte.is_ascii_whitespace())
    {
        Ok(XrefLocation::Table(offset))
    } else {
        Ok(XrefLocation::Stream(offset))
    }
}

fn parse_indexed_document<F: Fn() -> bool>(
    bytes: &[u8],
    offset: usize,
    should_stop: &F,
) -> Result<(BTreeMap<u32, PdfObject>, Option<PdfRef>), PdfError> {
    let (entries, root) = read_xref_stream_chain(bytes, offset, should_stop)?;
    parse_indexed_objects(bytes, entries, root, should_stop)
}

fn parse_indexed_objects<F: Fn() -> bool>(
    bytes: &[u8],
    entries: BTreeMap<u32, XrefEntry>,
    root: PdfRef,
    should_stop: &F,
) -> Result<(BTreeMap<u32, PdfObject>, Option<PdfRef>), PdfError> {
    let mut objects = BTreeMap::new();
    let mut compressed = BTreeMap::<u32, Vec<(u32, usize)>>::new();
    for (entry_index, (&number, &entry)) in entries.iter().enumerate() {
        if entry_index % 256 == 0 && should_stop() {
            return Err(PdfError::Cancelled);
        }
        match entry {
            XrefEntry::Free => {}
            XrefEntry::Direct { offset, generation } => {
                let (reference, object) = read_object_at(bytes, offset, Some(&entries), None)?;
                if reference.object != number || reference.generation != generation {
                    return Err(PdfError::CorruptedPdf);
                }
                objects.insert(number, object);
            }
            XrefEntry::Compressed {
                stream_number,
                index,
            } => compressed
                .entry(stream_number)
                .or_default()
                .push((number, index)),
        }
    }
    for (stream_index, (stream_number, references)) in compressed.into_iter().enumerate() {
        if stream_index % 16 == 0 && should_stop() {
            return Err(PdfError::Cancelled);
        }
        if !matches!(
            entries.get(&stream_number),
            Some(XrefEntry::Direct { generation: 0, .. })
        ) {
            return Err(PdfError::CorruptedPdf);
        }
        let stream = objects.get(&stream_number).ok_or(PdfError::CorruptedPdf)?;
        let decoded = decode_object_stream(stream, should_stop)?;
        let dict = dictionary(&stream.value).ok_or(PdfError::CorruptedPdf)?;
        let first = required_pdf_integer(dict, "First", decoded.len())?;
        let count = required_pdf_integer(dict, "N", MAX_OBJECT_STREAM_OBJECTS)?;
        let offsets = object_stream_offsets(&decoded, first, count)?;
        for (number, index) in references {
            let (stored_number, relative_offset) =
                offsets.get(index).ok_or(PdfError::CorruptedPdf)?;
            if *stored_number != number {
                return Err(PdfError::CorruptedPdf);
            }
            let end = offsets
                .get(index + 1)
                .map_or(decoded.len(), |(_, next)| first + next);
            let mut value_tokens = Tokenizer::new(&decoded[first + relative_offset..end]);
            let value = parse_value(&mut value_tokens)?;
            value_tokens.skip_space_and_comments();
            if value_tokens.raw_position() != value_tokens.bytes.len() {
                return Err(PdfError::CorruptedPdf);
            }
            objects.insert(
                number,
                PdfObject {
                    value,
                    stream: None,
                },
            );
        }
    }
    let root_entry = entries.get(&root.object).ok_or(PdfError::CorruptedPdf)?;
    let root_generation_matches = match root_entry {
        XrefEntry::Free => false,
        XrefEntry::Direct { generation, .. } => *generation == root.generation,
        XrefEntry::Compressed { .. } => root.generation == 0,
    };
    if !root_generation_matches
        || objects
            .get(&root.object)
            .and_then(|object| dictionary(&object.value))
            .and_then(|dict| name_value(dict.get("Type")))
            != Some("Catalog")
    {
        return Err(PdfError::CorruptedPdf);
    }
    validate_indexed_references(&objects, &entries)?;
    Ok((objects, Some(root)))
}

fn read_xref_table_revision(bytes: &[u8], offset: usize) -> Result<XrefTableRevision, PdfError> {
    let mut tokens = Tokenizer::new(bytes);
    tokens.set_position(offset);
    if tokens.next() != Some(Token::Keyword("xref".to_owned())) {
        return Err(PdfError::UnsupportedStructure);
    }

    let mut entries = BTreeMap::new();
    let mut subsection_count = 0usize;
    loop {
        if tokens.peek() == Some(Token::Keyword("trailer".to_owned())) {
            tokens.next();
            break;
        }
        subsection_count = subsection_count
            .checked_add(1)
            .filter(|count| *count <= MAX_XREF_ENTRIES)
            .ok_or(PdfError::CorruptedPdf)?;
        let first = match tokens.next() {
            Some(Token::Number(number)) => {
                bounded_pdf_integer(number, MAX_XREF_ENTRIES).ok_or(PdfError::CorruptedPdf)?
            }
            _ => return Err(PdfError::CorruptedPdf),
        };
        let count = match tokens.next() {
            Some(Token::Number(number)) => {
                bounded_pdf_integer(number, MAX_XREF_ENTRIES).ok_or(PdfError::CorruptedPdf)?
            }
            _ => return Err(PdfError::CorruptedPdf),
        };
        let end = first
            .checked_add(count)
            .filter(|end| *end <= MAX_XREF_ENTRIES)
            .ok_or(PdfError::CorruptedPdf)?;
        if entries
            .len()
            .checked_add(count)
            .is_none_or(|total| total > MAX_XREF_ENTRIES)
        {
            return Err(PdfError::CorruptedPdf);
        }
        for number in first..end {
            let target = match tokens.next() {
                Some(Token::Number(value)) => {
                    bounded_pdf_integer(value, bytes.len().max(MAX_XREF_ENTRIES))
                        .ok_or(PdfError::CorruptedPdf)?
                }
                _ => return Err(PdfError::CorruptedPdf),
            };
            let generation = match tokens.next() {
                Some(Token::Number(value)) => {
                    bounded_pdf_integer(value, u16::MAX as usize).ok_or(PdfError::CorruptedPdf)?
                }
                _ => return Err(PdfError::CorruptedPdf),
            };
            let entry = match tokens.next() {
                Some(Token::Keyword(status)) if status == "f" => XrefEntry::Free,
                Some(Token::Keyword(status))
                    if status == "n" && number != 0 && target > 0 && target < offset =>
                {
                    XrefEntry::Direct {
                        offset: target,
                        generation: u16::try_from(generation)
                            .map_err(|_| PdfError::CorruptedPdf)?,
                    }
                }
                _ => return Err(PdfError::CorruptedPdf),
            };
            if entries
                .insert(
                    u32::try_from(number).map_err(|_| PdfError::CorruptedPdf)?,
                    entry,
                )
                .is_some()
            {
                return Err(PdfError::CorruptedPdf);
            }
        }
    }

    let PdfValue::Dict(trailer) = parse_value(&mut tokens)? else {
        return Err(PdfError::CorruptedPdf);
    };
    if let Some(value) = trailer.get("Size") {
        let size = number_from_value(value)
            .and_then(|number| bounded_pdf_integer(number, MAX_XREF_ENTRIES))
            .filter(|size| *size > 0)
            .ok_or(PdfError::CorruptedPdf)?;
        if entries.keys().any(|number| {
            usize::try_from(*number)
                .ok()
                .is_none_or(|number| number >= size)
        }) {
            return Err(PdfError::CorruptedPdf);
        }
    }
    Ok((entries, trailer))
}

fn read_xref_table_chain(
    bytes: &[u8],
    start: usize,
    first: XrefTableRevision,
) -> Result<(BTreeMap<u32, XrefEntry>, PdfRef), PdfError> {
    let mut offset = start;
    let mut revision = first;
    let mut entries = BTreeMap::new();
    let mut newest_size = None;
    let mut total_entries = 0usize;
    let root = reference_value(revision.1.get("Root").ok_or(PdfError::CorruptedPdf)?)
        .ok_or(PdfError::CorruptedPdf)?;

    for _ in 0..MAX_XREF_REVISIONS {
        let (current, trailer) = revision;
        if trailer.contains_key("XRefStm") || trailer.contains_key("Encrypt") {
            return Err(PdfError::UnsupportedStructure);
        }
        let size = required_pdf_integer(&trailer, "Size", MAX_XREF_ENTRIES)?;
        if size == 0 || newest_size.is_some_and(|latest| size > latest) {
            return Err(PdfError::CorruptedPdf);
        }
        newest_size.get_or_insert(size);
        total_entries = total_entries
            .checked_add(current.len())
            .filter(|total| *total <= MAX_XREF_ENTRIES)
            .ok_or(PdfError::CorruptedPdf)?;
        for (number, entry) in current {
            entries.entry(number).or_insert(entry);
        }

        let Some(previous) = trailer.get("Prev") else {
            return Ok((entries, root));
        };
        offset = number_from_value(previous)
            .and_then(|value| bounded_pdf_integer(value, offset.saturating_sub(1)))
            .filter(|previous| *previous > 0 && *previous < offset)
            .ok_or(PdfError::CorruptedPdf)?;
        revision = read_xref_table_revision(bytes, offset)?;
    }
    Err(PdfError::UnsupportedStructure)
}

fn read_xref_stream_chain<F: Fn() -> bool>(
    bytes: &[u8],
    start: usize,
    should_stop: &F,
) -> Result<(BTreeMap<u32, XrefEntry>, PdfRef), PdfError> {
    let mut offset = start;
    let mut entries = BTreeMap::new();
    let mut root = None;
    let mut newest_size = None;
    let mut total_entries = 0usize;

    for _ in 0..MAX_XREF_REVISIONS {
        if should_stop() {
            return Err(PdfError::Cancelled);
        }
        if !bytes.get(offset).is_some_and(u8::is_ascii_digit) {
            return Err(PdfError::UnsupportedStructure);
        }
        let (xref_ref, xref_object) = read_object_at(bytes, offset, None, Some("XRef"))?;
        let dict = dictionary(&xref_object.value).ok_or(PdfError::UnsupportedStructure)?;
        if name_value(dict.get("Type")) != Some("XRef") {
            return Err(PdfError::UnsupportedStructure);
        }
        if dict.contains_key("XRefStm") || dict.contains_key("Encrypt") {
            return Err(PdfError::UnsupportedStructure);
        }
        if root.is_none() {
            root = Some(
                reference_value(dict.get("Root").ok_or(PdfError::CorruptedPdf)?)
                    .ok_or(PdfError::CorruptedPdf)?,
            );
        }
        let size = required_pdf_integer(dict, "Size", MAX_XREF_ENTRIES)?;
        if newest_size.is_some_and(|latest| size > latest) {
            return Err(PdfError::CorruptedPdf);
        }
        newest_size.get_or_insert(size);

        let revision = parse_xref_stream(&xref_object, should_stop)?;
        if revision.get(&xref_ref.object)
            != Some(&XrefEntry::Direct {
                offset,
                generation: xref_ref.generation,
            })
        {
            return Err(PdfError::CorruptedPdf);
        }
        total_entries = total_entries
            .checked_add(revision.len())
            .filter(|total| *total <= MAX_XREF_ENTRIES)
            .ok_or(PdfError::CorruptedPdf)?;
        for (number, entry) in revision {
            if matches!(entry, XrefEntry::Direct { offset: target, .. } if target > offset) {
                return Err(PdfError::CorruptedPdf);
            }
            entries.entry(number).or_insert(entry);
        }

        let Some(previous) = dict.get("Prev") else {
            return Ok((entries, root.ok_or(PdfError::CorruptedPdf)?));
        };
        offset = number_from_value(previous)
            .and_then(|value| bounded_pdf_integer(value, offset.saturating_sub(1)))
            .filter(|previous| *previous > 0 && *previous < offset)
            .ok_or(PdfError::CorruptedPdf)?;
    }
    Err(PdfError::UnsupportedStructure)
}

fn validate_indexed_references(
    objects: &BTreeMap<u32, PdfObject>,
    entries: &BTreeMap<u32, XrefEntry>,
) -> Result<(), PdfError> {
    for object in objects.values() {
        let mut pending = vec![&object.value];
        while let Some(value) = pending.pop() {
            match value {
                PdfValue::Ref(reference) => {
                    let valid = match entries.get(&reference.object) {
                        Some(XrefEntry::Free) => false,
                        Some(XrefEntry::Direct { generation, .. }) => {
                            *generation == reference.generation
                        }
                        Some(XrefEntry::Compressed { .. }) => reference.generation == 0,
                        None => false,
                    };
                    if !valid {
                        return Err(PdfError::CorruptedPdf);
                    }
                }
                PdfValue::Array(values) => pending.extend(values),
                PdfValue::Dict(values) => pending.extend(values.values()),
                _ => {}
            }
        }
    }
    Ok(())
}

fn read_object_at(
    bytes: &[u8],
    offset: usize,
    entries: Option<&BTreeMap<u32, XrefEntry>>,
    expected_type: Option<&str>,
) -> Result<(PdfRef, PdfObject), PdfError> {
    if !bytes.get(offset).is_some_and(u8::is_ascii_digit) {
        return Err(PdfError::CorruptedPdf);
    }
    let mut tokens = Tokenizer::new(bytes);
    tokens.set_position(offset);
    let (Some(Token::Number(number)), Some(Token::Number(generation))) =
        (tokens.next(), tokens.next())
    else {
        return Err(PdfError::CorruptedPdf);
    };
    let reference = PdfRef {
        object: u32::try_from(
            bounded_pdf_integer(number, u32::MAX as usize).ok_or(PdfError::CorruptedPdf)?,
        )
        .map_err(|_| PdfError::CorruptedPdf)?,
        generation: u16::try_from(
            bounded_pdf_integer(generation, u16::MAX as usize).ok_or(PdfError::CorruptedPdf)?,
        )
        .map_err(|_| PdfError::CorruptedPdf)?,
    };
    if tokens.next() != Some(Token::Keyword("obj".to_owned())) {
        return Err(PdfError::CorruptedPdf);
    }
    let value = parse_value(&mut tokens)?;
    if expected_type.is_some_and(|expected| {
        dictionary(&value).and_then(|dict| name_value(dict.get("Type"))) != Some(expected)
    }) {
        return Err(PdfError::UnsupportedStructure);
    }
    let stream = if tokens.peek() == Some(Token::Keyword("stream".to_owned())) {
        tokens.next();
        Some(read_stream(&mut tokens, &value, None, entries)?)
    } else {
        None
    };
    if tokens.next() != Some(Token::Keyword("endobj".to_owned())) {
        return Err(PdfError::CorruptedPdf);
    }
    Ok((reference, PdfObject { value, stream }))
}

fn required_pdf_integer(
    dict: &BTreeMap<String, PdfValue>,
    key: &str,
    max: usize,
) -> Result<usize, PdfError> {
    dict.get(key)
        .and_then(number_from_value)
        .and_then(|value| bounded_pdf_integer(value, max))
        .ok_or(PdfError::CorruptedPdf)
}

fn parse_xref_stream<F: Fn() -> bool>(
    object: &PdfObject,
    should_stop: &F,
) -> Result<BTreeMap<u32, XrefEntry>, PdfError> {
    let dict = dictionary(&object.value).ok_or(PdfError::CorruptedPdf)?;
    if dict.contains_key("DecodeParms") {
        return Err(PdfError::UnsupportedStructure);
    }
    let size = required_pdf_integer(dict, "Size", MAX_XREF_ENTRIES)?;
    if size == 0 {
        return Err(PdfError::CorruptedPdf);
    }
    let Some(PdfValue::Array(widths)) = dict.get("W") else {
        return Err(PdfError::CorruptedPdf);
    };
    if widths.len() != 3 {
        return Err(PdfError::CorruptedPdf);
    }
    let widths = widths
        .iter()
        .map(|value| number_from_value(value).and_then(|number| bounded_pdf_integer(number, 8)))
        .collect::<Option<Vec<_>>>()
        .ok_or(PdfError::CorruptedPdf)?;
    let entry_width = widths.iter().sum::<usize>();
    if entry_width == 0 {
        return Err(PdfError::CorruptedPdf);
    }
    let ranges = match dict.get("Index") {
        None => vec![(0, size)],
        Some(PdfValue::Array(values))
            if values.len() % 2 == 0 && values.len() <= MAX_XREF_ENTRIES * 2 =>
        {
            let mut ranges = Vec::new();
            for pair in values.chunks_exact(2) {
                let start = number_from_value(&pair[0])
                    .and_then(|value| bounded_pdf_integer(value, size))
                    .ok_or(PdfError::CorruptedPdf)?;
                let count = number_from_value(&pair[1])
                    .and_then(|value| bounded_pdf_integer(value, size))
                    .ok_or(PdfError::CorruptedPdf)?;
                if start.checked_add(count).is_none_or(|end| end > size) {
                    return Err(PdfError::CorruptedPdf);
                }
                ranges.push((start, count));
            }
            ranges
        }
        _ => return Err(PdfError::CorruptedPdf),
    };
    let count = ranges.iter().try_fold(0usize, |total, (_, count)| {
        total
            .checked_add(*count)
            .filter(|total| *total <= MAX_XREF_ENTRIES)
    });
    let count = count.ok_or(PdfError::CorruptedPdf)?;
    let decoded = decode_stream_with_control(object, should_stop)?;
    if count.checked_mul(entry_width) != Some(decoded.len()) {
        return Err(PdfError::CorruptedPdf);
    }

    let mut entries = BTreeMap::new();
    let mut seen = HashSet::new();
    let mut position = 0;
    for (start, count) in ranges {
        for (range_index, number) in (start..start + count).enumerate() {
            if range_index % 256 == 0 && should_stop() {
                return Err(PdfError::Cancelled);
            }
            let number = u32::try_from(number).map_err(|_| PdfError::CorruptedPdf)?;
            if !seen.insert(number) {
                return Err(PdfError::CorruptedPdf);
            }
            let record = &decoded[position..position + entry_width];
            position += entry_width;
            let (first, rest) = record.split_at(widths[0]);
            let (second, third) = rest.split_at(widths[1]);
            let kind = if first.is_empty() {
                1
            } else {
                read_be_field(first)
            };
            let second = read_be_field(second);
            let third = read_be_field(third);
            let entry = match kind {
                0 => XrefEntry::Free,
                1 => XrefEntry::Direct {
                    offset: usize::try_from(second).map_err(|_| PdfError::CorruptedPdf)?,
                    generation: u16::try_from(third).map_err(|_| PdfError::CorruptedPdf)?,
                },
                2 => XrefEntry::Compressed {
                    stream_number: u32::try_from(second).map_err(|_| PdfError::CorruptedPdf)?,
                    index: usize::try_from(third)
                        .ok()
                        .filter(|index| *index < MAX_OBJECT_STREAM_OBJECTS)
                        .ok_or(PdfError::CorruptedPdf)?,
                },
                _ => return Err(PdfError::UnsupportedStructure),
            };
            if (number == 0 && !matches!(entry, XrefEntry::Free))
                || matches!(entry, XrefEntry::Direct { offset: 0, .. })
                || matches!(
                    entry,
                    XrefEntry::Compressed {
                        stream_number: 0,
                        ..
                    }
                )
            {
                return Err(PdfError::CorruptedPdf);
            }
            entries.insert(number, entry);
        }
    }
    Ok(entries)
}

fn read_be_field(bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .fold(0, |value, byte| (value << 8) | u64::from(*byte))
}

fn decode_object_stream<F: Fn() -> bool>(
    object: &PdfObject,
    should_stop: &F,
) -> Result<Vec<u8>, PdfError> {
    let dict = dictionary(&object.value).ok_or(PdfError::CorruptedPdf)?;
    if name_value(dict.get("Type")) != Some("ObjStm") || dict.contains_key("DecodeParms") {
        return Err(PdfError::UnsupportedStructure);
    }
    decode_stream_with_control(object, should_stop)
}

fn object_stream_offsets(
    decoded: &[u8],
    first: usize,
    count: usize,
) -> Result<Vec<(u32, usize)>, PdfError> {
    let header = decoded.get(..first).ok_or(PdfError::CorruptedPdf)?;
    let mut tokens = Tokenizer::new(header);
    let mut offsets = Vec::with_capacity(count);
    let mut numbers = HashSet::new();
    for _ in 0..count {
        let (Some(Token::Number(number)), Some(Token::Number(offset))) =
            (tokens.next(), tokens.next())
        else {
            return Err(PdfError::CorruptedPdf);
        };
        let number = number_from_u32(number)?;
        let offset = bounded_pdf_integer(offset, decoded.len() - first)
            .filter(|offset| first + offset < decoded.len())
            .ok_or(PdfError::CorruptedPdf)?;
        if number == 0
            || !numbers.insert(number)
            || offsets
                .last()
                .is_some_and(|(_, previous)| *previous >= offset)
        {
            return Err(PdfError::CorruptedPdf);
        }
        offsets.push((number, offset));
    }
    tokens.skip_space_and_comments();
    if tokens.raw_position() != header.len() {
        return Err(PdfError::CorruptedPdf);
    }
    Ok(offsets)
}

fn number_from_u32(value: f64) -> Result<u32, PdfError> {
    u32::try_from(bounded_pdf_integer(value, u32::MAX as usize).ok_or(PdfError::CorruptedPdf)?)
        .map_err(|_| PdfError::CorruptedPdf)
}

fn parse_value(tokenizer: &mut Tokenizer<'_>) -> Result<PdfValue, PdfError> {
    parse_value_with_depth(tokenizer, 0)
}

fn parse_value_with_depth(
    tokenizer: &mut Tokenizer<'_>,
    depth: usize,
) -> Result<PdfValue, PdfError> {
    if depth > MAX_VALUE_DEPTH {
        return Err(PdfError::CorruptedPdf);
    }
    let token = tokenizer.next().ok_or(PdfError::CorruptedPdf)?;
    match token {
        Token::Number(number) => {
            if let Some(object_number) = bounded_pdf_integer(number, u32::MAX as usize) {
                let mut probe = tokenizer.clone();
                if let Some(Token::Number(generation)) = probe.next() {
                    if let Some(generation) = bounded_pdf_integer(generation, u16::MAX as usize) {
                        if probe.next() == Some(Token::Keyword("R".to_owned())) {
                            *tokenizer = probe;
                            return Ok(PdfValue::Ref(PdfRef {
                                object: u32::try_from(object_number)
                                    .map_err(|_| PdfError::CorruptedPdf)?,
                                generation: u16::try_from(generation)
                                    .map_err(|_| PdfError::CorruptedPdf)?,
                            }));
                        }
                    }
                }
            }
            Ok(PdfValue::Number(number))
        }
        Token::Name(name) => Ok(PdfValue::Name(name)),
        Token::LiteralString(value) => Ok(PdfValue::String(value)),
        Token::HexString(value) => Ok(PdfValue::HexString(value)),
        Token::StartArray => parse_array(tokenizer, depth + 1),
        Token::StartDictionary => parse_dictionary(tokenizer, depth + 1),
        Token::Keyword(keyword) => match keyword.as_str() {
            "null" => Ok(PdfValue::Null),
            "true" => Ok(PdfValue::Bool(true)),
            "false" => Ok(PdfValue::Bool(false)),
            _ => Ok(PdfValue::Keyword(keyword)),
        },
        Token::EndArray | Token::EndDictionary => Err(PdfError::CorruptedPdf),
    }
}

fn parse_array(tokenizer: &mut Tokenizer<'_>, depth: usize) -> Result<PdfValue, PdfError> {
    let mut values = Vec::new();
    while let Some(token) = tokenizer.peek() {
        if token == Token::EndArray {
            tokenizer.next();
            return Ok(PdfValue::Array(values));
        }
        values.push(parse_value_with_depth(tokenizer, depth)?);
    }
    Err(PdfError::CorruptedPdf)
}

fn parse_dictionary(tokenizer: &mut Tokenizer<'_>, depth: usize) -> Result<PdfValue, PdfError> {
    let mut values = BTreeMap::new();
    while let Some(token) = tokenizer.peek() {
        if token == Token::EndDictionary {
            tokenizer.next();
            return Ok(PdfValue::Dict(values));
        }
        let Some(Token::Name(name)) = tokenizer.next() else {
            return Err(PdfError::CorruptedPdf);
        };
        let value = parse_value_with_depth(tokenizer, depth)?;
        values.insert(name, value);
    }
    Err(PdfError::CorruptedPdf)
}

fn read_stream(
    tokenizer: &mut Tokenizer<'_>,
    value: &PdfValue,
    xref_offset: Option<usize>,
    entries: Option<&BTreeMap<u32, XrefEntry>>,
) -> Result<Vec<u8>, PdfError> {
    let start = tokenizer.raw_position();
    let mut stream_start = start;
    if tokenizer.bytes.get(stream_start) == Some(&b'\r') {
        stream_start += 1;
        if tokenizer.bytes.get(stream_start) == Some(&b'\n') {
            stream_start += 1;
        }
    } else if tokenizer.bytes.get(stream_start) == Some(&b'\n') {
        stream_start += 1;
    }

    let length = match dictionary(value).and_then(|dict| dict.get("Length")) {
        Some(PdfValue::Ref(reference)) => match entries {
            Some(entries) => indexed_stream_length(tokenizer.bytes, *reference, entries)?,
            None => referenced_stream_length(tokenizer.bytes, *reference, xref_offset)?,
        },
        Some(PdfValue::Number(length)) => bounded_stream_length(*length)?,
        _ => return Err(PdfError::CorruptedPdf),
    };
    let stream_end = stream_start
        .checked_add(length)
        .filter(|end| *end <= tokenizer.bytes.len())
        .ok_or(PdfError::CorruptedPdf)?;
    let output = tokenizer.bytes[stream_start..stream_end].to_vec();
    tokenizer.set_position(stream_end);
    if tokenizer.next() != Some(Token::Keyword("endstream".to_owned())) {
        return Err(PdfError::CorruptedPdf);
    }
    Ok(output)
}

fn indexed_stream_length(
    bytes: &[u8],
    reference: PdfRef,
    entries: &BTreeMap<u32, XrefEntry>,
) -> Result<usize, PdfError> {
    match entries.get(&reference.object) {
        Some(XrefEntry::Direct { offset, generation }) if *generation == reference.generation => {
            read_length_object(bytes, *offset, reference)
        }
        Some(XrefEntry::Compressed { .. }) => Err(PdfError::UnsupportedStreamLength),
        _ => Err(PdfError::CorruptedPdf),
    }
}

fn bounded_stream_length(length: f64) -> Result<usize, PdfError> {
    if !length.is_finite() || length < 0.0 || length.trunc() != length {
        return Err(PdfError::CorruptedPdf);
    }
    if length > MAX_STREAM_BYTES as f64 {
        return Err(PdfError::StreamLimitExceeded);
    }
    Ok(length as usize)
}

fn bounded_pdf_integer(value: f64, max: usize) -> Option<usize> {
    (value.is_finite() && value >= 0.0 && value.trunc() == value && value <= max as f64)
        .then_some(value as usize)
}

fn referenced_stream_length(
    bytes: &[u8],
    reference: PdfRef,
    xref_offset: Option<usize>,
) -> Result<usize, PdfError> {
    let offset = xref_offset.ok_or(PdfError::UnsupportedStreamLength)?;
    let mut table = Tokenizer::new(bytes.get(offset..).ok_or(PdfError::CorruptedPdf)?);
    if table.next() != Some(Token::Keyword("xref".to_owned())) {
        return Err(PdfError::CorruptedPdf);
    }

    loop {
        let first = match table.next() {
            Some(Token::Number(value)) => {
                bounded_pdf_integer(value, u32::MAX as usize).ok_or(PdfError::CorruptedPdf)?
            }
            Some(Token::Keyword(keyword)) if keyword == "trailer" => {
                return Err(PdfError::UnsupportedStreamLength);
            }
            _ => return Err(PdfError::CorruptedPdf),
        };
        let count = match table.next() {
            Some(Token::Number(value)) => {
                bounded_pdf_integer(value, 1_000_000).ok_or(PdfError::CorruptedPdf)?
            }
            _ => return Err(PdfError::CorruptedPdf),
        };
        for index in 0..count {
            let object_offset = match table.next() {
                Some(Token::Number(value)) => {
                    bounded_pdf_integer(value, bytes.len()).ok_or(PdfError::CorruptedPdf)?
                }
                _ => return Err(PdfError::CorruptedPdf),
            };
            let generation = match table.next() {
                Some(Token::Number(value)) => {
                    bounded_pdf_integer(value, u16::MAX as usize).ok_or(PdfError::CorruptedPdf)?
                }
                _ => return Err(PdfError::CorruptedPdf),
            };
            let Some(Token::Keyword(status)) = table.next() else {
                return Err(PdfError::CorruptedPdf);
            };
            if status != "n" && status != "f" {
                return Err(PdfError::CorruptedPdf);
            }
            if first.checked_add(index) == usize::try_from(reference.object).ok()
                && generation == usize::from(reference.generation)
            {
                if status == "f" {
                    return Err(PdfError::CorruptedPdf);
                }
                return read_length_object(bytes, object_offset, reference);
            }
        }
    }
}

fn read_length_object(bytes: &[u8], offset: usize, reference: PdfRef) -> Result<usize, PdfError> {
    let mut object = Tokenizer::new(bytes.get(offset..).ok_or(PdfError::CorruptedPdf)?);
    let (Some(Token::Number(number)), Some(Token::Number(generation))) =
        (object.next(), object.next())
    else {
        return Err(PdfError::CorruptedPdf);
    };
    if bounded_pdf_integer(number, u32::MAX as usize) != usize::try_from(reference.object).ok()
        || bounded_pdf_integer(generation, u16::MAX as usize)
            != Some(usize::from(reference.generation))
        || object.next() != Some(Token::Keyword("obj".to_owned()))
    {
        return Err(PdfError::CorruptedPdf);
    }
    let Some(Token::Number(length)) = object.next() else {
        return Err(PdfError::CorruptedPdf);
    };
    if object.next() != Some(Token::Keyword("endobj".to_owned())) {
        return Err(PdfError::CorruptedPdf);
    }
    bounded_stream_length(length)
}

fn dictionary(value: &PdfValue) -> Option<&BTreeMap<String, PdfValue>> {
    match value {
        PdfValue::Dict(value) => Some(value),
        _ => None,
    }
}

fn number_from_value(value: &PdfValue) -> Option<f64> {
    match value {
        PdfValue::Number(number) => Some(*number),
        _ => None,
    }
}

fn ordered_pages(
    objects: &BTreeMap<u32, PdfObject>,
    root: Option<PdfRef>,
) -> Result<Vec<PdfRef>, PdfError> {
    let catalog = root.or_else(|| {
        objects.iter().find_map(|(number, object)| {
            let dict = dictionary(&object.value)?;
            (name_value(dict.get("Type")) == Some("Catalog")).then_some(PdfRef {
                object: *number,
                generation: 0,
            })
        })
    });
    let mut pages = Vec::new();
    let mut visited = HashSet::new();
    if let Some(catalog_ref) = catalog {
        if let Some(catalog) = objects.get(&catalog_ref.object) {
            if let Some(root) = dictionary(&catalog.value).and_then(|dict| dict.get("Pages")) {
                collect_page_tree(objects, root, &mut pages, &mut visited, 0)?;
            }
        }
    }
    if pages.is_empty() && root.is_none() {
        pages = objects
            .iter()
            .filter_map(|(number, object)| {
                let dict = dictionary(&object.value)?;
                (name_value(dict.get("Type")) == Some("Page")).then_some(PdfRef {
                    object: *number,
                    generation: 0,
                })
            })
            .collect();
    }
    Ok(pages)
}

fn page_size(objects: &BTreeMap<u32, PdfObject>, page_ref: PdfRef) -> (u32, u32) {
    let mut current = Some(page_ref);
    let mut visited = HashSet::new();
    while let Some(reference) = current {
        if !visited.insert(reference.object) {
            break;
        }
        let Some(page) = objects.get(&reference.object) else {
            break;
        };
        let Some(dict) = dictionary(&page.value) else {
            break;
        };
        if let Some(PdfValue::Array(values)) = dict.get("MediaBox") {
            if values.len() >= 4 {
                let Some(x0) = values.first().and_then(number_from_value) else {
                    break;
                };
                let Some(y0) = values.get(1).and_then(number_from_value) else {
                    break;
                };
                let Some(x1) = values.get(2).and_then(number_from_value) else {
                    break;
                };
                let Some(y1) = values.get(3).and_then(number_from_value) else {
                    break;
                };
                let width = (x1 - x0).abs().round().clamp(1.0, 2000.0) as u32;
                let height = (y1 - y0).abs().round().clamp(1.0, 2000.0) as u32;
                return (width, height);
            }
        }
        current = match dict.get("Parent") {
            Some(PdfValue::Ref(parent)) => Some(*parent),
            _ => None,
        };
    }
    (612, 792)
}

fn collect_page_tree(
    objects: &BTreeMap<u32, PdfObject>,
    value: &PdfValue,
    pages: &mut Vec<PdfRef>,
    visited: &mut HashSet<u32>,
    depth: usize,
) -> Result<(), PdfError> {
    if depth > MAX_VALUE_DEPTH {
        return Err(PdfError::CorruptedPdf);
    }
    match value {
        PdfValue::Array(kids) => {
            for kid in kids {
                collect_page_tree(objects, kid, pages, visited, depth + 1)?;
            }
        }
        PdfValue::Ref(reference) => {
            if !visited.insert(reference.object) {
                return Err(PdfError::CorruptedPdf);
            }
            let object = objects
                .get(&reference.object)
                .ok_or(PdfError::CorruptedPdf)?;
            let Some(dict) = dictionary(&object.value) else {
                return collect_page_tree(objects, &object.value, pages, visited, depth + 1);
            };
            match name_value(dict.get("Type")) {
                Some("Page") => {
                    if pages.len() >= MAX_PAGES {
                        return Err(PdfError::PageLimitExceeded);
                    }
                    pages.push(*reference);
                }
                Some("Pages") | None => {
                    let kids = dict.get("Kids").ok_or(PdfError::CorruptedPdf)?;
                    collect_page_tree(objects, kids, pages, visited, depth + 1)?;
                }
                Some(_) => return Err(PdfError::CorruptedPdf),
            }
        }
        _ => return Err(PdfError::CorruptedPdf),
    }
    Ok(())
}

fn name_value(value: Option<&PdfValue>) -> Option<&str> {
    match value {
        Some(PdfValue::Name(name)) => Some(name.as_str()),
        _ => None,
    }
}

#[derive(Debug, Clone, Default)]
struct FontMap {
    code_width: usize,
    values: BTreeMap<Vec<u8>, String>,
}

fn page_font_maps<F: Fn() -> bool>(
    objects: &BTreeMap<u32, PdfObject>,
    page_ref: PdfRef,
    should_stop: &F,
) -> Result<BTreeMap<String, FontMap>, PdfError> {
    let mut current = Some(page_ref);
    let mut visited = HashSet::new();
    while let Some(reference) = current {
        if should_stop() {
            return Err(PdfError::Cancelled);
        }
        if !visited.insert(reference.object) {
            break;
        }
        let Some(page) = objects.get(&reference.object) else {
            break;
        };
        let Some(page_dict) = dictionary(&page.value) else {
            break;
        };
        if let Some(resources) = page_dict.get("Resources") {
            if let Some(resources_dict) = resolved_dictionary(objects, resources) {
                let fonts = font_maps_for_resources(objects, resources_dict, should_stop)?;
                if !fonts.is_empty() {
                    return Ok(fonts);
                }
            }
        }
        current = match page_dict.get("Parent") {
            Some(PdfValue::Ref(parent)) => Some(*parent),
            _ => None,
        };
    }
    Ok(BTreeMap::new())
}

fn font_maps_for_resources<F: Fn() -> bool>(
    objects: &BTreeMap<u32, PdfObject>,
    resources: &BTreeMap<String, PdfValue>,
    should_stop: &F,
) -> Result<BTreeMap<String, FontMap>, PdfError> {
    let Some(fonts) = resources.get("Font") else {
        return Ok(BTreeMap::new());
    };
    let Some(font_dict) = resolved_dictionary(objects, fonts) else {
        return Ok(BTreeMap::new());
    };
    let mut output = BTreeMap::new();
    for (name, font_value) in font_dict {
        if should_stop() {
            return Err(PdfError::Cancelled);
        }
        let Some(font) = resolved_dictionary(objects, font_value) else {
            continue;
        };
        let Some(reference) = font.get("ToUnicode").and_then(reference_value) else {
            continue;
        };
        let Some(object) = objects.get(&reference.object) else {
            continue;
        };
        let stream = match decode_stream_with_control(object, should_stop) {
            Ok(stream) => stream,
            Err(error)
                if matches!(
                    error,
                    PdfError::Cancelled
                        | PdfError::StreamLimitExceeded
                        | PdfError::CMapLimitExceeded
                ) =>
            {
                return Err(error);
            }
            Err(_) => continue,
        };
        if let Some(map) = parse_to_unicode_cmap(&stream, should_stop)? {
            output.insert(name.clone(), map);
        }
    }
    Ok(output)
}

fn page_resources(
    objects: &BTreeMap<u32, PdfObject>,
    page_ref: PdfRef,
) -> Option<&BTreeMap<String, PdfValue>> {
    let mut current = Some(page_ref);
    let mut visited = HashSet::new();
    while let Some(reference) = current {
        if !visited.insert(reference.object) {
            return None;
        }
        let dict = objects
            .get(&reference.object)
            .and_then(|object| dictionary(&object.value))?;
        if let Some(resources) = dict.get("Resources") {
            return resolved_dictionary(objects, resources);
        }
        current = dict.get("Parent").and_then(reference_value);
    }
    None
}

fn resolved_dictionary<'a>(
    objects: &'a BTreeMap<u32, PdfObject>,
    value: &'a PdfValue,
) -> Option<&'a BTreeMap<String, PdfValue>> {
    match value {
        PdfValue::Dict(dict) => Some(dict),
        PdfValue::Ref(reference) => objects
            .get(&reference.object)
            .and_then(|object| dictionary(&object.value)),
        _ => None,
    }
}

fn reference_value(value: &PdfValue) -> Option<PdfRef> {
    match value {
        PdfValue::Ref(reference) => Some(*reference),
        _ => None,
    }
}

#[derive(Debug, Default)]
struct CmapBudget {
    entries: usize,
    mapping_bytes: usize,
    tokens: usize,
}

impl CmapBudget {
    fn charge_token(&mut self) -> Result<(), PdfError> {
        self.tokens = self
            .tokens
            .checked_add(1)
            .filter(|count| *count <= MAX_CMAP_TOKENS)
            .ok_or(PdfError::CMapLimitExceeded)?;
        Ok(())
    }

    fn ensure_entry_capacity(&self, count: usize) -> Result<(), PdfError> {
        self.entries
            .checked_add(count)
            .filter(|total| *total <= MAX_CMAP_ENTRIES)
            .map(|_| ())
            .ok_or(PdfError::CMapLimitExceeded)
    }

    fn insert(&mut self, map: &mut FontMap, source: Vec<u8>, text: String) -> Result<(), PdfError> {
        self.ensure_entry_capacity(1)?;
        let bytes = source
            .len()
            .checked_add(text.len())
            .ok_or(PdfError::CMapLimitExceeded)?;
        self.mapping_bytes = self
            .mapping_bytes
            .checked_add(bytes)
            .filter(|total| *total <= MAX_CMAP_MAPPING_BYTES)
            .ok_or(PdfError::CMapLimitExceeded)?;
        self.entries = self
            .entries
            .checked_add(1)
            .ok_or(PdfError::CMapLimitExceeded)?;
        map.code_width = map.code_width.max(source.len());
        map.values.insert(source, text);
        Ok(())
    }
}

struct CmapHexTokens<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> CmapHexTokens<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn advance(&mut self, count: usize) -> Result<(), PdfError> {
        self.position = self
            .position
            .checked_add(count)
            .filter(|position| *position <= self.bytes.len())
            .ok_or(PdfError::CMapLimitExceeded)?;
        Ok(())
    }

    fn next<F: Fn() -> bool>(
        &mut self,
        budget: &mut CmapBudget,
        should_stop: &F,
    ) -> Result<Option<Vec<u8>>, PdfError> {
        loop {
            while self.position < self.bytes.len() && self.bytes[self.position] != b'<' {
                if self.position % 4_096 == 0 && should_stop() {
                    return Err(PdfError::Cancelled);
                }
                self.advance(1)?;
            }
            if self.position >= self.bytes.len() {
                return Ok(None);
            }
            let next_position = self
                .position
                .checked_add(1)
                .ok_or(PdfError::CMapLimitExceeded)?;
            if self.bytes.get(next_position) == Some(&b'<') {
                self.advance(2)?;
                continue;
            }

            self.advance(1)?;
            let start = self.position;
            let mut nibble_count = 0_usize;
            let mut valid = true;
            let max_nibbles = MAX_CMAP_TOKEN_BYTES
                .checked_mul(2)
                .ok_or(PdfError::CMapLimitExceeded)?;
            while let Some(&byte) = self.bytes.get(self.position) {
                if self.position % 4_096 == 0 && should_stop() {
                    return Err(PdfError::Cancelled);
                }
                if byte == b'>' {
                    break;
                }
                if !byte.is_ascii_whitespace() {
                    nibble_count = nibble_count
                        .checked_add(1)
                        .ok_or(PdfError::CMapLimitExceeded)?;
                    if nibble_count > max_nibbles {
                        return Err(PdfError::CMapLimitExceeded);
                    }
                    valid &= hex_nibble(byte).is_some();
                }
                self.advance(1)?;
            }
            if self.bytes.get(self.position) != Some(&b'>') {
                return Ok(None);
            }
            let end = self.position;
            self.advance(1)?;
            budget.charge_token()?;
            if !valid || nibble_count % 2 != 0 {
                continue;
            }

            let mut output = Vec::with_capacity(nibble_count / 2);
            let mut high = None;
            for &byte in &self.bytes[start..end] {
                if byte.is_ascii_whitespace() {
                    continue;
                }
                let nibble = hex_nibble(byte).ok_or(PdfError::CorruptedPdf)?;
                if let Some(high) = high.take() {
                    output.push(high << 4 | nibble);
                } else {
                    high = Some(nibble);
                }
            }
            return Ok(Some(output));
        }
    }
}

fn parse_to_unicode_cmap<F: Fn() -> bool>(
    bytes: &[u8],
    should_stop: &F,
) -> Result<Option<FontMap>, PdfError> {
    if should_stop() {
        return Err(PdfError::Cancelled);
    }
    let mut map = FontMap::default();
    let mut budget = CmapBudget::default();
    let blocks: [(&[u8], &[u8], bool); 2] = [
        (b"beginbfchar", b"endbfchar", false),
        (b"beginbfrange", b"endbfrange", true),
    ];
    for (begin, end, is_range) in blocks {
        let mut remainder = bytes;
        while let Some(begin_index) = find_bytes_with_control(remainder, begin, should_stop)? {
            let block_start = begin_index
                .checked_add(begin.len())
                .ok_or(PdfError::CMapLimitExceeded)?;
            remainder = remainder
                .get(block_start..)
                .ok_or(PdfError::CMapLimitExceeded)?;
            let Some(end_index) = find_bytes_with_control(remainder, end, should_stop)? else {
                break;
            };
            let block = &remainder[..end_index];
            if is_range {
                parse_bfrange_block(block, &mut map, &mut budget, should_stop)?;
            } else {
                parse_bfchar_block(block, &mut map, &mut budget, should_stop)?;
            }
            let next = end_index
                .checked_add(end.len())
                .ok_or(PdfError::CMapLimitExceeded)?;
            remainder = remainder.get(next..).ok_or(PdfError::CMapLimitExceeded)?;
        }
    }
    Ok((map.code_width > 0 && !map.values.is_empty()).then_some(map))
}

fn find_bytes_with_control<F: Fn() -> bool>(
    haystack: &[u8],
    needle: &[u8],
    should_stop: &F,
) -> Result<Option<usize>, PdfError> {
    let Some(last_start) = haystack.len().checked_sub(needle.len()) else {
        return Ok(None);
    };
    for index in 0..=last_start {
        if index % 4_096 == 0 && should_stop() {
            return Err(PdfError::Cancelled);
        }
        let end = index
            .checked_add(needle.len())
            .ok_or(PdfError::CMapLimitExceeded)?;
        if haystack.get(index..end) == Some(needle) {
            return Ok(Some(index));
        }
    }
    Ok(None)
}

fn parse_bfchar_block<F: Fn() -> bool>(
    block: &[u8],
    map: &mut FontMap,
    budget: &mut CmapBudget,
    should_stop: &F,
) -> Result<(), PdfError> {
    let mut tokens = CmapHexTokens::new(block);
    loop {
        let Some(source) = tokens.next(budget, should_stop)? else {
            return Ok(());
        };
        let Some(target) = tokens.next(budget, should_stop)? else {
            return Ok(());
        };
        if source.is_empty() {
            continue;
        }
        if source.len() > size_of::<u32>() {
            return Err(PdfError::CMapLimitExceeded);
        }
        if let Some(text) = decode_unicode_hex(&target) {
            budget.insert(map, source, text)?;
        }
    }
}

fn parse_bfrange_block<F: Fn() -> bool>(
    block: &[u8],
    map: &mut FontMap,
    budget: &mut CmapBudget,
    should_stop: &F,
) -> Result<(), PdfError> {
    let mut tokens = CmapHexTokens::new(block);
    loop {
        let Some(start_bytes) = tokens.next(budget, should_stop)? else {
            return Ok(());
        };
        let Some(end_bytes) = tokens.next(budget, should_stop)? else {
            return Ok(());
        };
        let Some(target) = tokens.next(budget, should_stop)? else {
            return Ok(());
        };
        if start_bytes.is_empty() || start_bytes.len() != end_bytes.len() || target.is_empty() {
            continue;
        }
        if start_bytes.len() > size_of::<u32>() {
            return Err(PdfError::CMapLimitExceeded);
        }
        let start = bytes_to_number(&start_bytes).ok_or(PdfError::CMapLimitExceeded)?;
        let end = bytes_to_number(&end_bytes).ok_or(PdfError::CMapLimitExceeded)?;
        if start > end {
            continue;
        }
        let range_entries = end
            .checked_sub(start)
            .and_then(|difference| difference.checked_add(1))
            .and_then(|count| usize::try_from(count).ok())
            .filter(|count| *count <= MAX_CMAP_RANGE_ENTRIES)
            .ok_or(PdfError::CMapLimitExceeded)?;
        let Some(mut unicode) = decode_unicode_scalar(&target) else {
            continue;
        };
        budget.ensure_entry_capacity(range_entries)?;
        for (index, code) in (start..=end).enumerate() {
            if index % 256 == 0 && should_stop() {
                return Err(PdfError::Cancelled);
            }
            let source =
                number_to_bytes(code, start_bytes.len()).ok_or(PdfError::CMapLimitExceeded)?;
            budget.insert(map, source, unicode.clone())?;
            unicode = next_unicode_scalar(&unicode);
        }
    }
}

fn decode_unicode_hex(bytes: &[u8]) -> Option<String> {
    if bytes.len() >= 2 && bytes.len() % 2 == 0 {
        let mut output = String::new();
        for pair in bytes.chunks_exact(2) {
            let code = u16::from_be_bytes([pair[0], pair[1]]);
            output.push(char::from_u32(u32::from(code))?);
        }
        Some(output)
    } else {
        String::from_utf8(bytes.to_vec()).ok()
    }
}

fn decode_unicode_scalar(bytes: &[u8]) -> Option<String> {
    if bytes.len() == 2 {
        char::from_u32(u32::from(u16::from_be_bytes([bytes[0], bytes[1]])))
            .map(|value| value.to_string())
    } else {
        decode_unicode_hex(bytes)
    }
}

fn next_unicode_scalar(value: &str) -> String {
    let Some(character) = value.chars().next() else {
        return value.to_owned();
    };
    char::from_u32(u32::from(character).saturating_add(1))
        .map(|next| next.to_string())
        .unwrap_or_else(|| value.to_owned())
}

fn bytes_to_number(bytes: &[u8]) -> Option<u32> {
    bytes.iter().try_fold(0_u32, |value, byte| {
        value.checked_mul(256)?.checked_add(u32::from(*byte))
    })
}

fn number_to_bytes(value: u32, width: usize) -> Option<Vec<u8>> {
    let bytes = value.to_be_bytes();
    let start = bytes.len().checked_sub(width)?;
    Some(bytes[start..].to_vec())
}

fn page_content_streams<F: Fn() -> bool>(
    objects: &BTreeMap<u32, PdfObject>,
    page_ref: PdfRef,
    should_stop: &F,
) -> Result<Vec<Vec<u8>>, PdfError> {
    let page = objects
        .get(&page_ref.object)
        .ok_or(PdfError::CorruptedPdf)?;
    let page_dict = dictionary(&page.value).ok_or(PdfError::CorruptedPdf)?;
    let Some(contents) = page_dict.get("Contents") else {
        return Ok(Vec::new());
    };
    let mut references = Vec::new();
    collect_content_references(objects, contents, &mut references, &mut HashSet::new())?;
    let mut streams = Vec::with_capacity(references.len());
    let mut decoded_bytes = 0usize;
    for reference in references {
        if should_stop() {
            return Err(PdfError::Cancelled);
        }
        let object = objects
            .get(&reference.object)
            .ok_or(PdfError::CorruptedPdf)?;
        let stream = decode_stream_with_control(object, should_stop)?;
        decoded_bytes = decoded_bytes
            .checked_add(stream.len())
            .filter(|total| *total <= MAX_STREAM_BYTES)
            .ok_or(PdfError::StreamLimitExceeded)?;
        streams.push(stream);
    }
    Ok(streams)
}

fn collect_content_references(
    objects: &BTreeMap<u32, PdfObject>,
    value: &PdfValue,
    output: &mut Vec<PdfRef>,
    visiting: &mut HashSet<u32>,
) -> Result<(), PdfError> {
    match value {
        PdfValue::Ref(reference) => {
            if visiting.len() >= MAX_VALUE_DEPTH || !visiting.insert(reference.object) {
                return Err(PdfError::CorruptedPdf);
            }
            let object = objects
                .get(&reference.object)
                .ok_or(PdfError::CorruptedPdf)?;
            let result = if object.stream.is_some() {
                if output.len() >= MAX_SELECTED_PAGES {
                    return Err(PdfError::CorruptedPdf);
                }
                output.push(*reference);
                Ok(())
            } else {
                collect_content_references(objects, &object.value, output, visiting)
            };
            visiting.remove(&reference.object);
            result?;
        }
        PdfValue::Array(values) => {
            for value in values {
                collect_content_references(objects, value, output, visiting)?;
            }
        }
        _ => return Err(PdfError::CorruptedPdf),
    }
    Ok(())
}

fn decode_stream_with_control<F: Fn() -> bool>(
    object: &PdfObject,
    should_stop: &F,
) -> Result<Vec<u8>, PdfError> {
    if should_stop() {
        return Err(PdfError::Cancelled);
    }
    let bytes = object.stream.as_ref().ok_or(PdfError::CorruptedPdf)?;
    if bytes.len() > MAX_STREAM_BYTES {
        return Err(PdfError::StreamLimitExceeded);
    }
    let Some(dict) = dictionary(&object.value) else {
        return Ok(bytes.clone());
    };
    let filters = match dict.get("Filter") {
        None => Vec::new(),
        Some(PdfValue::Name(name)) => vec![name.as_str()],
        Some(PdfValue::Array(values)) => values
            .iter()
            .map(|value| match value {
                PdfValue::Name(name) => Some(name.as_str()),
                _ => None,
            })
            .collect::<Option<Vec<_>>>()
            .ok_or(PdfError::CorruptedPdf)?,
        Some(_) => return Err(PdfError::CorruptedPdf),
    };

    let mut decoded = bytes.clone();
    for filter in filters {
        if should_stop() {
            return Err(PdfError::Cancelled);
        }
        decoded = match filter {
            "FlateDecode" | "Fl" => {
                decode_flate_with_control(&decoded, MAX_STREAM_BYTES, should_stop)?
            }
            "ASCIIHexDecode" | "AHx" => {
                decode_ascii_hex_with_control(&decoded, MAX_STREAM_BYTES, should_stop)?
            }
            other => return Err(PdfError::UnsupportedFilter(other.to_owned())),
        };
    }
    Ok(decoded)
}

#[cfg(test)]
fn decode_flate(bytes: &[u8], limit: usize) -> Result<Vec<u8>, PdfError> {
    decode_flate_with_control(bytes, limit, &|| false)
}

fn decode_flate_with_control<F: Fn() -> bool>(
    bytes: &[u8],
    limit: usize,
    should_stop: &F,
) -> Result<Vec<u8>, PdfError> {
    let mut decoder = ZlibDecoder::new(bytes);
    let mut output = Vec::with_capacity(limit.min(DECODE_BUFFER_BYTES));
    let mut buffer = [0_u8; DECODE_BUFFER_BYTES];
    loop {
        if should_stop() {
            return Err(PdfError::Cancelled);
        }
        let read = decoder
            .read(&mut buffer)
            .map_err(|_| PdfError::Decompression)?;
        if read == 0 {
            return Ok(output);
        }
        let next_length = output
            .len()
            .checked_add(read)
            .filter(|length| *length <= limit)
            .ok_or(PdfError::StreamLimitExceeded)?;
        output.reserve(next_length - output.len());
        output.extend_from_slice(&buffer[..read]);
    }
}

#[cfg(test)]
fn decode_ascii_hex(bytes: &[u8], limit: usize) -> Result<Vec<u8>, PdfError> {
    decode_ascii_hex_with_control(bytes, limit, &|| false)
}

fn decode_ascii_hex_with_control<F: Fn() -> bool>(
    bytes: &[u8],
    limit: usize,
    should_stop: &F,
) -> Result<Vec<u8>, PdfError> {
    let mut output = Vec::new();
    let mut high_nibble = None;
    for (index, &byte) in bytes.iter().enumerate() {
        if index % 4_096 == 0 && should_stop() {
            return Err(PdfError::Cancelled);
        }
        if byte == b'>' {
            if let Some(high) = high_nibble {
                if output.len() >= limit {
                    return Err(PdfError::StreamLimitExceeded);
                }
                output.push(high << 4);
            }
            return Ok(output);
        }
        if byte.is_ascii_whitespace() {
            continue;
        }
        let nibble = hex_nibble(byte).ok_or(PdfError::CorruptedPdf)?;
        if let Some(high) = high_nibble.take() {
            if output.len() >= limit {
                return Err(PdfError::StreamLimitExceeded);
            }
            output.push(high << 4 | nibble);
        } else {
            high_nibble = Some(nibble);
        }
    }
    Err(PdfError::CorruptedPdf)
}

#[derive(Debug, Default)]
struct ContentText {
    text: String,
    text_object_count: usize,
}

fn extract_page_content<F: Fn() -> bool>(
    objects: &BTreeMap<u32, PdfObject>,
    page_ref: PdfRef,
    should_stop: &F,
) -> Result<ContentText, PdfError> {
    let streams = page_content_streams(objects, page_ref, should_stop)?;
    let fonts = page_font_maps(objects, page_ref, should_stop)?;
    let resources = page_resources(objects, page_ref);
    let mut extractor = ContentExtractor {
        objects,
        should_stop,
        decoded_bytes: 0,
        form_invocations: 0,
        visiting: HashSet::new(),
    };
    let mut output = ContentText::default();
    for stream in streams {
        extractor.charge(stream.len())?;
        let extracted = extractor.extract_stream(&stream, &fonts, resources, 0)?;
        output.text_object_count += extracted.text_object_count;
        append_page_text(&mut output.text, &extracted.text);
    }
    output.text = output.text.trim_end().to_owned();
    Ok(output)
}

struct ContentExtractor<'a, F: Fn() -> bool> {
    objects: &'a BTreeMap<u32, PdfObject>,
    should_stop: &'a F,
    decoded_bytes: usize,
    form_invocations: usize,
    visiting: HashSet<u32>,
}

impl<'a, F: Fn() -> bool> ContentExtractor<'a, F> {
    fn charge(&mut self, bytes: usize) -> Result<(), PdfError> {
        self.decoded_bytes = self
            .decoded_bytes
            .checked_add(bytes)
            .filter(|total| *total <= MAX_STREAM_BYTES)
            .ok_or(PdfError::StreamLimitExceeded)?;
        Ok(())
    }

    fn extract_stream(
        &mut self,
        bytes: &[u8],
        fonts: &BTreeMap<String, FontMap>,
        resources: Option<&'a BTreeMap<String, PdfValue>>,
        depth: usize,
    ) -> Result<ContentText, PdfError> {
        let xobjects = resources
            .and_then(|dict| dict.get("XObject"))
            .and_then(|value| resolved_dictionary(self.objects, value));
        let should_stop = self.should_stop;
        extract_content_text(
            bytes,
            fonts,
            |name| self.extract_form(name, xobjects, resources, depth),
            should_stop,
        )
    }

    fn extract_form(
        &mut self,
        name: &str,
        xobjects: Option<&'a BTreeMap<String, PdfValue>>,
        surrounding: Option<&'a BTreeMap<String, PdfValue>>,
        depth: usize,
    ) -> Result<Option<ContentText>, PdfError> {
        let Some(value) = xobjects.and_then(|dict| dict.get(name)) else {
            return Ok(None);
        };
        let reference = reference_value(value).ok_or(PdfError::CorruptedPdf)?;
        let objects = self.objects;
        let object = objects
            .get(&reference.object)
            .ok_or(PdfError::CorruptedPdf)?;
        let dict = dictionary(&object.value).ok_or(PdfError::CorruptedPdf)?;
        if name_value(dict.get("Subtype")) != Some("Form") {
            return Ok(None);
        }
        if depth >= MAX_VALUE_DEPTH || !self.visiting.insert(reference.object) {
            return Err(PdfError::CorruptedPdf);
        }
        let result = (|| {
            self.form_invocations = self
                .form_invocations
                .checked_add(1)
                .filter(|count| *count <= MAX_FORM_INVOCATIONS)
                .ok_or(PdfError::StreamLimitExceeded)?;
            let stream = decode_stream_with_control(object, self.should_stop)?;
            self.charge(stream.len())?;
            let resources = match dict.get("Resources") {
                Some(value) => {
                    Some(resolved_dictionary(objects, value).ok_or(PdfError::CorruptedPdf)?)
                }
                None => surrounding,
            };
            let fonts = resources
                .map(|dict| font_maps_for_resources(objects, dict, self.should_stop))
                .transpose()?
                .unwrap_or_default();
            self.extract_stream(&stream, &fonts, resources, depth + 1)
        })();
        self.visiting.remove(&reference.object);
        result.map(Some)
    }
}

fn extract_content_text(
    bytes: &[u8],
    font_maps: &BTreeMap<String, FontMap>,
    mut on_form: impl FnMut(&str) -> Result<Option<ContentText>, PdfError>,
    should_stop: impl Fn() -> bool,
) -> Result<ContentText, PdfError> {
    let mut tokenizer = Tokenizer::new(bytes);
    let mut operands = Vec::new();
    let mut output = String::new();
    let mut in_text = false;
    let mut text_object_count = 0;
    let mut current_y = None;
    let mut last_text_y = None;
    let mut current_font = None;
    let mut token_count = 0usize;
    while let Some(token) = tokenizer.next() {
        token_count += 1;
        if token_count % 1024 == 1 && should_stop() {
            return Err(PdfError::Cancelled);
        }
        match token {
            Token::Keyword(operator) => {
                match operator.as_str() {
                    "BT" => {
                        in_text = true;
                        current_y = None;
                    }
                    "ET" => in_text = false,
                    "Tf" if in_text => {
                        current_font =
                            operands
                                .get(operands.len().saturating_sub(2))
                                .and_then(|value| match value {
                                    PdfValue::Name(name) => Some(name.clone()),
                                    _ => None,
                                });
                    }
                    "Tj" if in_text => {
                        if let Some(value) = operands.last() {
                            if let Some(text) = string_value(
                                value,
                                current_font.as_deref().and_then(|name| font_maps.get(name)),
                            ) {
                                append_text(&mut output, &text, current_y, &mut last_text_y);
                                text_object_count += 1;
                            }
                        }
                    }
                    "TJ" if in_text => {
                        if let Some(PdfValue::Array(values)) = operands.last() {
                            // Strings in one TJ operation are fragments of the same
                            // text-showing operation. Append them as one unit so
                            // append_text cannot invent spaces at every fragment
                            // boundary (one string per character is common for
                            // kerning and glyph positioning).
                            let mut combined = String::new();
                            let mut string_count = 0;
                            for value in values {
                                if let Some(text) = string_value(
                                    value,
                                    current_font.as_deref().and_then(|name| font_maps.get(name)),
                                ) {
                                    combined.push_str(&text);
                                    string_count += 1;
                                }
                            }
                            if !combined.is_empty() {
                                append_text(&mut output, &combined, current_y, &mut last_text_y);
                                text_object_count += string_count;
                            }
                        }
                    }
                    "'" if in_text => {
                        ensure_line_break(&mut output);
                        if let Some(value) = operands.last() {
                            if let Some(text) = string_value(
                                value,
                                current_font.as_deref().and_then(|name| font_maps.get(name)),
                            ) {
                                append_text(&mut output, &text, current_y, &mut last_text_y);
                                text_object_count += 1;
                            }
                        }
                    }
                    "\"" if in_text => {
                        ensure_line_break(&mut output);
                        if let Some(value) = operands.last() {
                            if let Some(text) = string_value(
                                value,
                                current_font.as_deref().and_then(|name| font_maps.get(name)),
                            ) {
                                append_text(&mut output, &text, current_y, &mut last_text_y);
                                text_object_count += 1;
                            }
                        }
                    }
                    "T*" if in_text => ensure_line_break(&mut output),
                    "Td" | "TD" if in_text => {
                        if let (Some(PdfValue::Number(x)), Some(PdfValue::Number(y))) = (
                            operands.get(operands.len().saturating_sub(2)),
                            operands.last(),
                        ) {
                            apply_text_move(&mut output, &mut current_y, *x, *y);
                        }
                    }
                    "Tm" if in_text => {
                        if let Some(PdfValue::Number(y)) = operands.last() {
                            let y_changed = current_y
                                .map(|previous| (previous - y).abs() > 0.5)
                                .unwrap_or(false);
                            if y_changed {
                                ensure_line_break(&mut output);
                            }
                            current_y = Some(*y);
                        }
                    }
                    "Do" if !in_text => {
                        if let Some(PdfValue::Name(name)) = operands.last() {
                            if let Some(form) = on_form(name)? {
                                if output
                                    .len()
                                    .checked_add(form.text.len())
                                    .is_none_or(|length| length > MAX_STREAM_BYTES)
                                {
                                    return Err(PdfError::StreamLimitExceeded);
                                }
                                append_page_text(&mut output, &form.text);
                                if !form.text.is_empty() {
                                    ensure_line_break(&mut output);
                                }
                                text_object_count += form.text_object_count;
                            }
                        }
                    }
                    _ => {}
                }
                operands.clear();
            }
            token => {
                if token == Token::StartArray {
                    operands.push(parse_content_array(&mut tokenizer)?);
                } else if let Some(value) = token_to_value(token) {
                    operands.push(value);
                }
            }
        }
    }
    Ok(ContentText {
        text: output.trim_end().to_owned(),
        text_object_count,
    })
}

fn parse_content_array(tokenizer: &mut Tokenizer<'_>) -> Result<PdfValue, PdfError> {
    let mut values = Vec::new();
    while let Some(token) = tokenizer.next() {
        match token {
            Token::EndArray => return Ok(PdfValue::Array(values)),
            Token::StartArray => values.push(parse_content_array(tokenizer)?),
            other => {
                if let Some(value) = token_to_value(other) {
                    values.push(value);
                }
            }
        }
    }
    Err(PdfError::CorruptedPdf)
}

fn token_to_value(token: Token) -> Option<PdfValue> {
    match token {
        Token::Number(value) => Some(PdfValue::Number(value)),
        Token::Name(value) => Some(PdfValue::Name(value)),
        Token::LiteralString(value) => Some(PdfValue::String(value)),
        Token::HexString(value) => Some(PdfValue::HexString(value)),
        Token::StartArray => None,
        Token::EndArray | Token::StartDictionary | Token::EndDictionary => None,
        Token::Keyword(value) => match value.as_str() {
            "null" => Some(PdfValue::Null),
            "true" => Some(PdfValue::Bool(true)),
            "false" => Some(PdfValue::Bool(false)),
            _ => Some(PdfValue::Keyword(value)),
        },
    }
}

fn string_value(value: &PdfValue, font_map: Option<&FontMap>) -> Option<String> {
    match value {
        PdfValue::String(bytes) | PdfValue::HexString(bytes) => {
            Some(font_map.map_or_else(|| decode_text(bytes), |map| decode_font_text(bytes, map)))
        }
        _ => None,
    }
}

fn decode_font_text(bytes: &[u8], map: &FontMap) -> String {
    if map.code_width == 0 || bytes.len() < map.code_width {
        return decode_text(bytes);
    }
    let mut output = String::new();
    for chunk in bytes.chunks(map.code_width) {
        if chunk.len() != map.code_width {
            output.push_str(&decode_text(chunk));
            continue;
        }
        if let Some(value) = map.values.get(chunk) {
            output.push_str(value);
        } else {
            output.push_str(&decode_text(chunk));
        }
    }
    output
}

fn decode_text(bytes: &[u8]) -> String {
    if bytes.starts_with(&[0xfe, 0xff]) && bytes.len() >= 2 {
        let mut output = String::new();
        for pair in bytes[2..].chunks_exact(2) {
            let code = u16::from_be_bytes([pair[0], pair[1]]);
            output.push(char::from_u32(code as u32).unwrap_or('\u{fffd}'));
        }
        return output;
    }
    bytes
        .iter()
        .copied()
        .filter(|byte| *byte != 0)
        .map(|byte| match byte {
            0x80 => '€',
            0x82 => '‚',
            0x83 => 'ƒ',
            0x84 => '„',
            0x85 => '…',
            0x86 => '†',
            0x87 => '‡',
            0x88 => 'ˆ',
            0x89 => '‰',
            0x8a => 'Š',
            0x8b => '‹',
            0x8c => 'Œ',
            0x8e => 'Ž',
            0x91 => '‘',
            0x92 => '’',
            0x93 => '“',
            0x94 => '”',
            0x95 => '•',
            0x96 => '–',
            0x97 => '—',
            0x98 => '˜',
            0x99 => '™',
            0x9a => 'š',
            0x9b => '›',
            0x9c => 'œ',
            0x9e => 'ž',
            0x9f => 'Ÿ',
            value => value as char,
        })
        .collect()
}

fn append_text(
    output: &mut String,
    text: &str,
    current_y: Option<f64>,
    last_text_y: &mut Option<f64>,
) {
    if text.is_empty() {
        return;
    }
    if let (Some(previous), Some(current)) = (*last_text_y, current_y) {
        if (previous - current).abs() > 0.5 {
            ensure_line_break(output);
        }
    }
    // A PDF producer may split one word over several Tj operations. The
    // operation boundary itself is not a whitespace signal; explicit spaces
    // in the string and line-position operators are the only safe signals we
    // preserve here. This avoids turning kerning and glyph positioning into
    // visible spaces (for example, "Dumm" + "y").
    output.push_str(text);
    *last_text_y = current_y;
}

fn ensure_line_break(output: &mut String) {
    while output.ends_with(' ') {
        output.pop();
    }
    if !output.is_empty() && !output.ends_with('\n') {
        output.push('\n');
    }
}

fn apply_text_move(output: &mut String, current_y: &mut Option<f64>, x: f64, y: f64) {
    if current_y.is_some() && y.abs() > EPSILON {
        ensure_line_break(output);
    }
    // `Td` moves the text line matrix, not the end of the last glyph. A
    // positive x therefore cannot be treated as an automatic word separator:
    // doing so breaks PDFs that position each glyph/chunk with Td. Keep the
    // parameter in the signature because the caller still parses both values,
    // and deliberately ignore horizontal movement until a font-metric based
    // layout model is available.
    let _ = x;
    // Td/TD move from the current text line matrix; repeated -13 moves
    // reach different baselines even though the operands are identical.
    *current_y = Some(current_y.unwrap_or(0.0) + y);
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    use std::io::Write;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TestFile(std::path::PathBuf);

    impl TestFile {
        fn new(label: &str, bytes: &[u8]) -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "minimalpdf-pdf-{label}-{}-{}.pdf",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::write(&path, bytes).expect("write PDF test file");
            Self(path)
        }
    }

    impl Drop for TestFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn pdf_with_stream(stream: &[u8], filter: Option<&str>, pages: usize) -> Vec<u8> {
        let mut output = b"%PDF-1.7\n".to_vec();
        let page_count = pages.max(1);
        output.extend_from_slice(b"1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj\n");
        output.extend_from_slice(
            format!("2 0 obj << /Type /Pages /Kids [3 0 R] /Count {page_count} >> endobj\n")
                .as_bytes(),
        );
        output
            .extend_from_slice(b"3 0 obj << /Type /Page /Parent 2 0 R /Contents 4 0 R >> endobj\n");
        let filter_entry = filter
            .map(|value| format!(" /Filter /{value}"))
            .unwrap_or_default();
        output.extend_from_slice(
            format!(
                "4 0 obj << /Length {}{} >>\nstream\n",
                stream.len(),
                filter_entry
            )
            .as_bytes(),
        );
        output.extend_from_slice(stream);
        output.extend_from_slice(b"\nendstream\nendobj\ntrailer << /Root 1 0 R >>\n%%EOF\n");
        output
    }

    #[test]
    fn bounded_pages_only_extraction_rejects_total_text_over_budget() {
        let bytes = pdf_with_stream(b"BT (bounded text) Tj ET", None, 1);
        let input = TestFile::new("text-budget", &bytes);
        let result = extract_pages_from_path_bounded_with_control(
            &input.0,
            &PageSelection::All,
            bytes.len() as u64,
            3,
            || false,
        );
        assert!(matches!(
            result,
            Err(BoundedPageExtractionError::TextLimitExceeded)
        ));
    }

    #[test]
    fn path_reader_checks_cancellation_between_chunks() {
        use std::cell::Cell;

        let input = TestFile::new("read-cancel", &vec![0_u8; FILE_READ_BUFFER_BYTES * 2]);
        let checks = Cell::new(0_usize);
        let result = read_path_with_control(&input.0, None, &|| {
            checks.set(checks.get() + 1);
            checks.get() >= 3
        });
        assert!(matches!(
            result,
            Err(ControlledReadError::Pdf(PdfError::Cancelled))
        ));
    }

    fn pdf_with_extra_object(object: &[u8]) -> Vec<u8> {
        let bytes = pdf_with_stream(b"BT (valid) Tj ET", None, 1);
        let trailer = b"trailer << /Root 1 0 R >>\n%%EOF\n";
        let mut output = bytes[..bytes.len() - trailer.len()].to_vec();
        output.extend_from_slice(object);
        output.extend_from_slice(trailer);
        output
    }

    fn form_stream(content: &[u8], resources: &str) -> Vec<u8> {
        let mut object = format!(
            "<< /Type /XObject /Subtype /Form /BBox [0 0 612 792] /Resources {resources} /Length {} >>\nstream\n",
            content.len()
        )
        .into_bytes();
        object.extend_from_slice(content);
        object.extend_from_slice(b"\nendstream");
        object
    }

    fn pdf_with_form_objects(
        content: &[u8],
        resources: &str,
        extra_objects: &[(u32, Vec<u8>)],
    ) -> Vec<u8> {
        let mut objects = BTreeMap::from([
            (1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
            (2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec()),
            (
                3,
                format!("<< /Type /Page /Parent 2 0 R /Resources {resources} /Contents 4 0 R >>")
                    .into_bytes(),
            ),
            (4, {
                let mut stream = format!("<< /Length {} >>\nstream\n", content.len()).into_bytes();
                stream.extend_from_slice(content);
                stream.extend_from_slice(b"\nendstream");
                stream
            }),
        ]);
        objects.extend(extra_objects.iter().cloned());
        let mut pdf = b"%PDF-1.7\n".to_vec();
        let mut offsets = Vec::new();
        for number in 1..=objects.len() as u32 {
            let object = objects.get(&number).expect("sequential fixture IDs");
            offsets.push(pdf.len());
            pdf.extend_from_slice(format!("{number} 0 obj\n").as_bytes());
            pdf.extend_from_slice(object);
            pdf.extend_from_slice(b"\nendobj\n");
        }
        let xref = pdf.len();
        pdf.extend_from_slice(
            format!("xref\n0 {}\n0000000000 65535 f \n", offsets.len() + 1).as_bytes(),
        );
        for offset in offsets {
            pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(
            format!(
                "trailer << /Root 1 0 R /Size {} >>\nstartxref\n{xref}\n%%EOF\n",
                objects.len() + 1
            )
            .as_bytes(),
        );
        pdf
    }

    #[test]
    fn extracts_form_text_between_page_text_fragments() {
        let pdf = pdf_with_form_objects(
            b"BT (before) Tj ET /F0 Do BT (after) Tj ET",
            "<< /XObject << /F0 5 0 R >> >>",
            &[(5, form_stream(b"BT (inside) Tj ET", "<< >>"))],
        );
        let result = extract_text(&pdf, &PageSelection::All).expect("form text");
        assert_eq!(result.text, "before\ninside\nafter");
        assert_eq!(result.text_object_count, 3);
    }

    #[test]
    fn extracts_chinese_using_form_local_to_unicode_font() {
        let cmap = b"1 begincodespacerange <0000> <FFFF> endcodespacerange 2 beginbfchar <0001> <4E2D> <0002> <6587> endbfchar";
        let pdf = pdf_with_form_objects(
            b"/F0 Do",
            "<< /XObject << /F0 5 0 R >> >>",
            &[
                (
                    5,
                    form_stream(
                        b"BT /T 12 Tf <00010002> Tj ET",
                        "<< /Font << /T 6 0 R >> >>",
                    ),
                ),
                (
                    6,
                    b"<< /Type /Font /Subtype /Type0 /Encoding /Identity-H /ToUnicode 7 0 R >>"
                        .to_vec(),
                ),
                (7, {
                    let mut stream = format!("<< /Length {} >>\nstream\n", cmap.len()).into_bytes();
                    stream.extend_from_slice(cmap);
                    stream.extend_from_slice(b"\nendstream");
                    stream
                }),
            ],
        );
        assert_eq!(
            extract_text(&pdf, &PageSelection::All)
                .expect("form-local CMap")
                .text,
            "中文"
        );
    }

    #[test]
    fn cmap_limit_error_is_not_silently_downgraded_to_fallback_text() {
        let cmap = b"1 beginbfrange <00000000> <FFFFFFFF> <0041> endbfrange";
        let pdf = pdf_with_form_objects(
            b"/F0 Do",
            "<< /XObject << /F0 5 0 R >> >>",
            &[
                (
                    5,
                    form_stream(
                        b"BT /T 12 Tf <00000000> Tj ET",
                        "<< /Font << /T 6 0 R >> >>",
                    ),
                ),
                (
                    6,
                    b"<< /Type /Font /Subtype /Type0 /Encoding /Identity-H /ToUnicode 7 0 R >>"
                        .to_vec(),
                ),
                (7, {
                    let mut stream = format!("<< /Length {} >>\nstream\n", cmap.len()).into_bytes();
                    stream.extend_from_slice(cmap);
                    stream.extend_from_slice(b"\nendstream");
                    stream
                }),
            ],
        );
        assert!(matches!(
            extract_text(&pdf, &PageSelection::All),
            Err(PdfError::CMapLimitExceeded)
        ));
    }

    #[test]
    fn nested_form_can_be_reused_without_false_cycle_detection() {
        let pdf = pdf_with_form_objects(
            b"/Outer Do /Outer Do",
            "<< /XObject << /Outer 5 0 R >> >>",
            &[
                (
                    5,
                    form_stream(b"/Inner Do", "<< /XObject << /Inner 6 0 R >> >>"),
                ),
                (6, form_stream(b"BT (nested) Tj ET", "<< >>")),
            ],
        );
        let result = extract_text(&pdf, &PageSelection::All).expect("reused form");
        assert_eq!(result.text, "nested\nnested");
        assert_eq!(result.text_object_count, 2);
    }

    #[test]
    fn rejects_cyclic_form_xobjects() {
        let pdf = pdf_with_form_objects(
            b"/Loop Do",
            "<< /XObject << /Loop 5 0 R >> >>",
            &[(
                5,
                form_stream(b"/Loop Do", "<< /XObject << /Loop 5 0 R >> >>"),
            )],
        );
        assert!(matches!(
            extract_text(&pdf, &PageSelection::All),
            Err(PdfError::CorruptedPdf)
        ));
    }

    #[test]
    fn rejects_form_chain_over_depth_limit() {
        let mut forms = Vec::new();
        for number in 5..=(5 + MAX_VALUE_DEPTH as u32) {
            let next = number + 1;
            let content = if next <= 5 + MAX_VALUE_DEPTH as u32 {
                b"/Next Do".as_slice()
            } else {
                b"BT (deep) Tj ET".as_slice()
            };
            forms.push((
                number,
                form_stream(content, &format!("<< /XObject << /Next {next} 0 R >> >>")),
            ));
        }
        let pdf = pdf_with_form_objects(b"/Next Do", "<< /XObject << /Next 5 0 R >> >>", &forms);
        assert!(matches!(
            extract_text(&pdf, &PageSelection::All),
            Err(PdfError::CorruptedPdf)
        ));
    }

    #[test]
    fn cancellation_is_checked_inside_form_text_streams() {
        use std::cell::Cell;

        let pdf = pdf_with_form_objects(
            b"/F0 Do",
            "<< /XObject << /F0 5 0 R >> >>",
            &[(5, form_stream(b"BT (inside) Tj ET", "<< >>"))],
        );
        let checks = Cell::new(0usize);
        let result = extract_text_with_control(&pdf, &PageSelection::All, || {
            checks.set(checks.get() + 1);
            checks.get() >= 3
        });
        assert!(matches!(result, Err(PdfError::Cancelled)));
    }

    #[test]
    fn ignores_non_form_xobjects_without_hiding_page_text() {
        let pdf = pdf_with_form_objects(
            b"/Im0 Do BT (visible) Tj ET",
            "<< /XObject << /Im0 5 0 R >> >>",
            &[(5, b"<< /Type /XObject /Subtype /Image >>".to_vec())],
        );
        assert_eq!(
            extract_text(&pdf, &PageSelection::All)
                .expect("text beside image")
                .text,
            "visible"
        );
    }

    fn pdf_with_indirect_length(stream: &[u8], declared_length: usize) -> Vec<u8> {
        let mut output = b"%PDF-1.7\n".to_vec();
        let mut offsets = Vec::new();
        offsets.push(output.len());
        output.extend_from_slice(b"1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj\n");
        offsets.push(output.len());
        output.extend_from_slice(b"2 0 obj << /Type /Pages /Kids [3 0 R] /Count 1 >> endobj\n");
        offsets.push(output.len());
        output
            .extend_from_slice(b"3 0 obj << /Type /Page /Parent 2 0 R /Contents 4 0 R >> endobj\n");
        offsets.push(output.len());
        output.extend_from_slice(b"4 0 obj << /Length 5 0 R >>\nstream\n");
        output.extend_from_slice(stream);
        output.extend_from_slice(b"\nendstream\nendobj\n");
        offsets.push(output.len());
        output.extend_from_slice(format!("5 0 obj\n{declared_length}\nendobj\n").as_bytes());
        let xref_offset = output.len();
        output.extend_from_slice(b"xref\n0 6\n0000000000 65535 f \n");
        for offset in offsets {
            output.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        output.extend_from_slice(
            format!("trailer << /Root 1 0 R /Size 6 >>\nstartxref\n{xref_offset}\n%%EOF\n")
                .as_bytes(),
        );
        output
    }

    fn modern_pdf_with_xref(
        content: &[u8],
        extra_xref_fields: &str,
        change: impl FnOnce(&mut [[u8; 7]; 8]),
    ) -> Vec<u8> {
        let catalog = b"<< /Type /Catalog /Pages 2 0 R >>";
        let pages = b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>";
        let page = b"<< /Type /Page /Parent 2 0 R /Contents 4 0 R /MediaBox [0 0 612 792] >>";
        let header = format!(
            "1 0 2 {} 3 {} ",
            catalog.len() + 1,
            catalog.len() + pages.len() + 2
        );
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(header.as_bytes()).expect("object header");
        encoder.write_all(catalog).expect("catalog");
        encoder.write_all(b" ").expect("separator");
        encoder.write_all(pages).expect("pages");
        encoder.write_all(b" ").expect("separator");
        encoder.write_all(page).expect("page");
        let compressed_objects = encoder.finish().expect("object stream");

        let mut bytes = b"%PDF-1.7\n".to_vec();
        let mut offsets = [0usize; 8];
        offsets[4] = bytes.len();
        bytes.extend_from_slice(b"4 0 obj << /Length 7 0 R >>\nstream\n");
        bytes.extend_from_slice(content);
        bytes.extend_from_slice(b"\nendstream\nendobj\n");
        offsets[5] = bytes.len();
        bytes.extend_from_slice(
            format!(
                "5 0 obj << /Type /ObjStm /N 3 /First {} /Filter /FlateDecode /Length {} >>\nstream\n",
                header.len(),
                compressed_objects.len()
            )
            .as_bytes(),
        );
        bytes.extend_from_slice(&compressed_objects);
        bytes.extend_from_slice(b"\nendstream\nendobj\n");
        offsets[7] = bytes.len();
        bytes.extend_from_slice(format!("7 0 obj {} endobj\n", content.len()).as_bytes());
        offsets[6] = bytes.len();

        let mut rows = [[0u8; 7]; 8];
        rows[0][5..].copy_from_slice(&u16::MAX.to_be_bytes());
        for (index, row) in rows.iter_mut().enumerate().take(4).skip(1) {
            row[0] = 2;
            row[1..5].copy_from_slice(&5u32.to_be_bytes());
            row[5..].copy_from_slice(
                &u16::try_from(index - 1)
                    .expect("fixture index")
                    .to_be_bytes(),
            );
        }
        for index in [4, 5, 6, 7] {
            rows[index][0] = 1;
            rows[index][1..5].copy_from_slice(
                &u32::try_from(offsets[index])
                    .expect("small fixture offset")
                    .to_be_bytes(),
            );
        }
        change(&mut rows);
        let raw_xref = rows.into_iter().flatten().collect::<Vec<_>>();
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&raw_xref).expect("xref data");
        let compressed_xref = encoder.finish().expect("xref stream");
        bytes.extend_from_slice(
            format!(
                "6 0 obj << /Type /XRef /Size 8 /W [1 4 2] /Root 1 0 R{extra_xref_fields} /Filter /FlateDecode /Length {} >>\nstream\n",
                compressed_xref.len()
            )
            .as_bytes(),
        );
        bytes.extend_from_slice(&compressed_xref);
        bytes.extend_from_slice(
            format!("\nendstream\nendobj\nstartxref\n{}\n%%EOF\n", offsets[6]).as_bytes(),
        );
        bytes
    }

    fn append_modern_revision(
        mut bytes: Vec<u8>,
        content: &[u8],
        xref_number: u32,
        free_content: bool,
        previous_override: Option<usize>,
    ) -> Vec<u8> {
        let previous = match verify_xref_structure(&bytes).expect("previous xref") {
            XrefLocation::Stream(offset) | XrefLocation::Table(offset) => offset,
            XrefLocation::Missing => panic!("fixture requires an xref"),
        };
        let content_offset = bytes.len();
        if !free_content {
            assert_eq!(content.len(), b"BT (indexed) Tj ET".len());
            bytes.extend_from_slice(b"4 0 obj << /Length 7 0 R >>\nstream\n");
            bytes.extend_from_slice(content);
            bytes.extend_from_slice(b"\nendstream\nendobj\n");
        }
        let xref_offset = bytes.len();
        let mut rows = [[0u8; 7]; 2];
        if free_content {
            rows[0][5..].copy_from_slice(&1u16.to_be_bytes());
        } else {
            rows[0][0] = 1;
            rows[0][1..5].copy_from_slice(
                &u32::try_from(content_offset)
                    .expect("fixture content offset")
                    .to_be_bytes(),
            );
        }
        rows[1][0] = 1;
        rows[1][1..5].copy_from_slice(
            &u32::try_from(xref_offset)
                .expect("fixture xref offset")
                .to_be_bytes(),
        );
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder
            .write_all(&rows.into_iter().flatten().collect::<Vec<_>>())
            .expect("xref rows");
        let stream = encoder.finish().expect("compress xref");
        bytes.extend_from_slice(
            format!(
                "{xref_number} 0 obj << /Type /XRef /Root 1 0 R /Size {} /W [1 4 2] /Index [4 1 {xref_number} 1] /Prev {} /Filter /FlateDecode /Length {} >>\nstream\n",
                xref_number + 1,
                previous_override.unwrap_or(previous),
                stream.len()
            )
            .as_bytes(),
        );
        bytes.extend_from_slice(&stream);
        bytes.extend_from_slice(
            format!("\nendstream\nendobj\nstartxref\n{xref_offset}\n%%EOF\n").as_bytes(),
        );
        bytes
    }

    fn append_traditional_revision(
        mut bytes: Vec<u8>,
        content: Option<&[u8]>,
        extra_trailer: &str,
        previous_override: Option<usize>,
    ) -> Vec<u8> {
        let previous = match verify_xref_structure(&bytes).expect("previous xref") {
            XrefLocation::Table(offset) | XrefLocation::Stream(offset) => offset,
            XrefLocation::Missing => panic!("fixture requires an xref"),
        };
        let entry = if let Some(content) = content {
            let object_offset = bytes.len();
            bytes.extend_from_slice(b"4 0 obj << /Length 5 0 R >>\nstream\n");
            bytes.extend_from_slice(content);
            bytes.extend_from_slice(b"\nendstream\nendobj\n");
            format!("{object_offset:010} 00000 n")
        } else {
            "0000000000 00001 f".to_owned()
        };
        let offset = bytes.len();
        bytes.extend_from_slice(
            format!(
                "xref\n4 1\n{entry} \ntrailer << /Size 6 /Root 1 0 R /Prev {}{extra_trailer} >>\nstartxref\n{offset}\n%%EOF\n",
                previous_override.unwrap_or(previous)
            )
            .as_bytes(),
        );
        bytes
    }

    #[test]
    fn extracts_latest_content_across_traditional_xref_revisions() {
        let original = pdf_with_indirect_length(b"BT (before) Tj ET", 17);
        let first = append_traditional_revision(original, Some(b"BT (middle) Tj ET"), "", None);
        let latest = append_traditional_revision(first, Some(b"BT (newest) Tj ET"), "", None);
        let result =
            extract_text(&latest, &PageSelection::All).expect("latest traditional revision");
        assert_eq!(result.text, "newest");
        assert_eq!(result.page_count, 1);
    }

    #[test]
    fn traditional_xref_free_entry_masks_older_content() {
        let original = pdf_with_indirect_length(b"BT (before) Tj ET", 17);
        let updated = append_traditional_revision(original, Some(b"BT (middle) Tj ET"), "", None);
        let deleted = append_traditional_revision(updated, None, "", None);
        assert!(matches!(
            extract_text(&deleted, &PageSelection::All),
            Err(PdfError::CorruptedPdf)
        ));
    }

    #[test]
    fn traditional_xref_ignores_unindexed_object_redefinition() {
        let original = pdf_with_indirect_length(b"BT (before) Tj ET", 17);
        let mut updated =
            append_traditional_revision(original, Some(b"BT (newest) Tj ET"), "", None);
        updated.extend_from_slice(b"4 0 obj << /Length 0 >> stream\n\nendstream endobj\n");
        let result = extract_text(&updated, &PageSelection::All).expect("indexed revision only");
        assert_eq!(result.text, "newest");
    }

    #[test]
    fn traditional_xref_rejects_wrong_generation_in_latest_entry() {
        let original = pdf_with_indirect_length(b"BT (before) Tj ET", 17);
        let mut updated =
            append_traditional_revision(original, Some(b"BT (newest) Tj ET"), "", None);
        let offset = match verify_xref_structure(&updated).expect("latest table") {
            XrefLocation::Table(offset) => offset,
            _ => panic!("expected traditional table"),
        };
        let row = offset + b"xref\n4 1\n".len();
        updated[row + 11..row + 16].copy_from_slice(b"00001");
        assert!(matches!(
            extract_text(&updated, &PageSelection::All),
            Err(PdfError::CorruptedPdf)
        ));
    }

    #[test]
    fn traditional_xref_rejects_duplicate_entries_in_one_revision() {
        let original = pdf_with_indirect_length(b"BT (before) Tj ET", 17);
        let mut updated =
            append_traditional_revision(original, Some(b"BT (newest) Tj ET"), "", None);
        let marker = b"trailer << /Size 6";
        let at = updated
            .windows(marker.len())
            .rposition(|part| part == marker)
            .expect("latest trailer");
        updated.splice(at..at, b"4 1\n0000000000 00001 f \n".iter().copied());
        assert!(matches!(
            extract_text(&updated, &PageSelection::All),
            Err(PdfError::CorruptedPdf)
        ));
    }

    #[test]
    fn traditional_xref_rejects_invalid_previous_offset() {
        let original = pdf_with_indirect_length(b"BT (before) Tj ET", 17);
        let updated =
            append_traditional_revision(original, Some(b"BT (newest) Tj ET"), "", Some(0));
        assert!(matches!(
            extract_text(&updated, &PageSelection::All),
            Err(PdfError::CorruptedPdf)
        ));
    }

    #[test]
    fn traditional_xref_requires_a_root_in_latest_trailer() {
        let original = pdf_with_indirect_length(b"BT (before) Tj ET", 17);
        let mut updated =
            append_traditional_revision(original, Some(b"BT (newest) Tj ET"), "", None);
        let marker = b"/Root 1 0 R";
        let at = updated
            .windows(marker.len())
            .rposition(|part| part == marker)
            .expect("latest root");
        updated[at + 1..at + 5].copy_from_slice(b"Nope");
        assert!(matches!(
            extract_text(&updated, &PageSelection::All),
            Err(PdfError::CorruptedPdf)
        ));
    }

    #[test]
    fn traditional_xref_rejects_hybrid_and_mixed_revisions() {
        let original = pdf_with_indirect_length(b"BT (before) Tj ET", 17);
        let hybrid = append_traditional_revision(
            original.clone(),
            Some(b"BT (newest) Tj ET"),
            " /XRefStm 9",
            None,
        );
        assert!(matches!(
            extract_text(&hybrid, &PageSelection::All),
            Err(PdfError::UnsupportedStructure)
        ));
        let previous_stream = modern_pdf_with_xref(b"BT (indexed) Tj ET", "", |_| {});
        let mixed = append_traditional_revision(previous_stream, None, "", None);
        assert!(matches!(
            extract_text(&mixed, &PageSelection::All),
            Err(PdfError::UnsupportedStructure)
        ));
    }

    #[test]
    fn traditional_xref_rejects_chain_over_revision_limit() {
        let mut bytes = pdf_with_indirect_length(b"BT (before) Tj ET", 17);
        for _ in 0..MAX_XREF_REVISIONS {
            bytes = append_traditional_revision(bytes, Some(b"BT (newest) Tj ET"), "", None);
        }
        assert!(matches!(
            extract_text(&bytes, &PageSelection::All),
            Err(PdfError::UnsupportedStructure)
        ));
    }

    #[test]
    fn extracts_compressed_page_objects_from_xref_stream() {
        let bytes = modern_pdf_with_xref(b"BT (indexed) Tj ET", "", |_| {});
        let result = extract_text(&bytes, &PageSelection::All).expect("extract modern PDF");
        assert_eq!(result.text, "indexed");
        assert_eq!(
            (result.pages[0].width_points, result.pages[0].height_points),
            (612, 792)
        );
    }

    #[test]
    fn ignores_unindexed_object_headers_in_page_content() {
        let bytes = modern_pdf_with_xref(
            b"BT (1 0 obj << /Type /Catalog >> endobj) Tj ET",
            "",
            |_| {},
        );
        let result = extract_text(&bytes, &PageSelection::All).expect("indexed page");
        assert_eq!(result.text, "1 0 obj << /Type /Catalog >> endobj");
    }

    #[test]
    fn rejects_mismatched_compressed_object_index() {
        let bytes = modern_pdf_with_xref(b"BT (indexed) Tj ET", "", |rows| {
            rows[3][6] = 1;
        });
        assert!(matches!(
            extract_text(&bytes, &PageSelection::All),
            Err(PdfError::CorruptedPdf)
        ));
    }

    #[test]
    fn rejects_mismatched_direct_object_offset() {
        let bytes = modern_pdf_with_xref(b"BT (indexed) Tj ET", "", |rows| {
            rows[4][1..5].copy_from_slice(&1u32.to_be_bytes());
        });
        assert!(matches!(
            extract_text(&bytes, &PageSelection::All),
            Err(PdfError::CorruptedPdf)
        ));
    }

    #[test]
    fn rejects_stale_generation_references_in_indexed_objects() {
        let objects = BTreeMap::from([(
            1,
            PdfObject {
                value: PdfValue::Ref(PdfRef {
                    object: 4,
                    generation: 1,
                }),
                stream: None,
            },
        )]);
        let entries = BTreeMap::from([(
            4,
            XrefEntry::Direct {
                offset: 10,
                generation: 0,
            },
        )]);
        assert!(matches!(
            validate_indexed_references(&objects, &entries),
            Err(PdfError::CorruptedPdf)
        ));
    }

    #[test]
    fn rejects_prev_pointer_to_non_xref_object() {
        let bytes = modern_pdf_with_xref(b"BT (indexed) Tj ET", " /Prev 9", |_| {});
        assert!(matches!(
            extract_text(&bytes, &PageSelection::All),
            Err(PdfError::UnsupportedStructure)
        ));
    }

    #[test]
    fn extracts_latest_content_across_multiple_xref_stream_revisions() {
        let original = modern_pdf_with_xref(b"BT (indexed) Tj ET", "", |_| {});
        let first = append_modern_revision(original, b"BT (revised) Tj ET", 8, false, None);
        let latest = append_modern_revision(first, b"BT (current) Tj ET", 9, false, None);
        let result = extract_text(&latest, &PageSelection::All).expect("latest revision");
        assert_eq!(result.text, "current");
        assert_eq!(result.page_count, 1);
    }

    #[test]
    fn free_entry_in_new_revision_does_not_resurrect_old_content() {
        let original = modern_pdf_with_xref(b"BT (indexed) Tj ET", "", |_| {});
        let deleted = append_modern_revision(original, b"", 8, true, None);
        assert!(matches!(
            extract_text(&deleted, &PageSelection::All),
            Err(PdfError::CorruptedPdf)
        ));
    }

    #[test]
    fn rejects_invalid_previous_xref_offset() {
        let original = modern_pdf_with_xref(b"BT (indexed) Tj ET", "", |_| {});
        let bytes = append_modern_revision(original, b"BT (updated) Tj ET", 8, false, Some(0));
        assert!(matches!(
            extract_text(&bytes, &PageSelection::All),
            Err(PdfError::CorruptedPdf)
        ));
    }

    #[test]
    fn rejects_latest_xref_stream_without_a_root() {
        let original = modern_pdf_with_xref(b"BT (indexed) Tj ET", "", |_| {});
        let mut bytes = append_modern_revision(original, b"BT (updated) Tj ET", 8, false, None);
        let marker = b"/Root 1 0 R";
        let position = bytes
            .windows(marker.len())
            .rposition(|part| part == marker)
            .expect("newest root");
        bytes[position + 1..position + 5].copy_from_slice(b"Nope");
        assert!(matches!(
            extract_text(&bytes, &PageSelection::All),
            Err(PdfError::CorruptedPdf)
        ));
    }

    #[test]
    fn rejects_xref_stream_revision_pointing_to_a_traditional_table() {
        let mut original = modern_pdf_with_xref(b"BT (indexed) Tj ET", "", |_| {});
        let table_offset = original.len();
        original.extend_from_slice(
            format!(
                "xref\n0 1\n0000000000 65535 f \ntrailer << /Size 8 /Root 1 0 R >>\nstartxref\n{table_offset}\n%%EOF\n"
            )
            .as_bytes(),
        );
        let bytes = append_modern_revision(original, b"BT (updated) Tj ET", 8, false, None);
        assert!(matches!(
            extract_text(&bytes, &PageSelection::All),
            Err(PdfError::UnsupportedStructure)
        ));
    }

    #[test]
    fn rejects_xref_stream_chain_over_revision_limit() {
        let mut bytes = modern_pdf_with_xref(b"BT (indexed) Tj ET", "", |_| {});
        for number in 8..(8 + MAX_XREF_REVISIONS as u32) {
            bytes = append_modern_revision(bytes, b"BT (updated) Tj ET", number, false, None);
        }
        assert!(matches!(
            extract_text(&bytes, &PageSelection::All),
            Err(PdfError::UnsupportedStructure)
        ));
    }

    #[test]
    fn rejects_unknown_xref_entry_type() {
        let bytes = modern_pdf_with_xref(b"BT (indexed) Tj ET", "", |rows| {
            rows[3][0] = 3;
        });
        assert!(matches!(
            extract_text(&bytes, &PageSelection::All),
            Err(PdfError::UnsupportedStructure)
        ));
    }

    #[test]
    fn rejects_xref_widths_outside_bounded_reader() {
        let mut bytes = modern_pdf_with_xref(b"BT (indexed) Tj ET", "", |_| {});
        let marker = b"/W [1 4 2]";
        let at = bytes
            .windows(marker.len())
            .position(|part| part == marker)
            .expect("xref widths");
        bytes[at + 4] = b'9';
        assert!(matches!(
            extract_text(&bytes, &PageSelection::All),
            Err(PdfError::CorruptedPdf)
        ));
    }

    #[test]
    fn rejects_excessively_nested_pdf_values() {
        let mut bytes = b"%PDF-1.7\n1 0 obj ".to_vec();
        bytes.extend(std::iter::repeat_n(b'[', MAX_VALUE_DEPTH + 1));
        bytes.extend_from_slice(b"null");
        bytes.extend(std::iter::repeat_n(b']', MAX_VALUE_DEPTH + 1));
        bytes.extend_from_slice(b" endobj\n%%EOF\n");
        assert!(matches!(
            extract_text(&bytes, &PageSelection::All),
            Err(PdfError::CorruptedPdf)
        ));
    }

    #[test]
    fn rejects_out_of_range_object_numbers_without_aliasing_a_real_object() {
        let bytes = b"%PDF-1.7\n4294967297 0 obj << /Type /Catalog >> endobj\n%%EOF\n";
        assert!(matches!(
            extract_text(bytes, &PageSelection::All),
            Err(PdfError::CorruptedPdf)
        ));
        let mut reference = Tokenizer::new(b"4294967297 0 R");
        assert!(matches!(
            parse_value(&mut reference),
            Ok(PdfValue::Number(4_294_967_297.0))
        ));
    }

    #[test]
    fn rejects_page_tree_cycles_and_excessive_depth() {
        let cyclic = b"%PDF-1.7\n1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj\n2 0 obj << /Type /Pages /Kids [2 0 R] >> endobj\n%%EOF\n";
        assert!(matches!(
            extract_text(cyclic, &PageSelection::All),
            Err(PdfError::CorruptedPdf)
        ));

        let mut deep = b"%PDF-1.7\n1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj\n".to_vec();
        for number in 2..=70 {
            deep.extend_from_slice(
                format!(
                    "{number} 0 obj << /Type /Pages /Kids [{} 0 R] >> endobj\n",
                    number + 1
                )
                .as_bytes(),
            );
        }
        deep.extend_from_slice(b"71 0 obj << /Type /Page >> endobj\n%%EOF\n");
        assert!(matches!(
            extract_text(&deep, &PageSelection::All),
            Err(PdfError::CorruptedPdf)
        ));
    }

    #[test]
    fn rejects_cyclic_and_deep_content_references() {
        let mut cyclic = pdf_with_extra_object(b"5 0 obj 6 0 R endobj\n6 0 obj 5 0 R endobj\n");
        let marker = b"/Contents 4 0 R";
        let position = cyclic
            .windows(marker.len())
            .position(|part| part == marker)
            .expect("page contents");
        cyclic[position + b"/Contents ".len()] = b'5';
        assert!(matches!(
            extract_text(&cyclic, &PageSelection::All),
            Err(PdfError::CorruptedPdf)
        ));

        let mut deep = pdf_with_stream(b"BT (valid) Tj ET", None, 1);
        let position = deep
            .windows(marker.len())
            .position(|part| part == marker)
            .expect("page contents");
        deep[position + b"/Contents ".len()] = b'5';
        let trailer = b"trailer << /Root 1 0 R >>\n%%EOF\n";
        deep.truncate(deep.len() - trailer.len());
        for number in 5..=70 {
            deep.extend_from_slice(
                format!("{number} 0 obj {} 0 R endobj\n", number + 1).as_bytes(),
            );
        }
        deep.extend_from_slice(b"71 0 obj 4 0 R endobj\n");
        deep.extend_from_slice(trailer);
        assert!(matches!(
            extract_text(&deep, &PageSelection::All),
            Err(PdfError::CorruptedPdf)
        ));
    }

    #[test]
    fn reads_indirect_stream_length_from_matching_xref_entry() {
        let stream = b"BT (before endstream after) Tj ET";
        let bytes = pdf_with_indirect_length(stream, stream.len());
        let result = extract_text(&bytes, &PageSelection::All).expect("extract indirect stream");
        assert_eq!(result.text, "before endstream after");
    }

    #[test]
    fn rejects_indirect_length_without_a_verifiable_xref() {
        let bytes = b"%PDF-1.7\n1 0 obj << /Length 2 0 R >>\nstream\n(endstream)\nendstream\nendobj\n2 0 obj 11 endobj\n%%EOF\n";
        assert!(matches!(
            extract_text(bytes, &PageSelection::All),
            Err(PdfError::UnsupportedStreamLength)
        ));
    }

    #[test]
    fn rejects_indirect_length_that_extends_beyond_stream() {
        let bytes = pdf_with_indirect_length(b"BT (short) Tj ET", 100_000);
        assert!(matches!(
            extract_text(&bytes, &PageSelection::All),
            Err(PdfError::CorruptedPdf)
        ));
    }

    #[test]
    fn rejects_indirect_length_when_xref_target_has_wrong_object_number() {
        let mut bytes = pdf_with_indirect_length(b"BT (text) Tj ET", 15);
        let header = b"5 0 obj\n15\nendobj";
        let offset = bytes
            .windows(header.len())
            .position(|window| window == header)
            .expect("length object header");
        bytes[offset] = b'6';
        assert!(matches!(
            extract_text(&bytes, &PageSelection::All),
            Err(PdfError::CorruptedPdf)
        ));
    }

    #[test]
    fn rejects_streams_with_missing_or_excessive_length() {
        let missing =
            b"%PDF-1.7\n1 0 obj << >>\nstream\nBT (test) Tj ET\nendstream\nendobj\n%%EOF\n";
        assert!(matches!(
            extract_text(missing, &PageSelection::All),
            Err(PdfError::CorruptedPdf)
        ));
        let excessive = format!(
            "%PDF-1.7\n1 0 obj << /Length {} >>\nstream\nendstream\nendobj\n%%EOF\n",
            MAX_STREAM_BYTES + 1
        );
        assert!(matches!(
            extract_text(excessive.as_bytes(), &PageSelection::All),
            Err(PdfError::StreamLimitExceeded)
        ));
    }

    #[test]
    fn flate_decode_enforces_limit_without_full_expansion() {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(b"five!!").expect("compress fixture");
        let encoded = encoder.finish().expect("finish fixture");
        assert!(matches!(
            decode_flate(&encoded, 5),
            Err(PdfError::StreamLimitExceeded)
        ));
        assert_eq!(decode_flate(&encoded, 6).expect("exact limit"), b"five!!");
    }

    #[test]
    fn flate_decode_checks_cancellation_between_output_chunks() {
        use std::cell::Cell;

        let expanded = vec![0_u8; DECODE_BUFFER_BYTES * 4];
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&expanded).expect("compress fixture");
        let encoded = encoder.finish().expect("finish fixture");
        let checks = Cell::new(0_usize);
        let result = decode_flate_with_control(&encoded, expanded.len(), &|| {
            checks.set(checks.get() + 1);
            checks.get() >= 3
        });
        assert!(matches!(result, Err(PdfError::Cancelled)));
        assert!(checks.get() >= 3);
    }

    #[test]
    fn ascii_hex_decode_enforces_limit_including_odd_nibble() {
        assert_eq!(decode_ascii_hex(b"4142>", 2).expect("exact limit"), b"AB");
        assert!(matches!(
            decode_ascii_hex(b"414243>", 2),
            Err(PdfError::StreamLimitExceeded)
        ));
        assert!(matches!(
            decode_ascii_hex(b"41424>", 2),
            Err(PdfError::StreamLimitExceeded)
        ));
    }

    #[test]
    fn rejects_direct_object_stream_even_with_a_valid_page() {
        let bytes = pdf_with_extra_object(
            b"5 0 obj << /Type /ObjStm /N 0 /First 0 /Length 0 >>\nstream\n\nendstream\nendobj\n",
        );
        assert!(matches!(
            extract_text(&bytes, &PageSelection::All),
            Err(PdfError::UnsupportedStructure)
        ));
    }

    #[test]
    fn rejects_direct_xref_stream_even_with_a_valid_page() {
        let bytes = pdf_with_extra_object(
            b"5 0 obj << /Type /XRef /Size 6 /W [1 1 1] /Length 0 >>\nstream\n\nendstream\nendobj\n",
        );
        assert!(matches!(
            extract_text(&bytes, &PageSelection::All),
            Err(PdfError::UnsupportedStructure)
        ));
    }

    #[test]
    fn rejects_hybrid_xref_marker_in_trailer() {
        let bytes = b"%PDF-1.5\n1 0 obj << /Type /Catalog >> endobj\ntrailer << /Root 1 0 R /XRefStm 42 >>\n%%EOF\n";
        assert!(matches!(
            extract_text(bytes, &PageSelection::All),
            Err(PdfError::UnsupportedStructure)
        ));
    }

    #[test]
    fn rejects_prev_marker_without_a_verifiable_xref_table() {
        let bytes = b"%PDF-1.5\n1 0 obj << /Type /Catalog >> endobj\ntrailer << /Root 1 0 R /Prev 9 >>\n%%EOF\n";
        assert!(matches!(
            extract_text(bytes, &PageSelection::All),
            Err(PdfError::UnsupportedStructure)
        ));
    }

    #[test]
    fn rejects_startxref_pointing_to_an_indirect_object() {
        let bytes = b"%PDF-1.5\n1 0 obj << /Type /Catalog >> endobj\nstartxref\n9\n%%EOF\n";
        assert!(matches!(
            extract_text(bytes, &PageSelection::All),
            Err(PdfError::UnsupportedStructure)
        ));
    }

    #[test]
    fn still_extracts_text_with_a_traditional_xref_table() {
        let mut bytes = pdf_with_stream(b"BT (classic) Tj ET", None, 1);
        let trailer = b"trailer << /Root 1 0 R >>\n%%EOF\n";
        bytes.truncate(bytes.len() - trailer.len());
        let xref_offset = bytes.len();
        bytes.extend_from_slice(
            format!("xref\n0 1\n0000000000 65535 f \ntrailer << /Root 1 0 R >>\nstartxref\n{xref_offset}\n%%EOF\n")
                .as_bytes(),
        );
        let result = extract_text(&bytes, &PageSelection::All).expect("extract classic PDF");
        assert_eq!(result.text, "classic");
    }

    #[test]
    fn ignores_object_stream_markers_inside_page_text() {
        let bytes = pdf_with_stream(b"BT (5 0 obj /Type /ObjStm endobj) Tj ET", None, 1);
        let result = extract_text(&bytes, &PageSelection::All).expect("extract page text");
        assert_eq!(result.text, "5 0 obj /Type /ObjStm endobj");
    }

    #[test]
    fn extracts_text_and_line_breaks() {
        let bytes = pdf_with_stream(
            b"BT /F1 12 Tf 72 720 Td (Hello) Tj T* (world) Tj ET",
            None,
            1,
        );
        let result = extract_text(&bytes, &PageSelection::All).expect("extract text");
        assert_eq!(result.text, "Hello\nworld");
        assert_eq!(result.page_count, 1);
        assert_eq!(result.text_object_count, 2);
    }

    #[test]
    fn preserves_every_line_after_repeated_relative_text_moves() {
        let bytes = pdf_with_stream(
            b"BT 54 748 Td (first) Tj 0 -13 Td (second) Tj 0 -13 Td (third) Tj ET",
            None,
            1,
        );
        let result = extract_text(&bytes, &PageSelection::All).expect("extract text");
        assert_eq!(result.pages[0].text, "first\nsecond\nthird");
    }

    #[test]
    fn separates_text_objects_on_different_baselines_but_joins_same_line_fragments() {
        let bytes = pdf_with_stream(
            b"BT 54 748 Td (first) Tj ET BT 54 735 Td (second) Tj ET BT 54 735 Td ( and) Tj ET BT 54 722 Td (third) Tj ET",
            None,
            1,
        );
        let result = extract_text(&bytes, &PageSelection::All).expect("extract text");
        assert_eq!(result.pages[0].text, "first\nsecond and\nthird");

        let bytes = pdf_with_stream(
            b"BT 1 0 0 1 54 748 Tm (first) Tj ET BT 1 0 0 1 54 735 Tm (second) Tj ET",
            None,
            1,
        );
        let result = extract_text(&bytes, &PageSelection::All).expect("extract Tm text");
        assert_eq!(result.pages[0].text, "first\nsecond");

        let bytes = pdf_with_stream(
            b"BT 1 0 0 1 54 748 Tm (first) Tj ET BT 1 0 0 1 54 735 Tm ET BT 1 0 0 1 54 748 Tm (same) Tj ET",
            None,
            1,
        );
        let result = extract_text(&bytes, &PageSelection::All).expect("ignore empty text object");
        assert_eq!(result.pages[0].text, "firstsame");
    }

    #[test]
    fn extracts_flate_streams() {
        let mut compressed = ZlibEncoder::new(Vec::new(), Compression::default());
        compressed
            .write_all(b"BT (compressed) Tj ET")
            .expect("compress fixture");
        let compressed = compressed.finish().expect("finish fixture");
        let bytes = pdf_with_stream(&compressed, Some("FlateDecode"), 1);
        let result = extract_text(&bytes, &PageSelection::All).expect("extract text");
        assert_eq!(result.text, "compressed");
    }

    #[test]
    fn extracts_text_arrays() {
        let bytes = pdf_with_stream(b"BT [(Hello ) 120 (world)] TJ ET", None, 1);
        let result = extract_text(&bytes, &PageSelection::All).expect("extract text");
        assert_eq!(result.text, "Hello world");
    }

    #[test]
    fn does_not_insert_spaces_between_tj_fragments() {
        let bytes = pdf_with_stream(b"BT [(H) (e) (l) (l) (o)] TJ ET", None, 1);
        let result = extract_text(&bytes, &PageSelection::All).expect("extract text");
        assert_eq!(result.text, "Hello");
    }

    #[test]
    fn keeps_adjacent_cjk_text_showing_operations_together() {
        let bytes = pdf_with_stream(b"BT <FEFF4F60> Tj <FEFF597D> Tj ET", None, 1);
        let result = extract_text(&bytes, &PageSelection::All).expect("extract text");
        assert_eq!(result.text, "你好");
    }

    #[test]
    fn keeps_positioned_word_fragments_together_and_preserves_explicit_spaces() {
        let bytes = pdf_with_stream(
            b"BT 56.8 758.1 Td (Dumm) Tj 50.1 0 Td (y) Tj 9 0 Td ( ) Tj 4.4 0 Td (PDF) Tj 32.2 0 Td ( fi) Tj 14.3 0 Td (le) Tj ET",
            None,
            1,
        );
        let result = extract_text(&bytes, &PageSelection::All).expect("extract text");
        assert_eq!(result.text, "Dummy PDF file");
    }

    #[test]
    fn rejects_unterminated_ascii_hex_streams() {
        let bytes = pdf_with_stream(b"4254202848692920546A204554", Some("ASCIIHexDecode"), 1);
        assert!(matches!(
            extract_text(&bytes, &PageSelection::All),
            Err(PdfError::CorruptedPdf)
        ));
    }

    #[test]
    fn decodes_utf16be_strings() {
        let bytes = pdf_with_stream(b"BT <FEFF4F60597D> Tj ET", None, 1);
        let result = extract_text(&bytes, &PageSelection::All).expect("extract text");
        assert_eq!(result.text, "你好");
    }

    #[test]
    fn rejects_invalid_page_ranges() {
        assert!(matches!(
            PageSelection::parse(Some("All")),
            Ok(PageSelection::All)
        ));
        assert!(matches!(
            PageSelection::parse(Some("all")),
            Ok(PageSelection::All)
        ));
        assert!(PageSelection::parse(Some("1-3,8")).is_ok());
        assert!(PageSelection::parse(Some("3-1")).is_err());
        assert!(PageSelection::parse(Some("0,2")).is_err());
        assert!(PageSelection::parse(Some("1-10001")).is_err());
    }

    #[test]
    fn rejects_pages_outside_document() {
        let bytes = pdf_with_stream(b"BT (one page) Tj ET", None, 1);
        let selection = PageSelection::parse(Some("2")).expect("parse selection");
        assert!(matches!(
            extract_text(&bytes, &selection),
            Err(PdfError::InvalidPageRange)
        ));
    }

    #[test]
    fn reports_missing_text_layer() {
        let bytes = pdf_with_stream(b"q 0 0 10 10 re f Q", None, 1);
        assert!(matches!(
            extract_text(&bytes, &PageSelection::All),
            Err(PdfError::NoTextLayer)
        ));
    }

    #[test]
    fn inherits_page_size_from_parent_node() {
        let mut objects = BTreeMap::new();
        objects.insert(
            1,
            PdfObject {
                value: PdfValue::Dict(BTreeMap::from([
                    ("Type".to_owned(), PdfValue::Name("Page".to_owned())),
                    (
                        "Parent".to_owned(),
                        PdfValue::Ref(PdfRef {
                            object: 2,
                            generation: 0,
                        }),
                    ),
                ])),
                stream: None,
            },
        );
        objects.insert(
            2,
            PdfObject {
                value: PdfValue::Dict(BTreeMap::from([(
                    "MediaBox".to_owned(),
                    PdfValue::Array(vec![
                        PdfValue::Number(0.0),
                        PdfValue::Number(0.0),
                        PdfValue::Number(600.0),
                        PdfValue::Number(800.0),
                    ]),
                )])),
                stream: None,
            },
        );
        assert_eq!(
            page_size(
                &objects,
                PdfRef {
                    object: 1,
                    generation: 0
                }
            ),
            (600, 800)
        );
    }

    #[test]
    fn decodes_to_unicode_cmap_for_custom_font_codes() {
        let cmap = br#"
            1 begincodespacerange <00> <FF> endcodespacerange
            2 beginbfchar
            <01> <4F60>
            <02> <597D>
            endbfchar
        "#;
        let map = parse_to_unicode_cmap(cmap, &|| false)
            .expect("parse CMap")
            .expect("non-empty CMap");
        assert_eq!(decode_font_text(&[1, 2], &map), "你好");
    }

    #[test]
    fn cmap_rejects_huge_bfrange_before_expansion() {
        let cmap = b"1 beginbfrange <00000000> <FFFFFFFF> <0041> endbfrange";
        assert!(matches!(
            parse_to_unicode_cmap(cmap, &|| false),
            Err(PdfError::CMapLimitExceeded)
        ));
    }

    #[test]
    fn cmap_accepts_exact_range_limit_and_rejects_one_more() {
        let last_allowed = MAX_CMAP_RANGE_ENTRIES - 1;
        let allowed = format!("1 beginbfrange <0000> <{last_allowed:04X}> <0041> endbfrange");
        let map = parse_to_unicode_cmap(allowed.as_bytes(), &|| false)
            .expect("range at limit")
            .expect("non-empty range");
        assert_eq!(map.values.len(), MAX_CMAP_RANGE_ENTRIES);

        let first_rejected = MAX_CMAP_RANGE_ENTRIES;
        let rejected = format!("1 beginbfrange <0000> <{first_rejected:04X}> <0041> endbfrange");
        assert!(matches!(
            parse_to_unicode_cmap(rejected.as_bytes(), &|| false),
            Err(PdfError::CMapLimitExceeded)
        ));
    }

    #[test]
    fn cmap_rejects_total_entries_across_individually_allowed_ranges() {
        let mut cmap = String::from("beginbfrange\n");
        let range_count = MAX_CMAP_ENTRIES / MAX_CMAP_RANGE_ENTRIES;
        for range in 0..range_count {
            let start = range * MAX_CMAP_RANGE_ENTRIES;
            let end = start + MAX_CMAP_RANGE_ENTRIES - 1;
            cmap.push_str(&format!("<{start:08X}> <{end:08X}> <0041>\n"));
        }
        cmap.push_str("<00010000> <00010000> <0041>\nendbfrange");
        assert!(matches!(
            parse_to_unicode_cmap(cmap.as_bytes(), &|| false),
            Err(PdfError::CMapLimitExceeded)
        ));
    }

    #[test]
    fn cmap_mapping_byte_budget_accepts_exact_limit_and_rejects_overflow() {
        let mut map = FontMap::default();
        let mut budget = CmapBudget {
            mapping_bytes: MAX_CMAP_MAPPING_BYTES - 2,
            ..CmapBudget::default()
        };
        budget
            .insert(&mut map, vec![1], "A".to_owned())
            .expect("exact mapping byte limit");
        assert!(matches!(
            budget.insert(&mut map, vec![2], "B".to_owned()),
            Err(PdfError::CMapLimitExceeded)
        ));
    }

    #[test]
    fn cmap_parser_propagates_cancellation_during_range_expansion() {
        use std::cell::Cell;

        let last_allowed = MAX_CMAP_RANGE_ENTRIES - 1;
        let cmap = format!("1 beginbfrange <0000> <{last_allowed:04X}> <0041> endbfrange");
        let checks = Cell::new(0_usize);
        let result = parse_to_unicode_cmap(cmap.as_bytes(), &|| {
            checks.set(checks.get() + 1);
            checks.get() >= 8
        });
        assert!(matches!(result, Err(PdfError::Cancelled)));
        assert!(checks.get() >= 8);
    }
}
