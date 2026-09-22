use super::{
    check_image_dimensions, next_output_bytes, write_image, ImageFormat, OutputFiles, RenderError,
    RenderOptions, RenderResult,
};
use crate::pdf::{PageSelection, PdfError};
use core_graphics::base::{kCGBitmapByteOrder32Big, kCGImageAlphaPremultipliedLast};
use core_graphics::color_space::CGColorSpace;
use core_graphics::context::CGContext;
use core_graphics::data_provider::CGDataProvider;
use core_graphics::geometry::{CGPoint, CGRect, CGSize};
use foreign_types::ForeignType;
use std::ffi::c_void;
use std::fs;
use std::path::Path;
use std::ptr;
use std::sync::Arc;

// CoreGraphics exposes these as opaque reference types. `c_void` keeps the
// Rust declaration ABI-safe without pretending to know their private layout.
type CGPDFDocumentRef = *mut c_void;
type CGPDFPageRef = *mut c_void;

const MEDIA_BOX: u32 = 0;
const CROP_BOX: u32 = 1;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPDFDocumentCreateWithProvider(
        provider: *mut core_graphics::sys::CGDataProvider,
    ) -> CGPDFDocumentRef;
    fn CGPDFDocumentRelease(document: CGPDFDocumentRef);
    fn CGPDFDocumentGetNumberOfPages(document: CGPDFDocumentRef) -> usize;
    fn CGPDFDocumentGetPage(document: CGPDFDocumentRef, page: usize) -> CGPDFPageRef;
    fn CGPDFPageGetBoxRect(page: CGPDFPageRef, box_type: u32) -> CGRect;
    fn CGContextDrawPDFPage(context: *mut core_graphics::sys::CGContext, page: CGPDFPageRef);
    fn CGBitmapContextCreate(
        data: *mut c_void,
        width: usize,
        height: usize,
        bits_per_component: usize,
        bytes_per_row: usize,
        space: *mut core_graphics::sys::CGColorSpace,
        bitmap_info: u32,
    ) -> *mut core_graphics::sys::CGContext;
    fn CGBitmapContextGetData(context: *mut core_graphics::sys::CGContext) -> *mut c_void;
}

struct Document(CGPDFDocumentRef);

impl Drop for Document {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the pointer is returned by CoreGraphics and is released
            // exactly once when this RAII wrapper is dropped.
            unsafe { CGPDFDocumentRelease(self.0) };
        }
    }
}

pub(crate) fn render_pdf_native(
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
    let bytes = fs::read(input)?;
    let provider = CGDataProvider::from_buffer(Arc::new(bytes));
    // SAFETY: `provider` owns a stable reference-counted byte buffer for the
    // entire lifetime of the CoreGraphics document.
    let document = Document(unsafe { CGPDFDocumentCreateWithProvider(provider.as_ptr()) });
    if document.0.is_null() {
        return Err(RenderError::Pdf(PdfError::CorruptedPdf));
    }
    // SAFETY: `document` is a valid CoreGraphics PDF document.
    let page_count = unsafe { CGPDFDocumentGetNumberOfPages(document.0) };
    if page_count == 0 {
        return Err(RenderError::Pdf(PdfError::CorruptedPdf));
    }
    if page_count > super::super::pdf::MAX_PAGES {
        return Err(RenderError::Pdf(PdfError::PageLimitExceeded));
    }
    let pages = selected_pages(&options.selection, page_count)?;
    if pages.len() > options.max_selected_pages {
        return Err(RenderError::Pdf(PdfError::PageLimitExceeded));
    }
    fs::create_dir_all(output_dir)?;
    let mut outputs = OutputFiles::new(pages.len());
    let mut output_bytes = 0u64;
    for (index, page_number) in pages.iter().copied().enumerate() {
        if should_stop() {
            return Err(RenderError::Pdf(PdfError::Cancelled));
        }
        progress(index, pages.len(), "rendering");
        if should_stop() {
            return Err(RenderError::Pdf(PdfError::Cancelled));
        }
        // SAFETY: `page_number` is validated against the document page count.
        let page = unsafe { CGPDFDocumentGetPage(document.0, page_number) };
        if page.is_null() {
            return Err(RenderError::Pdf(PdfError::CorruptedPdf));
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
    Ok(outputs.finish(pages.len()))
}

fn selected_pages(selection: &PageSelection, page_count: usize) -> Result<Vec<usize>, RenderError> {
    match selection {
        PageSelection::All => Ok((1..=page_count).collect()),
        PageSelection::Pages(pages) => {
            if pages.iter().any(|page| *page == 0 || *page > page_count) {
                return Err(RenderError::Pdf(PdfError::InvalidPageRange));
            }
            Ok(pages.iter().copied().collect())
        }
    }
}

fn rasterize_page(
    page: CGPDFPageRef,
    dpi: u16,
    format: ImageFormat,
) -> Result<(u32, u32, Vec<u8>), RenderError> {
    // SAFETY: `page` is a non-null page borrowed from a live document.
    let mut rect = unsafe { CGPDFPageGetBoxRect(page, MEDIA_BOX) };
    if rect.size.width <= 0.0 || rect.size.height <= 0.0 {
        // SAFETY: the crop box query has the same lifetime and validity rules.
        rect = unsafe { CGPDFPageGetBoxRect(page, CROP_BOX) };
    }
    let scale = f64::from(dpi) / 72.0;
    let width = (rect.size.width.max(1.0) * scale).ceil();
    let height = (rect.size.height.max(1.0) * scale).ceil();
    let width = u32::try_from(width as u64).map_err(|_| RenderError::PixelLimit)?;
    let height = u32::try_from(height as u64).map_err(|_| RenderError::PixelLimit)?;
    check_image_dimensions(width, height, format)?;
    let pixels = u64::from(width) * u64::from(height);

    let color_space = CGColorSpace::create_device_rgb();
    // The safe `CGContext::create_bitmap_context` wrapper asserts on a null
    // CoreGraphics result. Use the raw API so allocation failure is reported
    // as a normal conversion error instead of aborting the process.
    // SAFETY: all scalar values were bounded above, and `color_space` remains
    // alive for the duration of the CoreGraphics call.
    let raw_context = unsafe {
        CGBitmapContextCreate(
            ptr::null_mut(),
            width as usize,
            height as usize,
            8,
            0,
            color_space.as_ptr(),
            kCGBitmapByteOrder32Big | kCGImageAlphaPremultipliedLast,
        )
    };
    if raw_context.is_null() {
        return Err(RenderError::Encoding);
    }
    // SAFETY: CoreGraphics returned a non-null owned CGContext pointer.
    let context = unsafe { CGContext::from_ptr(raw_context) };
    context.set_rgb_fill_color(1.0, 1.0, 1.0, 1.0);
    context.fill_rect(CGRect::new(
        &CGPoint::new(0.0, 0.0),
        &CGSize::new(f64::from(width), f64::from(height)),
    ));
    context.save();
    context.translate(0.0, f64::from(height));
    context.scale(scale, -scale);
    context.translate(-rect.origin.x, -rect.origin.y);
    // SAFETY: `context` and `page` are valid CoreGraphics objects; this call
    // only draws into the context and does not retain either pointer.
    unsafe { CGContextDrawPDFPage(context.as_ptr(), page) };
    context.restore();
    context.flush();

    let stride = context.bytes_per_row();
    // SAFETY: `context` is a live bitmap context and CoreGraphics returns a
    // buffer covering `height * stride` bytes for this context.
    let data_ptr = unsafe { CGBitmapContextGetData(context.as_ptr()) };
    if data_ptr.is_null() {
        return Err(RenderError::Encoding);
    }
    // The bitmap context's first row is the bottom row in the CoreGraphics
    // coordinate system. PNG/JPG encoders expect rows from top to bottom.
    // SAFETY: the null check above and the context dimensions establish the
    // exact readable range.
    let data =
        unsafe { std::slice::from_raw_parts(data_ptr.cast::<u8>(), height as usize * stride) };
    let mut rgb = vec![255u8; pixels as usize * 3];
    for y in 0..height as usize {
        let source_y = height as usize - 1 - y;
        let row = data
            .get(source_y * stride..(source_y + 1) * stride)
            .ok_or(RenderError::Encoding)?;
        let target = &mut rgb[y * width as usize * 3..(y + 1) * width as usize * 3];
        for x in 0..width as usize {
            let source = x * 4;
            let destination = x * 3;
            target[destination] = row[source];
            target[destination + 1] = row[source + 1];
            target[destination + 2] = row[source + 2];
        }
    }
    Ok((width, height, rgb))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn colored_fixture() -> Vec<u8> {
        let content = "q\n1 0 0 rg\n0 396 612 396 re\nf\n0 0 1 rg\n0 0 612 396 re\nf\nQ\n";
        let objects = [
            "1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n".to_owned(),
            "2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n".to_owned(),
            "3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /ProcSet [/PDF] >> /Contents 4 0 R >>\nendobj\n".to_owned(),
            format!(
                "4 0 obj\n<< /Length {} >>\nstream\n{}endstream\nendobj\n",
                content.len(),
                content
            ),
        ];
        let mut pdf = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::with_capacity(objects.len() + 1);
        offsets.push(0usize);
        for object in &objects {
            offsets.push(pdf.len());
            pdf.extend_from_slice(object.as_bytes());
        }
        let xref_offset = pdf.len();
        pdf.extend_from_slice(format!("xref\n0 {}\n", objects.len() + 1).as_bytes());
        pdf.extend_from_slice(b"0000000000 65535 f \n");
        for offset in offsets.iter().skip(1) {
            pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_offset}\n%%EOF\n",
                objects.len() + 1
            )
            .as_bytes(),
        );
        pdf
    }

    fn test_paths(label: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let root =
            std::env::temp_dir().join(format!("minimal-pdf-native-{label}-{}", std::process::id()));
        (root.join("fixture.pdf"), root.join("output"))
    }

    #[test]
    fn rasterizes_vector_geometry_with_correct_orientation_and_channels() {
        let (input, output_dir) = test_paths("colors");
        fs::create_dir_all(input.parent().expect("fixture parent")).expect("create fixture dir");
        fs::write(&input, colored_fixture()).expect("write fixture");
        let result = render_pdf_native(
            &input,
            &output_dir,
            "colored",
            &RenderOptions {
                selection: PageSelection::All,
                dpi: 150,
                format: ImageFormat::Png,
                jpg_quality: 90,
                max_selected_pages: crate::pdf::MAX_PAGES,
                max_output_bytes: u64::MAX,
            },
            || false,
            |_, _, _| {},
        )
        .expect("render vector fixture");

        let file = fs::File::open(&result.outputs[0]).expect("open png");
        let decoder = png::Decoder::new(file);
        let mut reader = decoder.read_info().expect("read png info");
        let mut buffer = vec![0; reader.output_buffer_size()];
        let frame = reader.next_frame(&mut buffer).expect("decode png");
        assert_eq!(frame.color_type, png::ColorType::Rgb);
        assert_eq!(frame.bit_depth, png::BitDepth::Eight);
        let width = frame.width as usize;
        let height = frame.height as usize;
        let pixel = |x: usize, y: usize| {
            let offset = (y * width + x) * 3;
            &buffer[offset..offset + 3]
        };
        let top = pixel(width / 2, height / 8);
        let bottom = pixel(width / 2, height * 7 / 8);
        assert!(
            top[0] > 220 && top[1] < 40 && top[2] < 40,
            "top pixel: {top:?}"
        );
        assert!(
            bottom[0] < 40 && bottom[1] < 40 && bottom[2] > 220,
            "bottom pixel: {bottom:?}"
        );

        let _ = fs::remove_dir_all(input.parent().expect("fixture parent"));
    }

    #[test]
    fn rasterizes_jpeg_output() {
        let (input, output_dir) = test_paths("jpeg");
        fs::create_dir_all(input.parent().expect("fixture parent")).expect("create fixture dir");
        fs::write(&input, colored_fixture()).expect("write fixture");
        let result = render_pdf_native(
            &input,
            &output_dir,
            "colored",
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
        .expect("render jpeg fixture");
        let bytes = fs::read(&result.outputs[0]).expect("read jpeg");
        assert!(bytes.starts_with(b"\xff\xd8"));
        assert!(bytes.len() > 1024);
        let _ = fs::remove_dir_all(input.parent().expect("fixture parent"));
    }
}
