//! Bounded, local-only rendering for a deliberately small OOXML presentation subset.
//! Unsupported visual features fail instead of producing an incomplete PDF.

use flate2::read::DeflateDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use quick_xml::events::{BytesStart, Event};
use quick_xml::name::ResolveResult;
use quick_xml::reader::NsReader;
use quick_xml::XmlVersion;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Cursor, Read, Seek, SeekFrom, Write};
use std::path::Path;
use thiserror::Error;

const MAX_INPUT_BYTES: u64 = 250 * 1024 * 1024;
const MAX_OUTPUT_BYTES: u64 = 768 * 1024 * 1024;
const MAX_UNCOMPRESSED_BYTES: u64 = 512 * 1024 * 1024;
const MAX_IMAGE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_XML_BYTES: u64 = 4 * 1024 * 1024;
const MAX_CENTRAL_BYTES: u64 = 4 * 1024 * 1024;
const MAX_ENTRIES: usize = 2_048;
const MAX_SLIDES: usize = 200;
const MAX_PAGE_OBJECTS: usize = 64;
const MAX_PIXELS: u64 = 40_000_000;
const MAX_DECODED_BYTES: usize = 160 * 1024 * 1024;
const MAX_PDF_SIDE_EMU: u64 = 14_400 * 12_700;
const P: &[u8] = b"http://schemas.openxmlformats.org/presentationml/2006/main";
const A: &[u8] = b"http://schemas.openxmlformats.org/drawingml/2006/main";
const R: &[u8] = b"http://schemas.openxmlformats.org/package/2006/relationships";

/// Errors returned by the restricted native presentation reader.
#[derive(Debug, Error)]
pub enum PresentationPdfError {
    #[error("Legacy binary PPT is not supported; save it as PPTX and retry")]
    UnsupportedLegacyPpt,
    #[error("Only local regular PPTX files are accepted; symlinks are not allowed")]
    InvalidInput,
    #[error("PDF output must be an absolute path in an existing directory")]
    InvalidOutputPath,
    #[error("The PPTX ZIP or OOXML structure is invalid")]
    InvalidPackage,
    #[error("The PPTX contains layouts, objects, macros, or external references unsupported by this engine")]
    UnsupportedContent,
    #[error("The PPTX image encoding is invalid or unsupported")]
    UnsupportedImage,
    #[error("The PPTX or generated PDF exceeds a page, extraction, pixel, or file-size limit")]
    LimitExceeded,
    #[error("Presentation conversion cancelled")]
    Cancelled,
    #[error("Presentation read/write failed: {0}")]
    Io(#[from] io::Error),
}

/// Converts supported PPTX slides into equally sized PDF pages. The returned
/// count is the number of pages written. `.ppt` files are never misread as ZIP.
///
/// # Errors
/// Invalid, unsupported, oversized, cancelled or inaccessible inputs fail
/// without publishing a partial output. An existing output is never replaced.
pub fn write_presentation_pdf(
    input: &Path,
    output: &Path,
    should_stop: impl Fn() -> bool,
) -> Result<usize, PresentationPdfError> {
    if !output.is_absolute() || !output.parent().is_some_and(Path::is_dir) {
        return Err(PresentationPdfError::InvalidOutputPath);
    }
    if !input.is_absolute() || !fs::symlink_metadata(input)?.file_type().is_file() {
        return Err(PresentationPdfError::InvalidInput);
    }
    let mut archive = Archive::open(input, &should_stop)?;
    let deck = read_deck(&mut archive, &should_stop)?;
    if should_stop() {
        return Err(PresentationPdfError::Cancelled);
    }
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)?;
    let result = write_document(file, &mut archive, &deck, &should_stop);
    if result.is_err() {
        let _ = fs::remove_file(output);
    }
    result.map(|()| deck.slides.len())
}

#[derive(Clone)]
struct ZipEntry {
    offset: u64,
    compressed: u64,
    uncompressed: u64,
    crc: u32,
    method: u16,
    flags: u16,
}

struct Archive {
    file: File,
    entries: BTreeMap<String, ZipEntry>,
    central_start: u64,
}

fn invalid() -> PresentationPdfError {
    PresentationPdfError::InvalidPackage
}

fn u16_at(bytes: &[u8], offset: usize) -> Result<u16, PresentationPdfError> {
    let raw = bytes
        .get(offset..offset.checked_add(2).ok_or_else(invalid)?)
        .ok_or_else(invalid)?;
    Ok(u16::from_le_bytes([raw[0], raw[1]]))
}

fn u32_at(bytes: &[u8], offset: usize) -> Result<u32, PresentationPdfError> {
    let raw = bytes
        .get(offset..offset.checked_add(4).ok_or_else(invalid)?)
        .ok_or_else(invalid)?;
    Ok(u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]))
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb88320 & 0u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

impl Archive {
    fn open(path: &Path, should_stop: &impl Fn() -> bool) -> Result<Self, PresentationPdfError> {
        let mut file = File::open(path)?;
        let size = file.metadata()?.len();
        if size < 22 {
            return Err(PresentationPdfError::InvalidInput);
        }
        if size > MAX_INPUT_BYTES {
            return Err(PresentationPdfError::LimitExceeded);
        }
        let mut signature = [0u8; 8];
        file.read_exact(&mut signature)?;
        if signature == [0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1]
            || path
                .extension()
                .and_then(|x| x.to_str())
                .is_some_and(|x| x.eq_ignore_ascii_case("ppt"))
        {
            return Err(PresentationPdfError::UnsupportedLegacyPpt);
        }
        if !path
            .extension()
            .and_then(|x| x.to_str())
            .is_some_and(|x| x.eq_ignore_ascii_case("pptx"))
            || &signature[..4] != b"PK\x03\x04"
        {
            return Err(PresentationPdfError::InvalidInput);
        }
        if should_stop() {
            return Err(PresentationPdfError::Cancelled);
        }
        let tail_size = size.min(65_535 + 22);
        file.seek(SeekFrom::End(
            -i64::try_from(tail_size).map_err(|_| invalid())?,
        ))?;
        let mut tail = vec![0u8; usize::try_from(tail_size).map_err(|_| invalid())?];
        file.read_exact(&mut tail)?;
        let end = tail
            .windows(4)
            .rposition(|window| {
                window == b"PK\x05\x06" && window.as_ptr() as usize >= tail.as_ptr() as usize
            })
            .ok_or_else(invalid)?;
        if end
            .checked_add(22)
            .and_then(|n| n.checked_add(usize::from(u16_at(&tail, end + 20).ok()?)))
            != Some(tail.len())
            || u16_at(&tail, end + 4)? != 0
            || u16_at(&tail, end + 6)? != 0
            || u16_at(&tail, end + 8)? != u16_at(&tail, end + 10)?
        {
            return Err(invalid());
        }
        let count = usize::from(u16_at(&tail, end + 10)?);
        if count == 0 || count > MAX_ENTRIES {
            return Err(PresentationPdfError::LimitExceeded);
        }
        let central_size = u64::from(u32_at(&tail, end + 12)?);
        let central_start = u64::from(u32_at(&tail, end + 16)?);
        if central_size > MAX_CENTRAL_BYTES
            || central_size == u64::from(u32::MAX)
            || central_start == u64::from(u32::MAX)
            || central_start.checked_add(central_size)
                != Some(size - tail_size + u64::try_from(end).map_err(|_| invalid())?)
        {
            return Err(invalid());
        }
        file.seek(SeekFrom::Start(central_start))?;
        let mut central = vec![0u8; usize::try_from(central_size).map_err(|_| invalid())?];
        file.read_exact(&mut central)?;
        let mut cursor = 0usize;
        let mut entries = BTreeMap::new();
        let mut total_uncompressed = 0u64;
        for _ in 0..count {
            if should_stop() {
                return Err(PresentationPdfError::Cancelled);
            }
            if u32_at(&central, cursor)? != 0x02014b50 {
                return Err(invalid());
            }
            let flags = u16_at(&central, cursor + 8)?;
            let method = u16_at(&central, cursor + 10)?;
            if flags & !(0x0008 | 0x0800) != 0 {
                return Err(PresentationPdfError::UnsupportedContent);
            }
            if !matches!(method, 0 | 8) {
                return Err(PresentationPdfError::UnsupportedContent);
            }
            let compressed = u64::from(u32_at(&central, cursor + 20)?);
            let uncompressed = u64::from(u32_at(&central, cursor + 24)?);
            if compressed == u64::from(u32::MAX)
                || uncompressed == u64::from(u32::MAX)
                || compressed > MAX_INPUT_BYTES
            {
                return Err(PresentationPdfError::LimitExceeded);
            }
            total_uncompressed = total_uncompressed
                .checked_add(uncompressed)
                .filter(|&x| x <= MAX_UNCOMPRESSED_BYTES)
                .ok_or(PresentationPdfError::LimitExceeded)?;
            let name_len = usize::from(u16_at(&central, cursor + 28)?);
            let extra_len = usize::from(u16_at(&central, cursor + 30)?);
            let comment_len = usize::from(u16_at(&central, cursor + 32)?);
            if u16_at(&central, cursor + 34)? != 0 {
                return Err(PresentationPdfError::UnsupportedContent);
            }
            let entry_end = cursor
                .checked_add(46)
                .and_then(|n| n.checked_add(name_len))
                .and_then(|n| n.checked_add(extra_len))
                .and_then(|n| n.checked_add(comment_len))
                .ok_or_else(invalid)?;
            if entry_end > central.len() {
                return Err(invalid());
            }
            let name = std::str::from_utf8(&central[cursor + 46..cursor + 46 + name_len])
                .map_err(|_| invalid())?;
            if name.is_empty()
                || name.starts_with('/')
                || name.contains('\\')
                || name.contains(':')
                || name
                    .split('/')
                    .any(|component| component.is_empty() || component == "." || component == "..")
            {
                return Err(PresentationPdfError::UnsupportedContent);
            }
            if name.to_ascii_lowercase().contains("vbaproject")
                || name.to_ascii_lowercase().ends_with(".bin")
                || name.to_ascii_lowercase().ends_with(".rels") && uncompressed > MAX_XML_BYTES
            {
                return Err(PresentationPdfError::UnsupportedContent);
            }
            let offset = u64::from(u32_at(&central, cursor + 42)?);
            if offset >= central_start || u32_at(&central, cursor + 38)? >> 16 & 0xf000 == 0xa000 {
                return Err(invalid());
            }
            let entry = ZipEntry {
                offset,
                compressed,
                uncompressed,
                crc: u32_at(&central, cursor + 16)?,
                method,
                flags,
            };
            if entries.insert(name.to_owned(), entry).is_some() {
                return Err(invalid());
            }
            cursor = entry_end;
        }
        if cursor != central.len() {
            return Err(invalid());
        }
        Ok(Self {
            file,
            entries,
            central_start,
        })
    }

    fn read(
        &mut self,
        name: &str,
        limit: u64,
        should_stop: &impl Fn() -> bool,
    ) -> Result<Vec<u8>, PresentationPdfError> {
        if should_stop() {
            return Err(PresentationPdfError::Cancelled);
        }
        let entry = self.entries.get(name).ok_or_else(invalid)?.clone();
        if entry.uncompressed > limit {
            return Err(PresentationPdfError::LimitExceeded);
        }
        self.file.seek(SeekFrom::Start(entry.offset))?;
        let mut header = [0u8; 30];
        self.file.read_exact(&mut header)?;
        if u32_at(&header, 0)? != 0x04034b50
            || u16_at(&header, 6)? != entry.flags
            || u16_at(&header, 8)? != entry.method
        {
            return Err(invalid());
        }
        let name_len = usize::from(u16_at(&header, 26)?);
        let extra_len = usize::from(u16_at(&header, 28)?);
        if name_len != name.len() || name_len > 4096 || extra_len > 4096 {
            return Err(invalid());
        }
        let data_start = entry
            .offset
            .checked_add(30)
            .and_then(|n| n.checked_add(u64::try_from(name_len + extra_len).ok()?))
            .ok_or_else(invalid)?;
        if data_start
            .checked_add(entry.compressed)
            .is_none_or(|end| end > self.central_start)
        {
            return Err(invalid());
        }
        let mut local_name = vec![0; name_len];
        self.file.read_exact(&mut local_name)?;
        if local_name != name.as_bytes() {
            return Err(invalid());
        }
        self.file.seek(SeekFrom::Start(data_start))?;
        let max =
            usize::try_from(entry.uncompressed).map_err(|_| PresentationPdfError::LimitExceeded)?;
        let mut bytes = Vec::with_capacity(max.min(256 * 1024));
        let mut buffer = [0u8; 64 * 1024];
        match entry.method {
            0 => {
                if entry.compressed != entry.uncompressed {
                    return Err(invalid());
                }
                read_bounded(
                    &mut (&mut self.file).take(entry.compressed),
                    &mut bytes,
                    max,
                    &mut buffer,
                    should_stop,
                )?;
            }
            8 => {
                let mut decoder = DeflateDecoder::new((&mut self.file).take(entry.compressed));
                read_bounded(&mut decoder, &mut bytes, max, &mut buffer, should_stop)?;
                if decoder.total_in() != entry.compressed {
                    return Err(invalid());
                }
            }
            _ => return Err(PresentationPdfError::UnsupportedContent),
        }
        if bytes.len() != max || crc32(&bytes) != entry.crc {
            return Err(invalid());
        }
        Ok(bytes)
    }
}

fn read_bounded(
    input: &mut impl Read,
    output: &mut Vec<u8>,
    limit: usize,
    buffer: &mut [u8],
    should_stop: &impl Fn() -> bool,
) -> Result<(), PresentationPdfError> {
    loop {
        if should_stop() {
            return Err(PresentationPdfError::Cancelled);
        }
        let got = input.read(buffer).map_err(|_| invalid())?;
        if got == 0 {
            break;
        }
        if output
            .len()
            .checked_add(got)
            .is_none_or(|size| size > limit)
        {
            return Err(PresentationPdfError::LimitExceeded);
        }
        output.extend_from_slice(&buffer[..got]);
    }
    Ok(())
}

fn namespace(value: &ResolveResult<'_>, expected: &[u8]) -> bool {
    matches!(value, ResolveResult::Bound(uri) if uri.as_ref().as_bytes() == expected)
}

fn attribute(tag: &BytesStart<'_>, key: &[u8]) -> Result<Option<String>, PresentationPdfError> {
    for field in tag.attributes() {
        let field = field.map_err(|_| invalid())?;
        if field.key.as_ref().as_bytes() == key {
            return field
                .normalized_value(XmlVersion::Implicit1_0)
                .map(|value| Some(value.into_owned()))
                .map_err(|_| invalid());
        }
    }
    Ok(None)
}

fn required_attribute(tag: &BytesStart<'_>, key: &[u8]) -> Result<String, PresentationPdfError> {
    attribute(tag, key)?.ok_or_else(invalid)
}

fn number(tag: &BytesStart<'_>, key: &[u8]) -> Result<u64, PresentationPdfError> {
    required_attribute(tag, key)?.parse().map_err(|_| invalid())
}

fn xml_reader(bytes: &[u8]) -> Result<NsReader<&[u8]>, PresentationPdfError> {
    let xml = std::str::from_utf8(bytes).map_err(|_| invalid())?;
    Ok(NsReader::from_str(xml))
}

#[derive(Clone)]
struct Relationship {
    kind: String,
    target: String,
}

fn parse_rels(
    bytes: &[u8],
    owner: &str,
) -> Result<BTreeMap<String, Relationship>, PresentationPdfError> {
    let mut reader = xml_reader(bytes)?;
    let mut found = BTreeMap::new();
    let mut saw_root = false;
    loop {
        let (ns, event) = reader.read_resolved_event().map_err(|_| invalid())?;
        match event {
            Event::Start(tag) | Event::Empty(tag) if namespace(&ns, R) => {
                match tag.local_name().as_ref().as_bytes() {
                    b"Relationships" => saw_root = true,
                    b"Relationship" if saw_root => {
                        let id = required_attribute(&tag, b"Id")?;
                        let kind = required_attribute(&tag, b"Type")?;
                        let target = required_attribute(&tag, b"Target")?;
                        if id.is_empty()
                            || id.len() > 128
                            || target.len() > 1024
                            || attribute(&tag, b"TargetMode")?
                                .as_deref()
                                .is_some_and(|mode| mode != "Internal")
                        {
                            return Err(PresentationPdfError::UnsupportedContent);
                        }
                        let target = resolve_target(owner, &target)?;
                        if found.insert(id, Relationship { kind, target }).is_some() {
                            return Err(invalid());
                        }
                    }
                    _ => return Err(PresentationPdfError::UnsupportedContent),
                }
            }
            Event::Start(_) | Event::Empty(_) | Event::DocType(_) | Event::GeneralRef(_) => {
                return Err(PresentationPdfError::UnsupportedContent)
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if !saw_root {
        return Err(invalid());
    }
    Ok(found)
}

fn resolve_target(owner: &str, target: &str) -> Result<String, PresentationPdfError> {
    if target.starts_with('/')
        || target.contains('\\')
        || target.contains(':')
        || target.contains('%')
        || target.contains('?')
        || target.contains('#')
    {
        return Err(PresentationPdfError::UnsupportedContent);
    }
    let mut parts: Vec<&str> = owner
        .rsplit_once('/')
        .map_or(Vec::new(), |(prefix, _)| prefix.split('/').collect());
    for component in target.split('/') {
        match component {
            "" | "." => return Err(PresentationPdfError::UnsupportedContent),
            ".." => {
                parts
                    .pop()
                    .ok_or(PresentationPdfError::UnsupportedContent)?;
            }
            value => parts.push(value),
        }
    }
    if parts.is_empty() {
        return Err(invalid());
    }
    Ok(parts.join("/"))
}

fn rels_path(owner: &str) -> Result<String, PresentationPdfError> {
    let (parent, file) = owner.rsplit_once('/').unwrap_or(("", owner));
    if file.is_empty() {
        return Err(invalid());
    }
    if parent.is_empty() {
        Ok(format!("_rels/{file}.rels"))
    } else {
        Ok(format!("{parent}/_rels/{file}.rels"))
    }
}

fn relationships(
    archive: &mut Archive,
    owner: &str,
    should_stop: &impl Fn() -> bool,
) -> Result<BTreeMap<String, Relationship>, PresentationPdfError> {
    let path = rels_path(owner)?;
    let data = archive.read(&path, MAX_XML_BYTES, should_stop)?;
    parse_rels(&data, owner)
}

fn relationship<'a>(
    rels: &'a BTreeMap<String, Relationship>,
    id: &str,
    kind: &str,
) -> Result<&'a str, PresentationPdfError> {
    let rel = rels.get(id).ok_or_else(invalid)?;
    let expected =
        format!("http://schemas.openxmlformats.org/officeDocument/2006/relationships/{kind}");
    if rel.kind != expected {
        return Err(PresentationPdfError::UnsupportedContent);
    }
    Ok(&rel.target)
}

fn validate_content_types(bytes: &[u8]) -> Result<(), PresentationPdfError> {
    const CT: &[u8] = b"http://schemas.openxmlformats.org/package/2006/content-types";
    let mut reader = xml_reader(bytes)?;
    let mut presentation = false;
    loop {
        let (ns, event) = reader.read_resolved_event().map_err(|_| invalid())?;
        match event {
            Event::Start(tag) | Event::Empty(tag) if namespace(&ns, CT) => {
                match tag.local_name().as_ref().as_bytes() {
                    b"Override" => {
                        let part = required_attribute(&tag, b"PartName")?;
                        let content = required_attribute(&tag, b"ContentType")?;
                        if content.to_ascii_lowercase().contains("macroenabled")
                            || content.contains("vbaProject")
                        {
                            return Err(PresentationPdfError::UnsupportedContent);
                        }
                        if part == "/ppt/presentation.xml" {
                            presentation=content=="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml";
                        }
                    }
                    b"Types" | b"Default" => {}
                    _ => return Err(PresentationPdfError::UnsupportedContent),
                }
            }
            Event::Start(_) | Event::Empty(_) | Event::DocType(_) | Event::GeneralRef(_) => {
                return Err(PresentationPdfError::UnsupportedContent)
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if !presentation {
        return Err(invalid());
    }
    Ok(())
}

struct Deck {
    width: u64,
    height: u64,
    slides: Vec<Slide>,
}

struct Slide {
    background: Option<[u8; 3]>,
    objects: Vec<SlideObject>,
}

enum SlideObject {
    Picture { media: String, rect: Rect },
    SolidRect { rect: Rect, color: [u8; 3] },
}

#[derive(Clone, Copy)]
struct Rect {
    x: u64,
    y: u64,
    w: u64,
    h: u64,
}

fn presentation_index(
    bytes: &[u8],
) -> Result<(u64, u64, Vec<String>, Vec<String>), PresentationPdfError> {
    let mut reader = xml_reader(bytes)?;
    let mut width = None;
    let mut height = None;
    let mut slide_ids = Vec::new();
    let mut master_ids = Vec::new();
    let mut saw_root = false;
    loop {
        let (ns, event) = reader.read_resolved_event().map_err(|_| invalid())?;
        match event {
            Event::Start(tag) | Event::Empty(tag) if namespace(&ns, P) => {
                match tag.local_name().as_ref().as_bytes() {
                    b"presentation" => saw_root = true,
                    b"sldSz" if saw_root => {
                        if width.replace(number(&tag, b"cx")?).is_some()
                            || height.replace(number(&tag, b"cy")?).is_some()
                        {
                            return Err(invalid());
                        }
                    }
                    b"sldId" if saw_root => {
                        slide_ids.push(required_attribute(&tag, b"r:id")?);
                        if slide_ids.len() > MAX_SLIDES {
                            return Err(PresentationPdfError::LimitExceeded);
                        }
                    }
                    b"sldMasterId" if saw_root => {
                        master_ids.push(required_attribute(&tag, b"r:id")?)
                    }
                    b"sldMasterIdLst" | b"sldIdLst" | b"notesSz" | b"defaultTextStyle"
                        if saw_root => {}
                    _ => return Err(PresentationPdfError::UnsupportedContent),
                }
            }
            Event::Start(_) | Event::Empty(_) | Event::DocType(_) | Event::GeneralRef(_) => {
                return Err(PresentationPdfError::UnsupportedContent)
            }
            Event::Eof => break,
            _ => {}
        }
    }
    let (width, height) = (width.ok_or_else(invalid)?, height.ok_or_else(invalid)?);
    if !saw_root
        || slide_ids.is_empty()
        || width == 0
        || height == 0
        || width > MAX_PDF_SIDE_EMU
        || height > MAX_PDF_SIDE_EMU
        || master_ids.len() != 1
    {
        return Err(invalid());
    }
    Ok((width, height, slide_ids, master_ids))
}

fn validate_blank_master(
    archive: &mut Archive,
    path: &str,
    should_stop: &impl Fn() -> bool,
) -> Result<(), PresentationPdfError> {
    let data = archive.read(path, MAX_XML_BYTES, should_stop)?;
    let mut reader = xml_reader(&data)?;
    let mut saw_root = false;
    loop {
        let (ns, event) = reader.read_resolved_event().map_err(|_| invalid())?;
        match event {
            Event::Start(tag) | Event::Empty(tag) if namespace(&ns, P) => {
                match tag.local_name().as_ref().as_bytes() {
                    b"sldMaster" | b"sldLayout" => saw_root = true,
                    b"sp" | b"pic" | b"graphicFrame" | b"grpSp" | b"cxnSp" | b"bg" | b"bgRef"
                    | b"ph" | b"transition" | b"timing" | b"extLst" => {
                        return Err(PresentationPdfError::UnsupportedContent)
                    }
                    _ => {}
                }
            }
            Event::DocType(_) | Event::GeneralRef(_) => {
                return Err(PresentationPdfError::UnsupportedContent)
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if !saw_root {
        return Err(invalid());
    }
    Ok(())
}

fn read_deck(
    archive: &mut Archive,
    should_stop: &impl Fn() -> bool,
) -> Result<Deck, PresentationPdfError> {
    validate_content_types(&archive.read("[Content_Types].xml", MAX_XML_BYTES, should_stop)?)?;
    for path in archive
        .entries
        .keys()
        .filter(|name| name.ends_with(".rels"))
        .cloned()
        .collect::<Vec<_>>()
    {
        if should_stop() {
            return Err(PresentationPdfError::Cancelled);
        }
        let owner = if path == "_rels/.rels" {
            String::new()
        } else {
            let (prefix, file) = path.rsplit_once("/_rels/").ok_or_else(invalid)?;
            format!(
                "{prefix}/{}",
                file.strip_suffix(".rels").ok_or_else(invalid)?
            )
        };
        parse_rels(&archive.read(&path, MAX_XML_BYTES, should_stop)?, &owner)?;
    }
    let root = parse_rels(
        &archive.read("_rels/.rels", MAX_XML_BYTES, should_stop)?,
        "",
    )?;
    let office=root.values().filter(|rel|rel.kind=="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument").collect::<Vec<_>>();
    if office.len() != 1 || office[0].target != "ppt/presentation.xml" {
        return Err(invalid());
    }
    let (width, height, ids, masters) =
        presentation_index(&archive.read("ppt/presentation.xml", MAX_XML_BYTES, should_stop)?)?;
    let presentation_rels = relationships(archive, "ppt/presentation.xml", should_stop)?;
    let master = relationship(&presentation_rels, &masters[0], "slideMaster")?;
    validate_blank_master(archive, master, should_stop)?;
    let master_rels = relationships(archive, master, should_stop)?;
    let layouts = master_rels
        .values()
        .filter(|rel| rel.kind.ends_with("/slideLayout"))
        .collect::<Vec<_>>();
    if layouts.len() != 1 {
        return Err(PresentationPdfError::UnsupportedContent);
    }
    let layout = &layouts[0].target;
    validate_blank_master(archive, layout, should_stop)?;
    let layout_rels = relationships(archive, layout, should_stop)?;
    if !layout_rels
        .values()
        .any(|rel| rel.kind.ends_with("/slideMaster") && rel.target == master)
    {
        return Err(invalid());
    }
    let mut slides = Vec::with_capacity(ids.len());
    let mut seen = BTreeSet::new();
    for id in ids {
        if should_stop() {
            return Err(PresentationPdfError::Cancelled);
        }
        let slide_path = relationship(&presentation_rels, &id, "slide")?;
        if !seen.insert(slide_path.to_owned()) {
            return Err(invalid());
        }
        let rels = relationships(archive, slide_path, should_stop)?;
        if !rels
            .values()
            .any(|rel| rel.kind.ends_with("/slideLayout") && rel.target == *layout)
            || rels
                .values()
                .any(|rel| !rel.kind.ends_with("/slideLayout") && !rel.kind.ends_with("/image"))
        {
            return Err(PresentationPdfError::UnsupportedContent);
        }
        let xml = archive.read(slide_path, MAX_XML_BYTES, should_stop)?;
        slides.push(read_slide(&xml, &rels, width, height)?);
    }
    Ok(Deck {
        width,
        height,
        slides,
    })
}

#[derive(Default)]
struct PictureBuilder {
    embed: Option<String>,
    x: Option<u64>,
    y: Option<u64>,
    w: Option<u64>,
    h: Option<u64>,
    rectangle: bool,
    stretch: bool,
}

#[derive(Default)]
struct SolidRectBuilder {
    x: Option<u64>,
    y: Option<u64>,
    w: Option<u64>,
    h: Option<u64>,
    color: Option<[u8; 3]>,
    geometry: bool,
    no_stroke: bool,
    nonvisual: bool,
    properties: bool,
}

fn only_attributes(tag: &BytesStart<'_>, allowed: &[&[u8]]) -> Result<(), PresentationPdfError> {
    for field in tag.attributes() {
        let field = field.map_err(|_| invalid())?;
        if !allowed.contains(&field.key.as_ref().as_bytes()) {
            return Err(PresentationPdfError::UnsupportedContent);
        }
    }
    Ok(())
}

fn srgb_color(tag: &BytesStart<'_>) -> Result<[u8; 3], PresentationPdfError> {
    only_attributes(tag, &[b"val"])?;
    let hex = required_attribute(tag, b"val")?;
    if hex.len() != 6 || !hex.is_ascii() {
        return Err(invalid());
    }
    let mut color = [0u8; 3];
    for (index, component) in color.iter_mut().enumerate() {
        *component =
            u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).map_err(|_| invalid())?;
    }
    Ok(color)
}

fn shape_tag(namespace_uri: &ResolveResult<'_>, name: &[u8]) -> Option<&'static str> {
    if namespace(namespace_uri, P) {
        match name {
            b"sp" => Some("p:sp"),
            b"nvSpPr" => Some("p:nvSpPr"),
            b"cNvPr" => Some("p:cNvPr"),
            b"cNvSpPr" => Some("p:cNvSpPr"),
            b"nvPr" => Some("p:nvPr"),
            b"spPr" => Some("p:spPr"),
            _ => None,
        }
    } else if namespace(namespace_uri, A) {
        match name {
            b"xfrm" => Some("a:xfrm"),
            b"off" => Some("a:off"),
            b"ext" => Some("a:ext"),
            b"prstGeom" => Some("a:prstGeom"),
            b"avLst" => Some("a:avLst"),
            b"solidFill" => Some("a:solidFill"),
            b"srgbClr" => Some("a:srgbClr"),
            b"ln" => Some("a:ln"),
            b"noFill" => Some("a:noFill"),
            _ => None,
        }
    } else {
        None
    }
}

fn read_solid_rect(
    reader: &mut NsReader<&[u8]>,
    width: u64,
    height: u64,
) -> Result<SlideObject, PresentationPdfError> {
    let mut parents = vec!["p:sp"];
    let mut shape = SolidRectBuilder::default();
    loop {
        let (ns, event) = reader.read_resolved_event().map_err(|_| invalid())?;
        let empty = matches!(&event, Event::Empty(_));
        match event {
            Event::Start(tag) | Event::Empty(tag) => {
                let node = shape_tag(&ns, tag.local_name().as_ref().as_bytes())
                    .ok_or(PresentationPdfError::UnsupportedContent)?;
                match (parents.last().copied(), node) {
                    (Some("p:sp"), "p:nvSpPr") => {
                        only_attributes(&tag, &[])?;
                        if shape.nonvisual {
                            return Err(invalid());
                        }
                        shape.nonvisual = true;
                    }
                    (Some("p:nvSpPr"), "p:cNvPr") => {
                        only_attributes(&tag, &[b"id", b"name"])?;
                        number(&tag, b"id")?;
                        required_attribute(&tag, b"name")?;
                    }
                    (Some("p:nvSpPr"), "p:cNvSpPr") => {
                        only_attributes(&tag, &[b"txBox"])?;
                        if attribute(&tag, b"txBox")?.is_some_and(|value| value != "0") {
                            return Err(PresentationPdfError::UnsupportedContent);
                        }
                    }
                    (Some("p:nvSpPr"), "p:nvPr") => only_attributes(&tag, &[])?,
                    (Some("p:sp"), "p:spPr") => {
                        only_attributes(&tag, &[])?;
                        if shape.properties {
                            return Err(invalid());
                        }
                        shape.properties = true;
                    }
                    (Some("p:spPr"), "a:xfrm") => only_attributes(&tag, &[])?,
                    (Some("a:xfrm"), "a:off") => {
                        only_attributes(&tag, &[b"x", b"y"])?;
                        if shape.x.replace(number(&tag, b"x")?).is_some()
                            || shape.y.replace(number(&tag, b"y")?).is_some()
                        {
                            return Err(invalid());
                        }
                    }
                    (Some("a:xfrm"), "a:ext") => {
                        only_attributes(&tag, &[b"cx", b"cy"])?;
                        if shape.w.replace(number(&tag, b"cx")?).is_some()
                            || shape.h.replace(number(&tag, b"cy")?).is_some()
                        {
                            return Err(invalid());
                        }
                    }
                    (Some("p:spPr"), "a:prstGeom") => {
                        only_attributes(&tag, &[b"prst"])?;
                        if required_attribute(&tag, b"prst")? != "rect" || shape.geometry {
                            return Err(PresentationPdfError::UnsupportedContent);
                        }
                        shape.geometry = true;
                    }
                    (Some("a:prstGeom"), "a:avLst") => only_attributes(&tag, &[])?,
                    (Some("p:spPr"), "a:solidFill") => only_attributes(&tag, &[])?,
                    (Some("a:solidFill"), "a:srgbClr") => {
                        if shape.color.replace(srgb_color(&tag)?).is_some() {
                            return Err(invalid());
                        }
                    }
                    (Some("p:spPr"), "a:ln") => only_attributes(&tag, &[])?,
                    (Some("a:ln"), "a:noFill") => {
                        only_attributes(&tag, &[])?;
                        if shape.no_stroke {
                            return Err(invalid());
                        }
                        shape.no_stroke = true;
                    }
                    _ => return Err(PresentationPdfError::UnsupportedContent),
                }
                if !empty {
                    parents.push(node);
                }
            }
            Event::End(tag) => {
                let node =
                    shape_tag(&ns, tag.local_name().as_ref().as_bytes()).ok_or_else(invalid)?;
                if parents.pop() != Some(node) {
                    return Err(invalid());
                }
                if parents.is_empty() {
                    break;
                }
            }
            Event::Text(text) if text.as_ref().as_bytes().iter().all(u8::is_ascii_whitespace) => {}
            Event::Eof => return Err(invalid()),
            _ => return Err(PresentationPdfError::UnsupportedContent),
        }
    }
    let rect = Rect {
        x: shape.x.ok_or(PresentationPdfError::UnsupportedContent)?,
        y: shape.y.ok_or(PresentationPdfError::UnsupportedContent)?,
        w: shape.w.ok_or(PresentationPdfError::UnsupportedContent)?,
        h: shape.h.ok_or(PresentationPdfError::UnsupportedContent)?,
    };
    if !shape.nonvisual
        || !shape.properties
        || !shape.geometry
        || !shape.no_stroke
        || rect.w == 0
        || rect.h == 0
        || rect.x.checked_add(rect.w).is_none_or(|end| end > width)
        || rect.y.checked_add(rect.h).is_none_or(|end| end > height)
    {
        return Err(PresentationPdfError::UnsupportedContent);
    }
    Ok(SlideObject::SolidRect {
        rect,
        color: shape
            .color
            .ok_or(PresentationPdfError::UnsupportedContent)?,
    })
}

fn read_slide(
    bytes: &[u8],
    rels: &BTreeMap<String, Relationship>,
    width: u64,
    height: u64,
) -> Result<Slide, PresentationPdfError> {
    let mut reader = xml_reader(bytes)?;
    let mut slide = Slide {
        background: None,
        objects: Vec::new(),
    };
    let mut in_tree = false;
    let mut in_bg = false;
    let mut in_group_properties = false;
    let mut in_group_transform = false;
    let mut in_picture_transform = false;
    let mut in_picture_properties = false;
    let mut picture: Option<PictureBuilder> = None;
    let mut saw_root = false;
    loop {
        let (ns, event) = reader.read_resolved_event().map_err(|_| invalid())?;
        let empty = matches!(&event, Event::Empty(_));
        match event {
            Event::Start(tag) | Event::Empty(tag) => {
                if namespace(&ns, P) {
                    match tag.local_name().as_ref().as_bytes() {
                        b"sld" => saw_root = true,
                        b"cSld" | b"nvGrpSpPr" | b"cNvPr" | b"cNvGrpSpPr" | b"nvPr"
                        | b"nvPicPr" | b"cNvPicPr" | b"blipFill" | b"clrMapOvr" => {}
                        b"bg" if !in_tree && picture.is_none() => in_bg = !empty,
                        b"bgPr" if in_bg => {}
                        b"spTree" if saw_root => in_tree = !empty,
                        b"grpSpPr" if in_tree && picture.is_none() => in_group_properties = !empty,
                        b"pic" if in_tree && picture.is_none() && !empty => {
                            if slide.objects.len() >= MAX_PAGE_OBJECTS {
                                return Err(PresentationPdfError::LimitExceeded);
                            }
                            picture = Some(PictureBuilder::default());
                        }
                        b"sp" if in_tree && picture.is_none() && !empty => {
                            if slide.objects.len() >= MAX_PAGE_OBJECTS {
                                return Err(PresentationPdfError::LimitExceeded);
                            }
                            only_attributes(&tag, &[])?;
                            slide
                                .objects
                                .push(read_solid_rect(&mut reader, width, height)?);
                        }
                        b"spPr" if picture.is_some() => in_picture_properties = !empty,
                        _ => return Err(PresentationPdfError::UnsupportedContent),
                    }
                } else if namespace(&ns, A) {
                    match tag.local_name().as_ref().as_bytes() {
                        b"solidFill" | b"effectLst" if in_bg && picture.is_none() => {}
                        b"srgbClr" if in_bg && picture.is_none() => {
                            if slide.background.is_some() {
                                return Err(PresentationPdfError::UnsupportedContent);
                            }
                            slide.background = Some(srgb_color(&tag)?);
                        }
                        b"xfrm" if picture.is_some() && in_picture_properties => {
                            if tag.attributes().next().is_some() {
                                return Err(PresentationPdfError::UnsupportedContent);
                            }
                            in_picture_transform = !empty;
                        }
                        b"xfrm" if in_group_properties => {
                            if tag.attributes().next().is_some() {
                                return Err(PresentationPdfError::UnsupportedContent);
                            }
                            in_group_transform = !empty;
                        }
                        b"off" | b"ext" if in_picture_transform => {
                            let part = picture.as_mut().ok_or_else(invalid)?;
                            if tag.local_name().as_ref().as_bytes() == b"off" {
                                part.x = Some(number(&tag, b"x")?);
                                part.y = Some(number(&tag, b"y")?);
                            } else {
                                part.w = Some(number(&tag, b"cx")?);
                                part.h = Some(number(&tag, b"cy")?);
                            }
                        }
                        b"off" | b"ext" | b"chOff" | b"chExt" if in_group_transform => {
                            let (first, second) = if matches!(
                                tag.local_name().as_ref().as_bytes(),
                                b"off" | b"chOff"
                            ) {
                                (b"x".as_slice(), b"y".as_slice())
                            } else {
                                (b"cx".as_slice(), b"cy".as_slice())
                            };
                            if number(&tag, first)? != 0 || number(&tag, second)? != 0 {
                                return Err(PresentationPdfError::UnsupportedContent);
                            }
                        }
                        b"blip" if picture.is_some() => {
                            let part = picture.as_mut().ok_or_else(invalid)?;
                            if attribute(&tag, b"r:link")?.is_some()
                                || part
                                    .embed
                                    .replace(required_attribute(&tag, b"r:embed")?)
                                    .is_some()
                            {
                                return Err(PresentationPdfError::UnsupportedContent);
                            }
                        }
                        b"stretch" if picture.is_some() => {}
                        b"fillRect" if picture.is_some() => {
                            if tag.attributes().next().is_some() {
                                return Err(PresentationPdfError::UnsupportedContent);
                            }
                            picture.as_mut().ok_or_else(invalid)?.stretch = true
                        }
                        b"prstGeom" if picture.is_some() => {
                            if required_attribute(&tag, b"prst")? != "rect" {
                                return Err(PresentationPdfError::UnsupportedContent);
                            }
                            picture.as_mut().ok_or_else(invalid)?.rectangle = true;
                        }
                        b"avLst" | b"picLocks" if picture.is_some() => {}
                        b"masterClrMapping" if picture.is_none() => {}
                        _ => return Err(PresentationPdfError::UnsupportedContent),
                    }
                } else {
                    return Err(PresentationPdfError::UnsupportedContent);
                }
            }
            Event::End(tag) if namespace(&ns, P) => match tag.local_name().as_ref().as_bytes() {
                b"bg" => {
                    if slide.background.is_none() {
                        return Err(PresentationPdfError::UnsupportedContent);
                    }
                    in_bg = false;
                }
                b"spTree" => in_tree = false,
                b"grpSpPr" => in_group_properties = false,
                b"spPr" => in_picture_properties = false,
                b"pic" => {
                    let part = picture.take().ok_or_else(invalid)?;
                    let rect = Rect {
                        x: part.x.ok_or_else(invalid)?,
                        y: part.y.ok_or_else(invalid)?,
                        w: part.w.ok_or_else(invalid)?,
                        h: part.h.ok_or_else(invalid)?,
                    };
                    if !part.rectangle
                        || !part.stretch
                        || rect.w == 0
                        || rect.h == 0
                        || rect.x.checked_add(rect.w).is_none_or(|end| end > width)
                        || rect.y.checked_add(rect.h).is_none_or(|end| end > height)
                    {
                        return Err(PresentationPdfError::UnsupportedContent);
                    }
                    let embed = part.embed.ok_or_else(invalid)?;
                    let media = relationship(rels, &embed, "image")?.to_owned();
                    if !media.starts_with("ppt/media/")
                        || !matches!(media.rsplit('.').next(), Some("png" | "jpg" | "jpeg"))
                    {
                        return Err(PresentationPdfError::UnsupportedImage);
                    }
                    slide.objects.push(SlideObject::Picture { media, rect });
                }
                _ => {}
            },
            Event::End(tag) if namespace(&ns, A) => {
                if tag.local_name().as_ref().as_bytes() == b"xfrm" {
                    in_group_transform = false;
                    in_picture_transform = false;
                }
            }
            Event::Text(text) if !text.as_ref().as_bytes().iter().all(u8::is_ascii_whitespace) => {
                return Err(PresentationPdfError::UnsupportedContent)
            }
            Event::CData(_) | Event::DocType(_) | Event::GeneralRef(_) => {
                return Err(PresentationPdfError::UnsupportedContent)
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if !saw_root || in_tree || in_bg || picture.is_some() {
        return Err(invalid());
    }
    Ok(slide)
}

struct PdfOutput<W: Write> {
    writer: W,
    position: u64,
    offsets: Vec<u64>,
}

impl<W: Write> PdfOutput<W> {
    fn new(writer: W) -> Self {
        Self {
            writer,
            position: 0,
            offsets: vec![0],
        }
    }

    fn append(&mut self, bytes: &[u8]) -> Result<(), PresentationPdfError> {
        let next = self
            .position
            .checked_add(
                u64::try_from(bytes.len()).map_err(|_| PresentationPdfError::LimitExceeded)?,
            )
            .filter(|&n| n <= MAX_OUTPUT_BYTES)
            .ok_or(PresentationPdfError::LimitExceeded)?;
        self.writer.write_all(bytes)?;
        self.position = next;
        Ok(())
    }

    fn start_object(&mut self, id: usize) -> Result<(), PresentationPdfError> {
        if self.offsets.len() != id {
            return Err(invalid());
        }
        self.offsets.push(self.position);
        self.append(format!("{id} 0 obj\n").as_bytes())
    }

    fn object(&mut self, id: usize, body: &[u8]) -> Result<(), PresentationPdfError> {
        self.start_object(id)?;
        self.append(body)?;
        self.append(b"\nendobj\n")
    }

    fn image_object(
        &mut self,
        id: usize,
        image: &PdfImage,
        mask: bool,
    ) -> Result<(), PresentationPdfError> {
        self.start_object(id)?;
        let data = if mask {
            image.alpha.as_deref().unwrap_or(&[])
        } else {
            image.data.as_slice()
        };
        let (colorspace, filter, mask_ref) = if mask {
            ("/DeviceGray", "/FlateDecode", String::new())
        } else {
            let mask_ref = if image.alpha.is_some() {
                format!(" /SMask {} 0 R", id + 1)
            } else {
                String::new()
            };
            (image.colorspace(), image.filter(), mask_ref)
        };
        let decode = if !mask && image.jpeg && image.channels == 3 {
            " /DecodeParms << /ColorTransform 1 >>"
        } else {
            ""
        };
        self.append(format!("<< /Type /XObject /Subtype /Image /Width {} /Height {} /ColorSpace {colorspace} /BitsPerComponent 8 /Filter {filter}{decode}{mask_ref} /Length {} >>\nstream\n",image.width,image.height,data.len()).as_bytes())?;
        self.append(data)?;
        self.append(b"\nendstream\nendobj\n")
    }

    fn finish(mut self) -> Result<(), PresentationPdfError> {
        let xref = self.position;
        let count = self.offsets.len();
        self.append(format!("xref\n0 {count}\n0000000000 65535 f \n").as_bytes())?;
        for offset in std::mem::take(&mut self.offsets).into_iter().skip(1) {
            self.append(format!("{offset:010} 00000 n \n").as_bytes())?;
        }
        self.append(
            format!("trailer << /Size {count} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n")
                .as_bytes(),
        )?;
        self.writer.flush()?;
        Ok(())
    }
}

struct PdfImage {
    width: u32,
    height: u32,
    channels: u8,
    jpeg: bool,
    data: Vec<u8>,
    alpha: Option<Vec<u8>>,
}

impl PdfImage {
    fn colorspace(&self) -> &'static str {
        if self.channels == 1 {
            "/DeviceGray"
        } else {
            "/DeviceRGB"
        }
    }
    fn filter(&self) -> &'static str {
        if self.jpeg {
            "/DCTDecode"
        } else {
            "/FlateDecode"
        }
    }
}

fn validate_pixels(width: u32, height: u32) -> Result<(), PresentationPdfError> {
    if width == 0
        || height == 0
        || u64::from(width)
            .checked_mul(u64::from(height))
            .is_none_or(|pixels| pixels > MAX_PIXELS)
    {
        return Err(PresentationPdfError::LimitExceeded);
    }
    Ok(())
}

fn decode_image(
    data: Vec<u8>,
    path: &str,
    should_stop: &impl Fn() -> bool,
) -> Result<PdfImage, PresentationPdfError> {
    if should_stop() {
        return Err(PresentationPdfError::Cancelled);
    }
    if path.ends_with(".png") {
        let mut decoder = png::Decoder::new(Cursor::new(&data));
        decoder.set_transformations(png::Transformations::EXPAND);
        decoder.set_ignore_text_chunk(true);
        decoder.set_limits(png::Limits {
            bytes: MAX_DECODED_BYTES,
        });
        let mut reader = decoder
            .read_info()
            .map_err(|_| PresentationPdfError::UnsupportedImage)?;
        let info = reader.info();
        validate_pixels(info.width, info.height)?;
        if info.bit_depth == png::BitDepth::Sixteen
            || info.animation_control.is_some()
            || info.icc_profile.is_some()
        {
            return Err(PresentationPdfError::UnsupportedImage);
        }
        let (color, depth) = reader.output_color_type();
        if depth != png::BitDepth::Eight {
            return Err(PresentationPdfError::UnsupportedImage);
        }
        let width = info.width;
        let height = info.height;
        let size = reader.output_buffer_size();
        if size > MAX_DECODED_BYTES {
            return Err(PresentationPdfError::LimitExceeded);
        }
        let mut pixels = vec![0u8; size];
        let actual = reader
            .next_frame(&mut pixels)
            .map_err(|_| PresentationPdfError::UnsupportedImage)?;
        if actual.width != width || actual.height != height {
            return Err(PresentationPdfError::UnsupportedImage);
        }
        pixels.truncate(actual.buffer_size());
        reader
            .finish()
            .map_err(|_| PresentationPdfError::UnsupportedImage)?;
        if should_stop() {
            return Err(PresentationPdfError::Cancelled);
        }
        let (channels, pdf_channels, alpha_channel) = match color {
            png::ColorType::Grayscale => (1usize, 1u8, None),
            png::ColorType::Rgb => (3, 3, None),
            png::ColorType::GrayscaleAlpha => (2, 1, Some(1usize)),
            png::ColorType::Rgba => (4, 3, Some(3usize)),
            _ => return Err(PresentationPdfError::UnsupportedImage),
        };
        let stride = usize::try_from(width)
            .ok()
            .and_then(|w| w.checked_mul(channels))
            .ok_or(PresentationPdfError::LimitExceeded)?;
        if pixels.len()
            != stride
                .checked_mul(
                    usize::try_from(height).map_err(|_| PresentationPdfError::LimitExceeded)?,
                )
                .ok_or(PresentationPdfError::LimitExceeded)?
        {
            return Err(PresentationPdfError::UnsupportedImage);
        }
        let mut rgb = ZlibEncoder::new(Vec::new(), Compression::default());
        let mut mask = alpha_channel.map(|_| ZlibEncoder::new(Vec::new(), Compression::default()));
        let mut row = Vec::with_capacity(
            usize::try_from(width).map_err(|_| PresentationPdfError::LimitExceeded)?
                * usize::from(pdf_channels),
        );
        let mut alpha_row = Vec::with_capacity(
            usize::try_from(width).map_err(|_| PresentationPdfError::LimitExceeded)?,
        );
        for source in pixels.chunks_exact(stride) {
            if should_stop() {
                return Err(PresentationPdfError::Cancelled);
            }
            if let Some(alpha_index) = alpha_channel {
                row.clear();
                alpha_row.clear();
                for pixel in source.chunks_exact(channels) {
                    row.extend_from_slice(&pixel[..usize::from(pdf_channels)]);
                    alpha_row.push(pixel[alpha_index]);
                }
                rgb.write_all(&row)?;
                mask.as_mut().ok_or_else(invalid)?.write_all(&alpha_row)?;
            } else {
                rgb.write_all(source)?;
            }
        }
        let compressed = rgb.finish()?;
        let alpha = mask.map(ZlibEncoder::finish).transpose()?;
        if compressed.len() > MAX_IMAGE_BYTES as usize
            || alpha
                .as_ref()
                .is_some_and(|m| m.len() > MAX_IMAGE_BYTES as usize)
        {
            return Err(PresentationPdfError::LimitExceeded);
        }
        Ok(PdfImage {
            width,
            height,
            channels: pdf_channels,
            jpeg: false,
            data: compressed,
            alpha,
        })
    } else if path.ends_with(".jpg") || path.ends_with(".jpeg") {
        if !data.starts_with(b"\xff\xd8") || !data.ends_with(b"\xff\xd9") {
            return Err(PresentationPdfError::UnsupportedImage);
        }
        let metadata = &data[..data.len().min(64 * 1024)];
        if metadata
            .windows(6)
            .any(|window| window == b"Exif\x00\x00" || window == b"Adobe\x00")
            || metadata
                .windows(12)
                .any(|window| window == b"ICC_PROFILE\x00")
        {
            return Err(PresentationPdfError::UnsupportedImage);
        }
        let mut decoder = jpeg_decoder::Decoder::new(Cursor::new(&data));
        decoder.set_max_decoding_buffer_size(MAX_DECODED_BYTES);
        let pixels = decoder
            .decode()
            .map_err(|_| PresentationPdfError::UnsupportedImage)?;
        if should_stop() {
            return Err(PresentationPdfError::Cancelled);
        }
        let info = decoder
            .info()
            .ok_or(PresentationPdfError::UnsupportedImage)?;
        let channels = match info.pixel_format {
            jpeg_decoder::PixelFormat::L8 => 1u8,
            jpeg_decoder::PixelFormat::RGB24 => 3u8,
            _ => return Err(PresentationPdfError::UnsupportedImage),
        };
        let width = u32::from(info.width);
        let height = u32::from(info.height);
        validate_pixels(width, height)?;
        let expected = usize::from(info.width)
            .checked_mul(usize::from(info.height))
            .and_then(|n| n.checked_mul(usize::from(channels)))
            .ok_or(PresentationPdfError::LimitExceeded)?;
        if pixels.len() != expected {
            return Err(PresentationPdfError::UnsupportedImage);
        }
        Ok(PdfImage {
            width,
            height,
            channels,
            jpeg: true,
            data,
            alpha: None,
        })
    } else {
        Err(PresentationPdfError::UnsupportedImage)
    }
}

fn write_document(
    file: File,
    archive: &mut Archive,
    deck: &Deck,
    should_stop: &impl Fn() -> bool,
) -> Result<(), PresentationPdfError> {
    let mut pdf = PdfOutput::new(BufWriter::new(file));
    pdf.append(b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n")?;
    pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
    let pages = deck.slides.len();
    let kids = (0..pages)
        .map(|i| format!("{} 0 R", 3 + 2 * i))
        .collect::<Vec<_>>()
        .join(" ");
    pdf.object(
        2,
        format!("<< /Type /Pages /Kids [{kids}] /Count {pages} >>").as_bytes(),
    )?;
    let mut next_image_id = 3 + 2 * pages;
    let width = deck.width as f64 / 12_700.0;
    let height = deck.height as f64 / 12_700.0;
    for (index, slide) in deck.slides.iter().enumerate() {
        if should_stop() {
            return Err(PresentationPdfError::Cancelled);
        }
        let page = 3 + 2 * index;
        let content = page + 1;
        let mut resources = String::from("<< /XObject << ");
        let mut stream = String::new();
        let background = slide.background.unwrap_or([255, 255, 255]);
        stream.push_str(&format!(
            "q\n{:.6} {:.6} {:.6} rg\n0 0 {width:.4} {height:.4} re f\nQ\n",
            f64::from(background[0]) / 255.0,
            f64::from(background[1]) / 255.0,
            f64::from(background[2]) / 255.0
        ));
        let mut image_index = 0usize;
        for item in &slide.objects {
            match item {
                SlideObject::Picture { rect, .. } => {
                    resources.push_str(&format!("/Im{image_index} {next_image_id} 0 R "));
                    let x = rect.x as f64 / 12_700.0;
                    let y = (deck.height - rect.y - rect.h) as f64 / 12_700.0;
                    let w = rect.w as f64 / 12_700.0;
                    let h = rect.h as f64 / 12_700.0;
                    stream.push_str(&format!(
                        "q\n{w:.4} 0 0 {h:.4} {x:.4} {y:.4} cm\n/Im{image_index} Do\nQ\n"
                    ));
                    image_index += 1;
                    next_image_id += 2;
                }
                SlideObject::SolidRect { rect, color } => {
                    let x = rect.x as f64 / 12_700.0;
                    let y = (deck.height - rect.y - rect.h) as f64 / 12_700.0;
                    let w = rect.w as f64 / 12_700.0;
                    let h = rect.h as f64 / 12_700.0;
                    stream.push_str(&format!(
                        "q\n{:.6} {:.6} {:.6} rg\n{x:.4} {y:.4} {w:.4} {h:.4} re f\nQ\n",
                        f64::from(color[0]) / 255.0,
                        f64::from(color[1]) / 255.0,
                        f64::from(color[2]) / 255.0
                    ));
                }
            }
        }
        resources.push_str(">> >>");
        pdf.object(page,format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {width:.4} {height:.4}] /Resources {resources} /Contents {content} 0 R >>").as_bytes())?;
        pdf.object(
            content,
            format!("<< /Length {} >>\nstream\n{stream}endstream", stream.len()).as_bytes(),
        )?;
    }
    let mut image_id = 3 + 2 * pages;
    for slide in &deck.slides {
        for item in &slide.objects {
            if should_stop() {
                return Err(PresentationPdfError::Cancelled);
            }
            if let SlideObject::Picture { media, .. } = item {
                let bytes = archive.read(media, MAX_IMAGE_BYTES, should_stop)?;
                let image = decode_image(bytes, media, should_stop)?;
                pdf.image_object(image_id, &image, false)?;
                if image.alpha.is_some() {
                    pdf.image_object(image_id + 1, &image, true)?;
                } else {
                    pdf.object(image_id + 1, b"null")?;
                }
                image_id += 2;
            }
        }
    }
    if image_id != next_image_id || should_stop() {
        return Err(PresentationPdfError::Cancelled);
    }
    pdf.finish()?;
    if should_stop() {
        return Err(PresentationPdfError::Cancelled);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pptx::{self, SlideImage, SlideImageFormat};
    use flate2::write::DeflateEncoder;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    const FILLED_RECT: &str = r#"<p:sp><p:nvSpPr><p:cNvPr id="3" name="Red rectangle"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:spPr><a:xfrm><a:off x="1270000" y="1270000"/><a:ext cx="1524000" cy="1524000"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom><a:solidFill><a:srgbClr val="D82F4A"/></a:solidFill><a:ln><a:noFill/></a:ln></p:spPr></p:sp>"#;

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "presentation-pdf-{}-{stamp}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).expect("create fixture directory");
            Self(path)
        }

        fn png(&self, name: &str, width: u32, height: u32, alpha: bool) -> SlideImage {
            let path = self.0.join(name);
            let mut encoder =
                png::Encoder::new(File::create(&path).expect("png output"), width, height);
            let channels = if alpha { 4 } else { 3 };
            encoder.set_color(if alpha {
                png::ColorType::Rgba
            } else {
                png::ColorType::Rgb
            });
            encoder.set_depth(png::BitDepth::Eight);
            let mut colors =
                vec![0u8; usize::try_from(width * height * channels).expect("tiny sample")];
            for (index, pixel) in colors.chunks_exact_mut(channels as usize).enumerate() {
                let x = u32::try_from(index).expect("sample index") % width;
                let y = u32::try_from(index).expect("sample index") / width;
                pixel[..3].copy_from_slice(if x < width / 2 {
                    &[230, 46, 44]
                } else {
                    &[19, 104, 219]
                });
                if alpha {
                    pixel[3] = if y < height / 2 { 255 } else { 96 };
                }
            }
            encoder
                .write_header()
                .expect("png header")
                .write_image_data(&colors)
                .expect("png pixels");
            SlideImage {
                path,
                width,
                height,
                format: SlideImageFormat::Png,
            }
        }

        fn jpeg(&self, name: &str, width: u16, height: u16) -> SlideImage {
            let path = self.0.join(name);
            let mut pixels = vec![0u8; usize::from(width) * usize::from(height) * 3];
            for (index, pixel) in pixels.chunks_exact_mut(3).enumerate() {
                pixel.copy_from_slice(if index % usize::from(width) < usize::from(width) / 2 {
                    &[244, 183, 52]
                } else {
                    &[49, 160, 112]
                });
            }
            jpeg_encoder::Encoder::new(File::create(&path).expect("jpeg output"), 85)
                .encode(&pixels, width, height, jpeg_encoder::ColorType::Rgb)
                .expect("jpeg pixels");
            SlideImage {
                path,
                width: u32::from(width),
                height: u32::from(height),
                format: SlideImageFormat::Jpeg,
            }
        }

        fn pptx(&self, images: &[SlideImage]) -> PathBuf {
            let output = self.0.join("sample.pptx");
            pptx::write_image_pptx(&output, images, || false).expect("sample presentation");
            output
        }

        fn output(&self) -> PathBuf {
            self.0.join("converted.pdf")
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            if std::env::var_os("KEEP_PRESENTATION_PDF_FIXTURE").is_none() {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
    }

    fn object_offsets(pdf: &[u8]) -> Vec<usize> {
        let end = pdf
            .windows(10)
            .rposition(|window| window == b"startxref\n")
            .expect("xref marker");
        let xref: usize = std::str::from_utf8(
            pdf[end + 10..]
                .split(|&byte| byte == b'\n')
                .next()
                .expect("xref offset"),
        )
        .expect("ascii offset")
        .parse()
        .expect("numeric xref");
        let central = std::str::from_utf8(&pdf[xref..]).expect("ascii xref");
        let mut lines = central.lines();
        assert_eq!(lines.next(), Some("xref"));
        let count = lines
            .next()
            .expect("xref count")
            .split_whitespace()
            .last()
            .expect("count")
            .parse::<usize>()
            .expect("integer count");
        assert_eq!(lines.next(), Some("0000000000 65535 f "));
        (1..count)
            .map(|id| {
                let position = lines.next().expect("object offset")[..10]
                    .parse::<usize>()
                    .expect("numeric object offset");
                assert!(pdf[position..].starts_with(format!("{id} 0 obj\n").as_bytes()));
                position
            })
            .collect()
    }

    fn repackage(path: &Path, mut transform: impl FnMut(&str, &mut Vec<u8>)) {
        let mut source = Archive::open(path, &|| false).expect("valid original presentation");
        let names = source.entries.keys().cloned().collect::<Vec<_>>();
        let mut package = Vec::new();
        let mut central = Vec::new();
        for name in &names {
            let mut payload = source
                .read(name, MAX_IMAGE_BYTES, &|| false)
                .expect("read entry");
            transform(name, &mut payload);
            let crc = crc32(&payload);
            let (method, compressed) = if name.ends_with(".xml") || name.ends_with(".rels") {
                let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
                encoder.write_all(&payload).expect("compress XML");
                (8u16, encoder.finish().expect("finish compression"))
            } else {
                (0u16, payload.clone())
            };
            let offset = u32::try_from(package.len()).expect("small fixture offset");
            let name_len = u16::try_from(name.len()).expect("short fixture name");
            let compressed_len = u32::try_from(compressed.len()).expect("tiny compressed fixture");
            let uncompressed_len = u32::try_from(payload.len()).expect("tiny fixture");
            package.extend_from_slice(&0x04034b50u32.to_le_bytes());
            package.extend_from_slice(&20u16.to_le_bytes());
            package.extend_from_slice(&0u16.to_le_bytes());
            package.extend_from_slice(&method.to_le_bytes());
            package.extend_from_slice(&[0u8; 4]);
            package.extend_from_slice(&crc.to_le_bytes());
            package.extend_from_slice(&compressed_len.to_le_bytes());
            package.extend_from_slice(&uncompressed_len.to_le_bytes());
            package.extend_from_slice(&name_len.to_le_bytes());
            package.extend_from_slice(&0u16.to_le_bytes());
            package.extend_from_slice(name.as_bytes());
            package.extend_from_slice(&compressed);

            central.extend_from_slice(&0x02014b50u32.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&method.to_le_bytes());
            central.extend_from_slice(&[0u8; 4]);
            central.extend_from_slice(&crc.to_le_bytes());
            central.extend_from_slice(&compressed_len.to_le_bytes());
            central.extend_from_slice(&uncompressed_len.to_le_bytes());
            central.extend_from_slice(&name_len.to_le_bytes());
            central.extend_from_slice(&[0u8; 12]);
            central.extend_from_slice(&offset.to_le_bytes());
            central.extend_from_slice(name.as_bytes());
        }
        let central_offset = u32::try_from(package.len()).expect("small fixture ZIP");
        let central_size = u32::try_from(central.len()).expect("small central directory");
        let count = u16::try_from(names.len()).expect("small entry count");
        package.extend_from_slice(&central);
        package.extend_from_slice(&0x06054b50u32.to_le_bytes());
        package.extend_from_slice(&[0u8; 4]);
        package.extend_from_slice(&count.to_le_bytes());
        package.extend_from_slice(&count.to_le_bytes());
        package.extend_from_slice(&central_size.to_le_bytes());
        package.extend_from_slice(&central_offset.to_le_bytes());
        package.extend_from_slice(&0u16.to_le_bytes());
        fs::write(path, package).expect("repackage presentation");
    }

    fn add_first_slide_shape(path: &Path, shape: &str) {
        repackage(path, |name, payload| {
            if name == "ppt/slides/slide1.xml" {
                let xml = String::from_utf8(payload.clone()).expect("slide XML");
                assert!(xml.contains("</p:spTree>"));
                *payload = xml
                    .replacen("</p:spTree>", &format!("{shape}</p:spTree>"), 1)
                    .into_bytes();
            }
        });
    }

    #[test]
    fn converts_mixed_aspect_png_alpha_and_jpeg_into_ordered_pdf_pages() {
        let case = Fixture::new();
        let png = case.png("alpha.png", 400, 200, true);
        let jpeg = case.jpeg("portrait.jpg", 160, 320);
        let pptx = case.pptx(&[png, jpeg]);
        let output = case.output();
        let pages = write_presentation_pdf(&pptx, &output, || false).expect("convert two slides");
        assert_eq!(pages, 2);
        if std::env::var_os("KEEP_PRESENTATION_PDF_FIXTURE").is_some() {
            eprintln!("roundtrip PDF: {}", output.display());
        }
        let pdf = fs::read(&output).expect("result");
        assert!(pdf.starts_with(b"%PDF-1.4"));
        assert!(pdf
            .windows(b"/Count 2".len())
            .any(|window| window == b"/Count 2"));
        assert!(pdf
            .windows(b"/MediaBox [0 0 960.0000 480.0000]".len())
            .any(|window| window == b"/MediaBox [0 0 960.0000 480.0000]"));
        assert!(pdf
            .windows(b"/SMask".len())
            .any(|window| window == b"/SMask"));
        assert!(pdf
            .windows(b"/DCTDecode".len())
            .any(|window| window == b"/DCTDecode"));
        assert!(pdf
            .windows(b"/Im0 Do".len())
            .any(|window| window == b"/Im0 Do"));
        assert!(pdf
            .windows(b"240.0000 0 0 480.0000 360.0000 0.0000 cm".len())
            .any(|window| window == b"240.0000 0 0 480.0000 360.0000 0.0000 cm"));
        assert_eq!(object_offsets(&pdf).len(), 10);

        #[cfg(target_os = "macos")]
        {
            let rendered = crate::render::render_pdf(
                &output,
                &case.0,
                "page2",
                &crate::render::RenderOptions {
                    selection: crate::pdf::PageSelection::Pages([2].into_iter().collect()),
                    dpi: 150,
                    format: crate::render::ImageFormat::Png,
                    jpg_quality: 90,
                    max_selected_pages: 2,
                    max_output_bytes: 16 * 1024 * 1024,
                },
                || false,
                |_, _, _| {},
            )
            .expect("render second PDF page with CoreGraphics");
            assert_eq!(rendered.selected_page_count, 1);
            let page2 = &rendered.outputs[0];
            if std::env::var_os("KEEP_PRESENTATION_PDF_FIXTURE").is_some() {
                eprintln!("second page PNG: {}", page2.display());
            }
            let mut image = png::Decoder::new(File::open(page2).expect("second page preview"))
                .read_info()
                .expect("PNG header");
            let mut pixels = vec![0; image.output_buffer_size()];
            let info = image.next_frame(&mut pixels).expect("PNG pixels");
            assert!(
                (2000..=2001).contains(&info.width) && (1000..=1001).contains(&info.height),
                "CoreGraphics rounded page size unexpectedly: {}x{}",
                info.width,
                info.height
            );
            assert_eq!(info.color_type, png::ColorType::Rgb);
            let sample = |x: usize, y: usize| {
                let index = (y * usize::try_from(info.width).expect("sample width") + x) * 3;
                &pixels[index..index + 3]
            };
            let white = sample(200, 500);
            assert!(
                white.iter().all(|&component| component > 240),
                "left margin should remain white: {white:?}"
            );
            let yellow = sample(850, 500);
            assert!(
                yellow[0] > yellow[1] && yellow[1] > yellow[2],
                "left half of portrait image should be yellow: {yellow:?}"
            );
            let green = sample(1150, 500);
            assert!(
                green[1] > green[0] && green[1] > green[2],
                "right half should be green: {green:?}"
            );
        }
    }

    #[test]
    fn rejects_legacy_ppt_and_oversized_or_corrupt_pptx_without_output() {
        let case = Fixture::new();
        let output = case.output();
        let legacy = case.0.join("old.ppt");
        let mut ole = vec![0u8; 512];
        ole[..8].copy_from_slice(&[0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1]);
        fs::write(&legacy, ole).expect("legacy input");
        assert!(matches!(
            write_presentation_pdf(&legacy, &output, || false),
            Err(PresentationPdfError::UnsupportedLegacyPpt)
        ));
        assert!(!output.exists());

        let pptx = case.pptx(&[case.png("one.png", 2, 3, false)]);
        let mut bytes = fs::read(&pptx).expect("sample");
        let media = bytes
            .windows(4)
            .position(|window| window == b"\x89PNG")
            .expect("png entry");
        bytes[media] ^= 1;
        fs::write(&pptx, bytes).expect("corrupt entry");
        assert!(matches!(
            write_presentation_pdf(&pptx, &output, || false),
            Err(PresentationPdfError::InvalidPackage)
        ));
        assert!(!output.exists());

        File::create(&pptx)
            .expect("truncate presentation")
            .set_len(MAX_INPUT_BYTES + 1)
            .expect("sparse limit sample");
        assert!(matches!(
            write_presentation_pdf(&pptx, &output, || false),
            Err(PresentationPdfError::LimitExceeded)
        ));
        assert!(!output.exists());
    }

    #[test]
    fn cancellation_cleans_up_and_never_replaces_an_existing_pdf() {
        let case = Fixture::new();
        let pptx = case.pptx(&[case.png("one.png", 2, 2, false)]);
        let output = case.output();
        let calls = AtomicUsize::new(0);
        let result = write_presentation_pdf(&pptx, &output, || {
            calls.fetch_add(1, Ordering::Relaxed) > 25
        });
        assert!(matches!(result, Err(PresentationPdfError::Cancelled)));
        assert!(!output.exists());
        fs::write(&output, b"keep original").expect("existing output");
        assert!(
            matches!(write_presentation_pdf(&pptx,&output,||false),Err(PresentationPdfError::Io(error)) if error.kind()==io::ErrorKind::AlreadyExists)
        );
        assert_eq!(
            fs::read(output).expect("original unchanged"),
            b"keep original"
        );
    }

    #[test]
    fn bad_slide_relationship_does_not_publish_pdf() {
        let case = Fixture::new();
        let pptx = case.pptx(&[case.png("one.png", 2, 2, false)]);
        let output = case.output();
        let mut bytes = fs::read(&pptx).expect("sample");
        let pattern = b"../media/page1.png";
        let at = bytes
            .windows(pattern.len())
            .position(|window| window == pattern)
            .expect("image target");
        bytes[at] = b'/';
        fs::write(&pptx, bytes).expect("mutate relationship");
        assert!(matches!(
            write_presentation_pdf(&pptx, &output, || false),
            Err(PresentationPdfError::InvalidPackage)
        ));
        assert!(!output.exists());
    }

    #[test]
    fn reads_deflated_ooxml_parts_without_changing_slide_visuals() {
        let case = Fixture::new();
        let pptx = case.pptx(&[case.jpeg("photo.jpg", 64, 32)]);
        repackage(&pptx, |_, _| {});
        assert_eq!(
            write_presentation_pdf(&pptx, &case.output(), || false).expect("deflated presentation"),
            1
        );
        let pdf = fs::read(case.output()).expect("result");
        assert!(pdf.windows(10).any(|window| window == b"/DCTDecode"));
        assert_eq!(object_offsets(&pdf).len(), 6);
    }

    #[test]
    fn refuses_real_shapes_in_slide_or_master_instead_of_dropping_them() {
        for master in [false, true] {
            let case = Fixture::new();
            let pptx = case.pptx(&[case.png("page.png", 6, 3, false)]);
            let output = case.output();
            repackage(&pptx, |name, payload| {
                let edit = if master {
                    name == "ppt/slideMasters/slideMaster1.xml"
                } else {
                    name == "ppt/slides/slide1.xml"
                };
                if edit {
                    let xml = String::from_utf8(payload.clone()).expect("fixture XML");
                    *payload = xml
                        .replacen("</p:spTree>", "<p:sp><p:spPr/></p:sp></p:spTree>", 1)
                        .into_bytes();
                }
            });
            assert!(matches!(
                write_presentation_pdf(&pptx, &output, || false),
                Err(PresentationPdfError::UnsupportedContent)
            ));
            assert!(!output.exists());
        }
    }

    #[test]
    fn refuses_external_relationships_with_valid_crc() {
        let case = Fixture::new();
        let pptx = case.pptx(&[case.png("page.png", 4, 2, false)]);
        let output = case.output();
        repackage(&pptx, |name, payload| {
            if name == "ppt/slides/_rels/slide1.xml.rels" {
                let xml = String::from_utf8(payload.clone()).expect("fixture XML");
                *payload = xml
                    .replace(
                        "Target=\"../media/page1.png\"",
                        "Target=\"https://invalid.example/image.png\" TargetMode=\"External\"",
                    )
                    .into_bytes();
            }
        });
        assert!(matches!(
            write_presentation_pdf(&pptx, &output, || false),
            Err(PresentationPdfError::UnsupportedContent)
        ));
        assert!(!output.exists());
    }

    #[test]
    fn refuses_cropped_picture_instead_of_rendering_the_full_image() {
        let case = Fixture::new();
        let pptx = case.pptx(&[case.png("page.png", 4, 2, false)]);
        let output = case.output();
        repackage(&pptx, |name, payload| {
            if name == "ppt/slides/slide1.xml" {
                let xml = String::from_utf8(payload.clone()).expect("fixture XML");
                assert!(xml.contains("<a:fillRect/>"));
                *payload = xml
                    .replace("<a:fillRect/>", "<a:fillRect l=\"25000\"/>")
                    .into_bytes();
            }
        });
        assert!(matches!(
            write_presentation_pdf(&pptx, &output, || false),
            Err(PresentationPdfError::UnsupportedContent)
        ));
        assert!(!output.exists());
    }

    #[test]
    fn refuses_non_file_input_before_reading_archive_bytes() {
        let case = Fixture::new();
        let non_file = case.0.join("not-a-file.pptx");
        fs::create_dir(&non_file).expect("directory fixture");
        let output = case.output();
        assert!(matches!(
            write_presentation_pdf(&non_file, &output, || false),
            Err(PresentationPdfError::InvalidInput)
        ));
        assert!(!output.exists());
    }

    #[test]
    fn draws_solid_rectangle_at_slide_coordinates_without_hiding_the_image() {
        let case = Fixture::new();
        let pptx = case.pptx(&[case.jpeg("landscape.jpg", 320, 160)]);
        add_first_slide_shape(&pptx, FILLED_RECT);
        let output = case.output();
        assert_eq!(
            write_presentation_pdf(&pptx, &output, || false).expect("rectangle and image"),
            1
        );
        let pdf = fs::read(&output).expect("PDF page");
        assert!(pdf
            .windows(b"100.0000 260.0000 120.0000 120.0000 re f".len())
            .any(|window| window == b"100.0000 260.0000 120.0000 120.0000 re f"));
        assert!(pdf.windows(b"/Im0 Do".len()).any(|part| part == b"/Im0 Do"));
        assert_eq!(object_offsets(&pdf).len(), 6);

        #[cfg(target_os = "macos")]
        {
            let rendered = crate::render::render_pdf(
                &output,
                &case.0,
                "solid-rect",
                &crate::render::RenderOptions {
                    selection: crate::pdf::PageSelection::All,
                    dpi: 150,
                    format: crate::render::ImageFormat::Png,
                    jpg_quality: 90,
                    max_selected_pages: 1,
                    max_output_bytes: 16 * 1024 * 1024,
                },
                || false,
                |_, _, _| {},
            )
            .expect("render rectangle PDF page");
            let mut image = png::Decoder::new(
                File::open(&rendered.outputs[0]).expect("rectangle page preview"),
            )
            .read_info()
            .expect("PNG header");
            let mut pixels = vec![0; image.output_buffer_size()];
            let info = image.next_frame(&mut pixels).expect("PNG pixels");
            assert!((2000..=2001).contains(&info.width));
            assert!((1000..=1001).contains(&info.height));
            assert_eq!(info.color_type, png::ColorType::Rgb);
            let pixel = |x: usize, y: usize| {
                let index = (y * usize::try_from(info.width).expect("PNG width") + x) * 3;
                &pixels[index..index + 3]
            };
            assert_eq!(pixel(320, 300), &[216, 47, 74]);
            let yellow = pixel(600, 300);
            assert!(yellow[0] > yellow[1] && yellow[1] > yellow[2]);
            let green = pixel(1500, 300);
            assert!(green[1] > green[0] && green[1] > green[2]);
        }
    }

    #[test]
    fn refuses_text_rotation_and_gradient_in_basic_shapes() {
        let text = "<p:txBody><a:bodyPr/><a:p><a:r><a:t>中文</a:t></a:r></a:p></p:txBody>";
        for shape in [
            FILLED_RECT.replace("</p:sp>", &format!("{text}</p:sp>")),
            FILLED_RECT.replace("<a:xfrm>", "<a:xfrm rot=\"5400000\">"),
            FILLED_RECT.replace("<a:solidFill>", "<a:gradFill>"),
        ] {
            let case = Fixture::new();
            let pptx = case.pptx(&[case.png("page.png", 4, 2, false)]);
            add_first_slide_shape(&pptx, &shape);
            let output = case.output();
            assert!(matches!(
                write_presentation_pdf(&pptx, &output, || false),
                Err(PresentationPdfError::UnsupportedContent)
            ));
            assert!(!output.exists());
        }
    }
}
