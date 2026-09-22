use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::Path;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PdfWriterError {
    #[error("Could not write the PDF: {0}")]
    Io(#[from] io::Error),
    #[error("Text PDF currently does not support Unicode supplementary-plane characters")]
    UnsupportedUnicode,
}

pub fn write_text_pdf(path: &Path, text: &str) -> Result<(), PdfWriterError> {
    write_text_pages_pdf(path, &[text.to_owned()])
}

pub fn write_text_pages_pdf(path: &Path, text_pages: &[String]) -> Result<(), PdfWriterError> {
    let pages = paginate_pages(text_pages, 55);
    write_pages_pdf(path, &pages)
}

pub fn write_pages_pdf(path: &Path, pages: &[String]) -> Result<(), PdfWriterError> {
    if pages.iter().any(|page| {
        page.chars()
            .any(|character| u32::from(character) > u32::from(u16::MAX))
    }) {
        return Err(PdfWriterError::UnsupportedUnicode);
    }
    let font_object = 3 + pages.len() * 2;
    let descendant_font_object = font_object + 1;
    let to_unicode_object = descendant_font_object + 1;
    let latin_font_object = to_unicode_object + 1;
    let mut objects = Vec::new();
    objects.push(b"<< /Type /Catalog /Pages 2 0 R >>".to_vec());
    let kids = (0..pages.len())
        .map(|index| format!("{} 0 R", 3 + index * 2))
        .collect::<Vec<_>>()
        .join(" ");
    objects.push(format!("<< /Type /Pages /Kids [{kids}] /Count {} >>", pages.len()).into_bytes());
    for (index, page) in pages.iter().enumerate() {
        let page_object = 3 + index * 2;
        let content_object = page_object + 1;
        let page_body = format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 {font_object} 0 R /F2 {latin_font_object} 0 R >> >> /Contents {content_object} 0 R >>"
        );
        objects.push(page_body.into_bytes());
        let stream = page_stream(page);
        objects.push(
            format!(
                "<< /Length {} >>\nstream\n{}\nendstream",
                stream.len(),
                stream
            )
            .into_bytes(),
        );
    }
    objects.push(
        format!(
            "<< /Type /Font /Subtype /Type0 /BaseFont /STSong-Light /Encoding /UniGB-UCS2-H /DescendantFonts [{descendant_font_object} 0 R] /ToUnicode {to_unicode_object} 0 R >>"
        )
        .into_bytes(),
    );
    objects.push(
        "<< /Type /Font /Subtype /CIDFontType0 /BaseFont /STSong-Light /CIDSystemInfo << /Registry (Adobe) /Ordering (GB1) /Supplement 4 >> /DW 1000 >>"
            .to_owned()
            .into_bytes(),
    );
    let cmap = to_unicode_cmap(pages);
    objects.push(format!("<< /Length {} >>\nstream\n{}\nendstream", cmap.len(), cmap).into_bytes());
    objects.push(
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>"
            .to_vec(),
    );
    let mut output = b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n".to_vec();
    let mut offsets = Vec::with_capacity(objects.len());
    for (index, object) in objects.iter().enumerate() {
        offsets.push(output.len());
        output.extend_from_slice(format!("{} 0 obj\n", index + 1).as_bytes());
        output.extend_from_slice(object);
        output.extend_from_slice(b"\nendobj\n");
    }
    let xref_offset = output.len();
    output.extend_from_slice(format!("xref\n0 {}\n", objects.len() + 1).as_bytes());
    output.extend_from_slice(b"0000000000 65535 f \n");
    for offset in offsets {
        output.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    output.extend_from_slice(
        format!(
            "trailer << /Size {} /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n",
            objects.len() + 1,
            xref_offset
        )
        .as_bytes(),
    );
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, output)?;
    Ok(())
}

fn paginate_pages(text_pages: &[String], lines_per_page: usize) -> Vec<String> {
    // The CID font's declared default advance is 11pt. 45 glyphs fit in 504pt.
    const MAX_CHARS_PER_LINE: usize = 45;
    let mut pages = Vec::new();
    for text in text_pages {
        let completed_before = pages.len();
        let mut current = Vec::new();
        for line in text.split('\n') {
            let expanded = line.replace('\t', "    ");
            let characters = expanded.chars().collect::<Vec<_>>();
            if characters.is_empty() {
                current.push(String::new());
            } else {
                for chunk in characters.chunks(MAX_CHARS_PER_LINE) {
                    current.push(chunk.iter().collect());
                    if current.len() == lines_per_page {
                        pages.push(current.join("\n"));
                        current.clear();
                    }
                }
            }
            if current.len() == lines_per_page {
                pages.push(current.join("\n"));
                current.clear();
            }
        }
        if !current.is_empty() || pages.len() == completed_before {
            pages.push(current.join("\n"));
        }
    }
    if pages.is_empty() {
        pages.push(String::new());
    }
    pages
}

fn pdf_code_unit(character: char) -> u16 {
    let code_point = u32::from(character);
    if code_point <= u32::from(u16::MAX) {
        code_point as u16
    } else {
        0xfffd
    }
}

fn to_unicode_cmap(pages: &[String]) -> String {
    let mut code_units = BTreeSet::new();
    for page in pages {
        for character in page.chars() {
            code_units.insert(pdf_code_unit(character));
        }
    }
    let mut cmap = String::from("/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n");
    cmap.push_str("/CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def\n");
    cmap.push_str("/CMapName /Adobe-Identity-UCS def\n/CMapType 2 def\n");
    cmap.push_str("1 begincodespacerange\n<0000> <FFFF>\nendcodespacerange\n");
    let code_units = code_units.into_iter().collect::<Vec<_>>();
    for group in code_units.chunks(100) {
        cmap.push_str(&format!("{} beginbfchar\n", group.len()));
        for code_unit in group {
            cmap.push_str(&format!("<{code_unit:04X}> <{code_unit:04X}>\n"));
        }
        cmap.push_str("endbfchar\n");
    }
    cmap.push_str("endcmap\nCMapName currentdict /CMap defineresource pop\nend\nend");
    cmap
}

fn page_stream(page: &str) -> String {
    let mut stream = String::from("BT\n54 748 Td\n");
    for (index, line) in page.lines().enumerate() {
        if index > 0 {
            stream.push_str("0 -13 Td\n");
        }
        let mut ascii_run = None;
        for character in line.chars() {
            let ascii = character.is_ascii() && !character.is_ascii_control();
            if ascii_run != Some(ascii) {
                if let Some(previous_ascii) = ascii_run {
                    stream.push_str(if previous_ascii { ") Tj\n" } else { "> Tj\n" });
                }
                stream.push_str(if ascii {
                    "/F2 11 Tf\n("
                } else {
                    "/F1 11 Tf\n<"
                });
                ascii_run = Some(ascii);
            }
            if ascii {
                if matches!(character, '(' | ')' | '\\') {
                    stream.push('\\');
                }
                stream.push(character);
            } else {
                stream.push_str(&format!("{:04X}", pdf_code_unit(character)));
            }
        }
        if let Some(ascii) = ascii_run {
            stream.push_str(if ascii { ") Tj\n" } else { "> Tj\n" });
        }
    }
    stream.push_str("ET");
    stream
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_a_parseable_pdf_header_and_xref() {
        let path = std::env::temp_dir().join("minimal-pdf-converter-test-output.pdf");
        write_text_pdf(&path, "Hello\n世界").expect("write");
        let bytes = fs::read(&path).expect("read");
        assert!(bytes.starts_with(b"%PDF-1.7"));
        assert!(bytes.windows(5).any(|window| window == b"xref\n"));
        assert!(bytes
            .windows(b"STSong-Light".len())
            .any(|window| window == b"STSong-Light"));
        assert!(bytes.windows(b"4E16".len()).any(|window| window == b"4E16"));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn embeds_to_unicode_for_generated_text() {
        let path = std::env::temp_dir().join("minimal-pdf-converter-tounicode.pdf");
        write_text_pdf(&path, "Hello\n你好").expect("write");
        let text = crate::pdf::extract_text_from_path(&path, &crate::pdf::PageSelection::All)
            .expect("extract generated PDF")
            .text;
        assert_eq!(text, "Hello\n你好");
        let _ = fs::remove_file(path);
    }

    #[test]
    fn wraps_long_cjk_lines_within_pdf_page_margins() {
        let input = "中".repeat(96);
        let pages = paginate_pages(&[input.clone()], 58);
        assert_eq!(pages.len(), 1);
        let lines = pages[0].lines().collect::<Vec<_>>();
        assert_eq!(
            lines
                .iter()
                .map(|line| line.chars().count())
                .collect::<Vec<_>>(),
            [45, 45, 6]
        );
        assert_eq!(lines.concat(), input);
    }

    #[test]
    fn explicit_pages_do_not_get_an_extra_blank_page_at_line_limit() {
        let first = (0..55).map(|_| "row").collect::<Vec<_>>().join("\n");
        let pages = paginate_pages(&[first, "second".to_owned()], 55);
        assert_eq!(pages.len(), 2);
        assert!(pages[0].contains("row"));
        assert_eq!(pages[1], "second");
    }

    #[test]
    fn preserves_empty_source_pages_and_expands_tabs() {
        let pages = paginate_pages(
            &["one\ttwo".to_owned(), String::new(), "three".to_owned()],
            55,
        );
        assert_eq!(pages, ["one    two", "", "three"]);
    }

    #[test]
    fn round_trips_mixed_latin_and_cjk_with_escaped_punctuation() {
        let path = std::env::temp_dir().join("minimal-pdf-converter-mixed-fonts.pdf");
        write_text_pdf(&path, "Second page (中文) \\ sample").expect("write PDF");
        let text = crate::pdf::extract_text_from_path(&path, &crate::pdf::PageSelection::All)
            .expect("read generated PDF")
            .text;
        assert_eq!(text, "Second page (中文) \\ sample");
        fs::remove_file(path).expect("remove PDF");
    }

    #[test]
    fn rejects_supplementary_unicode_instead_of_silently_replacing_it() {
        let path = std::env::temp_dir().join("minimal-pdf-converter-emoji.pdf");
        assert!(matches!(
            write_text_pdf(&path, "emoji: 😀"),
            Err(PdfWriterError::UnsupportedUnicode)
        ));
        assert!(!path.exists());
    }

    #[test]
    fn splits_large_to_unicode_maps_into_bounded_blocks() {
        let text = (0x4e00..0x4e96)
            .filter_map(char::from_u32)
            .collect::<String>();
        let cmap = to_unicode_cmap(&[text]);
        assert!(cmap.contains("100 beginbfchar\n"));
        assert!(cmap.contains("50 beginbfchar\n"));
        assert_eq!(cmap.matches("endbfchar\n").count(), 2);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn rendered_two_page_text_keeps_legible_bottom_margin() {
        struct FixtureDir(std::path::PathBuf);

        impl Drop for FixtureDir {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }

        let root =
            std::env::temp_dir().join(format!("minimal-pdf-writer-visual-{}", std::process::id()));
        let pdf_path = root.join("mixed-two-page.pdf");
        let output_dir = root.join("png");
        fs::create_dir_all(&root).expect("create fixture directory");
        let _fixture = FixtureDir(root);

        let text = (0..58)
            .map(|line| match line {
                0 => "Latin ABC xyz 123 (top)".to_owned(),
                54 => "底部中文 Bottom sentinel 55".to_owned(),
                55 => "第二页中文".to_owned(),
                56 => "Latin glyph visibility".to_owned(),
                57 => "混合 CJK + ASCII (escaped) \\ end".to_owned(),
                _ => format!("中英文边界行 {:02} Latin", line + 1),
            })
            .collect::<Vec<_>>()
            .join("\n");
        write_text_pages_pdf(&pdf_path, &[text]).expect("write two-page PDF");
        let rendered = crate::render::render_pdf(
            &pdf_path,
            &output_dir,
            "mixed",
            &crate::render::RenderOptions {
                selection: crate::pdf::PageSelection::All,
                dpi: 150,
                format: crate::render::ImageFormat::Png,
                jpg_quality: 90,
                max_selected_pages: crate::pdf::MAX_PAGES,
                max_output_bytes: u64::MAX,
            },
            || false,
            |_, _, _| {},
        )
        .expect("render two-page PDF");
        assert_eq!(rendered.outputs.len(), 2);

        for (page_index, output) in rendered.outputs.iter().enumerate() {
            let file = fs::File::open(output).expect("open rendered page");
            let mut reader = png::Decoder::new(file).read_info().expect("read PNG info");
            let mut buffer = vec![0; reader.output_buffer_size()];
            let frame = reader.next_frame(&mut buffer).expect("decode PNG");
            assert_eq!(frame.color_type, png::ColorType::Rgb);
            let (width, height) = (frame.width as usize, frame.height as usize);
            assert_eq!(width, 1275);
            assert!((1650..=1651).contains(&height));
            let dark_pixels = |top: usize, bottom: usize| {
                (top..bottom).fold(0, |total, y| {
                    total
                        + (100..600)
                            .filter(|&x| {
                                let offset = (y * width + x) * 3;
                                buffer[offset..offset + 3]
                                    .iter()
                                    .all(|component| *component < 200)
                            })
                            .count()
                })
            };
            if page_index == 0 {
                assert!(dark_pixels(65, 101) > 100);
                assert!(dark_pixels(height - 150, height - 72) > 100);
                assert_eq!(dark_pixels(height - 72, height), 0);
            } else {
                assert!(dark_pixels(65, 101) > 100);
                assert!(dark_pixels(101, 137) > 100);
            }
        }
    }
}
