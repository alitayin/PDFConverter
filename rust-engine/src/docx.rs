use flate2::read::DeflateDecoder;
use quick_xml::escape::resolve_predefined_entity;
use quick_xml::events::{BytesStart, Event};
use quick_xml::name::ResolveResult;
use quick_xml::reader::NsReader;
use quick_xml::XmlVersion;
use std::fs;
use std::io::{self, Read};
use std::path::Path;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum DocxError {
    #[error("DOCX package structure is invalid")]
    InvalidPackage,
    #[error("DOCX content could not be read")]
    InvalidDocument,
    #[error(
        "DOCX to PDF does not support images, drawings, embedded objects, headers, or footnotes"
    )]
    UnsupportedVisualContent,
    #[error("Legacy binary DOC files are not supported")]
    LegacyDocUnsupported,
    #[error("DOCX read/write failed: {0}")]
    Io(#[from] io::Error),
    #[error("DOCX extraction failed")]
    Decompression,
}

const CONTENT_TYPES: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
<Default Extension="xml" ContentType="application/xml"/>
<Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
</Types>"#;

const ROOT_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"#;

const DOCUMENT_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"></Relationships>"#;
const MAX_DOCUMENT_XML_BYTES: usize = 64 * 1024 * 1024;
const WORD_NAMESPACE: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";
const STRICT_WORD_NAMESPACE: &str = "http://purl.oclc.org/ooxml/wordprocessingml/main";

pub fn write_text_docx(path: &Path, pages: &[String]) -> Result<(), DocxError> {
    let document = document_xml(pages);
    let entries = [
        ("[Content_Types].xml", CONTENT_TYPES.as_bytes()),
        ("_rels/.rels", ROOT_RELS.as_bytes()),
        ("word/document.xml", document.as_bytes()),
        ("word/_rels/document.xml.rels", DOCUMENT_RELS.as_bytes()),
    ];
    let mut package = ZipPackage::new();
    for (name, data) in entries {
        package.add_stored(name, data)?;
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, package.finish())?;
    Ok(())
}

pub fn extract_text_from_path(path: &Path) -> Result<String, DocxError> {
    Ok(extract_pages_from_path(path)?.join("\n"))
}

pub fn extract_pages_from_path(path: &Path) -> Result<Vec<String>, DocxError> {
    let bytes = fs::read(path)?;
    if !bytes.starts_with(b"PK") {
        return Err(DocxError::LegacyDocUnsupported);
    }
    let document = read_zip_entry(&bytes, "word/document.xml")?;
    parse_document_pages(&document)
}

fn document_xml(pages: &[String]) -> String {
    let mut body = String::from(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
<w:body>"#,
    );
    for (page_index, page) in pages.iter().enumerate() {
        for line in page.lines() {
            body.push_str("<w:p><w:r><w:t xml:space=\"preserve\">");
            body.push_str(&escape_xml(line));
            body.push_str("</w:t></w:r></w:p>");
        }
        if page.is_empty() {
            body.push_str("<w:p/>");
        }
        if page_index + 1 < pages.len() {
            body.push_str("<w:p><w:r><w:br w:type=\"page\"/></w:r></w:p>");
        }
    }
    body.push_str("</w:body></w:document>");
    body
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
fn parse_document_text(bytes: &[u8]) -> Result<String, DocxError> {
    Ok(parse_document_pages(bytes)?.join("\n"))
}

fn is_word_namespace(namespace: &ResolveResult<'_>) -> bool {
    matches!(namespace, ResolveResult::Bound(value)
        if value.as_ref() == WORD_NAMESPACE || value.as_ref() == STRICT_WORD_NAMESPACE)
}

fn attribute_value(tag: &BytesStart<'_>, name: &str) -> Result<Option<String>, DocxError> {
    for attribute in tag.attributes() {
        let attribute = attribute.map_err(|_| DocxError::InvalidDocument)?;
        if attribute.key.local_name().as_ref() == name {
            return attribute
                .normalized_value(XmlVersion::Implicit1_0)
                .map(|value| Some(value.into_owned()))
                .map_err(|_| DocxError::InvalidDocument);
        }
    }
    Ok(None)
}

fn finish_page(pages: &mut Vec<String>, force: bool) {
    if let Some(page) = pages.last_mut() {
        *page = page.trim_end().to_owned();
        if force || !page.is_empty() {
            pages.push(String::new());
        }
    }
}

fn parse_document_pages(bytes: &[u8]) -> Result<Vec<String>, DocxError> {
    let xml = std::str::from_utf8(bytes).map_err(|_| DocxError::InvalidDocument)?;
    let mut reader = NsReader::from_str(xml);
    let mut pages = vec![String::new()];
    let mut buffer = Vec::new();
    let mut saw_body = false;
    let mut in_body = false;
    let mut in_text = false;
    loop {
        let (namespace, event) = reader
            .read_resolved_event_into(&mut buffer)
            .map_err(|_| DocxError::InvalidDocument)?;
        let word = is_word_namespace(&namespace);
        let empty = matches!(&event, Event::Empty(_));
        match event {
            Event::Start(tag) | Event::Empty(tag) if word => {
                let name = tag.local_name();
                match name.as_ref() {
                    "body" => {
                        saw_body = true;
                        in_body = true;
                    }
                    "t" if in_body => in_text = !empty,
                    "drawing" | "pict" | "object" | "headerReference" | "footerReference"
                    | "footnoteReference" | "endnoteReference"
                        if in_body =>
                    {
                        return Err(DocxError::UnsupportedVisualContent);
                    }
                    "br" if in_body => {
                        if attribute_value(&tag, "type")?.as_deref() == Some("page") {
                            finish_page(&mut pages, true);
                        } else {
                            pages
                                .last_mut()
                                .ok_or(DocxError::InvalidDocument)?
                                .push('\n');
                        }
                    }
                    "pageBreakBefore" if in_body => {
                        if !matches!(
                            attribute_value(&tag, "val")?.as_deref(),
                            Some("0" | "false" | "off")
                        ) {
                            finish_page(&mut pages, false);
                        }
                    }
                    "cr" if in_body => {
                        pages
                            .last_mut()
                            .ok_or(DocxError::InvalidDocument)?
                            .push('\n');
                    }
                    "tab" if in_body => {
                        pages
                            .last_mut()
                            .ok_or(DocxError::InvalidDocument)?
                            .push('\t');
                    }
                    _ => {}
                }
            }
            Event::End(tag) if word => match tag.local_name().as_ref() {
                "t" => in_text = false,
                "p" if in_body => {
                    let page = pages.last_mut().ok_or(DocxError::InvalidDocument)?;
                    while page.ends_with(' ') {
                        page.pop();
                    }
                    if !page.is_empty() && !page.ends_with('\n') {
                        page.push('\n');
                    }
                }
                "tc" if in_body => {
                    let page = pages.last_mut().ok_or(DocxError::InvalidDocument)?;
                    *page = page.trim_end_matches('\n').to_owned();
                    page.push('\t');
                }
                "tr" if in_body => {
                    let page = pages.last_mut().ok_or(DocxError::InvalidDocument)?;
                    if page.ends_with('\t') {
                        page.pop();
                    }
                    if !page.is_empty() && !page.ends_with('\n') {
                        page.push('\n');
                    }
                }
                "body" => in_body = false,
                _ => {}
            },
            Event::Text(content) if in_text => {
                pages
                    .last_mut()
                    .ok_or(DocxError::InvalidDocument)?
                    .push_str(&content.xml10_content());
            }
            Event::GeneralRef(reference) if in_text => {
                let page = pages.last_mut().ok_or(DocxError::InvalidDocument)?;
                if let Some(character) = reference
                    .resolve_char_ref()
                    .map_err(|_| DocxError::InvalidDocument)?
                {
                    page.push(character);
                } else {
                    page.push_str(
                        resolve_predefined_entity(reference.as_ref())
                            .ok_or(DocxError::InvalidDocument)?,
                    );
                }
            }
            Event::CData(content) if in_text => {
                pages
                    .last_mut()
                    .ok_or(DocxError::InvalidDocument)?
                    .push_str(&content.xml10_content());
            }
            Event::DocType(_) => return Err(DocxError::InvalidDocument),
            Event::Eof => break,
            _ => {}
        }
        buffer.clear();
    }
    if !saw_body || in_body || in_text || pages.iter().all(|page| page.trim().is_empty()) {
        return Err(DocxError::InvalidDocument);
    }
    for page in &mut pages {
        *page = page.trim_end().to_owned();
    }
    Ok(pages)
}

struct ZipPackage {
    bytes: Vec<u8>,
    central: Vec<CentralEntry>,
}

#[derive(Clone)]
struct CentralEntry {
    name: String,
    crc32: u32,
    compressed_size: u32,
    uncompressed_size: u32,
    local_offset: u32,
}

impl ZipPackage {
    fn new() -> Self {
        Self {
            bytes: Vec::new(),
            central: Vec::new(),
        }
    }

    fn add_stored(&mut self, name: &str, data: &[u8]) -> Result<(), DocxError> {
        let name_bytes = name.as_bytes();
        let name_len = u16::try_from(name_bytes.len()).map_err(|_| DocxError::InvalidPackage)?;
        let size = u32::try_from(data.len()).map_err(|_| DocxError::InvalidPackage)?;
        let offset = u32::try_from(self.bytes.len()).map_err(|_| DocxError::InvalidPackage)?;
        let crc32 = crc32(data);
        self.bytes.extend_from_slice(&0x04034b50u32.to_le_bytes());
        self.bytes.extend_from_slice(&20u16.to_le_bytes());
        self.bytes.extend_from_slice(&0u16.to_le_bytes());
        self.bytes.extend_from_slice(&0u16.to_le_bytes());
        self.bytes.extend_from_slice(&0u16.to_le_bytes());
        self.bytes.extend_from_slice(&0u16.to_le_bytes());
        self.bytes.extend_from_slice(&crc32.to_le_bytes());
        self.bytes.extend_from_slice(&size.to_le_bytes());
        self.bytes.extend_from_slice(&size.to_le_bytes());
        self.bytes.extend_from_slice(&name_len.to_le_bytes());
        self.bytes.extend_from_slice(&0u16.to_le_bytes());
        self.bytes.extend_from_slice(name_bytes);
        self.bytes.extend_from_slice(data);
        self.central.push(CentralEntry {
            name: name.to_owned(),
            crc32,
            compressed_size: size,
            uncompressed_size: size,
            local_offset: offset,
        });
        Ok(())
    }

    fn finish(mut self) -> Vec<u8> {
        let central_offset = self.bytes.len() as u32;
        for entry in &self.central {
            let name = entry.name.as_bytes();
            self.bytes.extend_from_slice(&0x02014b50u32.to_le_bytes());
            self.bytes.extend_from_slice(&20u16.to_le_bytes());
            self.bytes.extend_from_slice(&20u16.to_le_bytes());
            self.bytes.extend_from_slice(&0u16.to_le_bytes());
            self.bytes.extend_from_slice(&0u16.to_le_bytes());
            self.bytes.extend_from_slice(&0u16.to_le_bytes());
            self.bytes.extend_from_slice(&0u16.to_le_bytes());
            self.bytes.extend_from_slice(&entry.crc32.to_le_bytes());
            self.bytes
                .extend_from_slice(&entry.compressed_size.to_le_bytes());
            self.bytes
                .extend_from_slice(&entry.uncompressed_size.to_le_bytes());
            self.bytes
                .extend_from_slice(&(name.len() as u16).to_le_bytes());
            self.bytes.extend_from_slice(&0u16.to_le_bytes());
            self.bytes.extend_from_slice(&0u16.to_le_bytes());
            self.bytes.extend_from_slice(&0u16.to_le_bytes());
            self.bytes.extend_from_slice(&0u16.to_le_bytes());
            self.bytes.extend_from_slice(&0u32.to_le_bytes());
            self.bytes
                .extend_from_slice(&entry.local_offset.to_le_bytes());
            self.bytes.extend_from_slice(name);
        }
        let central_size = self.bytes.len() as u32 - central_offset;
        let count = self.central.len() as u16;
        self.bytes.extend_from_slice(&0x06054b50u32.to_le_bytes());
        self.bytes.extend_from_slice(&0u16.to_le_bytes());
        self.bytes.extend_from_slice(&0u16.to_le_bytes());
        self.bytes.extend_from_slice(&count.to_le_bytes());
        self.bytes.extend_from_slice(&count.to_le_bytes());
        self.bytes.extend_from_slice(&central_size.to_le_bytes());
        self.bytes.extend_from_slice(&central_offset.to_le_bytes());
        self.bytes.extend_from_slice(&0u16.to_le_bytes());
        self.bytes
    }
}

fn read_zip_entry(bytes: &[u8], wanted: &str) -> Result<Vec<u8>, DocxError> {
    let end = bytes
        .windows(4)
        .rposition(|window| window == b"PK\x05\x06")
        .ok_or(DocxError::InvalidPackage)?;
    let central_size = read_u32(bytes, end + 12)? as usize;
    let central_offset = read_u32(bytes, end + 16)? as usize;
    let central_end = central_offset
        .checked_add(central_size)
        .ok_or(DocxError::InvalidPackage)?;
    if central_end > bytes.len() {
        return Err(DocxError::InvalidPackage);
    }
    let mut position = central_offset;
    while position < central_end {
        if read_u32(bytes, position)? != 0x02014b50 {
            return Err(DocxError::InvalidPackage);
        }
        let method = read_u16(bytes, position + 10)?;
        let expected_crc32 = read_u32(bytes, position + 16)?;
        let compressed_size = read_u32(bytes, position + 20)? as usize;
        let uncompressed_size = read_u32(bytes, position + 24)? as usize;
        let name_len = read_u16(bytes, position + 28)? as usize;
        let extra_len = read_u16(bytes, position + 30)? as usize;
        let comment_len = read_u16(bytes, position + 32)? as usize;
        let local_offset = read_u32(bytes, position + 42)? as usize;
        let name_start = position + 46;
        let name_end = name_start
            .checked_add(name_len)
            .ok_or(DocxError::InvalidPackage)?;
        let name = std::str::from_utf8(
            bytes
                .get(name_start..name_end)
                .ok_or(DocxError::InvalidPackage)?,
        )
        .map_err(|_| DocxError::InvalidPackage)?;
        if name == wanted {
            if uncompressed_size > MAX_DOCUMENT_XML_BYTES {
                return Err(DocxError::InvalidPackage);
            }
            let local_name_len = read_u16(bytes, local_offset + 26)? as usize;
            let local_extra_len = read_u16(bytes, local_offset + 28)? as usize;
            let data_start = local_offset
                .checked_add(30 + local_name_len + local_extra_len)
                .ok_or(DocxError::InvalidPackage)?;
            let data_end = data_start
                .checked_add(compressed_size)
                .ok_or(DocxError::InvalidPackage)?;
            let compressed = bytes
                .get(data_start..data_end)
                .ok_or(DocxError::InvalidPackage)?;
            let output = match method {
                0 => compressed.to_vec(),
                8 => {
                    let decoder = DeflateDecoder::new(compressed);
                    let mut output =
                        Vec::with_capacity(uncompressed_size.min(MAX_DOCUMENT_XML_BYTES));
                    decoder
                        .take((MAX_DOCUMENT_XML_BYTES + 1) as u64)
                        .read_to_end(&mut output)
                        .map_err(|_| DocxError::Decompression)?;
                    if output.len() > MAX_DOCUMENT_XML_BYTES {
                        return Err(DocxError::InvalidPackage);
                    }
                    output
                }
                _ => return Err(DocxError::InvalidPackage),
            };
            if output.len() != uncompressed_size || crc32(&output) != expected_crc32 {
                return Err(DocxError::InvalidPackage);
            }
            return Ok(output);
        }
        position = name_end
            .checked_add(extra_len + comment_len)
            .ok_or(DocxError::InvalidPackage)?;
    }
    Err(DocxError::InvalidDocument)
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, DocxError> {
    let slice = bytes
        .get(offset..offset + 2)
        .ok_or(DocxError::InvalidPackage)?;
    Ok(u16::from_le_bytes([slice[0], slice[1]]))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, DocxError> {
    let slice = bytes
        .get(offset..offset + 4)
        .ok_or(DocxError::InvalidPackage)?;
    Ok(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = 0u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xedb88320 & mask);
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_and_reads_a_minimal_docx() {
        let path = std::env::temp_dir().join("minimal-pdf-converter-test.docx");
        write_text_docx(&path, &["Hello\nworld".to_owned(), "第二页".to_owned()]).expect("write");
        let text = extract_text_from_path(&path).expect("read");
        assert!(text.contains("Hello"));
        assert!(text.contains("world"));
        assert!(text.contains("第二页"));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn pdf_to_docx_keeps_every_line_and_source_page_boundary() {
        struct FixtureDir(std::path::PathBuf);
        impl Drop for FixtureDir {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }

        let root = std::env::temp_dir().join(format!("pdf-to-docx-lines-{}", std::process::id()));
        fs::create_dir_all(&root).expect("create fixture directory");
        let _fixture = FixtureDir(root.clone());
        let pdf_path = root.join("source.pdf");
        let docx_path = root.join("result.docx");
        let source_pages = ["first\nsecond\nthird".to_owned(), "next\nlast".to_owned()];
        crate::pdf_writer::write_pages_pdf(&pdf_path, &source_pages).expect("create two-page PDF");

        let extracted =
            crate::pdf::extract_text_from_path(&pdf_path, &crate::pdf::PageSelection::All)
                .expect("extract source PDF text");
        assert_eq!(extracted.page_count, 2);
        let pages = extracted
            .pages
            .into_iter()
            .map(|page| page.text)
            .collect::<Vec<_>>();
        assert_eq!(pages, source_pages);
        write_text_docx(&docx_path, &pages).expect("write DOCX");
        assert_eq!(
            extract_pages_from_path(&docx_path).expect("read DOCX"),
            source_pages
        );
    }

    #[test]
    fn rejects_legacy_doc_bytes() {
        let path = std::env::temp_dir().join("minimal-pdf-converter-test.doc");
        fs::write(&path, b"\xd0\xcf\x11\xe0").expect("write");
        assert!(matches!(
            extract_text_from_path(&path),
            Err(DocxError::LegacyDocUnsupported)
        ));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn parses_text_without_treating_table_tags_as_text() {
        let xml = br#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:tbl><w:tr><w:tc><w:p><w:r><w:t>Cell</w:t></w:r></w:p></w:tc></w:tr></w:tbl></w:body></w:document>"#;
        assert_eq!(parse_document_text(xml).expect("parse XML"), "Cell");
    }

    #[test]
    fn decodes_numeric_xml_entities() {
        let xml = br#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>A&#x4F60;&#22909;</w:t></w:r></w:p></w:body></w:document>"#;
        assert_eq!(parse_document_text(xml).expect("parse XML"), "A你好");
    }

    #[test]
    fn preserves_tab_and_carriage_return_runs() {
        let xml = br#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>A</w:t><w:tab/><w:t>B</w:t><w:cr/><w:t>C</w:t></w:r></w:p></w:body></w:document>"#;
        assert_eq!(parse_document_text(xml).expect("parse XML"), "A\tB\nC");
    }

    #[test]
    fn preserves_explicit_page_breaks_and_table_cell_order() {
        let xml = br#"<d:document xmlns:d="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><d:body><d:p><d:r><d:t>First</d:t><d:br d:type='page'/><d:t>Second</d:t></d:r></d:p><d:p><d:pPr><d:pageBreakBefore d:val='1'/></d:pPr><d:r><d:t>Third</d:t></d:r></d:p><d:tbl><d:tr><d:tc><d:p><d:r><d:t>A</d:t></d:r></d:p></d:tc><d:tc><d:p><d:r><d:t>B</d:t></d:r></d:p></d:tc></d:tr></d:tbl></d:body></d:document>"#;
        assert_eq!(
            parse_document_pages(xml).expect("document pages"),
            ["First", "Second", "Third\nA\tB"]
        );
    }

    #[test]
    fn rejects_visual_content_instead_of_silently_dropping_it() {
        let xml = br#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>Caption</w:t><w:drawing/></w:r></w:p></w:body></w:document>"#;
        assert!(matches!(
            parse_document_pages(xml),
            Err(DocxError::UnsupportedVisualContent)
        ));
    }

    #[test]
    fn rejects_referenced_header_instead_of_silently_dropping_its_text() {
        let xml = br#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>Body</w:t></w:r></w:p><w:sectPr><w:headerReference w:type="default" r:id="rId5" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"/></w:sectPr></w:body></w:document>"#;
        assert!(matches!(
            parse_document_pages(xml),
            Err(DocxError::UnsupportedVisualContent)
        ));
    }

    #[test]
    fn rejects_doctype_and_malformed_xml() {
        let prefix = b"<!DOCTYPE document [<!ENTITY ext SYSTEM 'file:///private/text'>]>";
        let mut xml = prefix.to_vec();
        xml.extend_from_slice(br#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:t>&ext;</w:t></w:p></w:body></w:document>"#);
        assert!(matches!(
            parse_document_pages(&xml),
            Err(DocxError::InvalidDocument)
        ));
        assert!(matches!(
            parse_document_pages(br#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:t>missing close"#),
            Err(DocxError::InvalidDocument)
        ));
    }

    #[test]
    fn rejects_zip_entries_with_a_mismatched_crc() {
        let mut package = ZipPackage::new();
        package
            .add_stored("word/document.xml", b"<w:document/>")
            .expect("fixture entry");
        let mut bytes = package.finish();
        let marker = b"<w:document/>";
        let position = bytes
            .windows(marker.len())
            .position(|window| window == marker)
            .expect("stored document payload");
        bytes[position + 3] ^= 1;
        assert!(matches!(
            read_zip_entry(&bytes, "word/document.xml"),
            Err(DocxError::InvalidPackage)
        ));
    }

    #[test]
    fn keeps_docx_page_breaks_in_generated_pdf() {
        let root = std::env::temp_dir().join(format!("docx-page-layout-{}", std::process::id()));
        fs::create_dir_all(&root).expect("temporary fixture directory");
        let docx_path = root.join("source.docx");
        let pdf_path = root.join("result.pdf");
        let document = br#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>First</w:t><w:br w:type="page"/><w:t>Second</w:t></w:r></w:p></w:body></w:document>"#;
        let mut package = ZipPackage::new();
        package
            .add_stored("word/document.xml", document)
            .expect("fixture entry");
        fs::write(&docx_path, package.finish()).expect("write fixture");
        let pages = extract_pages_from_path(&docx_path).expect("read pages");
        crate::pdf_writer::write_text_pages_pdf(&pdf_path, &pages).expect("write PDF");
        let result = crate::pdf::extract_text_from_path(&pdf_path, &crate::pdf::PageSelection::All)
            .expect("read generated PDF");
        assert_eq!(result.page_count, 2);
        assert_eq!(result.pages[0].text, "First");
        assert_eq!(result.pages[1].text, "Second");
        fs::remove_dir_all(root).expect("remove fixture");
    }
}
