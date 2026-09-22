#[path = "../src/docx_layout_probe.rs"]
mod docx_layout_probe;

use docx_layout_probe::{inspect_docx, inspect_xml, ProbeError};
use std::path::Path;

const HEADER: &str = r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"
    xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006"
    xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing">
    <w:body>"#;

#[test]
fn writer_choice_counts_text_once_and_identifies_floating_text_box() {
    let xml = format!(
        "{HEADER}<w:p><mc:AlternateContent><mc:Choice Requires=\"wps\"><wp:anchor/>\
         <w:txbxContent><w:p><w:r><w:t>First</w:t></w:r></w:p></w:txbxContent>\
         </mc:Choice><mc:Fallback><w:txbxContent><w:p><w:r><w:t>First</w:t>\
         </w:r></w:p></w:txbxContent></mc:Fallback></mc:AlternateContent></w:p>\
         <w:p><w:r><w:br w:type=\"page\"/></w:r></w:p>\
         <w:sectPr/></w:body></w:document>"
    );
    let probe = inspect_xml(xml.as_bytes()).expect("parse Writer-compatible OOXML");
    assert_eq!(probe.alternate_content, 1);
    assert_eq!(probe.floating_anchors, 1);
    assert_eq!(probe.text_boxes, 1);
    assert_eq!(probe.fallback_text_boxes, 1);
    assert_eq!(probe.explicit_page_breaks, 1);
    assert_eq!(probe.visible_text_in_xml_order.matches("First").count(), 1);
}

#[test]
fn reflowed_paragraphs_and_inline_pictures_remain_distinct_from_floating_boxes() {
    let xml = format!(
        "{HEADER}<w:p><w:r><wp:inline/><w:t>Body</w:t></w:r></w:p>\
         <w:p><w:pPr><w:numPr/></w:pPr><w:r><w:t>Item</w:t></w:r></w:p>\
         <w:sectPr/><w:sectPr/></w:body></w:document>"
    );
    let probe = inspect_xml(xml.as_bytes()).expect("parse reflowed paragraphs");
    assert_eq!(probe.paragraphs, 2);
    assert_eq!(probe.inline_drawings, 1);
    assert_eq!(probe.floating_anchors, 0);
    assert_eq!(probe.section_properties, 2);
    assert_eq!(probe.numbered_paragraphs, 1);
    assert!(probe.visible_text_in_xml_order.contains("Body\nItem"));
}

#[test]
fn missing_body_and_doctype_are_rejected() {
    assert!(matches!(
        inspect_xml(b"<document/>"),
        Err(ProbeError::InvalidDocument)
    ));
    assert!(matches!(
        inspect_xml(b"<!DOCTYPE w:document><w:document/ >"),
        Err(ProbeError::InvalidDocument)
    ));
}

#[test]
#[ignore = "requires an authorized real DOCX path in MINIMALPDF_DOCX_LAYOUT_PROBE_PATH"]
fn inspect_authorized_real_docx_without_modifying_it() {
    let path = std::env::var_os("MINIMALPDF_DOCX_LAYOUT_PROBE_PATH")
        .expect("set MINIMALPDF_DOCX_LAYOUT_PROBE_PATH to an authorized DOCX file");
    let probe = inspect_docx(Path::new(&path)).expect("inspect DOCX without writing output");
    assert!(probe.paragraphs > 0);
    eprintln!(
        "paragraphs={}, anchors={}, inline={}, boxes={}, fallbacks={}, sections={}, breaks={}",
        probe.paragraphs,
        probe.floating_anchors,
        probe.inline_drawings,
        probe.text_boxes,
        probe.fallback_text_boxes,
        probe.section_properties,
        probe.explicit_page_breaks
    );
}
