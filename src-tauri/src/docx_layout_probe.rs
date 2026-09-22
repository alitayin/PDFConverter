//! Read-only OOXML layout inspection for evaluating paragraph reflow.
//! This module is deliberately not connected to the conversion path.

use quick_xml::escape::resolve_predefined_entity;
use quick_xml::events::Event;
use quick_xml::name::ResolveResult;
use quick_xml::reader::NsReader;
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;
use thiserror::Error;
use zip::ZipArchive;

const MAX_DOCUMENT_XML_BYTES: u64 = 16 * 1024 * 1024;
const WORD_NS: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";
const MARKUP_NS: &str = "http://schemas.openxmlformats.org/markup-compatibility/2006";
const DRAWING_NS: &str = "http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing";

#[derive(Debug, Error)]
pub(crate) enum ProbeError {
    #[error("cannot read DOCX: {0}")]
    Io(#[from] io::Error),
    #[error("DOCX ZIP is invalid: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("DOCX document XML exceeds inspection limit")]
    TooLarge,
    #[error("DOCX document XML is invalid")]
    InvalidDocument,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct DocxLayoutProbe {
    pub paragraphs: usize,
    pub floating_anchors: usize,
    pub inline_drawings: usize,
    pub text_boxes: usize,
    pub fallback_text_boxes: usize,
    pub alternate_content: usize,
    pub section_properties: usize,
    pub explicit_page_breaks: usize,
    pub numbered_paragraphs: usize,
    // Document order only. It is not the reading order of positioned PDF content.
    pub visible_text_in_xml_order: String,
}

pub(crate) fn inspect_docx(path: &Path) -> Result<DocxLayoutProbe, ProbeError> {
    let mut archive = ZipArchive::new(File::open(path)?)?;
    let mut entry = archive.by_name("word/document.xml")?;
    if entry.size() > MAX_DOCUMENT_XML_BYTES {
        return Err(ProbeError::TooLarge);
    }
    let mut xml = Vec::new();
    entry
        .by_ref()
        .take(MAX_DOCUMENT_XML_BYTES + 1)
        .read_to_end(&mut xml)?;
    if xml.len() as u64 > MAX_DOCUMENT_XML_BYTES {
        return Err(ProbeError::TooLarge);
    }
    inspect_xml(&xml)
}

fn in_namespace(namespace: &ResolveResult<'_>, expected: &str) -> bool {
    matches!(namespace, ResolveResult::Bound(value) if value.as_ref() == expected)
}

pub(crate) fn inspect_xml(bytes: &[u8]) -> Result<DocxLayoutProbe, ProbeError> {
    if bytes.len() as u64 > MAX_DOCUMENT_XML_BYTES {
        return Err(ProbeError::TooLarge);
    }
    let xml = std::str::from_utf8(bytes).map_err(|_| ProbeError::InvalidDocument)?;
    let mut reader = NsReader::from_str(xml);
    let mut probe = DocxLayoutProbe::default();
    let mut buffer = Vec::new();
    let mut in_body = false;
    let mut saw_body = false;
    let mut in_text = false;
    let mut in_fallback = false;
    loop {
        let (namespace, event) = reader
            .read_resolved_event_into(&mut buffer)
            .map_err(|_| ProbeError::InvalidDocument)?;
        let word = in_namespace(&namespace, WORD_NS);
        let markup = in_namespace(&namespace, MARKUP_NS);
        let drawing = in_namespace(&namespace, DRAWING_NS);
        match event {
            Event::Start(ref tag) | Event::Empty(ref tag) => {
                let empty = matches!(&event, Event::Empty(_));
                let name = tag.local_name();
                if word && name.as_ref() == "body" {
                    saw_body = true;
                    in_body = !empty;
                }
                if !in_body {
                    buffer.clear();
                    continue;
                }
                if markup && name.as_ref() == "Fallback" {
                    in_fallback = !empty;
                } else if markup && name.as_ref() == "AlternateContent" && !in_fallback {
                    probe.alternate_content += 1;
                } else if word && name.as_ref() == "txbxContent" {
                    if in_fallback {
                        probe.fallback_text_boxes += 1;
                    } else {
                        probe.text_boxes += 1;
                    }
                } else if !in_fallback {
                    if word {
                        match name.as_ref() {
                            "p" => probe.paragraphs += 1,
                            "t" => in_text = !empty,
                            "sectPr" => probe.section_properties += 1,
                            "numPr" => probe.numbered_paragraphs += 1,
                            "br" => {
                                for attribute in tag.attributes() {
                                    let attribute =
                                        attribute.map_err(|_| ProbeError::InvalidDocument)?;
                                    if attribute.key.local_name().as_ref() == "type"
                                        && attribute.value.as_ref() == "page"
                                    {
                                        probe.explicit_page_breaks += 1;
                                    }
                                }
                            }
                            _ => {}
                        }
                    } else if drawing {
                        match name.as_ref() {
                            "anchor" => probe.floating_anchors += 1,
                            "inline" => probe.inline_drawings += 1,
                            _ => {}
                        }
                    }
                }
            }
            Event::End(ref tag) => {
                let name = tag.local_name();
                if markup && name.as_ref() == "Fallback" {
                    in_fallback = false;
                } else if word && name.as_ref() == "t" {
                    in_text = false;
                } else if word && name.as_ref() == "p" && in_body && !in_fallback {
                    if !probe.visible_text_in_xml_order.ends_with('\n') {
                        probe.visible_text_in_xml_order.push('\n');
                    }
                } else if word && name.as_ref() == "body" {
                    in_body = false;
                }
            }
            Event::Text(text) if in_text && !in_fallback => {
                probe
                    .visible_text_in_xml_order
                    .push_str(&text.xml10_content());
            }
            Event::GeneralRef(reference) if in_text && !in_fallback => {
                if let Some(character) = reference
                    .resolve_char_ref()
                    .map_err(|_| ProbeError::InvalidDocument)?
                {
                    probe.visible_text_in_xml_order.push(character);
                } else {
                    let value = resolve_predefined_entity(reference.as_ref())
                        .ok_or(ProbeError::InvalidDocument)?;
                    probe.visible_text_in_xml_order.push_str(value);
                }
            }
            Event::DocType(_) => return Err(ProbeError::InvalidDocument),
            Event::Eof => break,
            _ => {}
        }
        buffer.clear();
    }
    if !saw_body || in_body || in_fallback || in_text {
        return Err(ProbeError::InvalidDocument);
    }
    Ok(probe)
}
