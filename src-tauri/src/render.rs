#[cfg(not(target_os = "windows"))]
use crate::pdf::{extract_pages_from_path_with_control, TextPage};
use crate::pdf::{PageSelection, PdfError};
use gif::{Encoder as GifEncoder, Frame as GifFrame};
use image_webp::{ColorType as WebpColorType, WebPEncoder};
use jpeg_encoder::{ColorType, Encoder as JpegEncoder};
use png::{BitDepth, ColorType as PngColorType, Encoder as PngEncoder};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use thiserror::Error;

#[cfg(target_os = "macos")]
mod native_render;
#[cfg(target_os = "windows")]
pub(crate) mod windows_render;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
    Png,
    Jpg,
    Bmp,
    Gif,
    Webp,
    Tiff,
}

impl ImageFormat {
    pub(crate) const fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpg => "jpg",
            Self::Bmp => "bmp",
            Self::Gif => "gif",
            Self::Webp => "webp",
            Self::Tiff => "tiff",
        }
    }
}

#[derive(Debug, Error)]
pub enum RenderError {
    #[error("Could not read the PDF page: {0}")]
    Pdf(#[from] PdfError),
    #[error("Image output failed: {0}")]
    Io(#[from] io::Error),
    #[error("The PDF page dimensions exceed the image safety limit")]
    PixelLimit,
    #[error("Image encoding failed")]
    Encoding,
    #[error("Page images exceed the temporary storage limit for this conversion")]
    OutputLimit,
    #[cfg(target_os = "windows")]
    #[error("The Windows PDF rendering runtime is missing or invalid")]
    NativeUnavailable,
    #[cfg(target_os = "windows")]
    #[error("The Windows PDF rendering process exited unexpectedly")]
    NativeFailed,
}

#[derive(Debug, Clone)]
pub struct RenderOptions {
    pub selection: PageSelection,
    pub dpi: u16,
    pub format: ImageFormat,
    pub jpg_quality: u8,
    pub max_selected_pages: usize,
    pub max_output_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct RenderResult {
    pub outputs: Vec<PathBuf>,
    pub selected_page_count: usize,
}

const MAX_PIXELS: u64 = 40_000_000;
const MAX_OUTPUT_BYTES: u64 = 1024 * 1024 * 1024;

pub(crate) struct OutputFiles {
    paths: Vec<PathBuf>,
    keep: bool,
}

impl OutputFiles {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            paths: Vec::with_capacity(capacity),
            keep: false,
        }
    }

    pub(crate) fn prepare(&mut self, path: &Path) -> Result<(), RenderError> {
        match fs::symlink_metadata(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.paths.push(path.to_path_buf());
                Ok(())
            }
            Err(error) => Err(error.into()),
            Ok(_) => Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "Image output already exists",
            )
            .into()),
        }
    }

    pub(crate) fn finish(mut self, selected_page_count: usize) -> RenderResult {
        self.keep = true;
        RenderResult {
            outputs: std::mem::take(&mut self.paths),
            selected_page_count,
        }
    }
}

impl Drop for OutputFiles {
    fn drop(&mut self) {
        if !self.keep {
            for path in &self.paths {
                let _ = fs::remove_file(path);
            }
        }
    }
}

pub(crate) fn check_image_dimensions(
    width: u32,
    height: u32,
    format: ImageFormat,
) -> Result<(), RenderError> {
    let pixels = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or(RenderError::PixelLimit)?;
    if width == 0 || height == 0 || pixels > MAX_PIXELS {
        return Err(RenderError::PixelLimit);
    }
    match format {
        ImageFormat::Jpg | ImageFormat::Gif
            if u16::try_from(width).is_err() || u16::try_from(height).is_err() =>
        {
            Err(RenderError::PixelLimit)
        }
        ImageFormat::Webp if width > 16_384 || height > 16_384 => Err(RenderError::PixelLimit),
        ImageFormat::Png
        | ImageFormat::Jpg
        | ImageFormat::Bmp
        | ImageFormat::Gif
        | ImageFormat::Webp
        | ImageFormat::Tiff => Ok(()),
    }
}

pub(crate) fn next_output_bytes(
    current: u64,
    page_bytes: u64,
    requested_limit: u64,
) -> Result<u64, RenderError> {
    let total = current
        .checked_add(page_bytes)
        .ok_or(RenderError::OutputLimit)?;
    if total > requested_limit.min(MAX_OUTPUT_BYTES) {
        return Err(RenderError::OutputLimit);
    }
    Ok(total)
}

fn check_rgb_buffer(
    width: u32,
    height: u32,
    pixels: &[u8],
    format: ImageFormat,
) -> Result<(), RenderError> {
    check_image_dimensions(width, height, format)?;
    let expected = u64::from(width) * u64::from(height) * 3;
    if u64::try_from(pixels.len()).ok() != Some(expected) {
        return Err(RenderError::Encoding);
    }
    Ok(())
}

pub(crate) fn write_image(
    path: &Path,
    width: u32,
    height: u32,
    pixels: &[u8],
    format: ImageFormat,
    jpg_quality: u8,
) -> Result<(), RenderError> {
    check_rgb_buffer(width, height, pixels, format)?;
    match format {
        ImageFormat::Png => write_png(path, width, height, pixels),
        ImageFormat::Jpg => write_jpg(path, width, height, pixels, jpg_quality),
        ImageFormat::Bmp => write_bmp(path, width, height, pixels),
        ImageFormat::Gif => write_gif(path, width, height, pixels),
        ImageFormat::Webp => write_webp(path, width, height, pixels),
        ImageFormat::Tiff => write_tiff(path, width, height, pixels),
    }
}

pub fn render_pdf(
    input: &Path,
    output_dir: &Path,
    stem: &str,
    options: &RenderOptions,
    should_stop: impl Fn() -> bool,
    mut progress: impl FnMut(usize, usize, &str),
) -> Result<RenderResult, RenderError> {
    if !(150..=300).contains(&options.dpi) {
        return Err(RenderError::PixelLimit);
    }
    if options.max_output_bytes == 0 {
        return Err(RenderError::OutputLimit);
    }
    if options.max_selected_pages == 0 || options.max_selected_pages > crate::pdf::MAX_PAGES {
        return Err(RenderError::Pdf(PdfError::PageLimitExceeded));
    }
    if let PageSelection::Pages(pages) = &options.selection {
        if pages.len() > options.max_selected_pages {
            return Err(RenderError::Pdf(PdfError::PageLimitExceeded));
        }
    }
    #[cfg(target_os = "windows")]
    {
        return windows_render::render_pdf_native(
            input,
            output_dir,
            stem,
            options,
            should_stop,
            progress,
        );
    }
    #[cfg(not(target_os = "windows"))]
    {
        #[cfg(target_os = "macos")]
        {
            match native_render::render_pdf_native(
                input,
                output_dir,
                stem,
                options,
                &should_stop,
                &mut progress,
            ) {
                Ok(result) => return Ok(result),
                Err(error) if !cfg!(test) => return Err(error),
                Err(_) => {}
            }
        }
        let pages = extract_pages_from_path_with_control(input, &options.selection, &should_stop)?;
        let page_count = pages.len();
        let selected_page_count = pages.len();
        if pages.is_empty() {
            return Err(RenderError::Pdf(PdfError::CorruptedPdf));
        }
        if pages.len() > options.max_selected_pages {
            return Err(RenderError::Pdf(PdfError::PageLimitExceeded));
        }
        fs::create_dir_all(output_dir)?;
        let mut outputs = OutputFiles::new(pages.len());
        let mut output_bytes = 0u64;
        for (index, page) in pages.iter().enumerate() {
            if should_stop() {
                return Err(RenderError::Pdf(PdfError::Cancelled));
            }
            progress(index, page_count, "rendering");
            if should_stop() {
                return Err(RenderError::Pdf(PdfError::Cancelled));
            }
            let (width, height, pixels) = rasterize_page(page, options.dpi, options.format)?;
            if should_stop() {
                return Err(RenderError::Pdf(PdfError::Cancelled));
            }
            let extension = options.format.extension();
            let output = output_dir.join(format!("{stem}-{:03}.{extension}", index + 1));
            outputs.prepare(&output)?;
            write_image(
                &output,
                width,
                height,
                &pixels,
                options.format,
                options.jpg_quality,
            )?;
            output_bytes = next_output_bytes(
                output_bytes,
                fs::metadata(&output)?.len(),
                options.max_output_bytes,
            )?;
        }
        if should_stop() {
            return Err(RenderError::Pdf(PdfError::Cancelled));
        }
        Ok(outputs.finish(selected_page_count))
    }
}

#[cfg(not(target_os = "windows"))]
fn rasterize_page(
    page: &TextPage,
    dpi: u16,
    format: ImageFormat,
) -> Result<(u32, u32, Vec<u8>), RenderError> {
    let scale = f64::from(dpi) / 72.0;
    let width = (f64::from(page.width_points.max(1)) * scale).round() as u32;
    let height = (f64::from(page.height_points.max(1)) * scale).round() as u32;
    check_image_dimensions(width, height, format)?;
    let pixels = u64::from(width) * u64::from(height);
    let mut image = vec![255u8; pixels as usize * 3];
    let margin = (f64::from(dpi) * 0.7).round() as u32;
    let glyph_scale = (u32::from(dpi) / 72).max(2);
    let glyph_width = 6 * glyph_scale;
    let glyph_height = 9 * glyph_scale;
    let max_columns = width.saturating_sub(margin * 2) / glyph_width.max(1);
    let max_lines = height.saturating_sub(margin * 2) / glyph_height.max(1);
    for (line_index, line) in page.text.lines().take(max_lines as usize).enumerate() {
        let y = margin + line_index as u32 * glyph_height;
        for (column, character) in line.chars().take(max_columns as usize).enumerate() {
            let x = margin + column as u32 * glyph_width;
            draw_glyph(&mut image, width, height, x, y, glyph_scale, character);
        }
    }
    Ok((width, height, image))
}

#[cfg(not(target_os = "windows"))]
fn draw_glyph(
    image: &mut [u8],
    width: u32,
    height: u32,
    x: u32,
    y: u32,
    scale: u32,
    character: char,
) {
    let glyph = glyph_rows(character);
    for (row, bits) in glyph.iter().enumerate() {
        for column in 0..5u32 {
            if bits & (1 << (4 - column)) == 0 {
                continue;
            }
            for dy in 0..scale {
                for dx in 0..scale {
                    let px = x + column * scale + dx;
                    let py = y + row as u32 * scale + dy;
                    if px >= width || py >= height {
                        continue;
                    }
                    let offset = ((py * width + px) * 3) as usize;
                    image[offset] = 24;
                    image[offset + 1] = 34;
                    image[offset + 2] = 48;
                }
            }
        }
    }
}

#[cfg(not(target_os = "windows"))]
fn glyph_rows(character: char) -> [u8; 7] {
    match character.to_ascii_uppercase() {
        'A' => [
            0b01110, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001,
        ],
        'B' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10001, 0b10001, 0b11110,
        ],
        'C' => [
            0b01111, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b01111,
        ],
        'D' => [
            0b11110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b11110,
        ],
        'E' => [
            0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111,
        ],
        'F' => [
            0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b10000,
        ],
        'G' => [
            0b01111, 0b10000, 0b10000, 0b10111, 0b10001, 0b10001, 0b01111,
        ],
        'H' => [
            0b10001, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001,
        ],
        'I' => [
            0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b11111,
        ],
        'J' => [
            0b00111, 0b00010, 0b00010, 0b00010, 0b00010, 0b10010, 0b01100,
        ],
        'K' => [
            0b10001, 0b10010, 0b10100, 0b11000, 0b10100, 0b10010, 0b10001,
        ],
        'L' => [
            0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b11111,
        ],
        'M' => [
            0b10001, 0b11011, 0b10101, 0b10101, 0b10001, 0b10001, 0b10001,
        ],
        'N' => [
            0b10001, 0b11001, 0b10101, 0b10011, 0b10001, 0b10001, 0b10001,
        ],
        'O' => [
            0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110,
        ],
        'P' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000, 0b10000,
        ],
        'Q' => [
            0b01110, 0b10001, 0b10001, 0b10001, 0b10101, 0b10010, 0b01101,
        ],
        'R' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10100, 0b10010, 0b10001,
        ],
        'S' => [
            0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110,
        ],
        'T' => [
            0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100,
        ],
        'U' => [
            0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110,
        ],
        'V' => [
            0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01010, 0b00100,
        ],
        'W' => [
            0b10001, 0b10001, 0b10001, 0b10101, 0b10101, 0b11011, 0b10001,
        ],
        'X' => [
            0b10001, 0b10001, 0b01010, 0b00100, 0b01010, 0b10001, 0b10001,
        ],
        'Y' => [
            0b10001, 0b10001, 0b01010, 0b00100, 0b00100, 0b00100, 0b00100,
        ],
        'Z' => [
            0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b10000, 0b11111,
        ],
        '0' => [
            0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110,
        ],
        '1' => [
            0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
        ],
        '2' => [
            0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111,
        ],
        '3' => [
            0b11110, 0b00001, 0b00001, 0b01110, 0b00001, 0b00001, 0b11110,
        ],
        '4' => [
            0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010,
        ],
        '5' => [
            0b11111, 0b10000, 0b10000, 0b11110, 0b00001, 0b00001, 0b11110,
        ],
        '6' => [
            0b01110, 0b10000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110,
        ],
        '7' => [
            0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000,
        ],
        '8' => [
            0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110,
        ],
        '9' => [
            0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00001, 0b01110,
        ],
        '-' => [0, 0, 0, 0b11111, 0, 0, 0],
        '_' => [0, 0, 0, 0, 0, 0, 0b11111],
        '.' => [0, 0, 0, 0, 0, 0, 0b00100],
        ',' => [0, 0, 0, 0, 0, 0b00100, 0b01000],
        ':' => [0, 0b00100, 0, 0, 0, 0b00100, 0],
        '!' => [0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0, 0b00100],
        '?' => [0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0, 0b00100],
        ' ' => [0; 7],
        _ => [
            0b11111, 0b10001, 0b10101, 0b10001, 0b10101, 0b10001, 0b11111,
        ],
    }
}

pub(crate) fn write_png(
    path: &Path,
    width: u32,
    height: u32,
    pixels: &[u8],
) -> Result<(), RenderError> {
    let writer = create_output(path)?;
    let mut encoder = PngEncoder::new(writer, width, height);
    encoder.set_color(PngColorType::Rgb);
    encoder.set_depth(BitDepth::Eight);
    let mut writer = encoder.write_header().map_err(|_| RenderError::Encoding)?;
    writer
        .write_image_data(pixels)
        .map_err(|_| RenderError::Encoding)?;
    Ok(())
}

pub(crate) fn write_jpg(
    path: &Path,
    width: u32,
    height: u32,
    pixels: &[u8],
    quality: u8,
) -> Result<(), RenderError> {
    let width = u16::try_from(width).map_err(|_| RenderError::PixelLimit)?;
    let height = u16::try_from(height).map_err(|_| RenderError::PixelLimit)?;
    let mut writer = create_output(path)?;
    let encoder = JpegEncoder::new(&mut writer, quality.clamp(1, 100));
    encoder
        .encode(pixels, width, height, ColorType::Rgb)
        .map_err(|_| RenderError::Encoding)?;
    writer.flush()?;
    Ok(())
}

fn create_output(path: &Path) -> Result<io::BufWriter<fs::File>, RenderError> {
    Ok(io::BufWriter::new(
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?,
    ))
}

fn write_bmp(path: &Path, width: u32, height: u32, pixels: &[u8]) -> Result<(), RenderError> {
    let width_signed = i32::try_from(width).map_err(|_| RenderError::PixelLimit)?;
    let height_signed = i32::try_from(height).map_err(|_| RenderError::PixelLimit)?;
    let row_bytes = usize::try_from(width)
        .ok()
        .and_then(|width| width.checked_mul(3))
        .ok_or(RenderError::PixelLimit)?;
    let row_stride = row_bytes
        .checked_add(3)
        .map(|bytes| bytes & !3)
        .ok_or(RenderError::PixelLimit)?;
    let pixel_bytes = row_stride
        .checked_mul(height as usize)
        .and_then(|bytes| u32::try_from(bytes).ok())
        .ok_or(RenderError::PixelLimit)?;
    let file_bytes = pixel_bytes.checked_add(54).ok_or(RenderError::PixelLimit)?;

    let mut writer = create_output(path)?;
    writer.write_all(b"BM")?;
    writer.write_all(&file_bytes.to_le_bytes())?;
    writer.write_all(&[0; 4])?;
    writer.write_all(&54u32.to_le_bytes())?;
    writer.write_all(&40u32.to_le_bytes())?;
    writer.write_all(&width_signed.to_le_bytes())?;
    writer.write_all(&height_signed.to_le_bytes())?;
    writer.write_all(&1u16.to_le_bytes())?;
    writer.write_all(&24u16.to_le_bytes())?;
    writer.write_all(&0u32.to_le_bytes())?;
    writer.write_all(&pixel_bytes.to_le_bytes())?;
    writer.write_all(&[0; 16])?;

    let mut output_row = vec![0u8; row_stride];
    for source_row in pixels.chunks_exact(row_bytes).rev() {
        for (source, target) in source_row
            .chunks_exact(3)
            .zip(output_row[..row_bytes].chunks_exact_mut(3))
        {
            target.copy_from_slice(&[source[2], source[1], source[0]]);
        }
        writer.write_all(&output_row)?;
    }
    writer.flush()?;
    Ok(())
}

fn write_gif(path: &Path, width: u32, height: u32, pixels: &[u8]) -> Result<(), RenderError> {
    let width = u16::try_from(width).map_err(|_| RenderError::PixelLimit)?;
    let height = u16::try_from(height).map_err(|_| RenderError::PixelLimit)?;
    let writer = create_output(path)?;
    let mut encoder =
        GifEncoder::new(writer, width, height, &[]).map_err(|_| RenderError::Encoding)?;
    let frame = GifFrame::from_rgb_speed(width, height, pixels, 10);
    encoder
        .write_frame(&frame)
        .map_err(|_| RenderError::Encoding)?;
    let mut writer = encoder.into_inner().map_err(|_| RenderError::Encoding)?;
    writer.flush()?;
    Ok(())
}

fn write_webp(path: &Path, width: u32, height: u32, pixels: &[u8]) -> Result<(), RenderError> {
    let mut writer = create_output(path)?;
    WebPEncoder::new(&mut writer)
        .encode(pixels, width, height, WebpColorType::Rgb8)
        .map_err(|_| RenderError::Encoding)?;
    writer.flush()?;
    Ok(())
}

fn tiff_output_error(error: tiff::TiffError) -> RenderError {
    match error {
        tiff::TiffError::IoError(error) => RenderError::Io(error),
        _ => RenderError::Encoding,
    }
}

fn write_tiff(path: &Path, width: u32, height: u32, pixels: &[u8]) -> Result<(), RenderError> {
    let mut writer = create_output(path)?;
    {
        let mut encoder = tiff::encoder::TiffEncoder::new(&mut writer)
            .map_err(tiff_output_error)?
            .with_compression(tiff::encoder::Compression::Deflate(Default::default()))
            .with_predictor(tiff::encoder::Predictor::Horizontal);
        encoder
            .write_image::<tiff::encoder::colortype::RGB8>(width, height, pixels)
            .map_err(tiff_output_error)?;
    }
    writer.flush()?;
    Ok(())
}

#[cfg(all(test, not(target_os = "windows")))]
mod tests {
    use super::*;
    use std::fs;

    fn fixture() -> Vec<u8> {
        b"%PDF-1.7\n1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj\n2 0 obj << /Type /Pages /Kids [3 0 R] /Count 1 >> endobj\n3 0 obj << /Type /Page /Parent 2 0 R /Contents 4 0 R >> endobj\n4 0 obj << /Length 22 >>\nstream\nBT (image test) Tj ET\nendstream\nendobj\ntrailer << /Root 1 0 R >>\n%%EOF\n".to_vec()
    }

    #[test]
    fn writes_png_and_jpg_pages() {
        let input = std::env::temp_dir().join("minimal-pdf-render-test.pdf");
        let output = std::env::temp_dir().join("minimal-pdf-render-output");
        fs::write(&input, fixture()).expect("write fixture");
        let png = render_pdf(
            &input,
            &output,
            "page",
            &RenderOptions {
                selection: PageSelection::All,
                dpi: 150,
                format: ImageFormat::Png,
                jpg_quality: 85,
                max_selected_pages: crate::pdf::MAX_PAGES,
                max_output_bytes: u64::MAX,
            },
            || false,
            |_, _, _| {},
        )
        .expect("png");
        assert!(fs::read(&png.outputs[0])
            .expect("read png")
            .starts_with(b"\x89PNG"));
        let jpg = render_pdf(
            &input,
            &output,
            "page-jpg",
            &RenderOptions {
                selection: PageSelection::All,
                dpi: 150,
                format: ImageFormat::Jpg,
                jpg_quality: 85,
                max_selected_pages: crate::pdf::MAX_PAGES,
                max_output_bytes: u64::MAX,
            },
            || false,
            |_, _, _| {},
        )
        .expect("jpg");
        assert!(fs::read(&jpg.outputs[0])
            .expect("read jpg")
            .starts_with(b"\xff\xd8"));
        let _ = fs::remove_file(input);
        let _ = fs::remove_dir_all(output);
    }

    #[test]
    fn bmp_gif_webp_and_tiff_encode_decodable_page_colors() {
        let root = std::env::temp_dir().join(format!(
            "minimal-pdf-image-codecs-{}",
            uuid::Uuid::new_v4().simple()
        ));
        fs::create_dir(&root).expect("create image directory");
        let pixels = [
            255, 0, 0, 0, 255, 0, // top row: red, green
            0, 0, 255, 255, 255, 255, // bottom row: blue, white
        ];

        let bmp = root.join("page.bmp");
        write_bmp(&bmp, 2, 2, &pixels).expect("encode BMP");
        let bytes = fs::read(bmp).expect("read BMP");
        assert_eq!(&bytes[..2], b"BM");
        assert_eq!(
            u32::from_le_bytes(bytes[2..6].try_into().expect("size")),
            70
        );
        assert_eq!(&bytes[54..62], &[255, 0, 0, 255, 255, 255, 0, 0]);
        assert_eq!(&bytes[62..70], &[0, 0, 255, 0, 255, 0, 0, 0]);

        let gif = root.join("page.gif");
        write_gif(&gif, 2, 2, &pixels).expect("encode GIF");
        let mut decoder = gif::DecodeOptions::new();
        decoder.set_color_output(gif::ColorOutput::RGBA);
        let mut reader = decoder
            .read_info(fs::File::open(gif).expect("open GIF"))
            .expect("decode GIF header");
        let frame = reader
            .read_next_frame()
            .expect("decode GIF frame")
            .expect("single GIF frame");
        assert_eq!((frame.width, frame.height), (2, 2));
        for (actual, original) in frame.buffer.chunks_exact(4).zip(pixels.chunks_exact(3)) {
            assert_eq!(&actual[..3], original);
            assert_eq!(actual[3], 255);
        }
        assert!(reader.read_next_frame().expect("GIF trailer").is_none());

        let webp = root.join("page.webp");
        write_webp(&webp, 2, 2, &pixels).expect("encode WebP");
        let mut decoder = image_webp::WebPDecoder::new(io::BufReader::new(
            fs::File::open(webp).expect("open WebP"),
        ))
        .expect("decode WebP header");
        assert_eq!(decoder.dimensions(), (2, 2));
        assert!(!decoder.is_lossy());
        let mut decoded = [0u8; 12];
        decoder.read_image(&mut decoded).expect("decode WebP image");
        assert_eq!(decoded, pixels);

        let tiff = root.join("page.tiff");
        write_tiff(&tiff, 2, 2, &pixels).expect("encode TIFF");
        let mut decoder = tiff::decoder::Decoder::new(io::BufReader::new(
            fs::File::open(tiff).expect("open TIFF"),
        ))
        .expect("decode TIFF header");
        assert_eq!(decoder.dimensions().expect("TIFF dimensions"), (2, 2));
        assert_eq!(
            decoder.colortype().expect("TIFF color type"),
            tiff::ColorType::RGB(8)
        );
        assert!(!decoder.more_images());
        match decoder.read_image().expect("decode TIFF pixels") {
            tiff::decoder::DecodingResult::U8(decoded) => assert_eq!(decoded, pixels),
            _ => panic!("TIFF encoder must produce 8-bit RGB"),
        }
        fs::remove_dir_all(root).expect("remove image directory");
    }

    #[test]
    fn renders_each_extended_format_as_a_separate_page_file() {
        let root = std::env::temp_dir().join(format!(
            "minimal-pdf-raster-outputs-{}",
            uuid::Uuid::new_v4().simple()
        ));
        fs::create_dir(&root).expect("create fixture directory");
        let input = root.join("source.pdf");
        crate::pdf_writer::write_text_pdf(&input, "Format names").expect("write page");
        for (format, extension, header) in [
            (ImageFormat::Bmp, "bmp", b"BM".as_slice()),
            (ImageFormat::Gif, "gif", b"GIF89a".as_slice()),
            (ImageFormat::Webp, "webp", b"RIFF".as_slice()),
            (ImageFormat::Tiff, "tiff", b"II*\0".as_slice()),
        ] {
            let mut progress = Vec::new();
            let result = render_pdf(
                &input,
                &root.join(extension),
                "page",
                &RenderOptions {
                    selection: PageSelection::All,
                    dpi: 150,
                    format,
                    jpg_quality: 85,
                    max_selected_pages: 200,
                    max_output_bytes: MAX_OUTPUT_BYTES,
                },
                || false,
                |index, total, phase| progress.push((index, total, phase.to_owned())),
            )
            .expect("render requested format");
            assert_eq!(result.selected_page_count, 1);
            assert!(result.outputs[0].ends_with(format!("page-001.{extension}")));
            assert!(fs::read(&result.outputs[0])
                .expect("read rendered page")
                .starts_with(header));
            assert_eq!(progress, [(0, 1, "rendering".to_owned())]);
        }
        fs::remove_dir_all(root).expect("remove fixture directory");
    }

    #[test]
    fn renders_pdf_pages_as_individual_single_image_tiffs() {
        let root = std::env::temp_dir().join(format!(
            "minimal-pdf-tiff-pages-{}",
            uuid::Uuid::new_v4().simple()
        ));
        fs::create_dir(&root).expect("create fixture directory");
        let input = root.join("two-pages.pdf");
        crate::pdf_writer::write_pages_pdf(&input, &["first".to_owned(), "second".to_owned()])
            .expect("write two pages");
        let output = root.join("tiff");

        let result = render_pdf(
            &input,
            &output,
            "page",
            &RenderOptions {
                selection: PageSelection::All,
                dpi: 150,
                format: ImageFormat::Tiff,
                jpg_quality: 85,
                max_selected_pages: 2,
                max_output_bytes: MAX_OUTPUT_BYTES,
            },
            || false,
            |_, _, _| {},
        )
        .expect("render TIFF pages");

        assert_eq!(result.selected_page_count, 2);
        assert!(result.outputs[0].ends_with("page-001.tiff"));
        assert!(result.outputs[1].ends_with("page-002.tiff"));
        for path in &result.outputs {
            let mut decoder = tiff::decoder::Decoder::new(io::BufReader::new(
                fs::File::open(path).expect("open rendered TIFF"),
            ))
            .expect("read rendered TIFF");
            assert_eq!(
                decoder.colortype().expect("rendered TIFF color"),
                tiff::ColorType::RGB(8)
            );
            assert!(!decoder.more_images());
        }
        fs::remove_dir_all(root).expect("remove fixture directory");
    }

    #[test]
    fn image_formats_reject_oversized_dimensions_and_malformed_rgb() {
        assert!(matches!(
            check_image_dimensions(65_536, 1, ImageFormat::Gif),
            Err(RenderError::PixelLimit)
        ));
        assert!(matches!(
            check_image_dimensions(16_385, 1, ImageFormat::Webp),
            Err(RenderError::PixelLimit)
        ));
        assert!(matches!(
            check_image_dimensions(40_000_001, 1, ImageFormat::Bmp),
            Err(RenderError::PixelLimit)
        ));
        assert!(matches!(
            check_rgb_buffer(2, 2, &[0; 11], ImageFormat::Webp),
            Err(RenderError::Encoding)
        ));
        assert_eq!(
            next_output_bytes(MAX_OUTPUT_BYTES - 1, 1, u64::MAX).expect("hard cap"),
            MAX_OUTPUT_BYTES
        );
        assert!(matches!(
            next_output_bytes(MAX_OUTPUT_BYTES, 1, u64::MAX),
            Err(RenderError::OutputLimit)
        ));
        assert!(matches!(
            next_output_bytes(u64::MAX, 1, u64::MAX),
            Err(RenderError::OutputLimit)
        ));
    }

    #[test]
    fn rejects_page_limit_before_creating_presentation_images() {
        let root = std::env::temp_dir().join(format!(
            "minimal-pdf-render-limit-{}",
            uuid::Uuid::new_v4().simple()
        ));
        fs::create_dir(&root).expect("create fixture directory");
        let input = root.join("two-pages.pdf");
        crate::pdf_writer::write_pages_pdf(&input, &["first".to_owned(), "second".to_owned()])
            .expect("write two pages");
        let output = root.join("images");
        let result = render_pdf(
            &input,
            &output,
            "slide",
            &RenderOptions {
                selection: PageSelection::All,
                dpi: 150,
                format: ImageFormat::Jpg,
                jpg_quality: 90,
                max_selected_pages: 1,
                max_output_bytes: 512 * 1024 * 1024,
            },
            || false,
            |_, _, _| {},
        );
        assert!(matches!(
            result,
            Err(RenderError::Pdf(PdfError::PageLimitExceeded))
        ));
        assert!(!output.exists());
        fs::remove_dir_all(root).expect("remove fixture directory");
    }

    #[test]
    fn rejects_image_budget_after_rendering_one_page() {
        let root = std::env::temp_dir().join(format!(
            "minimal-pdf-render-budget-{}",
            uuid::Uuid::new_v4().simple()
        ));
        fs::create_dir(&root).expect("create fixture directory");
        let input = root.join("one-page.pdf");
        crate::pdf_writer::write_text_pdf(&input, "budget").expect("write page");
        let output = root.join("images");
        let result = render_pdf(
            &input,
            &output,
            "slide",
            &RenderOptions {
                selection: PageSelection::All,
                dpi: 150,
                format: ImageFormat::Tiff,
                jpg_quality: 90,
                max_selected_pages: 200,
                max_output_bytes: 1,
            },
            || false,
            |_, _, _| {},
        );
        assert!(matches!(result, Err(RenderError::OutputLimit)));
        assert_eq!(fs::read_dir(output).expect("output directory").count(), 0);
        fs::remove_dir_all(root).expect("remove fixture directory");
    }

    #[test]
    fn cancellation_after_first_page_removes_partial_output() {
        let root = std::env::temp_dir().join(format!(
            "minimal-pdf-render-cancel-{}",
            uuid::Uuid::new_v4().simple()
        ));
        fs::create_dir(&root).expect("create fixture directory");
        let input = root.join("two-pages.pdf");
        crate::pdf_writer::write_pages_pdf(&input, &["first".to_owned(), "second".to_owned()])
            .expect("write two pages");
        let output = root.join("images");
        let cancelled = std::cell::Cell::new(false);
        let result = render_pdf(
            &input,
            &output,
            "page",
            &RenderOptions {
                selection: PageSelection::All,
                dpi: 150,
                format: ImageFormat::Tiff,
                jpg_quality: 85,
                max_selected_pages: 200,
                max_output_bytes: MAX_OUTPUT_BYTES,
            },
            || cancelled.get(),
            |index, total, _| {
                assert_eq!(total, 2);
                if index == 1 {
                    cancelled.set(true);
                }
            },
        );
        assert!(matches!(result, Err(RenderError::Pdf(PdfError::Cancelled))));
        assert_eq!(fs::read_dir(output).expect("output directory").count(), 0);
        fs::remove_dir_all(root).expect("remove fixture directory");
    }
}
