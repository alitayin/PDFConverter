//! Converts local PNG, JPEG, GIF, WebP, TIFF, or BMP images into image-backed PDFs.

use flate2::write::ZlibEncoder;
use flate2::Compression;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use thiserror::Error;

pub(crate) const MAX_INPUT_BYTES: u64 = 64 * 1024 * 1024;
const MAX_BATCH_INPUT_BYTES: u64 = 512 * 1024 * 1024;
const MAX_IMAGES: usize = 200;
const MAX_PIXELS: u64 = 12_000_000;
const MAX_SIDE: u32 = 20_000;
const MAX_DECODED_BYTES: usize = 48 * 1024 * 1024;
const MAX_TIFF_IFD_VALUE_BYTES: usize = 1024 * 1024;
const MAX_TIFF_INTERMEDIATE_BYTES: usize = 16 * 1024 * 1024;
const MAX_PDF_BYTES: u64 = 128 * 1024 * 1024;
const MAX_XREF_OFFSET: u64 = 9_999_999_999;
const MAX_PAGE_SIDE: f64 = 14_400.0;

/// Failures while parsing an image or producing its single-page PDF.
#[derive(Debug, Error)]
pub enum ImagePdfError {
    #[error("The image input must be a local regular PNG/JPEG/GIF/WebP/TIFF/BMP file and cannot be a symlink")]
    InvalidInput,
    #[error("The PDF output must be an absolute path in an existing directory")]
    InvalidOutputPath,
    #[error("The image file is invalid or corrupted")]
    InvalidImage,
    #[error("This image encoding, color profile, animation, or EXIF rotation is not supported")]
    UnsupportedImage,
    #[error("Image dimensions or decode memory exceed the safety limit")]
    TooLarge,
    #[error("The generated PDF exceeds the safety limit")]
    OutputTooLarge,
    #[error("Image to PDF conversion cancelled")]
    Cancelled,
    #[error("Could not read the image: {0}")]
    InputIo(#[source] io::Error),
    #[error("PDF output failed: {0}")]
    OutputIo(#[source] io::Error),
}

impl ImagePdfError {
    /// Returns the stable worker error code for this failure class.
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidInput => "PATH_NOT_ALLOWED",
            Self::InvalidOutputPath => "OUTPUT_PATH_NOT_ALLOWED",
            Self::InvalidImage => "CORRUPTED_IMAGE",
            Self::UnsupportedImage => "UNSUPPORTED_FORMAT",
            Self::TooLarge => "INPUT_TOO_LARGE",
            Self::OutputTooLarge => "OUTPUT_SIZE_EXCEEDED",
            Self::Cancelled => "CANCELLED",
            Self::InputIo(_) => "INPUT_READ_FAILED",
            Self::OutputIo(_) => "OUTPUT_WRITE_FAILED",
        }
    }
}

struct RasterImage {
    width: u32,
    height: u32,
    color: png::ColorType,
    pixels: Vec<u8>,
}

struct JpegImage {
    width: u32,
    height: u32,
    components: u8,
    color_transform: u8,
    length: u64,
    input: File,
}

enum InputImage {
    Png(RasterImage),
    Jpeg(JpegImage),
    Gif(RasterImage),
    Webp(RasterImage),
    Tiff(RasterImage),
    Bmp(RasterImage),
}

impl InputImage {
    fn dimensions(&self) -> (u32, u32) {
        match self {
            Self::Png(image)
            | Self::Gif(image)
            | Self::Webp(image)
            | Self::Tiff(image)
            | Self::Bmp(image) => (image.width, image.height),
            Self::Jpeg(image) => (image.width, image.height),
        }
    }

    fn has_alpha(&self) -> bool {
        matches!(
            self,
            Self::Png(RasterImage {
                color: png::ColorType::Rgba | png::ColorType::GrayscaleAlpha,
                ..
            }) | Self::Gif(RasterImage {
                color: png::ColorType::Rgba | png::ColorType::GrayscaleAlpha,
                ..
            }) | Self::Webp(RasterImage {
                color: png::ColorType::Rgba | png::ColorType::GrayscaleAlpha,
                ..
            }) | Self::Tiff(RasterImage {
                color: png::ColorType::Rgba | png::ColorType::GrayscaleAlpha,
                ..
            }) | Self::Bmp(RasterImage {
                color: png::ColorType::Rgba | png::ColorType::GrayscaleAlpha,
                ..
            })
        )
    }

    fn colorspace(&self) -> &'static str {
        match self {
            Self::Png(RasterImage {
                color: png::ColorType::Rgb | png::ColorType::Rgba,
                ..
            })
            | Self::Gif(RasterImage {
                color: png::ColorType::Rgb | png::ColorType::Rgba,
                ..
            })
            | Self::Webp(RasterImage {
                color: png::ColorType::Rgb | png::ColorType::Rgba,
                ..
            })
            | Self::Tiff(RasterImage {
                color: png::ColorType::Rgb | png::ColorType::Rgba,
                ..
            })
            | Self::Bmp(RasterImage {
                color: png::ColorType::Rgb | png::ColorType::Rgba,
                ..
            })
            | Self::Jpeg(JpegImage { components: 3, .. }) => "/DeviceRGB",
            _ => "/DeviceGray",
        }
    }
}

/// Writes an image-backed PDF without overwriting existing output.
///
/// The caller supplies a private staging path; a failed or cancelled write
/// removes only the output file created by this call.
///
/// # Errors
/// Rejects unsupported, malformed, oversized or changed image input; unsafe
/// paths, cancellation and I/O failures also return an error.
pub fn write_image_pdf(
    input: &Path,
    output: &Path,
    should_stop: impl Fn() -> bool,
) -> Result<(), ImagePdfError> {
    if !output.is_absolute() || !output.parent().is_some_and(Path::is_dir) {
        return Err(ImagePdfError::InvalidOutputPath);
    }
    if should_stop() {
        return Err(ImagePdfError::Cancelled);
    }
    let image = read_image(input, &should_stop)?;
    if should_stop() {
        return Err(ImagePdfError::Cancelled);
    }
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)
        .map_err(ImagePdfError::OutputIo)?;
    let result = write_pdf(file, image, &should_stop);
    if result.is_err() {
        let _ = fs::remove_file(output);
    }
    result
}

/// Writes between one and 200 images to one image-backed, multi-page PDF.
///
/// Images are decoded and emitted one at a time. The output is created with
/// `create_new` and removed if any input fails or conversion is cancelled.
pub(crate) fn write_images_pdf(
    inputs: &[PathBuf],
    output: &Path,
    should_stop: impl Fn() -> bool,
    mut progress: impl FnMut(usize, usize),
) -> Result<(), ImagePdfError> {
    if inputs.is_empty() || inputs.len() > MAX_IMAGES {
        return Err(ImagePdfError::InvalidInput);
    }
    if !output.is_absolute() || !output.parent().is_some_and(Path::is_dir) {
        return Err(ImagePdfError::InvalidOutputPath);
    }
    if should_stop() {
        return Err(ImagePdfError::Cancelled);
    }
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)
        .map_err(ImagePdfError::OutputIo)?;
    let result = (|| {
        if inputs.len() == 1 {
            let (image, length) = read_image_with_len(&inputs[0], &should_stop)?;
            if length > MAX_BATCH_INPUT_BYTES {
                return Err(ImagePdfError::TooLarge);
            }
            write_pdf(file, image, &should_stop)?;
            progress(1, 1);
            Ok(())
        } else {
            write_multi_page_pdf(file, inputs, &should_stop, &mut progress)
        }
    })();
    if result.is_err() {
        let _ = fs::remove_file(output);
    }
    result
}

fn read_image(input: &Path, should_stop: &impl Fn() -> bool) -> Result<InputImage, ImagePdfError> {
    read_image_with_len(input, should_stop).map(|(image, _)| image)
}

fn read_image_with_len(
    input: &Path,
    should_stop: &impl Fn() -> bool,
) -> Result<(InputImage, u64), ImagePdfError> {
    if !input.is_absolute()
        || fs::symlink_metadata(input)
            .map_err(input_io_error)?
            .file_type()
            .is_symlink()
    {
        return Err(ImagePdfError::InvalidInput);
    }
    let mut file = File::open(input).map_err(input_io_error)?;
    let metadata = file.metadata().map_err(input_io_error)?;
    if !metadata.is_file() || metadata.len() == 0 {
        return Err(ImagePdfError::InvalidInput);
    }
    if metadata.len() > MAX_INPUT_BYTES {
        return Err(ImagePdfError::TooLarge);
    }
    let extension = input
        .extension()
        .and_then(|extension| extension.to_str())
        .ok_or(ImagePdfError::InvalidInput)?;
    let image = if extension.eq_ignore_ascii_case("png") {
        InputImage::Png(decode_png(file, should_stop)?)
    } else if extension.eq_ignore_ascii_case("jpg") || extension.eq_ignore_ascii_case("jpeg") {
        let (width, height, components, color_transform) =
            inspect_jpeg(&mut file, metadata.len(), should_stop)?;
        file.seek(SeekFrom::Start(0)).map_err(input_io_error)?;
        validate_jpeg_pixels(
            &mut file,
            metadata.len(),
            (width, height, components),
            should_stop,
        )?;
        file.seek(SeekFrom::Start(0)).map_err(input_io_error)?;
        InputImage::Jpeg(JpegImage {
            width,
            height,
            components,
            color_transform,
            length: metadata.len(),
            input: file,
        })
    } else if extension.eq_ignore_ascii_case("gif") {
        InputImage::Gif(decode_gif(file, should_stop)?)
    } else if extension.eq_ignore_ascii_case("webp") {
        InputImage::Webp(decode_webp(file, should_stop)?)
    } else if extension.eq_ignore_ascii_case("tif") || extension.eq_ignore_ascii_case("tiff") {
        InputImage::Tiff(decode_tiff(file, should_stop)?)
    } else if extension.eq_ignore_ascii_case("bmp") {
        InputImage::Bmp(decode_bmp(file, metadata.len(), should_stop)?)
    } else {
        return Err(ImagePdfError::InvalidInput);
    };
    Ok((image, metadata.len()))
}

fn checked_dimensions(width: u32, height: u32) -> Result<(), ImagePdfError> {
    let pixels = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or(ImagePdfError::TooLarge)?;
    if width == 0 || height == 0 || width > MAX_SIDE || height > MAX_SIDE || pixels > MAX_PIXELS {
        return Err(ImagePdfError::TooLarge);
    }
    Ok(())
}

fn png_error(error: png::DecodingError) -> ImagePdfError {
    match error {
        png::DecodingError::LimitsExceeded => ImagePdfError::TooLarge,
        png::DecodingError::IoError(error) => input_io_error(error),
        _ => ImagePdfError::InvalidImage,
    }
}

fn input_io_error(error: io::Error) -> ImagePdfError {
    match error.kind() {
        io::ErrorKind::TimedOut => ImagePdfError::Cancelled,
        io::ErrorKind::UnexpectedEof => ImagePdfError::InvalidImage,
        _ => ImagePdfError::InputIo(error),
    }
}

struct ControlledReader<'a, R, F> {
    inner: R,
    should_stop: &'a F,
}

impl<R: Read, F: Fn() -> bool> Read for ControlledReader<'_, R, F> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if (self.should_stop)() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "image conversion cancelled",
            ));
        }
        self.inner.read(buf)
    }
}

impl<R: Seek, F> Seek for ControlledReader<'_, R, F> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.inner.seek(position)
    }
}

fn decode_png(file: File, should_stop: &impl Fn() -> bool) -> Result<RasterImage, ImagePdfError> {
    let mut decoder = png::Decoder::new(BufReader::new(ControlledReader {
        inner: file,
        should_stop,
    }));
    decoder.set_transformations(png::Transformations::EXPAND);
    decoder.set_ignore_text_chunk(true);
    decoder.set_limits(png::Limits {
        bytes: MAX_DECODED_BYTES,
    });
    let mut reader = decoder.read_info().map_err(png_error)?;
    let (width, height, unsupported) = {
        let info = reader.info();
        (
            info.width,
            info.height,
            info.bit_depth == png::BitDepth::Sixteen
                || info.animation_control.is_some()
                || info.icc_profile.is_some(),
        )
    };
    checked_dimensions(width, height)?;
    if unsupported {
        return Err(ImagePdfError::UnsupportedImage);
    }
    let (color, bit_depth) = reader.output_color_type();
    if bit_depth != png::BitDepth::Eight
        || !matches!(
            color,
            png::ColorType::Grayscale
                | png::ColorType::GrayscaleAlpha
                | png::ColorType::Rgb
                | png::ColorType::Rgba
        )
    {
        return Err(ImagePdfError::UnsupportedImage);
    }
    let capacity = reader.output_buffer_size();
    if capacity > MAX_DECODED_BYTES {
        return Err(ImagePdfError::TooLarge);
    }
    let mut pixels = vec![0u8; capacity];
    let output = reader.next_frame(&mut pixels).map_err(png_error)?;
    if output.width != width || output.height != height || output.buffer_size() > capacity {
        return Err(ImagePdfError::InvalidImage);
    }
    pixels.truncate(output.buffer_size());
    reader.finish().map_err(png_error)?;
    Ok(RasterImage {
        width: output.width,
        height: output.height,
        color,
        pixels,
    })
}

fn gif_error(error: gif::DecodingError) -> ImagePdfError {
    match error {
        gif::DecodingError::MemoryLimit | gif::DecodingError::OutOfMemory => {
            ImagePdfError::TooLarge
        }
        gif::DecodingError::Io(error) => input_io_error(error),
        _ => ImagePdfError::InvalidImage,
    }
}

fn decode_gif(file: File, should_stop: &impl Fn() -> bool) -> Result<RasterImage, ImagePdfError> {
    let limit = u64::try_from(MAX_DECODED_BYTES)
        .ok()
        .and_then(NonZeroU64::new)
        .ok_or(ImagePdfError::TooLarge)?;
    let mut options = gif::DecodeOptions::new();
    options.set_color_output(gif::ColorOutput::RGBA);
    options.set_memory_limit(gif::MemoryLimit::Bytes(limit));
    options.check_frame_consistency(true);
    options.check_lzw_end_code(true);
    let mut reader = options
        .read_info(ControlledReader {
            inner: BufReader::new(file),
            should_stop,
        })
        .map_err(gif_error)?;
    let width = u32::from(reader.width());
    let height = u32::from(reader.height());
    checked_dimensions(width, height)?;

    let (frame_width, frame_height, left, top) = {
        let frame = reader
            .next_frame_info()
            .map_err(gif_error)?
            .ok_or(ImagePdfError::InvalidImage)?;
        (frame.width, frame.height, frame.left, frame.top)
    };
    if u32::from(frame_width) != width || u32::from(frame_height) != height || left != 0 || top != 0
    {
        return Err(ImagePdfError::UnsupportedImage);
    }
    let expected = decoded_size(width, height, 4)?;
    if expected > MAX_DECODED_BYTES || reader.buffer_size() != expected {
        return Err(ImagePdfError::TooLarge);
    }
    let mut pixels = vec![0u8; expected];
    reader.read_into_buffer(&mut pixels).map_err(gif_error)?;
    if reader.next_frame_info().map_err(gif_error)?.is_some() {
        return Err(ImagePdfError::UnsupportedImage);
    }
    if reader.icc_profile().is_some() {
        return Err(ImagePdfError::UnsupportedImage);
    }
    if should_stop() {
        return Err(ImagePdfError::Cancelled);
    }
    Ok(RasterImage {
        width,
        height,
        color: png::ColorType::Rgba,
        pixels,
    })
}

fn webp_error(error: image_webp::DecodingError) -> ImagePdfError {
    match error {
        image_webp::DecodingError::IoError(error) => input_io_error(error),
        image_webp::DecodingError::ImageTooLarge
        | image_webp::DecodingError::MemoryLimitExceeded => ImagePdfError::TooLarge,
        image_webp::DecodingError::UnsupportedFeature(_) => ImagePdfError::UnsupportedImage,
        _ => ImagePdfError::InvalidImage,
    }
}

fn decode_webp(file: File, should_stop: &impl Fn() -> bool) -> Result<RasterImage, ImagePdfError> {
    let input = ControlledReader {
        inner: file,
        should_stop,
    };
    let mut decoder = image_webp::WebPDecoder::new(BufReader::new(input)).map_err(webp_error)?;
    decoder.set_memory_limit(MAX_DECODED_BYTES);
    let (width, height) = decoder.dimensions();
    checked_dimensions(width, height)?;
    if decoder.is_animated() || decoder.num_frames() != 0 {
        return Err(ImagePdfError::UnsupportedImage);
    }
    if decoder.icc_profile().map_err(webp_error)?.is_some()
        || decoder.exif_metadata().map_err(webp_error)?.is_some()
    {
        return Err(ImagePdfError::UnsupportedImage);
    }
    if should_stop() {
        return Err(ImagePdfError::Cancelled);
    }
    let channels = if decoder.has_alpha() { 4 } else { 3 };
    let expected = decoded_size(width, height, channels)?;
    if expected > MAX_DECODED_BYTES || decoder.output_buffer_size() != Some(expected) {
        return Err(ImagePdfError::TooLarge);
    }
    let mut pixels = vec![0u8; expected];
    decoder.read_image(&mut pixels).map_err(webp_error)?;
    if should_stop() {
        return Err(ImagePdfError::Cancelled);
    }
    Ok(RasterImage {
        width,
        height,
        color: if channels == 4 {
            png::ColorType::Rgba
        } else {
            png::ColorType::Rgb
        },
        pixels,
    })
}

fn tiff_error(error: tiff::TiffError) -> ImagePdfError {
    match error {
        tiff::TiffError::IoError(error) => input_io_error(error),
        tiff::TiffError::LimitsExceeded | tiff::TiffError::IntSizeError => ImagePdfError::TooLarge,
        tiff::TiffError::UnsupportedError(_) => ImagePdfError::UnsupportedImage,
        tiff::TiffError::FormatError(_) | tiff::TiffError::UsageError(_) => {
            ImagePdfError::InvalidImage
        }
    }
}

fn decode_tiff(file: File, should_stop: &impl Fn() -> bool) -> Result<RasterImage, ImagePdfError> {
    let input = ControlledReader {
        inner: BufReader::new(file),
        should_stop,
    };
    let mut limits = tiff::decoder::Limits::default();
    limits.decoding_buffer_size = MAX_DECODED_BYTES;
    limits.ifd_value_size = MAX_TIFF_IFD_VALUE_BYTES;
    limits.intermediate_buffer_size = MAX_TIFF_INTERMEDIATE_BYTES;
    let mut decoder = tiff::decoder::Decoder::new(input)
        .map_err(tiff_error)?
        .with_limits(limits);
    if should_stop() {
        return Err(ImagePdfError::Cancelled);
    }
    if decoder.more_images() {
        return Err(ImagePdfError::UnsupportedImage);
    }
    let (width, height) = decoder.dimensions().map_err(tiff_error)?;
    checked_dimensions(width, height)?;
    let extra_samples = decoder
        .find_tag_unsigned_vec::<u16>(tiff::tags::Tag::ExtraSamples)
        .map_err(tiff_error)?
        .unwrap_or_default();
    let alpha_is_associated = match extra_samples.as_slice() {
        [value] if *value == tiff::tags::ExtraSamples::AssociatedAlpha.to_u16() => Some(true),
        [value] if *value == tiff::tags::ExtraSamples::UnassociatedAlpha.to_u16() => Some(false),
        _ => None,
    };
    let photometric = decoder
        .find_tag_unsigned::<u16>(tiff::tags::Tag::PhotometricInterpretation)
        .map_err(tiff_error)?;
    let (color, channels, associated_alpha) = match decoder.colortype().map_err(tiff_error)? {
        tiff::ColorType::Gray(8) if extra_samples.is_empty() => {
            (png::ColorType::Grayscale, 1, false)
        }
        tiff::ColorType::GrayA(8) if alpha_is_associated.is_some() => (
            png::ColorType::GrayscaleAlpha,
            2,
            alpha_is_associated.unwrap_or(false),
        ),
        tiff::ColorType::Multiband {
            bit_depth: 8,
            num_samples: 2,
        } if photometric == Some(tiff::tags::PhotometricInterpretation::BlackIsZero.to_u16())
            && alpha_is_associated.is_some() =>
        {
            (
                png::ColorType::GrayscaleAlpha,
                2,
                alpha_is_associated.unwrap_or(false),
            )
        }
        tiff::ColorType::RGB(8) if extra_samples.is_empty() => (png::ColorType::Rgb, 3, false),
        tiff::ColorType::RGBA(8) if alpha_is_associated.is_some() => (
            png::ColorType::Rgba,
            4,
            alpha_is_associated.unwrap_or(false),
        ),
        _ => return Err(ImagePdfError::UnsupportedImage),
    };
    let expected = decoded_size(width, height, channels)?;
    if expected > MAX_DECODED_BYTES {
        return Err(ImagePdfError::TooLarge);
    }
    let orientation: Option<u16> = decoder
        .find_tag_unsigned(tiff::tags::Tag::Orientation)
        .map_err(tiff_error)?;
    let planar_configuration: Option<u16> = decoder
        .find_tag_unsigned(tiff::tags::Tag::PlanarConfiguration)
        .map_err(tiff_error)?;
    if planar_configuration.is_some_and(|configuration| {
        configuration != tiff::tags::PlanarConfiguration::Chunky.to_u16()
    }) || orientation.is_some_and(|orientation| orientation != 1)
        || decoder
            .find_tag(tiff::tags::Tag::IccProfile)
            .map_err(tiff_error)?
            .is_some()
        || decoder
            .find_tag(tiff::tags::Tag::SubIfd)
            .map_err(tiff_error)?
            .is_some()
    {
        return Err(ImagePdfError::UnsupportedImage);
    }
    if should_stop() {
        return Err(ImagePdfError::Cancelled);
    }
    let mut pixels = match decoder.read_image().map_err(tiff_error)? {
        tiff::decoder::DecodingResult::U8(pixels) => pixels,
        _ => return Err(ImagePdfError::UnsupportedImage),
    };
    if pixels.len() != expected {
        return Err(ImagePdfError::InvalidImage);
    }
    if associated_alpha {
        unpremultiply_alpha(&mut pixels, width, channels, should_stop)?;
    }
    if decoder.more_images() {
        return Err(ImagePdfError::UnsupportedImage);
    }
    if should_stop() {
        return Err(ImagePdfError::Cancelled);
    }
    Ok(RasterImage {
        width,
        height,
        color,
        pixels,
    })
}

fn unpremultiply_alpha(
    pixels: &mut [u8],
    width: u32,
    channels: usize,
    should_stop: &impl Fn() -> bool,
) -> Result<(), ImagePdfError> {
    let row_bytes = usize::try_from(width)
        .ok()
        .and_then(|width| width.checked_mul(channels))
        .ok_or(ImagePdfError::TooLarge)?;
    for row in pixels.chunks_exact_mut(row_bytes) {
        if should_stop() {
            return Err(ImagePdfError::Cancelled);
        }
        for pixel in row.chunks_exact_mut(channels) {
            let alpha = u16::from(pixel[channels - 1]);
            for component in &mut pixel[..channels - 1] {
                let value = if alpha == 0 {
                    0
                } else {
                    (u16::from(*component) * 255 + alpha / 2) / alpha
                };
                *component =
                    u8::try_from(value.min(255)).map_err(|_| ImagePdfError::InvalidImage)?;
            }
        }
    }
    Ok(())
}

fn decoded_size(width: u32, height: u32, channels: usize) -> Result<usize, ImagePdfError> {
    usize::try_from(width)
        .ok()
        .and_then(|width| width.checked_mul(usize::try_from(height).ok()?))
        .and_then(|pixels| pixels.checked_mul(channels))
        .ok_or(ImagePdfError::TooLarge)
}

fn bmp_u16(header: &[u8; 54], offset: usize) -> Result<u16, ImagePdfError> {
    let bytes: [u8; 2] = header
        .get(offset..offset + 2)
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or(ImagePdfError::InvalidImage)?;
    Ok(u16::from_le_bytes(bytes))
}

fn bmp_u32(header: &[u8; 54], offset: usize) -> Result<u32, ImagePdfError> {
    let bytes: [u8; 4] = header
        .get(offset..offset + 4)
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or(ImagePdfError::InvalidImage)?;
    Ok(u32::from_le_bytes(bytes))
}

fn bmp_i32(header: &[u8; 54], offset: usize) -> Result<i32, ImagePdfError> {
    let bytes: [u8; 4] = header
        .get(offset..offset + 4)
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or(ImagePdfError::InvalidImage)?;
    Ok(i32::from_le_bytes(bytes))
}

fn decode_bmp(
    file: File,
    length: u64,
    should_stop: &impl Fn() -> bool,
) -> Result<RasterImage, ImagePdfError> {
    const HEADER_BYTES: usize = 54;
    const INFO_HEADER_BYTES: u32 = 40;
    const BI_RGB: u32 = 0;

    let mut input = ControlledReader {
        inner: BufReader::new(file),
        should_stop,
    };
    let mut header = [0u8; HEADER_BYTES];
    input.read_exact(&mut header).map_err(input_io_error)?;
    if &header[..2] != b"BM" {
        return Err(ImagePdfError::InvalidImage);
    }
    if bmp_u32(&header, 2)? != u32::try_from(length).map_err(|_| ImagePdfError::TooLarge)?
        || bmp_u16(&header, 6)? != 0
        || bmp_u16(&header, 8)? != 0
    {
        return Err(ImagePdfError::InvalidImage);
    }
    let dib_size = bmp_u32(&header, 14)?;
    if dib_size != INFO_HEADER_BYTES {
        return Err(ImagePdfError::UnsupportedImage);
    }
    let pixel_offset = bmp_u32(&header, 10)?;
    if pixel_offset < u32::try_from(HEADER_BYTES).map_err(|_| ImagePdfError::TooLarge)? {
        return Err(ImagePdfError::InvalidImage);
    }
    if pixel_offset != u32::try_from(HEADER_BYTES).map_err(|_| ImagePdfError::TooLarge)? {
        return Err(ImagePdfError::UnsupportedImage);
    }
    let signed_width = bmp_i32(&header, 18)?;
    let signed_height = bmp_i32(&header, 22)?;
    if signed_width <= 0 || signed_height == 0 || signed_height == i32::MIN {
        return Err(ImagePdfError::InvalidImage);
    }
    let width = u32::try_from(signed_width).map_err(|_| ImagePdfError::InvalidImage)?;
    let top_down = signed_height < 0;
    let height = signed_height.unsigned_abs();
    checked_dimensions(width, height)?;
    if bmp_u16(&header, 26)? != 1 {
        return Err(ImagePdfError::UnsupportedImage);
    }
    let bits_per_pixel = bmp_u16(&header, 28)?;
    let channels = match bits_per_pixel {
        24 => 3usize,
        32 => 4usize,
        _ => return Err(ImagePdfError::UnsupportedImage),
    };
    if bmp_u32(&header, 30)? != BI_RGB || bmp_u32(&header, 46)? != 0 || bmp_u32(&header, 50)? != 0 {
        return Err(ImagePdfError::UnsupportedImage);
    }

    let width_usize = usize::try_from(width).map_err(|_| ImagePdfError::TooLarge)?;
    let height_usize = usize::try_from(height).map_err(|_| ImagePdfError::TooLarge)?;
    let row_bytes = width_usize
        .checked_mul(channels)
        .ok_or(ImagePdfError::TooLarge)?;
    let row_stride = row_bytes
        .checked_add(3)
        .map(|value| value & !3usize)
        .ok_or(ImagePdfError::TooLarge)?;
    let encoded_bytes = row_stride
        .checked_mul(height_usize)
        .ok_or(ImagePdfError::TooLarge)?;
    let expected_length = u64::try_from(HEADER_BYTES)
        .ok()
        .and_then(|header| header.checked_add(u64::try_from(encoded_bytes).ok()?))
        .ok_or(ImagePdfError::TooLarge)?;
    if expected_length != length {
        return Err(ImagePdfError::InvalidImage);
    }
    let declared_image_bytes = bmp_u32(&header, 34)?;
    if declared_image_bytes != 0
        && u64::from(declared_image_bytes)
            != u64::try_from(encoded_bytes).map_err(|_| ImagePdfError::TooLarge)?
    {
        return Err(ImagePdfError::InvalidImage);
    }
    let output_bytes = decoded_size(width, height, channels)?;
    if output_bytes > MAX_DECODED_BYTES {
        return Err(ImagePdfError::TooLarge);
    }

    let mut pixels = vec![0u8; output_bytes];
    let mut encoded_row = vec![0u8; row_stride];
    for file_row in 0..height_usize {
        if should_stop() {
            return Err(ImagePdfError::Cancelled);
        }
        input.read_exact(&mut encoded_row).map_err(input_io_error)?;
        let output_row = if top_down {
            file_row
        } else {
            height_usize
                .checked_sub(file_row + 1)
                .ok_or(ImagePdfError::InvalidImage)?
        };
        let output_start = output_row
            .checked_mul(row_bytes)
            .ok_or(ImagePdfError::TooLarge)?;
        let output_end = output_start
            .checked_add(row_bytes)
            .ok_or(ImagePdfError::TooLarge)?;
        let output = pixels
            .get_mut(output_start..output_end)
            .ok_or(ImagePdfError::InvalidImage)?;
        for (source, destination) in encoded_row[..row_bytes]
            .chunks_exact(channels)
            .zip(output.chunks_exact_mut(channels))
        {
            destination[0] = source[2];
            destination[1] = source[1];
            destination[2] = source[0];
            if channels == 4 {
                destination[3] = source[3];
            }
        }
    }
    if should_stop() {
        return Err(ImagePdfError::Cancelled);
    }
    Ok(RasterImage {
        width,
        height,
        color: if channels == 4 {
            png::ColorType::Rgba
        } else {
            png::ColorType::Rgb
        },
        pixels,
    })
}

fn inspect_jpeg(
    input: &mut File,
    length: u64,
    should_stop: &impl Fn() -> bool,
) -> Result<(u32, u32, u8, u8), ImagePdfError> {
    let mut reader = BufReader::new(ControlledReader {
        inner: input,
        should_stop,
    });
    let mut soi = [0u8; 2];
    reader.read_exact(&mut soi).map_err(input_io_error)?;
    if soi != [0xff, 0xd8] {
        return Err(ImagePdfError::InvalidImage);
    }
    let mut frame = None;
    let mut scans = 0u32;
    let mut quantization = false;
    let mut in_scan = false;
    let mut minimum_entropy_bits: Option<u64> = None;
    let mut entropy_bytes = 0u64;
    let mut color_transform = 1u8;
    loop {
        let marker = read_marker(&mut reader, in_scan, &mut entropy_bytes)?;
        if marker == 0xd9 {
            if frame.is_none()
                || scans == 0
                || !quantization
                || reader.stream_position().map_err(input_io_error)? != length
            {
                return Err(ImagePdfError::InvalidImage);
            }
            let (width, height, components) = frame.ok_or(ImagePdfError::InvalidImage)?;
            if let Some(bits) = minimum_entropy_bits {
                // Some decoders silently zero-fill after early EOI. Every
                // component block needs a DC code; sequential blocks also need
                // an AC code. Subsampling determines the minimum block count.
                let minimum = bits.checked_add(7).ok_or(ImagePdfError::TooLarge)? / 8;
                if entropy_bytes < minimum {
                    return Err(ImagePdfError::InvalidImage);
                }
            }
            return Ok((width, height, components, color_transform));
        }
        if marker == 0xd8 || marker == 0x01 || (0xd0..=0xd7).contains(&marker) {
            return Err(ImagePdfError::InvalidImage);
        }
        let segment_length = read_u16_be(&mut reader)?;
        if segment_length < 2 {
            return Err(ImagePdfError::InvalidImage);
        }
        let end = reader
            .stream_position()
            .map_err(input_io_error)?
            .checked_add(u64::from(segment_length - 2))
            .filter(|&offset| offset <= length)
            .ok_or(ImagePdfError::InvalidImage)?;
        match marker {
            0xc0 | 0xc2 => {
                if frame.is_some() || segment_length < 11 {
                    return Err(ImagePdfError::InvalidImage);
                }
                let mut header = [0u8; 6];
                reader.read_exact(&mut header).map_err(input_io_error)?;
                let height = u32::from(u16::from_be_bytes([header[1], header[2]]));
                let width = u32::from(u16::from_be_bytes([header[3], header[4]]));
                let components = header[5];
                if header[0] != 8
                    || !matches!(components, 1 | 3)
                    || segment_length != 8 + 3 * u16::from(components)
                {
                    return Err(ImagePdfError::UnsupportedImage);
                }
                checked_dimensions(width, height)?;
                let mut sample_bytes = [0u8; 12];
                reader
                    .read_exact(&mut sample_bytes[..usize::from(components) * 3])
                    .map_err(input_io_error)?;
                let factors = sample_bytes[..usize::from(components) * 3]
                    .chunks_exact(3)
                    .map(|entry| (entry[1] >> 4, entry[1] & 0x0f))
                    .collect::<Vec<_>>();
                if factors.iter().any(|(horizontal, vertical)| {
                    !(1..=4).contains(horizontal) || !(1..=4).contains(vertical)
                }) {
                    return Err(ImagePdfError::InvalidImage);
                }
                let max_horizontal = factors
                    .iter()
                    .map(|(horizontal, _)| *horizontal)
                    .max()
                    .ok_or(ImagePdfError::InvalidImage)?;
                let max_vertical = factors
                    .iter()
                    .map(|(_, vertical)| *vertical)
                    .max()
                    .ok_or(ImagePdfError::InvalidImage)?;
                let mut blocks = 0u64;
                for (horizontal, vertical) in factors {
                    let columns = (u64::from(width) * u64::from(horizontal))
                        .div_ceil(8 * u64::from(max_horizontal));
                    let rows = (u64::from(height) * u64::from(vertical))
                        .div_ceil(8 * u64::from(max_vertical));
                    blocks = blocks
                        .checked_add(columns.checked_mul(rows).ok_or(ImagePdfError::TooLarge)?)
                        .ok_or(ImagePdfError::TooLarge)?;
                }
                minimum_entropy_bits = Some(
                    blocks
                        .checked_mul(if marker == 0xc0 { 2 } else { 1 })
                        .ok_or(ImagePdfError::TooLarge)?,
                );
                frame = Some((width, height, components));
            }
            0xda => {
                let (_, _, components) = frame.ok_or(ImagePdfError::InvalidImage)?;
                let mut count = [0u8; 1];
                reader.read_exact(&mut count).map_err(input_io_error)?;
                if count[0] == 0
                    || count[0] > components
                    || segment_length != 6 + 2 * u16::from(count[0])
                    || !quantization
                {
                    return Err(ImagePdfError::InvalidImage);
                }
                scans = scans.checked_add(1).ok_or(ImagePdfError::TooLarge)?;
                in_scan = true;
            }
            0xdb => quantization = true,
            0xe1 => {
                let mut bytes = vec![0u8; usize::from(segment_length - 2)];
                reader.read_exact(&mut bytes).map_err(input_io_error)?;
                if bytes.starts_with(b"Exif\0\0") && exif_orientation(&bytes)? != 1 {
                    return Err(ImagePdfError::UnsupportedImage);
                }
            }
            0xe2 => {
                let mut signature = [0u8; 12];
                if segment_length >= 14 {
                    reader.read_exact(&mut signature).map_err(input_io_error)?;
                    if &signature == b"ICC_PROFILE\0" {
                        return Err(ImagePdfError::UnsupportedImage);
                    }
                }
            }
            0xee => {
                let mut signature = [0u8; 12];
                if segment_length >= 14 {
                    reader.read_exact(&mut signature).map_err(input_io_error)?;
                    if signature.starts_with(b"Adobe") {
                        if signature[11] > 1 {
                            return Err(ImagePdfError::UnsupportedImage);
                        }
                        color_transform = signature[11];
                    }
                }
            }
            0xc1 | 0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf | 0xc8 | 0xcc => {
                return Err(ImagePdfError::UnsupportedImage)
            }
            _ => {}
        }
        reader.seek(SeekFrom::Start(end)).map_err(input_io_error)?;
        if marker != 0xda {
            in_scan = false;
        }
    }
}

fn validate_jpeg_pixels(
    file: &mut File,
    length: u64,
    expected: (u32, u32, u8),
    should_stop: &impl Fn() -> bool,
) -> Result<(), ImagePdfError> {
    if should_stop() {
        return Err(ImagePdfError::Cancelled);
    }
    let input = ControlledReader {
        inner: (&mut *file).take(length),
        should_stop,
    };
    let mut decoder = jpeg_decoder::Decoder::new(BufReader::new(input));
    decoder.set_max_decoding_buffer_size(MAX_DECODED_BYTES);
    let pixels = decoder.decode().map_err(|error| match error {
        jpeg_decoder::Error::Unsupported(_) => ImagePdfError::UnsupportedImage,
        jpeg_decoder::Error::Io(error) => input_io_error(error),
        jpeg_decoder::Error::Format(_) | jpeg_decoder::Error::Internal(_) => {
            ImagePdfError::InvalidImage
        }
    })?;
    if should_stop() {
        return Err(ImagePdfError::Cancelled);
    }
    let info = decoder.info().ok_or(ImagePdfError::InvalidImage)?;
    let actual_components = match info.pixel_format {
        jpeg_decoder::PixelFormat::L8 => 1u8,
        jpeg_decoder::PixelFormat::RGB24 => 3u8,
        _ => return Err(ImagePdfError::UnsupportedImage),
    };
    let expected_size = usize::try_from(expected.0)
        .ok()
        .and_then(|width| width.checked_mul(usize::try_from(expected.1).ok()?))
        .and_then(|pixels| pixels.checked_mul(usize::from(expected.2)))
        .ok_or(ImagePdfError::TooLarge)?;
    if (
        u32::from(info.width),
        u32::from(info.height),
        actual_components,
    ) != expected
        || pixels.len() != expected_size
    {
        return Err(ImagePdfError::InvalidImage);
    }
    Ok(())
}

fn read_u16_be(reader: &mut impl Read) -> Result<u16, ImagePdfError> {
    let mut buffer = [0u8; 2];
    reader.read_exact(&mut buffer).map_err(input_io_error)?;
    Ok(u16::from_be_bytes(buffer))
}

fn read_marker(
    reader: &mut impl Read,
    in_scan: bool,
    entropy_bytes: &mut u64,
) -> Result<u8, ImagePdfError> {
    let mut byte = [0u8; 1];
    loop {
        reader.read_exact(&mut byte).map_err(input_io_error)?;
        if byte[0] == 0xff {
            loop {
                reader.read_exact(&mut byte).map_err(input_io_error)?;
                if byte[0] != 0xff {
                    break;
                }
            }
            if in_scan && (byte[0] == 0 || (0xd0..=0xd7).contains(&byte[0])) {
                if byte[0] == 0 {
                    *entropy_bytes = entropy_bytes
                        .checked_add(1)
                        .ok_or(ImagePdfError::TooLarge)?;
                }
                continue;
            }
            if byte[0] == 0 {
                return Err(ImagePdfError::InvalidImage);
            }
            return Ok(byte[0]);
        }
        if !in_scan {
            return Err(ImagePdfError::InvalidImage);
        }
        *entropy_bytes = entropy_bytes
            .checked_add(1)
            .ok_or(ImagePdfError::TooLarge)?;
    }
}

fn exif_orientation(segment: &[u8]) -> Result<u16, ImagePdfError> {
    let tiff = segment.get(6..).ok_or(ImagePdfError::InvalidImage)?;
    let little_endian = match tiff.get(..2) {
        Some(b"II") => true,
        Some(b"MM") => false,
        _ => return Err(ImagePdfError::InvalidImage),
    };
    let read_u16 = |start: usize| -> Option<u16> {
        let data: [u8; 2] = tiff.get(start..start.checked_add(2)?)?.try_into().ok()?;
        Some(if little_endian {
            u16::from_le_bytes(data)
        } else {
            u16::from_be_bytes(data)
        })
    };
    let read_u32 = |start: usize| -> Option<u32> {
        let data: [u8; 4] = tiff.get(start..start.checked_add(4)?)?.try_into().ok()?;
        Some(if little_endian {
            u32::from_le_bytes(data)
        } else {
            u32::from_be_bytes(data)
        })
    };
    if read_u16(2) != Some(42) {
        return Err(ImagePdfError::InvalidImage);
    }
    let offset = usize::try_from(read_u32(4).ok_or(ImagePdfError::InvalidImage)?)
        .map_err(|_| ImagePdfError::InvalidImage)?;
    let count = usize::from(read_u16(offset).ok_or(ImagePdfError::InvalidImage)?);
    for index in 0..count {
        let entry = offset
            .checked_add(2)
            .and_then(|value| value.checked_add(index.checked_mul(12)?))
            .ok_or(ImagePdfError::InvalidImage)?;
        if read_u16(entry) == Some(0x0112) {
            if read_u16(entry + 2) != Some(3) || read_u32(entry + 4) != Some(1) {
                return Err(ImagePdfError::InvalidImage);
            }
            return read_u16(entry + 8).ok_or(ImagePdfError::InvalidImage);
        }
    }
    Ok(1)
}

#[derive(Debug, Error)]
#[error("PDF exceeds output size limit")]
struct OutputLimitError;

fn output_io_error(error: io::Error) -> ImagePdfError {
    if error
        .get_ref()
        .is_some_and(|source| source.downcast_ref::<OutputLimitError>().is_some())
    {
        ImagePdfError::OutputTooLarge
    } else {
        ImagePdfError::OutputIo(error)
    }
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

    fn append(&mut self, bytes: &[u8]) -> Result<(), ImagePdfError> {
        self.write_all(bytes).map_err(output_io_error)?;
        Ok(())
    }

    fn object(&mut self, number: usize, body: &[u8]) -> Result<(), ImagePdfError> {
        self.start_object(number)?;
        self.append(body)?;
        self.append(b"\nendobj\n")
    }

    fn start_object(&mut self, number: usize) -> Result<(), ImagePdfError> {
        if self.offsets.len() != number || self.position > MAX_XREF_OFFSET {
            return Err(ImagePdfError::OutputTooLarge);
        }
        self.offsets.push(self.position);
        self.append(format!("{number} 0 obj\n").as_bytes())
    }

    fn finish(mut self) -> Result<(), ImagePdfError> {
        let xref = self.position;
        let object_count = self.offsets.len();
        self.append(format!("xref\n0 {object_count}\n0000000000 65535 f \n").as_bytes())?;
        let offsets = std::mem::take(&mut self.offsets);
        for offset in offsets.into_iter().skip(1) {
            self.append(format!("{offset:010} 00000 n \n").as_bytes())?;
        }
        self.append(
            format!("trailer << /Size {object_count} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n")
                .as_bytes(),
        )?;
        self.writer.flush().map_err(output_io_error)?;
        Ok(())
    }
}

impl<W: Write> Write for PdfOutput<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.position >= MAX_PDF_BYTES {
            return Err(io::Error::other(OutputLimitError));
        }
        let left = usize::try_from(MAX_PDF_BYTES - self.position).unwrap_or(usize::MAX);
        let written = self.writer.write(&bytes[..bytes.len().min(left)])?;
        self.position = self
            .position
            .checked_add(u64::try_from(written).map_err(|_| io::Error::other(OutputLimitError))?)
            .ok_or_else(|| io::Error::other(OutputLimitError))?;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

fn write_pdf(
    file: File,
    mut image: InputImage,
    should_stop: &impl Fn() -> bool,
) -> Result<(), ImagePdfError> {
    let mut pdf = PdfOutput::new(BufWriter::new(file));
    let (width, height) = image.dimensions();
    let longest = f64::from(width.max(height));
    let scale = if longest > MAX_PAGE_SIDE {
        MAX_PAGE_SIDE / longest
    } else {
        1.0
    };
    let page_width = f64::from(width) * scale;
    let page_height = f64::from(height) * scale;
    let mask = image.has_alpha();
    let image_length_object = if mask { 7 } else { 6 };
    pdf.append(b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n")?;
    pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
    pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
    pdf.object(
        3,
        format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {page_width:.4} {page_height:.4}] /Resources << /XObject << /Im0 5 0 R >> >> /Contents 4 0 R >>").as_bytes(),
    )?;
    let content = format!("q\n{page_width:.4} 0 0 {page_height:.4} 0 0 cm\n/Im0 Do\nQ\n");
    pdf.object(
        4,
        format!(
            "<< /Length {} >>\nstream\n{content}endstream",
            content.len()
        )
        .as_bytes(),
    )?;
    let filter = match &image {
        InputImage::Png(_)
        | InputImage::Gif(_)
        | InputImage::Webp(_)
        | InputImage::Tiff(_)
        | InputImage::Bmp(_) => "/FlateDecode",
        InputImage::Jpeg(_) => "/DCTDecode",
    };
    let color_transform = match &image {
        InputImage::Jpeg(jpeg) if jpeg.components == 3 => {
            format!(
                " /DecodeParms << /ColorTransform {} >>",
                jpeg.color_transform
            )
        }
        _ => String::new(),
    };
    let mask_ref = if mask { " /SMask 6 0 R" } else { "" };
    pdf.start_object(5)?;
    pdf.append(
        format!(
            "<< /Type /XObject /Subtype /Image /Width {width} /Height {height} /ColorSpace {} /BitsPerComponent 8 /Filter {filter}{color_transform}{mask_ref} /Length {image_length_object} 0 R >>\nstream\n",
            image.colorspace()
        )
        .as_bytes(),
    )?;
    let start = pdf.position;
    match &mut image {
        InputImage::Png(raster)
        | InputImage::Gif(raster)
        | InputImage::Webp(raster)
        | InputImage::Tiff(raster)
        | InputImage::Bmp(raster) => write_raster_component(&mut pdf, raster, false, should_stop)?,
        InputImage::Jpeg(jpeg) => {
            let mut input = ControlledReader {
                inner: (&mut jpeg.input).take(jpeg.length),
                should_stop,
            };
            let mut buffer = [0u8; 16 * 1024];
            let mut copied = 0u64;
            loop {
                let read = input.read(&mut buffer).map_err(input_io_error)?;
                if read == 0 {
                    break;
                }
                pdf.append(&buffer[..read])?;
                copied = copied
                    .checked_add(u64::try_from(read).map_err(|_| ImagePdfError::TooLarge)?)
                    .ok_or(ImagePdfError::TooLarge)?;
            }
            if copied != jpeg.length {
                return Err(ImagePdfError::InvalidImage);
            }
        }
    }
    let image_length = pdf.position - start;
    pdf.append(b"\nendstream\nendobj\n")?;
    if let InputImage::Png(raster)
    | InputImage::Gif(raster)
    | InputImage::Webp(raster)
    | InputImage::Tiff(raster)
    | InputImage::Bmp(raster) = &image
    {
        if mask {
            pdf.start_object(6)?;
            pdf.append(
                format!("<< /Type /XObject /Subtype /Image /Width {width} /Height {height} /ColorSpace /DeviceGray /BitsPerComponent 8 /Filter /FlateDecode /Length 8 0 R >>\nstream\n").as_bytes(),
            )?;
            let start = pdf.position;
            write_raster_component(&mut pdf, raster, true, should_stop)?;
            let mask_length = pdf.position - start;
            pdf.append(b"\nendstream\nendobj\n")?;
            pdf.object(7, image_length.to_string().as_bytes())?;
            pdf.object(8, mask_length.to_string().as_bytes())?;
        }
    }
    if !mask {
        pdf.object(6, image_length.to_string().as_bytes())?;
    }
    if should_stop() {
        return Err(ImagePdfError::Cancelled);
    }
    pdf.finish()?;
    if should_stop() {
        return Err(ImagePdfError::Cancelled);
    }
    Ok(())
}

fn write_multi_page_pdf(
    file: File,
    inputs: &[PathBuf],
    should_stop: &impl Fn() -> bool,
    progress: &mut impl FnMut(usize, usize),
) -> Result<(), ImagePdfError> {
    const OBJECTS_PER_PAGE: usize = 6;

    let mut pdf = PdfOutput::new(BufWriter::new(file));
    pdf.append(b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n")?;
    pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
    let mut kids = String::new();
    for index in 0..inputs.len() {
        let page_object = index
            .checked_mul(OBJECTS_PER_PAGE)
            .and_then(|value| value.checked_add(3))
            .ok_or(ImagePdfError::OutputTooLarge)?;
        kids.push_str(&format!("{page_object} 0 R "));
    }
    pdf.object(
        2,
        format!("<< /Type /Pages /Kids [{kids}] /Count {} >>", inputs.len()).as_bytes(),
    )?;

    let mut total_input = 0u64;
    for (index, input) in inputs.iter().enumerate() {
        if should_stop() {
            return Err(ImagePdfError::Cancelled);
        }
        let (mut image, input_bytes) = read_image_with_len(input, should_stop)?;
        total_input = total_input
            .checked_add(input_bytes)
            .ok_or(ImagePdfError::TooLarge)?;
        if total_input > MAX_BATCH_INPUT_BYTES {
            return Err(ImagePdfError::TooLarge);
        }
        let page_object = index
            .checked_mul(OBJECTS_PER_PAGE)
            .and_then(|value| value.checked_add(3))
            .ok_or(ImagePdfError::OutputTooLarge)?;
        write_multi_page_objects(&mut pdf, page_object, &mut image, should_stop)?;
        progress(index + 1, inputs.len());
    }
    if should_stop() {
        return Err(ImagePdfError::Cancelled);
    }
    pdf.finish()?;
    if should_stop() {
        return Err(ImagePdfError::Cancelled);
    }
    Ok(())
}

fn write_multi_page_objects(
    pdf: &mut PdfOutput<impl Write>,
    page_object: usize,
    image: &mut InputImage,
    should_stop: &impl Fn() -> bool,
) -> Result<(), ImagePdfError> {
    let content_object = page_object
        .checked_add(1)
        .ok_or(ImagePdfError::OutputTooLarge)?;
    let image_object = page_object
        .checked_add(2)
        .ok_or(ImagePdfError::OutputTooLarge)?;
    let image_length_object = page_object
        .checked_add(3)
        .ok_or(ImagePdfError::OutputTooLarge)?;
    let mask_object = page_object
        .checked_add(4)
        .ok_or(ImagePdfError::OutputTooLarge)?;
    let mask_length_object = page_object
        .checked_add(5)
        .ok_or(ImagePdfError::OutputTooLarge)?;
    let (width, height) = image.dimensions();
    let longest = f64::from(width.max(height));
    let scale = if longest > MAX_PAGE_SIDE {
        MAX_PAGE_SIDE / longest
    } else {
        1.0
    };
    let page_width = f64::from(width) * scale;
    let page_height = f64::from(height) * scale;
    pdf.object(
        page_object,
        format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {page_width:.4} {page_height:.4}] /Resources << /XObject << /Im0 {image_object} 0 R >> >> /Contents {content_object} 0 R >>").as_bytes(),
    )?;
    let content = format!("q\n{page_width:.4} 0 0 {page_height:.4} 0 0 cm\n/Im0 Do\nQ\n");
    pdf.object(
        content_object,
        format!(
            "<< /Length {} >>\nstream\n{content}endstream",
            content.len()
        )
        .as_bytes(),
    )?;
    let filter = match image {
        InputImage::Jpeg(_) => "/DCTDecode",
        _ => "/FlateDecode",
    };
    let color_transform = match image {
        InputImage::Jpeg(jpeg) if jpeg.components == 3 => {
            format!(
                " /DecodeParms << /ColorTransform {} >>",
                jpeg.color_transform
            )
        }
        _ => String::new(),
    };
    let has_alpha = image.has_alpha();
    let mask_ref = if has_alpha {
        format!(" /SMask {mask_object} 0 R")
    } else {
        String::new()
    };
    pdf.start_object(image_object)?;
    pdf.append(
        format!(
            "<< /Type /XObject /Subtype /Image /Width {width} /Height {height} /ColorSpace {} /BitsPerComponent 8 /Filter {filter}{color_transform}{mask_ref} /Length {image_length_object} 0 R >>\nstream\n",
            image.colorspace()
        )
        .as_bytes(),
    )?;
    let image_start = pdf.position;
    write_input_image_component(pdf, image, false, should_stop)?;
    let image_length = pdf.position - image_start;
    pdf.append(b"\nendstream\nendobj\n")?;
    pdf.object(image_length_object, image_length.to_string().as_bytes())?;

    if has_alpha {
        pdf.start_object(mask_object)?;
        pdf.append(
            format!("<< /Type /XObject /Subtype /Image /Width {width} /Height {height} /ColorSpace /DeviceGray /BitsPerComponent 8 /Filter /FlateDecode /Length {mask_length_object} 0 R >>\nstream\n").as_bytes(),
        )?;
        let mask_start = pdf.position;
        write_input_image_component(pdf, image, true, should_stop)?;
        let mask_length = pdf.position - mask_start;
        pdf.append(b"\nendstream\nendobj\n")?;
        pdf.object(mask_length_object, mask_length.to_string().as_bytes())?;
    } else {
        pdf.object(mask_object, b"null")?;
        pdf.object(mask_length_object, b"0")?;
    }
    Ok(())
}

fn write_input_image_component(
    pdf: &mut PdfOutput<impl Write>,
    image: &mut InputImage,
    alpha: bool,
    should_stop: &impl Fn() -> bool,
) -> Result<(), ImagePdfError> {
    match image {
        InputImage::Png(raster)
        | InputImage::Gif(raster)
        | InputImage::Webp(raster)
        | InputImage::Tiff(raster)
        | InputImage::Bmp(raster) => write_raster_component(pdf, raster, alpha, should_stop),
        InputImage::Jpeg(jpeg) if !alpha => {
            let mut input = ControlledReader {
                inner: (&mut jpeg.input).take(jpeg.length),
                should_stop,
            };
            let mut buffer = [0u8; 16 * 1024];
            let mut copied = 0u64;
            loop {
                let read = input.read(&mut buffer).map_err(input_io_error)?;
                if read == 0 {
                    break;
                }
                pdf.append(&buffer[..read])?;
                copied = copied
                    .checked_add(u64::try_from(read).map_err(|_| ImagePdfError::TooLarge)?)
                    .ok_or(ImagePdfError::TooLarge)?;
            }
            if copied != jpeg.length {
                return Err(ImagePdfError::InvalidImage);
            }
            Ok(())
        }
        InputImage::Jpeg(_) => Err(ImagePdfError::InvalidImage),
    }
}

fn write_raster_component(
    pdf: &mut PdfOutput<impl Write>,
    raster: &RasterImage,
    alpha: bool,
    should_stop: &impl Fn() -> bool,
) -> Result<(), ImagePdfError> {
    let channels: usize = match raster.color {
        png::ColorType::Grayscale => 1,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        _ => return Err(ImagePdfError::UnsupportedImage),
    };
    let width = usize::try_from(raster.width).map_err(|_| ImagePdfError::TooLarge)?;
    let height = usize::try_from(raster.height).map_err(|_| ImagePdfError::TooLarge)?;
    let stride = width.checked_mul(channels).ok_or(ImagePdfError::TooLarge)?;
    if raster.pixels.len() != height.checked_mul(stride).ok_or(ImagePdfError::TooLarge)? {
        return Err(ImagePdfError::InvalidImage);
    }
    let mut encoder = ZlibEncoder::new(pdf, Compression::default());
    let mut row_buffer = Vec::with_capacity(stride);
    for row in raster.pixels.chunks_exact(stride) {
        if should_stop() {
            return Err(ImagePdfError::Cancelled);
        }
        if channels == 2 || channels == 4 {
            row_buffer.clear();
            for pixel in row.chunks_exact(channels) {
                if alpha {
                    row_buffer.push(pixel[channels - 1]);
                } else {
                    row_buffer.extend_from_slice(&pixel[..channels - 1]);
                }
            }
            encoder.write_all(&row_buffer).map_err(output_io_error)?;
        } else {
            encoder.write_all(row).map_err(output_io_error)?;
        }
    }
    encoder.finish().map_err(output_io_error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct TestFiles {
        dir: PathBuf,
    }

    impl TestFiles {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "image-pdf-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4()
            ));
            fs::create_dir(&dir).expect("create private test directory");
            Self { dir }
        }

        fn path(&self, name: &str) -> PathBuf {
            self.dir.join(name)
        }
    }

    impl Drop for TestFiles {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn png_input(path: &Path, width: u32, height: u32, color: png::ColorType, pixels: &[u8]) {
        let file = File::create(path).expect("create PNG");
        let mut encoder = png::Encoder::new(file, width, height);
        encoder.set_color(color);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .write_header()
            .expect("PNG header")
            .write_image_data(pixels)
            .expect("PNG pixels");
    }

    fn jpeg_input(path: &Path, width: u16, height: u16, pixels: &[u8]) -> Vec<u8> {
        let mut jpeg = Vec::new();
        jpeg_encoder::Encoder::new(&mut jpeg, 90)
            .encode(pixels, width, height, jpeg_encoder::ColorType::Rgb)
            .expect("encode JPEG");
        fs::write(path, &jpeg).expect("write JPEG");
        jpeg
    }

    fn bmp_input(
        path: &Path,
        width: u32,
        height: i32,
        channels: usize,
        top_first_pixels: &[u8],
    ) -> Vec<u8> {
        let row_bytes = usize::try_from(width).expect("test width") * channels;
        let stride = (row_bytes + 3) & !3;
        let rows = usize::try_from(height.unsigned_abs()).expect("test height");
        assert_eq!(top_first_pixels.len(), rows * row_bytes);
        let mut bytes = vec![0u8; 54 + stride * rows];
        bytes[..2].copy_from_slice(b"BM");
        let file_size = u32::try_from(bytes.len()).expect("BMP size");
        bytes[2..6].copy_from_slice(&file_size.to_le_bytes());
        bytes[10..14].copy_from_slice(&54u32.to_le_bytes());
        bytes[14..18].copy_from_slice(&40u32.to_le_bytes());
        bytes[18..22].copy_from_slice(&width.to_le_bytes());
        bytes[22..26].copy_from_slice(&height.to_le_bytes());
        bytes[26..28].copy_from_slice(&1u16.to_le_bytes());
        bytes[28..30].copy_from_slice(
            &u16::try_from(channels * 8)
                .expect("BMP bit depth")
                .to_le_bytes(),
        );
        bytes[34..38].copy_from_slice(
            &u32::try_from(rows * stride)
                .expect("BMP pixels")
                .to_le_bytes(),
        );
        for row in 0..rows {
            let source_row = if height < 0 { row } else { rows - 1 - row };
            let source = &top_first_pixels[source_row * row_bytes..(source_row + 1) * row_bytes];
            let destination = &mut bytes[54 + row * stride..54 + row * stride + row_bytes];
            for (rgb, bgr) in source
                .chunks_exact(channels)
                .zip(destination.chunks_exact_mut(channels))
            {
                bgr[0] = rgb[2];
                bgr[1] = rgb[1];
                bgr[2] = rgb[0];
                if channels == 4 {
                    bgr[3] = rgb[3];
                }
            }
        }
        fs::write(path, &bytes).expect("write BMP");
        bytes
    }

    fn inflate_stream(pdf: &[u8], image_object: usize, length_object: usize) -> Vec<u8> {
        let encoded = stream(pdf, image_object, length_object);
        let mut decoded = Vec::new();
        flate2::read::ZlibDecoder::new(encoded.as_slice())
            .read_to_end(&mut decoded)
            .expect("inflate PDF image stream");
        decoded
    }

    fn gif_input(path: &Path, width: u16, height: u16, frames: &[&[u8]]) {
        let file = File::create(path).expect("create GIF");
        let mut encoder = gif::Encoder::new(file, width, height, &[]).expect("GIF header");
        for pixels in frames {
            let mut pixels = pixels.to_vec();
            let frame = gif::Frame::from_rgba_speed(width, height, &mut pixels, 10);
            encoder.write_frame(&frame).expect("GIF frame");
        }
        encoder.into_inner().expect("GIF trailer");
    }

    fn webp_bytes(width: u32, height: u32, pixels: &[u8], color: image_webp::ColorType) -> Vec<u8> {
        let mut bytes = Vec::new();
        image_webp::WebPEncoder::new(&mut bytes)
            .encode(pixels, width, height, color)
            .expect("encode WebP");
        bytes
    }

    fn rgba_tiff_input(path: &Path, width: u32, height: u32, frames: &[&[u8]]) {
        let mut file = File::create(path).expect("create TIFF");
        {
            let mut encoder = tiff::encoder::TiffEncoder::new(&mut file)
                .expect("TIFF header")
                .with_compression(tiff::encoder::Compression::Lzw);
            for pixels in frames {
                let mut image = encoder
                    .new_image::<tiff::encoder::colortype::RGB8>(width, height)
                    .expect("TIFF RGBA frame");
                image
                    .extra_samples(&[tiff::tags::ExtraSamples::UnassociatedAlpha])
                    .expect("TIFF alpha tag");
                image.write_data(pixels).expect("TIFF RGBA pixels");
            }
        }
        file.sync_all().expect("flush TIFF");
    }

    fn alpha_tiff_input(
        path: &Path,
        color: png::ColorType,
        alpha: tiff::tags::ExtraSamples,
        pixels: &[u8],
    ) {
        let mut file = File::create(path).expect("create TIFF");
        {
            let mut encoder = tiff::encoder::TiffEncoder::new(&mut file)
                .expect("TIFF header")
                .with_compression(tiff::encoder::Compression::Lzw);
            match color {
                png::ColorType::GrayscaleAlpha => {
                    let mut image = encoder
                        .new_image::<tiff::encoder::colortype::Gray8>(2, 1)
                        .expect("TIFF GrayA image");
                    image.extra_samples(&[alpha]).expect("TIFF alpha tag");
                    image.write_data(pixels).expect("TIFF GrayA pixels");
                }
                png::ColorType::Rgba => {
                    let mut image = encoder
                        .new_image::<tiff::encoder::colortype::RGB8>(1, 1)
                        .expect("TIFF RGBA image");
                    image.extra_samples(&[alpha]).expect("TIFF alpha tag");
                    image.write_data(pixels).expect("TIFF RGBA pixels");
                }
                _ => panic!("alpha TIFF fixture requires GrayA or RGBA"),
            }
        }
        file.sync_all().expect("flush TIFF");
    }

    fn rgb_tiff_input(path: &Path, width: u32, height: u32, pixels: &[u8]) {
        let mut file = File::create(path).expect("create TIFF");
        {
            let mut encoder = tiff::encoder::TiffEncoder::new(&mut file)
                .expect("TIFF header")
                .with_compression(tiff::encoder::Compression::Lzw)
                .with_predictor(tiff::encoder::Predictor::Horizontal);
            encoder
                .write_image::<tiff::encoder::colortype::RGB8>(width, height, pixels)
                .expect("TIFF RGB frame");
        }
        file.sync_all().expect("flush TIFF");
    }

    fn rgb_tiff_with_sub_ifd(path: &Path) {
        let mut file = File::create(path).expect("create TIFF");
        {
            let mut encoder = tiff::encoder::TiffEncoder::new(&mut file).expect("TIFF header");
            let sub_ifd = {
                let mut directory = encoder.extra_directory().expect("SubIFD directory");
                directory
                    .write_tag(tiff::tags::Tag::Software, "test")
                    .expect("SubIFD tag");
                directory.finish_with_offsets().expect("finish SubIFD")
            };
            let mut image = encoder
                .new_image::<tiff::encoder::colortype::RGB8>(1, 1)
                .expect("TIFF RGB image");
            image
                .encoder()
                .write_tag(tiff::tags::Tag::SubIfd, sub_ifd.offset)
                .expect("link SubIFD");
            image.write_data(&[10, 20, 30]).expect("TIFF RGB data");
        }
        file.sync_all().expect("flush TIFF");
    }

    fn append_webp_chunk(output: &mut Vec<u8>, name: &[u8; 4], bytes: &[u8]) {
        output.extend_from_slice(name);
        output.extend_from_slice(
            &u32::try_from(bytes.len())
                .expect("test chunk size")
                .to_le_bytes(),
        );
        output.extend_from_slice(bytes);
        if bytes.len() % 2 != 0 {
            output.push(0);
        }
    }

    fn animated_webp() -> Vec<u8> {
        let first = webp_bytes(2, 1, &[255, 0, 0, 0, 0, 255], image_webp::ColorType::Rgb8);
        let second = webp_bytes(
            2,
            1,
            &[0, 255, 0, 255, 255, 255],
            image_webp::ColorType::Rgb8,
        );
        let mut body = b"WEBP".to_vec();
        append_webp_chunk(&mut body, b"VP8X", &[2, 0, 0, 0, 1, 0, 0, 0, 0, 0]);
        append_webp_chunk(&mut body, b"ANIM", &[0, 0, 0, 0, 0, 0]);
        for frame in [&first, &second] {
            let mut payload = vec![0; 16];
            payload[6] = 1;
            payload[12] = 10;
            payload.extend_from_slice(&frame[12..]);
            append_webp_chunk(&mut body, b"ANMF", &payload);
        }
        let mut output = b"RIFF".to_vec();
        output.extend_from_slice(
            &u32::try_from(body.len())
                .expect("test RIFF size")
                .to_le_bytes(),
        );
        output.extend_from_slice(&body);
        output
    }

    fn stream(pdf: &[u8], object: usize, length_object: usize) -> Vec<u8> {
        let marker = format!("{object} 0 obj\n");
        let offset = pdf
            .windows(marker.len())
            .position(|bytes| bytes == marker.as_bytes())
            .expect("find image object");
        let stream = pdf[offset..]
            .windows(7)
            .position(|bytes| bytes == b"stream\n")
            .expect("find image stream")
            + offset
            + 7;
        let marker = format!("{length_object} 0 obj\n");
        let length_at = pdf
            .windows(marker.len())
            .position(|bytes| bytes == marker.as_bytes())
            .expect("find indirect stream length")
            + marker.len();
        let length_end = pdf[length_at..]
            .iter()
            .position(|byte| *byte == b'\n')
            .expect("end of length")
            + length_at;
        let length: usize = std::str::from_utf8(&pdf[length_at..length_end])
            .expect("length UTF-8")
            .parse()
            .expect("numeric length");
        pdf[stream..stream + length].to_vec()
    }

    #[test]
    fn error_variants_return_stable_worker_codes() {
        let cases = [
            (ImagePdfError::InvalidInput, "PATH_NOT_ALLOWED"),
            (ImagePdfError::InvalidOutputPath, "OUTPUT_PATH_NOT_ALLOWED"),
            (ImagePdfError::InvalidImage, "CORRUPTED_IMAGE"),
            (ImagePdfError::UnsupportedImage, "UNSUPPORTED_FORMAT"),
            (ImagePdfError::TooLarge, "INPUT_TOO_LARGE"),
            (ImagePdfError::OutputTooLarge, "OUTPUT_SIZE_EXCEEDED"),
            (ImagePdfError::Cancelled, "CANCELLED"),
            (
                ImagePdfError::InputIo(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "input denied",
                )),
                "INPUT_READ_FAILED",
            ),
            (
                ImagePdfError::OutputIo(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "output denied",
                )),
                "OUTPUT_WRITE_FAILED",
            ),
        ];

        for (error, expected) in cases {
            assert_eq!(error.code(), expected);
        }
    }

    #[test]
    fn bmp_24_bit_bottom_up_and_top_down_preserve_color_order_and_padding() {
        let files = TestFiles::new();
        let expected = [255u8, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 0];
        for (name, height) in [("bottom.bmp", 2), ("top.bmp", -2)] {
            let input = files.path(name);
            bmp_input(&input, 2, height, 3, &expected);
            let output = files.path(&format!("{name}.pdf"));
            write_image_pdf(&input, &output, || false).expect("convert BMP");
            assert_eq!(
                inflate_stream(&fs::read(output).expect("PDF"), 5, 6),
                expected
            );
        }
    }

    #[test]
    fn bmp_32_bit_preserves_alpha_as_pdf_soft_mask() {
        let files = TestFiles::new();
        let input = files.path("alpha.bmp");
        bmp_input(&input, 2, -1, 4, &[255, 0, 0, 255, 0, 0, 255, 64]);
        let output = files.path("alpha.pdf");
        write_image_pdf(&input, &output, || false).expect("convert BGRA BMP");
        let pdf = fs::read(output).expect("PDF");
        assert_eq!(inflate_stream(&pdf, 5, 7), [255, 0, 0, 0, 0, 255]);
        assert_eq!(inflate_stream(&pdf, 6, 8), [255, 64]);
    }

    #[test]
    fn bmp_rejects_compressed_paletted_profile_malformed_or_oversized_images() {
        let files = TestFiles::new();
        let input = files.path("image.bmp");
        let base = bmp_input(&input, 1, 1, 3, &[255, 0, 0]);
        let output = files.path("image.pdf");
        for (offset, replacement, error) in [
            (30, 1u32, "compression"),
            (46, 1u32, "palette"),
            (50, 1u32, "important colors"),
            (10, 55u32, "pixel offset"),
            (14, 124u32, "embedded profile header"),
            (2, 99u32, "file size"),
            (34, 99u32, "pixel size"),
        ] {
            let mut malformed = base.clone();
            malformed[offset..offset + 4].copy_from_slice(&replacement.to_le_bytes());
            fs::write(&input, malformed).expect("mutate BMP");
            assert!(
                write_image_pdf(&input, &output, || false).is_err(),
                "accepted {error}"
            );
            assert!(!output.exists());
        }
        fs::write(&input, &base[..base.len() - 1]).expect("truncate BMP");
        assert!(matches!(
            write_image_pdf(&input, &output, || false),
            Err(ImagePdfError::InvalidImage)
        ));
        let mut oversized = base;
        oversized[18..22].copy_from_slice(&(MAX_SIDE + 1).to_le_bytes());
        fs::write(&input, oversized).expect("oversize BMP");
        assert!(matches!(
            write_image_pdf(&input, &output, || false),
            Err(ImagePdfError::TooLarge)
        ));
        assert!(!output.exists());
    }

    #[test]
    fn bmp_cancellation_does_not_leave_an_output() {
        use std::cell::Cell;

        let files = TestFiles::new();
        let input = files.path("cancel.bmp");
        bmp_input(&input, 2, 2, 3, &[255, 0, 0].repeat(4));
        let output = files.path("cancel.pdf");
        let checks = Cell::new(0usize);
        let result = write_image_pdf(&input, &output, || {
            checks.set(checks.get() + 1);
            checks.get() > 4
        });
        assert!(matches!(result, Err(ImagePdfError::Cancelled)));
        assert!(!output.exists());
    }

    #[test]
    fn mixed_images_form_multi_page_pdf_with_distinct_dimensions() {
        let files = TestFiles::new();
        let png = files.path("first.png");
        png_input(&png, 2, 1, png::ColorType::Rgb, &[255, 0, 0, 0, 255, 0]);
        let bmp = files.path("second.bmp");
        bmp_input(&bmp, 1, -2, 3, &[0, 0, 255, 255, 255, 0]);
        let jpg = files.path("third.jpg");
        let jpeg = jpeg_input(&jpg, 3, 1, &[0, 255, 255].repeat(3));
        let output = files.path("album.pdf");
        let mut updates = Vec::new();
        write_images_pdf(
            &[png, bmp, jpg],
            &output,
            || false,
            |done, total| {
                updates.push((done, total));
            },
        )
        .expect("convert mixed album");
        let pdf = fs::read(&output).expect("multi-page PDF");
        assert!(pdf.ends_with(b"%%EOF\n"));
        assert_eq!(updates, [(1, 3), (2, 3), (3, 3)]);
        assert_eq!(
            pdf.windows(b"/Type /Page /Parent".len())
                .filter(|chunk| *chunk == b"/Type /Page /Parent")
                .count(),
            3
        );
        for expected in [
            b"/MediaBox [0 0 2.0000 1.0000]".as_slice(),
            b"/MediaBox [0 0 1.0000 2.0000]".as_slice(),
            b"/MediaBox [0 0 3.0000 1.0000]".as_slice(),
        ] {
            assert!(pdf.windows(expected.len()).any(|chunk| chunk == expected));
        }
        assert_eq!(inflate_stream(&pdf, 5, 6), [255, 0, 0, 0, 255, 0]);
        assert_eq!(inflate_stream(&pdf, 11, 12), [0, 0, 255, 255, 255, 0]);
        assert_eq!(stream(&pdf, 17, 18), jpeg);
    }

    #[test]
    fn multi_page_invalid_count_cancellation_and_failure_clean_up() {
        let files = TestFiles::new();
        let valid = files.path("valid.png");
        png_input(&valid, 1, 1, png::ColorType::Rgb, &[10, 20, 30]);
        let output = files.path("album.pdf");
        assert!(matches!(
            write_images_pdf(&[], &output, || false, |_, _| {}),
            Err(ImagePdfError::InvalidInput)
        ));
        assert!(matches!(
            write_images_pdf(
                &vec![valid.clone(); MAX_IMAGES + 1],
                &output,
                || false,
                |_, _| {}
            ),
            Err(ImagePdfError::InvalidInput)
        ));
        assert!(!output.exists());
        let missing = files.path("missing.bmp");
        assert!(write_images_pdf(&[valid.clone(), missing], &output, || false, |_, _| {}).is_err());
        assert!(!output.exists());
        assert!(matches!(
            write_images_pdf(
                &[valid.clone(), valid.clone()],
                &output,
                || output.exists(),
                |_, _| {}
            ),
            Err(ImagePdfError::Cancelled)
        ));
        assert!(!output.exists());
        write_images_pdf(
            &[valid],
            &output,
            || false,
            |done, total| {
                assert_eq!((done, total), (1, 1));
            },
        )
        .expect("single-page batch path");
        assert_eq!(
            inflate_stream(&fs::read(output).expect("PDF"), 5, 6),
            [10, 20, 30]
        );
    }

    #[test]
    fn io_error_variants_preserve_their_sources() {
        let input = ImagePdfError::InputIo(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "input denied",
        ));
        let output =
            ImagePdfError::OutputIo(io::Error::new(io::ErrorKind::BrokenPipe, "output closed"));

        let input_source =
            std::error::Error::source(&input).and_then(|source| source.downcast_ref::<io::Error>());
        let output_source = std::error::Error::source(&output)
            .and_then(|source| source.downcast_ref::<io::Error>());
        assert_eq!(
            input_source.map(io::Error::kind),
            Some(io::ErrorKind::PermissionDenied)
        );
        assert_eq!(
            output_source.map(io::Error::kind),
            Some(io::ErrorKind::BrokenPipe)
        );
    }

    #[test]
    fn missing_absolute_input_is_reported_as_input_read_failure() {
        let files = TestFiles::new();
        let input = files.path("missing.png");
        let output = files.path("missing.pdf");

        let error = write_image_pdf(&input, &output, || false).expect_err("missing input");
        assert!(matches!(
            &error,
            ImagePdfError::InputIo(source) if source.kind() == io::ErrorKind::NotFound
        ));
        assert_eq!(error.code(), "INPUT_READ_FAILED");
        assert!(!output.exists());
    }

    #[test]
    fn pdf_output_limit_has_a_distinct_error_code() {
        let mut pdf = PdfOutput::new(Vec::new());
        pdf.position = MAX_PDF_BYTES;

        let error = pdf.append(b"x").expect_err("output size limit");
        assert!(matches!(&error, ImagePdfError::OutputTooLarge));
        assert_eq!(error.code(), "OUTPUT_SIZE_EXCEEDED");
    }

    #[test]
    fn rgba_png_writes_exact_color_and_alpha_streams() {
        let files = TestFiles::new();
        let input = files.path("transparent.png");
        let output = files.path("transparent.pdf");
        png_input(
            &input,
            2,
            1,
            png::ColorType::Rgba,
            &[255, 0, 0, 255, 0, 0, 255, 64],
        );
        write_image_pdf(&input, &output, || false).expect("write transparent PDF");
        let bytes = fs::read(output).expect("read PDF");
        assert!(bytes.starts_with(b"%PDF-1.4"));
        assert!(bytes.windows(12).any(|part| part == b"/SMask 6 0 R"));
        assert!(bytes
            .windows(b"/MediaBox [0 0 2.0000 1.0000]".len())
            .any(|part| part == b"/MediaBox [0 0 2.0000 1.0000]"));
        let mut colors = Vec::new();
        flate2::read::ZlibDecoder::new(stream(&bytes, 5, 7).as_slice())
            .read_to_end(&mut colors)
            .expect("decode RGB");
        assert_eq!(colors, [255, 0, 0, 0, 0, 255]);
        let mut alpha = Vec::new();
        flate2::read::ZlibDecoder::new(stream(&bytes, 6, 8).as_slice())
            .read_to_end(&mut alpha)
            .expect("decode soft mask");
        assert_eq!(alpha, [255, 64]);
    }

    #[test]
    fn indexed_png_with_transparency_is_expanded_without_losing_alpha() {
        let files = TestFiles::new();
        let input = files.path("indexed.png");
        let mut encoder = png::Encoder::new(File::create(&input).expect("PNG"), 2, 1);
        encoder.set_color(png::ColorType::Indexed);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_palette(vec![255, 0, 0, 0, 255, 0]);
        encoder.set_trns(vec![255, 0]);
        encoder
            .write_header()
            .expect("palette PNG")
            .write_image_data(&[0, 1])
            .expect("palette pixels");
        let output = files.path("indexed.pdf");
        write_image_pdf(&input, &output, || false).expect("write indexed image");
        let bytes = fs::read(output).expect("PDF bytes");
        let mut alpha = Vec::new();
        flate2::read::ZlibDecoder::new(stream(&bytes, 6, 8).as_slice())
            .read_to_end(&mut alpha)
            .expect("decode transparency");
        assert_eq!(alpha, [255, 0]);
    }

    #[test]
    fn single_frame_gif_preserves_palette_color_and_transparency() {
        let files = TestFiles::new();
        let input = files.path("transparent.gif");
        gif_input(&input, 2, 1, &[&[255, 0, 0, 255, 0, 0, 255, 0]]);
        let output = files.path("transparent-gif.pdf");
        write_image_pdf(&input, &output, || false).expect("write GIF PDF");
        let bytes = fs::read(output).expect("read GIF PDF");
        assert!(bytes.windows(12).any(|part| part == b"/SMask 6 0 R"));
        let mut colors = Vec::new();
        flate2::read::ZlibDecoder::new(stream(&bytes, 5, 7).as_slice())
            .read_to_end(&mut colors)
            .expect("decode GIF colors");
        assert_eq!(colors, [255, 0, 0, 0, 0, 255]);
        let mut alpha = Vec::new();
        flate2::read::ZlibDecoder::new(stream(&bytes, 6, 8).as_slice())
            .read_to_end(&mut alpha)
            .expect("decode GIF alpha");
        assert_eq!(alpha, [255, 0]);
    }

    #[test]
    fn lossless_webp_preserves_rgba_and_lossy_webp_is_accepted() {
        let files = TestFiles::new();
        let lossless = files.path("transparent.webp");
        fs::write(
            &lossless,
            webp_bytes(
                2,
                1,
                &[10, 20, 30, 255, 100, 110, 120, 64],
                image_webp::ColorType::Rgba8,
            ),
        )
        .expect("write lossless WebP");
        let lossless_pdf = files.path("transparent-webp.pdf");
        write_image_pdf(&lossless, &lossless_pdf, || false).expect("write lossless WebP PDF");
        let bytes = fs::read(lossless_pdf).expect("read lossless WebP PDF");
        let mut colors = Vec::new();
        flate2::read::ZlibDecoder::new(stream(&bytes, 5, 7).as_slice())
            .read_to_end(&mut colors)
            .expect("decode WebP colors");
        assert_eq!(colors, [10, 20, 30, 100, 110, 120]);
        let mut alpha = Vec::new();
        flate2::read::ZlibDecoder::new(stream(&bytes, 6, 8).as_slice())
            .read_to_end(&mut alpha)
            .expect("decode WebP alpha");
        assert_eq!(alpha, [255, 64]);

        const LOSSY_WEBP: &[u8] = &[
            0x52, 0x49, 0x46, 0x46, 0x38, 0x00, 0x00, 0x00, 0x57, 0x45, 0x42, 0x50, 0x56, 0x50,
            0x38, 0x20, 0x2c, 0x00, 0x00, 0x00, 0xb0, 0x01, 0x00, 0x9d, 0x01, 0x2a, 0x01, 0x00,
            0x01, 0x00, 0x02, 0x40, 0x38, 0x25, 0xa0, 0x02, 0x74, 0xba, 0x00, 0x06, 0x22, 0x00,
            0x00, 0xda, 0x6f, 0xfd, 0x26, 0xcf, 0xf8, 0x9b, 0x3f, 0xe2, 0x6c, 0xf9, 0x24, 0x7f,
            0xf0, 0xad, 0x67, 0xad, 0xd1, 0xa2, 0x20, 0x00,
        ];
        let lossy = files.path("lossy.webp");
        fs::write(&lossy, LOSSY_WEBP).expect("write lossy WebP fixture");
        let lossy_pdf = files.path("lossy.pdf");
        write_image_pdf(&lossy, &lossy_pdf, || false).expect("write lossy WebP PDF");
        let bytes = fs::read(lossy_pdf).expect("read lossy WebP PDF");
        let mut colors = Vec::new();
        flate2::read::ZlibDecoder::new(stream(&bytes, 5, 6).as_slice())
            .read_to_end(&mut colors)
            .expect("decode lossy WebP PDF stream");
        assert_eq!(colors.len(), 3);
    }

    #[test]
    fn static_tiff_preserves_rgb_and_alpha_in_pdf_streams() {
        let files = TestFiles::new();
        let input = files.path("alpha.tif");
        let pixels = [10, 20, 30, 255, 100, 110, 120, 64];
        rgba_tiff_input(&input, 2, 1, &[&pixels]);
        let output = files.path("alpha-tiff.pdf");

        write_image_pdf(&input, &output, || false).expect("write TIFF PDF");
        let bytes = fs::read(output).expect("read TIFF PDF");
        let mut colors = Vec::new();
        flate2::read::ZlibDecoder::new(stream(&bytes, 5, 7).as_slice())
            .read_to_end(&mut colors)
            .expect("decode TIFF colors");
        assert_eq!(colors, [10, 20, 30, 100, 110, 120]);
        let mut alpha = Vec::new();
        flate2::read::ZlibDecoder::new(stream(&bytes, 6, 8).as_slice())
            .read_to_end(&mut alpha)
            .expect("decode TIFF alpha");
        assert_eq!(alpha, [255, 64]);
    }

    #[test]
    fn tiff_gray_alpha_and_associated_alpha_are_preserved() {
        let files = TestFiles::new();
        let gray = files.path("gray-alpha.tif");
        alpha_tiff_input(
            &gray,
            png::ColorType::GrayscaleAlpha,
            tiff::tags::ExtraSamples::UnassociatedAlpha,
            &[10, 255, 100, 64],
        );
        let gray_pdf = files.path("gray-alpha.pdf");
        write_image_pdf(&gray, &gray_pdf, || false).expect("write GrayA TIFF PDF");
        let bytes = fs::read(gray_pdf).expect("read GrayA TIFF PDF");
        let mut colors = Vec::new();
        flate2::read::ZlibDecoder::new(stream(&bytes, 5, 7).as_slice())
            .read_to_end(&mut colors)
            .expect("decode GrayA colors");
        assert_eq!(colors, [10, 100]);
        let mut alpha = Vec::new();
        flate2::read::ZlibDecoder::new(stream(&bytes, 6, 8).as_slice())
            .read_to_end(&mut alpha)
            .expect("decode GrayA alpha");
        assert_eq!(alpha, [255, 64]);

        let associated = files.path("associated-alpha.tiff");
        alpha_tiff_input(
            &associated,
            png::ColorType::Rgba,
            tiff::tags::ExtraSamples::AssociatedAlpha,
            &[64, 32, 16, 128],
        );
        let associated_pdf = files.path("associated-alpha.pdf");
        write_image_pdf(&associated, &associated_pdf, || false)
            .expect("write associated-alpha TIFF PDF");
        let bytes = fs::read(associated_pdf).expect("read associated-alpha TIFF PDF");
        let mut colors = Vec::new();
        flate2::read::ZlibDecoder::new(stream(&bytes, 5, 7).as_slice())
            .read_to_end(&mut colors)
            .expect("decode unpremultiplied colors");
        assert_eq!(colors, [128, 64, 32]);
        let mut alpha = Vec::new();
        flate2::read::ZlibDecoder::new(stream(&bytes, 6, 8).as_slice())
            .read_to_end(&mut alpha)
            .expect("decode associated alpha");
        assert_eq!(alpha, [128]);
    }

    #[test]
    fn multipage_tiff_is_rejected_without_selecting_the_first_page() {
        let files = TestFiles::new();
        let input = files.path("multipage.tiff");
        rgba_tiff_input(&input, 1, 1, &[&[255, 0, 0, 255], &[0, 0, 255, 255]]);
        let output = files.path("multipage.pdf");

        let error = write_image_pdf(&input, &output, || false).expect_err("multipage TIFF");
        assert!(matches!(&error, ImagePdfError::UnsupportedImage));
        assert_eq!(error.code(), "UNSUPPORTED_FORMAT");
        assert!(!output.exists());
    }

    #[test]
    fn tiff_sub_ifd_is_rejected_as_an_additional_image_directory() {
        let files = TestFiles::new();
        let input = files.path("sub-ifd.tiff");
        rgb_tiff_with_sub_ifd(&input);
        let output = files.path("sub-ifd.pdf");

        let error = write_image_pdf(&input, &output, || false).expect_err("TIFF SubIFD");
        assert!(matches!(&error, ImagePdfError::UnsupportedImage));
        assert_eq!(error.code(), "UNSUPPORTED_FORMAT");
        assert!(!output.exists());
    }

    #[test]
    fn tiff_dimensions_are_bounded_before_decoding_pixels() {
        let files = TestFiles::new();
        let input = files.path("wide.tif");
        let width = MAX_SIDE + 1;
        let pixels = vec![0u8; usize::try_from(width).expect("width") * 3];
        rgb_tiff_input(&input, width, 1, &pixels);
        let output = files.path("wide.pdf");

        assert!(matches!(
            write_image_pdf(&input, &output, || false),
            Err(ImagePdfError::TooLarge)
        ));
        assert!(!output.exists());
    }

    #[test]
    fn cancellation_from_tiff_decoder_io_leaves_no_output() {
        use std::cell::Cell;

        let files = TestFiles::new();
        let input = files.path("cancel.tiff");
        rgb_tiff_input(&input, 2, 1, &[10, 20, 30, 40, 50, 60]);
        let output = files.path("cancel-tiff.pdf");
        let checks = Cell::new(0u32);
        let should_stop = || {
            checks.set(checks.get() + 1);
            checks.get() >= 2
        };

        let error = write_image_pdf(&input, &output, should_stop).expect_err("cancel TIFF decode");
        assert!(matches!(&error, ImagePdfError::Cancelled));
        assert_eq!(error.code(), "CANCELLED");
        assert!(!output.exists());
    }

    #[test]
    fn animated_gif_and_webp_are_rejected_without_selecting_a_frame() {
        let files = TestFiles::new();
        let gif = files.path("animated.gif");
        gif_input(&gif, 1, 1, &[&[255, 0, 0, 255], &[0, 0, 255, 255]]);
        let gif_pdf = files.path("animated-gif.pdf");
        assert!(matches!(
            write_image_pdf(&gif, &gif_pdf, || false),
            Err(ImagePdfError::UnsupportedImage)
        ));
        assert!(!gif_pdf.exists());

        let webp = files.path("animated.webp");
        fs::write(&webp, animated_webp()).expect("write animated WebP");
        let webp_pdf = files.path("animated-webp.pdf");
        assert!(matches!(
            write_image_pdf(&webp, &webp_pdf, || false),
            Err(ImagePdfError::UnsupportedImage)
        ));
        assert!(!webp_pdf.exists());
    }

    #[test]
    fn partial_frame_gif_is_rejected_instead_of_guessing_canvas_composition() {
        let files = TestFiles::new();
        let input = files.path("partial.gif");
        let file = File::create(&input).expect("create GIF");
        let mut encoder = gif::Encoder::new(file, 2, 1, &[255, 0, 0]).expect("GIF header");
        let frame = gif::Frame::from_indexed_pixels(1, 1, vec![0], None);
        encoder.write_frame(&frame).expect("partial GIF frame");
        encoder.into_inner().expect("GIF trailer");
        let output = files.path("partial.pdf");
        assert!(matches!(
            write_image_pdf(&input, &output, || false),
            Err(ImagePdfError::UnsupportedImage)
        ));
        assert!(!output.exists());
    }

    #[test]
    fn baseline_jpeg_is_embedded_without_reencoding() {
        let files = TestFiles::new();
        let input = files.path("photo.jpeg");
        let output = files.path("photo.pdf");
        let jpeg = jpeg_input(&input, 2, 1, &[255, 0, 0, 0, 0, 255]);
        write_image_pdf(&input, &output, || false).expect("write JPEG PDF");
        let bytes = fs::read(output).expect("PDF bytes");
        assert!(bytes
            .windows(b"/DCTDecode".len())
            .any(|part| part == b"/DCTDecode"));
        assert_eq!(stream(&bytes, 5, 6), jpeg);
        assert!(bytes.ends_with(b"%%EOF\n"));
    }

    #[test]
    fn progressive_and_grayscale_jpeg_are_supported() {
        let files = TestFiles::new();
        let input = files.path("progressive.jpg");
        let mut jpeg = Vec::new();
        let mut encoder = jpeg_encoder::Encoder::new(&mut jpeg, 90);
        encoder.set_progressive(true);
        encoder
            .encode(
                &[255, 0, 0, 0, 0, 255, 255, 0, 0, 0, 0, 255],
                2,
                2,
                jpeg_encoder::ColorType::Rgb,
            )
            .expect("progressive JPEG");
        fs::write(&input, &jpeg).expect("save progressive JPEG");
        let output = files.path("progressive.pdf");
        write_image_pdf(&input, &output, || false).expect("progressive image PDF");
        assert_eq!(stream(&fs::read(&output).expect("read PDF"), 5, 6), jpeg);

        let gray = files.path("gray.jpg");
        let mut jpeg = Vec::new();
        jpeg_encoder::Encoder::new(&mut jpeg, 90)
            .encode(&[25, 225], 2, 1, jpeg_encoder::ColorType::Luma)
            .expect("grayscale JPEG");
        fs::write(&gray, &jpeg).expect("save grayscale JPEG");
        let gray_pdf = files.path("gray.pdf");
        write_image_pdf(&gray, &gray_pdf, || false).expect("grayscale image PDF");
        let bytes = fs::read(&gray_pdf).expect("grayscale PDF");
        assert!(bytes
            .windows(b"/ColorSpace /DeviceGray".len())
            .any(|part| part == b"/ColorSpace /DeviceGray"));
    }

    #[test]
    fn sixteen_bit_png_is_not_silently_reduced_to_eight_bits() {
        let files = TestFiles::new();
        let input = files.path("sixteen.png");
        let mut encoder = png::Encoder::new(File::create(&input).expect("PNG"), 1, 1);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Sixteen);
        encoder
            .write_header()
            .expect("16-bit PNG")
            .write_image_data(&[0xff, 0xff, 0x00, 0x00, 0x7f, 0xff])
            .expect("16-bit RGB");
        let output = files.path("sixteen.pdf");
        let error = write_image_pdf(&input, &output, || false).expect_err("16-bit PNG");
        assert!(matches!(&error, ImagePdfError::UnsupportedImage));
        assert_eq!(error.code(), "UNSUPPORTED_FORMAT");
        assert!(!output.exists());
    }

    #[test]
    fn oversized_input_is_rejected_before_decoding_or_creating_output() {
        let files = TestFiles::new();
        let input = files.path("sparse.jpg");
        let file = File::create(&input).expect("create sparse file");
        file.set_len(MAX_INPUT_BYTES + 1)
            .expect("extend sparse file");
        let output = files.path("sparse.pdf");
        assert!(matches!(
            write_image_pdf(&input, &output, || false),
            Err(ImagePdfError::TooLarge)
        ));
        assert!(!output.exists());
    }

    #[test]
    fn corrupt_jpeg_or_mismatched_extension_is_rejected_without_output() {
        let files = TestFiles::new();
        let input = files.path("broken.jpg");
        let output = files.path("broken.pdf");
        let mut jpeg = jpeg_input(&input, 1, 1, &[255, 0, 0]);
        jpeg.pop();
        fs::write(&input, &jpeg).expect("truncate image");
        let error = write_image_pdf(&input, &output, || false).expect_err("truncated JPEG");
        assert!(matches!(&error, ImagePdfError::InvalidImage));
        assert_eq!(error.code(), "CORRUPTED_IMAGE");
        assert!(!output.exists());
        let png = files.path("not-a-jpeg.jpg");
        png_input(&png, 1, 1, png::ColorType::Rgb, &[1, 2, 3]);
        assert!(matches!(
            write_image_pdf(&png, &output, || false),
            Err(ImagePdfError::InvalidImage)
        ));
    }

    #[test]
    fn structurally_valid_jpeg_with_truncated_entropy_is_not_embedded() {
        let files = TestFiles::new();
        let input = files.path("broken-entropy.jpg");
        let output = files.path("broken-entropy.pdf");
        let mut pixels = Vec::with_capacity(200 * 100 * 3);
        for _ in 0..100 {
            for x in 0..200 {
                pixels.extend_from_slice(if x < 100 { &[255, 0, 0] } else { &[0, 0, 255] });
            }
        }
        let jpeg = jpeg_input(&input, 200, 100, &pixels);
        let sos = jpeg
            .windows(2)
            .position(|bytes| bytes == [0xff, 0xda])
            .expect("start of scan");
        let header = usize::from(u16::from_be_bytes([jpeg[sos + 2], jpeg[sos + 3]]));
        let entropy_start = sos + 2 + header;
        assert!(entropy_start < jpeg.len() - 2);
        let mut broken = jpeg[..entropy_start].to_vec();
        broken.extend_from_slice(&[0x00, 0xff, 0xd9]);
        fs::write(&input, &broken).expect("save malformed JPEG");

        let mut scanned = File::open(&input).expect("open malformed JPEG");
        assert!(
            matches!(
                inspect_jpeg(&mut scanned, broken.len() as u64, &|| false),
                Err(ImagePdfError::InvalidImage)
            ),
            "the minimum entropy bound must reject early EOI"
        );
        assert!(matches!(
            write_image_pdf(&input, &output, || false),
            Err(ImagePdfError::InvalidImage)
        ));
        assert!(!output.exists());
    }

    #[test]
    fn invalid_huffman_tables_are_rejected_by_full_decode() {
        let files = TestFiles::new();
        let input = files.path("broken-huffman.jpg");
        let output = files.path("broken-huffman.pdf");
        let mut jpeg = jpeg_input(&input, 32, 32, &[140, 80, 40].repeat(32 * 32));
        let dht = jpeg
            .windows(2)
            .position(|bytes| bytes == [0xff, 0xc4])
            .expect("DHT segment");
        jpeg[dht + 4] = 0xff;
        fs::write(&input, &jpeg).expect("save broken Huffman table");

        let mut scanned = File::open(&input).expect("open broken JPEG");
        assert!(
            inspect_jpeg(&mut scanned, jpeg.len() as u64, &|| false).is_ok(),
            "outer JPEG marker structure remains intact"
        );
        assert!(matches!(
            write_image_pdf(&input, &output, || false),
            Err(ImagePdfError::InvalidImage)
        ));
        assert!(!output.exists());
    }

    #[test]
    fn cancellation_while_decoding_jpeg_is_reported_without_an_output() {
        use std::cell::Cell;

        let files = TestFiles::new();
        let input = files.path("cancel.jpg");
        let output = files.path("cancel.pdf");
        jpeg_input(&input, 200, 100, &[100, 90, 80].repeat(200 * 100));
        let checks = Cell::new(0u32);
        let should_stop = || {
            checks.set(checks.get() + 1);
            checks.get() >= 4
        };
        let error = write_image_pdf(&input, &output, should_stop).expect_err("cancel JPEG decode");
        assert!(matches!(&error, ImagePdfError::Cancelled));
        assert_eq!(error.code(), "CANCELLED");
        assert!(checks.get() >= 4);
        assert!(!output.exists());
    }

    #[test]
    fn exif_rotation_is_rejected_instead_of_writing_a_sideways_page() {
        let files = TestFiles::new();
        let input = files.path("rotated.jpg");
        let jpeg = jpeg_input(&input, 2, 1, &[255, 0, 0, 0, 0, 255]);
        let exif = b"Exif\0\0II*\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0\x06\0\0\0\0\0\0\0";
        assert_eq!(exif_orientation(exif).expect("EXIF orientation"), 6);
        let mut rotated = vec![0xff, 0xd8, 0xff, 0xe1];
        let app_length = u16::try_from(exif.len() + 2).expect("APP1 size");
        rotated.extend_from_slice(&app_length.to_be_bytes());
        rotated.extend_from_slice(exif);
        rotated.extend_from_slice(&jpeg[2..]);
        fs::write(&input, rotated).expect("EXIF JPEG");
        let output = files.path("rotated.pdf");
        assert!(matches!(
            write_image_pdf(&input, &output, || false),
            Err(ImagePdfError::UnsupportedImage)
        ));
        assert!(!output.exists());
    }

    #[test]
    fn pixel_and_axis_limits_are_checked_before_allocation() {
        assert!(matches!(
            checked_dimensions(MAX_SIDE + 1, 1),
            Err(ImagePdfError::TooLarge)
        ));
        assert!(matches!(
            checked_dimensions(4_000, 3_001),
            Err(ImagePdfError::TooLarge)
        ));
        assert!(checked_dimensions(4_000, 3_000).is_ok());
    }

    #[test]
    fn gif_and_webp_dimensions_are_bounded_before_pixel_allocation() {
        let files = TestFiles::new();
        let gif = files.path("oversized.gif");
        gif_input(&gif, 1, 1, &[&[0, 0, 0, 255]]);
        let mut gif_bytes = fs::read(&gif).expect("read valid GIF");
        gif_bytes[6..8]
            .copy_from_slice(&u16::try_from(MAX_SIDE + 1).expect("GIF side").to_le_bytes());
        fs::write(&gif, gif_bytes).expect("write oversized GIF header");
        let gif_pdf = files.path("oversized-gif.pdf");
        assert!(matches!(
            write_image_pdf(&gif, &gif_pdf, || false),
            Err(ImagePdfError::TooLarge)
        ));
        assert!(!gif_pdf.exists());

        let webp = files.path("oversized.webp");
        let mut bytes = webp_bytes(1, 1, &[0, 0, 0], image_webp::ColorType::Rgb8);
        let dimensions = 3_999u32 | (3_999u32 << 14);
        bytes[21..25].copy_from_slice(&dimensions.to_le_bytes());
        fs::write(&webp, bytes).expect("write oversized WebP header");
        let webp_pdf = files.path("oversized-webp.pdf");
        assert!(matches!(
            write_image_pdf(&webp, &webp_pdf, || false),
            Err(ImagePdfError::TooLarge)
        ));
        assert!(!webp_pdf.exists());
    }

    #[test]
    fn cancellation_cleans_partial_output_and_existing_file_is_untouched() {
        let files = TestFiles::new();
        let input = files.path("color.png");
        let output = files.path("color.pdf");
        png_input(&input, 2, 1, png::ColorType::Rgb, &[255, 0, 0, 0, 0, 255]);
        assert!(matches!(
            write_image_pdf(&input, &output, || output.exists()),
            Err(ImagePdfError::Cancelled)
        ));
        assert!(!output.exists());
        fs::write(&output, b"old result").expect("existing result");
        let error = write_image_pdf(&input, &output, || false).expect_err("existing output");
        assert!(matches!(
            &error,
            ImagePdfError::OutputIo(source) if source.kind() == io::ErrorKind::AlreadyExists
        ));
        assert_eq!(error.code(), "OUTPUT_WRITE_FAILED");
        assert_eq!(fs::read(output).expect("untouched result"), b"old result");
    }

    #[test]
    fn cancellation_while_writing_webp_pixels_removes_partial_pdf() {
        let files = TestFiles::new();
        let input = files.path("cancel.webp");
        fs::write(
            &input,
            webp_bytes(
                20,
                20,
                &[25, 100, 225, 128].repeat(20 * 20),
                image_webp::ColorType::Rgba8,
            ),
        )
        .expect("write WebP");
        let output = files.path("cancel-webp.pdf");
        assert!(matches!(
            write_image_pdf(&input, &output, || output.exists()),
            Err(ImagePdfError::Cancelled)
        ));
        assert!(!output.exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_system_renderer_opens_generated_pdf_and_displays_both_colors() {
        let files = TestFiles::new();
        let input = files.path("wide.png");
        let output = files.path("wide.pdf");
        let mut pixels = Vec::with_capacity(200 * 100 * 3);
        for _ in 0..100 {
            for x in 0..200 {
                pixels.extend_from_slice(if x < 100 { &[255, 0, 0] } else { &[0, 0, 255] });
            }
        }
        png_input(&input, 200, 100, png::ColorType::Rgb, &pixels);
        write_image_pdf(&input, &output, || false).expect("image-backed PDF");
        let render = crate::render::render_pdf(
            &output,
            &files.dir,
            "rendered",
            &crate::render::RenderOptions {
                selection: crate::pdf::PageSelection::All,
                dpi: 150,
                format: crate::render::ImageFormat::Png,
                jpg_quality: 85,
                max_selected_pages: 1,
                max_output_bytes: 16 * 1024 * 1024,
            },
            || false,
            |_, _, _| {},
        )
        .expect("system renderer should open PDF");
        assert_eq!(render.selected_page_count, 1);
        let reader = png::Decoder::new(File::open(&render.outputs[0]).expect("rendered PNG"))
            .read_info()
            .expect("PNG header");
        assert_eq!((reader.info().width, reader.info().height), (417, 209));
        let mut reader = reader;
        let mut buf = vec![0; reader.output_buffer_size()];
        let frame = reader.next_frame(&mut buf).expect("pixels");
        assert_eq!(frame.color_type, png::ColorType::Rgb);
        let stride = 3 * usize::try_from(frame.width).expect("bounded width");
        let red = &buf[100 * stride + 40 * 3..100 * stride + 40 * 3 + 3];
        let blue = &buf[100 * stride + 370 * 3..100 * stride + 370 * 3 + 3];
        assert!(
            red[0] > 200 && red[2] < 50,
            "left half should be red: {red:?}"
        );
        assert!(
            blue[2] > 200 && blue[0] < 50,
            "right half should be blue: {blue:?}"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_renderer_composites_png_soft_mask_over_white() {
        let files = TestFiles::new();
        let input = files.path("alpha.png");
        let output = files.path("alpha.pdf");
        let pixels = [0u8, 0, 255, 64].repeat(200 * 100);
        png_input(&input, 200, 100, png::ColorType::Rgba, &pixels);
        write_image_pdf(&input, &output, || false).expect("alpha PDF");
        let rendered = crate::render::render_pdf(
            &output,
            &files.dir,
            "alpha-render",
            &crate::render::RenderOptions {
                selection: crate::pdf::PageSelection::All,
                dpi: 150,
                format: crate::render::ImageFormat::Png,
                jpg_quality: 85,
                max_selected_pages: 1,
                max_output_bytes: 16 * 1024 * 1024,
            },
            || false,
            |_, _, _| {},
        )
        .expect("render transparent PDF");
        let mut reader =
            png::Decoder::new(File::open(&rendered.outputs[0]).expect("rendered image"))
                .read_info()
                .expect("PNG header");
        let mut pixels = vec![0; reader.output_buffer_size()];
        let frame = reader.next_frame(&mut pixels).expect("render pixels");
        let stride = 3 * usize::try_from(frame.width).expect("bounded width");
        let blue = &pixels[100 * stride + 200 * 3..100 * stride + 200 * 3 + 3];
        assert!(
            blue[0] > 170 && blue[1] > 170 && blue[2] > 230,
            "alpha should composite with white: {blue:?}"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_renderer_displays_embedded_jpeg_colors() {
        let files = TestFiles::new();
        let input = files.path("photo.jpg");
        let output = files.path("photo.pdf");
        let mut pixels = Vec::with_capacity(200 * 100 * 3);
        for _ in 0..100 {
            for x in 0..200 {
                pixels.extend_from_slice(if x < 100 { &[255, 0, 0] } else { &[0, 0, 255] });
            }
        }
        jpeg_input(&input, 200, 100, &pixels);
        write_image_pdf(&input, &output, || false).expect("JPEG PDF");
        let rendered = crate::render::render_pdf(
            &output,
            &files.dir,
            "jpeg-render",
            &crate::render::RenderOptions {
                selection: crate::pdf::PageSelection::All,
                dpi: 150,
                format: crate::render::ImageFormat::Png,
                jpg_quality: 85,
                max_selected_pages: 1,
                max_output_bytes: 16 * 1024 * 1024,
            },
            || false,
            |_, _, _| {},
        )
        .expect("render JPEG PDF");
        let mut reader =
            png::Decoder::new(File::open(&rendered.outputs[0]).expect("rendered image"))
                .read_info()
                .expect("PNG header");
        let mut pixels = vec![0; reader.output_buffer_size()];
        let frame = reader.next_frame(&mut pixels).expect("render pixels");
        let stride = 3 * usize::try_from(frame.width).expect("bounded width");
        let red = &pixels[100 * stride + 40 * 3..100 * stride + 40 * 3 + 3];
        let blue = &pixels[100 * stride + 370 * 3..100 * stride + 370 * 3 + 3];
        assert!(red[0] > 180 && red[2] < 60, "JPEG red: {red:?}");
        assert!(blue[2] > 180 && blue[0] < 60, "JPEG blue: {blue:?}");
    }
}
