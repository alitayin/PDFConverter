//! Image-backed PowerPoint presentations. Each slide preserves the appearance
//! of a rendered PDF page, but the page content is not editable in PowerPoint.

use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use thiserror::Error;
use zip::write::SimpleFileOptions;
use zip::CompressionMethod;

const MAX_SLIDES: usize = 200;
const MAX_PIXELS: u64 = 40_000_000;
const MAX_IMAGE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_XML_BYTES: u64 = 16 * 1024 * 1024;
const MAX_ZIP32_OFFSET: u64 = u32::MAX as u64 - 1;
const SLIDE_LONG_SIDE: u64 = 12_192_000;
const SLIDE_MIN_SIDE: u64 = 914_400;

const ROOT_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="ppt/presentation.xml"/>
<Relationship Id="rId2" Type="http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties" Target="docProps/core.xml"/>
<Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/extended-properties" Target="docProps/app.xml"/>
</Relationships>"#;

const CORE_PROPS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties" xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>PDF pages</dc:title><dc:creator>Minimal PDF Converter</dc:creator></cp:coreProperties>"#;

const GROUP_SHAPE: &str = r#"<p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/><a:chOff x="0" y="0"/><a:chExt cx="0" cy="0"/></a:xfrm></p:grpSpPr>"#;

const MASTER: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sldMaster xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"><p:cSld name="Blank Master"><p:spTree><p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/><a:chOff x="0" y="0"/><a:chExt cx="0" cy="0"/></a:xfrm></p:grpSpPr></p:spTree></p:cSld><p:clrMap accent1="accent1" accent2="accent2" accent3="accent3" accent4="accent4" accent5="accent5" accent6="accent6" bg1="lt1" bg2="lt2" folHlink="folHlink" hlink="hlink" tx1="dk1" tx2="dk2"/><p:sldLayoutIdLst><p:sldLayoutId id="2147483649" r:id="rId1"/></p:sldLayoutIdLst><p:txStyles><p:titleStyle/><p:bodyStyle/><p:otherStyle/></p:txStyles></p:sldMaster>"#;

const MASTER_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout" Target="../slideLayouts/slideLayout1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/theme" Target="../theme/theme1.xml"/></Relationships>"#;

const LAYOUT: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sldLayout xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" type="blank" preserve="1"><p:cSld name="Blank"><p:spTree><p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/><a:chOff x="0" y="0"/><a:chExt cx="0" cy="0"/></a:xfrm></p:grpSpPr></p:spTree></p:cSld><p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sldLayout>"#;

const LAYOUT_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster" Target="../slideMasters/slideMaster1.xml"/></Relationships>"#;

const THEME: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<a:theme xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" name="Minimal"><a:themeElements><a:clrScheme name="Minimal"><a:dk1><a:sysClr val="windowText" lastClr="000000"/></a:dk1><a:lt1><a:sysClr val="window" lastClr="FFFFFF"/></a:lt1><a:dk2><a:srgbClr val="292929"/></a:dk2><a:lt2><a:srgbClr val="F2F2F2"/></a:lt2><a:accent1><a:srgbClr val="177E89"/></a:accent1><a:accent2><a:srgbClr val="D65849"/></a:accent2><a:accent3><a:srgbClr val="6C944D"/></a:accent3><a:accent4><a:srgbClr val="C28D32"/></a:accent4><a:accent5><a:srgbClr val="4676A5"/></a:accent5><a:accent6><a:srgbClr val="7D6793"/></a:accent6><a:hlink><a:srgbClr val="0000FF"/></a:hlink><a:folHlink><a:srgbClr val="800080"/></a:folHlink></a:clrScheme><a:fontScheme name="Minimal"><a:majorFont><a:latin typeface="Arial"/><a:ea typeface=""/><a:cs typeface=""/></a:majorFont><a:minorFont><a:latin typeface="Arial"/><a:ea typeface=""/><a:cs typeface=""/></a:minorFont></a:fontScheme><a:fmtScheme name="Minimal"><a:fillStyleLst><a:solidFill><a:schemeClr val="accent1"/></a:solidFill><a:solidFill><a:schemeClr val="accent2"/></a:solidFill><a:solidFill><a:schemeClr val="accent3"/></a:solidFill></a:fillStyleLst><a:lnStyleLst><a:ln w="9525"><a:solidFill><a:schemeClr val="accent1"/></a:solidFill></a:ln><a:ln w="25400"><a:solidFill><a:schemeClr val="accent1"/></a:solidFill></a:ln><a:ln w="38100"><a:solidFill><a:schemeClr val="accent1"/></a:solidFill></a:ln></a:lnStyleLst><a:effectStyleLst><a:effectStyle><a:effectLst/></a:effectStyle><a:effectStyle><a:effectLst/></a:effectStyle><a:effectStyle><a:effectLst/></a:effectStyle></a:effectStyleLst><a:bgFillStyleLst><a:solidFill><a:schemeClr val="lt1"/></a:solidFill><a:solidFill><a:schemeClr val="lt1"/></a:solidFill><a:solidFill><a:schemeClr val="lt1"/></a:solidFill></a:bgFillStyleLst></a:fmtScheme></a:themeElements></a:theme>"#;

/// Image encoding of a previously rendered PDF page.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SlideImageFormat {
    Png,
    Jpeg,
}

impl SlideImageFormat {
    fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
        }
    }
}

/// One raster page. Dimensions must match the actual encoded image.
#[derive(Clone, Debug)]
pub struct SlideImage {
    pub path: PathBuf,
    pub width: u32,
    pub height: u32,
    pub format: SlideImageFormat,
}

impl SlideImage {
    /// Reads image dimensions from a trusted raster output without decoding it.
    ///
    /// # Errors
    /// Rejects relative or symbolic-link paths, oversized files, malformed
    /// headers and images beyond the 40-million-pixel rendering limit.
    pub fn from_file(path: PathBuf, format: SlideImageFormat) -> Result<Self, PptxError> {
        let (mut file, length) = open_image(&path)?;
        let (width, height) = image_dimensions(&mut file, format, length)?;
        if !valid_dimensions(width, height) {
            return Err(PptxError::InvalidDimensions);
        }
        Ok(Self {
            path,
            width,
            height,
            format,
        })
    }
}

/// Errors while packaging rendered pages as a PowerPoint presentation.
#[derive(Debug, Error)]
pub enum PptxError {
    #[error("PPTX must contain at least one page and no more than 200 pages")]
    SlideLimit,
    #[error("PPTX output must be an absolute path in an existing directory")]
    InvalidOutputPath,
    #[error("PPTX page dimensions are invalid or exceed the pixel limit")]
    InvalidDimensions,
    #[error("The PPTX page image is invalid or does not match the declared pixel size")]
    InvalidImage,
    #[error("PPTX images or the ZIP32 package exceed the safety limit")]
    PackageTooLarge,
    #[error("PPTX conversion cancelled")]
    Cancelled,
    #[error("PPTX read/write failed: {0}")]
    Io(#[from] io::Error),
}

/// Creates a PPTX with one full-page raster image per slide. Mixed page sizes
/// are fitted onto the first page's aspect ratio without stretching or cropping.
/// The output must not already exist; failed or cancelled writes remove it.
///
/// # Errors
/// Returns an error for missing, invalid, oversized or changed images, ZIP32
/// limits, cancellation, and any input/output I/O failure.
pub fn write_image_pptx(
    output: &Path,
    slides: &[SlideImage],
    should_stop: impl Fn() -> bool,
) -> Result<(), PptxError> {
    if !output.is_absolute() || !output.parent().is_some_and(Path::is_dir) {
        return Err(PptxError::InvalidOutputPath);
    }
    let canvas = validate_slides(slides)?;
    if should_stop() {
        return Err(PptxError::Cancelled);
    }
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)?;
    let result = write_package(file, slides, canvas, &should_stop);
    if result.is_err() {
        let _ = fs::remove_file(output);
    }
    result
}

#[derive(Clone, Copy)]
struct Canvas {
    width: u64,
    height: u64,
}

fn validate_slides(slides: &[SlideImage]) -> Result<Canvas, PptxError> {
    if slides.is_empty() || slides.len() > MAX_SLIDES {
        return Err(PptxError::SlideLimit);
    }
    for slide in slides {
        if !valid_dimensions(slide.width, slide.height) {
            return Err(PptxError::InvalidDimensions);
        }
    }
    let first = &slides[0];
    let (width, height) = if first.width >= first.height {
        (
            SLIDE_LONG_SIDE,
            SLIDE_LONG_SIDE * u64::from(first.height) / u64::from(first.width),
        )
    } else {
        (
            SLIDE_LONG_SIDE * u64::from(first.width) / u64::from(first.height),
            SLIDE_LONG_SIDE,
        )
    };
    if width < SLIDE_MIN_SIDE || height < SLIDE_MIN_SIDE {
        return Err(PptxError::InvalidDimensions);
    }
    Ok(Canvas { width, height })
}

fn valid_dimensions(width: u32, height: u32) -> bool {
    let pixels = u64::from(width) * u64::from(height);
    pixels != 0 && pixels <= MAX_PIXELS
}

fn write_package(
    file: File,
    slides: &[SlideImage],
    canvas: Canvas,
    should_stop: &impl Fn() -> bool,
) -> Result<(), PptxError> {
    let mut zip = PackageWriter::new(file);
    zip.add_bytes(
        "[Content_Types].xml",
        content_types(slides).as_bytes(),
        should_stop,
    )?;
    zip.add_bytes("_rels/.rels", ROOT_RELS.as_bytes(), should_stop)?;
    zip.add_bytes("docProps/core.xml", CORE_PROPS.as_bytes(), should_stop)?;
    let app_xml = format!("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?><Properties xmlns=\"http://schemas.openxmlformats.org/officeDocument/2006/extended-properties\" xmlns:vt=\"http://schemas.openxmlformats.org/officeDocument/2006/docPropsVTypes\"><Application>Minimal PDF Converter</Application><PresentationFormat>Custom</PresentationFormat><Slides>{}</Slides></Properties>", slides.len());
    zip.add_bytes("docProps/app.xml", app_xml.as_bytes(), should_stop)?;
    zip.add_bytes(
        "ppt/presentation.xml",
        presentation_xml(slides.len(), canvas).as_bytes(),
        should_stop,
    )?;
    zip.add_bytes(
        "ppt/_rels/presentation.xml.rels",
        presentation_rels(slides.len()).as_bytes(),
        should_stop,
    )?;
    zip.add_bytes(
        "ppt/slideMasters/slideMaster1.xml",
        MASTER.as_bytes(),
        should_stop,
    )?;
    zip.add_bytes(
        "ppt/slideMasters/_rels/slideMaster1.xml.rels",
        MASTER_RELS.as_bytes(),
        should_stop,
    )?;
    zip.add_bytes(
        "ppt/slideLayouts/slideLayout1.xml",
        LAYOUT.as_bytes(),
        should_stop,
    )?;
    zip.add_bytes(
        "ppt/slideLayouts/_rels/slideLayout1.xml.rels",
        LAYOUT_RELS.as_bytes(),
        should_stop,
    )?;
    zip.add_bytes("ppt/theme/theme1.xml", THEME.as_bytes(), should_stop)?;

    for (index, slide) in slides.iter().enumerate() {
        if should_stop() {
            return Err(PptxError::Cancelled);
        }
        let number = index + 1;
        let slide_xml = slide_xml(number, slide, canvas);
        zip.add_bytes(
            &format!("ppt/slides/slide{number}.xml"),
            slide_xml.as_bytes(),
            should_stop,
        )?;
        let media_name = format!("page{number}.{}", slide.format.extension());
        let slide_rels = format!("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?><Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout\" Target=\"../slideLayouts/slideLayout1.xml\"/><Relationship Id=\"rId2\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/image\" Target=\"../media/{media_name}\"/></Relationships>");
        zip.add_bytes(
            &format!("ppt/slides/_rels/slide{number}.xml.rels"),
            slide_rels.as_bytes(),
            should_stop,
        )?;
        let (mut image, length) = open_image(&slide.path)?;
        if image_dimensions(&mut image, slide.format, length)? != (slide.width, slide.height) {
            return Err(PptxError::InvalidImage);
        }
        image.seek(SeekFrom::Start(0))?;
        zip.add_stream(
            &format!("ppt/media/{media_name}"),
            &mut image,
            length,
            MAX_IMAGE_BYTES,
            should_stop,
        )?;
    }
    zip.finish()?;
    if should_stop() {
        return Err(PptxError::Cancelled);
    }
    Ok(())
}

fn content_types(slides: &[SlideImage]) -> String {
    let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?><Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\"><Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/><Default Extension=\"xml\" ContentType=\"application/xml\"/>");
    if slides
        .iter()
        .any(|slide| slide.format == SlideImageFormat::Png)
    {
        xml.push_str("<Default Extension=\"png\" ContentType=\"image/png\"/>");
    }
    if slides
        .iter()
        .any(|slide| slide.format == SlideImageFormat::Jpeg)
    {
        xml.push_str("<Default Extension=\"jpg\" ContentType=\"image/jpeg\"/>");
    }
    xml.push_str("<Override PartName=\"/ppt/presentation.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml\"/><Override PartName=\"/ppt/slideMasters/slideMaster1.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.presentationml.slideMaster+xml\"/><Override PartName=\"/ppt/slideLayouts/slideLayout1.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.presentationml.slideLayout+xml\"/><Override PartName=\"/ppt/theme/theme1.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.theme+xml\"/><Override PartName=\"/docProps/app.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.extended-properties+xml\"/><Override PartName=\"/docProps/core.xml\" ContentType=\"application/vnd.openxmlformats-package.core-properties+xml\"/>");
    for index in 1..=slides.len() {
        write!(xml, "<Override PartName=\"/ppt/slides/slide{index}.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.presentationml.slide+xml\"/>").expect("writing to String cannot fail");
    }
    xml.push_str("</Types>");
    xml
}

fn presentation_xml(count: usize, canvas: Canvas) -> String {
    let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?><p:presentation xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\" xmlns:p=\"http://schemas.openxmlformats.org/presentationml/2006/main\"><p:sldMasterIdLst><p:sldMasterId id=\"2147483648\" r:id=\"rId1\"/></p:sldMasterIdLst><p:sldIdLst>");
    for index in 0..count {
        write!(
            xml,
            "<p:sldId id=\"{}\" r:id=\"rId{}\"/>",
            index + 256,
            index + 2
        )
        .expect("writing to String cannot fail");
    }
    write!(xml, "</p:sldIdLst><p:sldSz cx=\"{}\" cy=\"{}\" type=\"custom\"/><p:notesSz cx=\"6858000\" cy=\"9144000\"/><p:defaultTextStyle/></p:presentation>", canvas.width, canvas.height).expect("writing to String cannot fail");
    xml
}

fn presentation_rels(count: usize) -> String {
    let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?><Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster\" Target=\"slideMasters/slideMaster1.xml\"/>");
    for index in 1..=count {
        write!(xml, "<Relationship Id=\"rId{}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide\" Target=\"slides/slide{index}.xml\"/>", index + 1).expect("writing to String cannot fail");
    }
    xml.push_str("</Relationships>");
    xml
}

fn slide_xml(number: usize, slide: &SlideImage, canvas: Canvas) -> String {
    let iw = u64::from(slide.width);
    let ih = u64::from(slide.height);
    let (width, height) = if canvas.width * ih <= canvas.height * iw {
        (canvas.width, canvas.width * ih / iw)
    } else {
        (canvas.height * iw / ih, canvas.height)
    };
    let x = (canvas.width - width) / 2;
    let y = (canvas.height - height) / 2;
    format!("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?><p:sld xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\" xmlns:p=\"http://schemas.openxmlformats.org/presentationml/2006/main\"><p:cSld><p:bg><p:bgPr><a:solidFill><a:srgbClr val=\"FFFFFF\"/></a:solidFill><a:effectLst/></p:bgPr></p:bg><p:spTree>{GROUP_SHAPE}<p:pic><p:nvPicPr><p:cNvPr id=\"2\" name=\"Page {number}\"/><p:cNvPicPr><a:picLocks noChangeAspect=\"1\"/></p:cNvPicPr><p:nvPr/></p:nvPicPr><p:blipFill><a:blip r:embed=\"rId2\"/><a:stretch><a:fillRect/></a:stretch></p:blipFill><p:spPr><a:xfrm><a:off x=\"{x}\" y=\"{y}\"/><a:ext cx=\"{width}\" cy=\"{height}\"/></a:xfrm><a:prstGeom prst=\"rect\"><a:avLst/></a:prstGeom></p:spPr></p:pic></p:spTree></p:cSld><p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sld>")
}

fn open_image(path: &Path) -> Result<(File, u64), PptxError> {
    if !path.is_absolute() || fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(PptxError::InvalidImage);
    }
    let image = File::open(path)?;
    let metadata = image.metadata()?;
    if !metadata.is_file() || metadata.len() < 4 {
        return Err(PptxError::InvalidImage);
    }
    if metadata.len() > MAX_IMAGE_BYTES {
        return Err(PptxError::PackageTooLarge);
    }
    Ok((image, metadata.len()))
}

fn image_dimensions(
    file: &mut File,
    format: SlideImageFormat,
    length: u64,
) -> Result<(u32, u32), PptxError> {
    match format {
        SlideImageFormat::Png => {
            if length < 33 {
                return Err(PptxError::InvalidImage);
            }
            let mut header = [0u8; 24];
            file.read_exact(&mut header)
                .map_err(|_| PptxError::InvalidImage)?;
            let width = u32::from_be_bytes([header[16], header[17], header[18], header[19]]);
            let height = u32::from_be_bytes([header[20], header[21], header[22], header[23]]);
            if &header[..16] != b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR" {
                return Err(PptxError::InvalidImage);
            }
            Ok((width, height))
        }
        SlideImageFormat::Jpeg => {
            let mut marker = [0u8; 2];
            file.read_exact(&mut marker)
                .map_err(|_| PptxError::InvalidImage)?;
            if marker != [0xff, 0xd8] {
                return Err(PptxError::InvalidImage);
            }
            let mut dimensions = None;
            while file.stream_position()? <= 1024 * 1024 {
                file.read_exact(&mut marker)
                    .map_err(|_| PptxError::InvalidImage)?;
                if marker[0] != 0xff || marker[1] == 0xda || marker[1] == 0xd9 {
                    return Err(PptxError::InvalidImage);
                }
                let tag = marker[1];
                if tag == 0xff || tag == 0x01 || (0xd0..=0xd7).contains(&tag) {
                    if tag == 0xff {
                        file.seek(SeekFrom::Current(-1))?;
                    }
                    continue;
                }
                file.read_exact(&mut marker)
                    .map_err(|_| PptxError::InvalidImage)?;
                let segment_len = u16::from_be_bytes(marker);
                if segment_len < 2 {
                    return Err(PptxError::InvalidImage);
                }
                if matches!(tag, 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf) {
                    if segment_len < 7 {
                        return Err(PptxError::InvalidImage);
                    }
                    let mut frame = [0u8; 5];
                    file.read_exact(&mut frame)
                        .map_err(|_| PptxError::InvalidImage)?;
                    if frame[0] != 8 {
                        return Err(PptxError::InvalidImage);
                    }
                    dimensions = Some((
                        u32::from(u16::from_be_bytes([frame[3], frame[4]])),
                        u32::from(u16::from_be_bytes([frame[1], frame[2]])),
                    ));
                    break;
                }
                file.seek(SeekFrom::Current(i64::from(segment_len) - 2))?;
            }
            file.seek(SeekFrom::End(-2))?;
            file.read_exact(&mut marker)
                .map_err(|_| PptxError::InvalidImage)?;
            if marker != [0xff, 0xd9] {
                return Err(PptxError::InvalidImage);
            }
            dimensions.ok_or(PptxError::InvalidImage)
        }
    }
}

struct PackageWriter {
    output: zip::ZipWriter<File>,
    estimated_size: u64,
}

impl PackageWriter {
    fn new(file: File) -> Self {
        Self {
            output: zip::ZipWriter::new(file),
            estimated_size: 0,
        }
    }

    fn add_bytes(
        &mut self,
        name: &str,
        content: &[u8],
        should_stop: &impl Fn() -> bool,
    ) -> Result<(), PptxError> {
        if u64::try_from(content.len()).map_err(|_| PptxError::PackageTooLarge)? > MAX_XML_BYTES {
            return Err(PptxError::PackageTooLarge);
        }
        self.add_stream(
            name,
            &mut io::Cursor::new(content),
            u64::try_from(content.len()).map_err(|_| PptxError::PackageTooLarge)?,
            MAX_XML_BYTES,
            should_stop,
        )
    }

    fn add_stream(
        &mut self,
        name: &str,
        input: &mut impl Read,
        expected: u64,
        limit: u64,
        should_stop: &impl Fn() -> bool,
    ) -> Result<(), PptxError> {
        let name_bytes = name.as_bytes();
        if name_bytes.len() > usize::from(u16::MAX) || expected > limit {
            return Err(PptxError::PackageTooLarge);
        }
        self.estimated_size = self
            .estimated_size
            .checked_add(expected)
            .and_then(|size| size.checked_add(u64::try_from(name_bytes.len() + 256).ok()?))
            .filter(|&size| size <= MAX_ZIP32_OFFSET)
            .ok_or(PptxError::PackageTooLarge)?;
        if should_stop() {
            return Err(PptxError::Cancelled);
        }
        self.output
            .start_file(
                name,
                SimpleFileOptions::default().compression_method(CompressionMethod::Stored),
            )
            .map_err(|error| PptxError::Io(io::Error::other(error)))?;
        let mut size = 0u64;
        let mut buffer = [0u8; 64 * 1024];
        loop {
            if should_stop() {
                return Err(PptxError::Cancelled);
            }
            let read = input.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            size = size
                .checked_add(u64::try_from(read).map_err(|_| PptxError::PackageTooLarge)?)
                .filter(|&value| value <= limit && value <= expected)
                .ok_or(PptxError::PackageTooLarge)?;
            self.output.write_all(&buffer[..read])?;
        }
        if size != expected {
            return Err(PptxError::InvalidImage);
        }
        Ok(())
    }

    fn finish(self) -> Result<(), PptxError> {
        let mut file = self
            .output
            .finish()
            .map_err(|error| PptxError::Io(io::Error::other(error)))?;
        file.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static NEXT_CASE: AtomicUsize = AtomicUsize::new(0);

    struct Case {
        dir: PathBuf,
    }

    impl Case {
        fn new() -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let dir = std::env::temp_dir().join(format!(
                "minimal-pptx-{}-{unique}-{}",
                std::process::id(),
                NEXT_CASE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&dir).expect("create test directory");
            Self { dir }
        }

        fn png(&self, name: &str, width: u32, height: u32) -> SlideImage {
            let path = self.dir.join(name);
            let file = File::create(&path).expect("create PNG");
            let mut encoder = png::Encoder::new(file, width, height);
            encoder.set_color(png::ColorType::Rgb);
            encoder.set_depth(png::BitDepth::Eight);
            encoder
                .write_header()
                .expect("PNG header")
                .write_image_data(&vec![
                    255;
                    usize::try_from(width * height * 3).expect("test size")
                ])
                .expect("PNG data");
            SlideImage {
                path,
                width,
                height,
                format: SlideImageFormat::Png,
            }
        }

        fn jpeg(&self, name: &str, width: u16, height: u16) -> SlideImage {
            let path = self.dir.join(name);
            let file = File::create(&path).expect("create JPEG");
            let encoder = jpeg_encoder::Encoder::new(file, 85);
            encoder
                .encode(
                    &vec![240; usize::from(width) * usize::from(height) * 3],
                    width,
                    height,
                    jpeg_encoder::ColorType::Rgb,
                )
                .expect("JPEG data");
            SlideImage {
                path,
                width: u32::from(width),
                height: u32::from(height),
                format: SlideImageFormat::Jpeg,
            }
        }
    }

    impl Drop for Case {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn entries(bytes: &[u8]) -> BTreeMap<String, Vec<u8>> {
        let mut archive = zip::ZipArchive::new(io::Cursor::new(bytes)).expect("open PPTX ZIP");
        let mut found = BTreeMap::new();
        for index in 0..archive.len() {
            let mut entry = archive.by_index(index).expect("open PPTX member");
            let name = entry.name().to_owned();
            let mut data = Vec::new();
            entry
                .read_to_end(&mut data)
                .expect("read and CRC-check member");
            assert!(found.insert(name, data).is_none());
        }
        found
    }

    #[test]
    fn writes_office_readable_zip_with_png_jpeg_relationships_and_xml() {
        let case = Case::new();
        let first = case.png("first.png", 4, 2);
        let second = case.jpeg("second.jpg", 2, 4);
        let output = case.dir.join("pages.pptx");
        write_image_pptx(&output, &[first, second], || false).expect("write PPTX");
        let content = fs::read(output).expect("read PPTX");
        let entries = entries(&content);
        assert_eq!(entries.len(), 17);
        let presentation =
            std::str::from_utf8(&entries["ppt/presentation.xml"]).expect("presentation XML");
        assert!(presentation.contains("<p:sldSz cx=\"12192000\" cy=\"6096000\""));
        assert!(presentation.contains("<p:sldId id=\"257\" r:id=\"rId3\""));
        let second_slide =
            std::str::from_utf8(&entries["ppt/slides/slide2.xml"]).expect("slide XML");
        assert!(second_slide
            .contains("<a:off x=\"4572000\" y=\"0\"/><a:ext cx=\"3048000\" cy=\"6096000\"/>"));
        assert!(
            std::str::from_utf8(&entries["ppt/slides/_rels/slide2.xml.rels"])
                .expect("relationships")
                .contains("../media/page2.jpg")
        );
        assert!(entries["ppt/media/page1.png"].starts_with(b"\x89PNG"));
        assert!(entries["ppt/media/page2.jpg"].starts_with(b"\xff\xd8"));

        for (name, xml) in &entries {
            if name.ends_with(".xml") || name.ends_with(".rels") {
                let mut reader = quick_xml::Reader::from_reader(xml.as_slice());
                loop {
                    match reader.read_event() {
                        Ok(quick_xml::events::Event::Eof) => break,
                        Ok(_) => {}
                        Err(error) => panic!("{name}: {error}"),
                    }
                }
            }
        }
    }

    #[test]
    fn streams_images_larger_than_one_buffer_with_exact_bytes() {
        let case = Case::new();
        let path = case.dir.join("large.png");
        let mut pixels = vec![0u8; 256 * 256 * 3];
        let mut state = 0x13579bdfu32;
        for pixel in &mut pixels {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            *pixel = state.to_le_bytes()[0];
        }
        let mut encoder = png::Encoder::new(File::create(&path).expect("create PNG"), 256, 256);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .write_header()
            .expect("PNG header")
            .write_image_data(&pixels)
            .expect("PNG data");
        let original = fs::read(&path).expect("read PNG");
        assert!(original.len() > 64 * 1024);
        let output = case.dir.join("pages.pptx");
        write_image_pptx(
            &output,
            &[SlideImage {
                path,
                width: 256,
                height: 256,
                format: SlideImageFormat::Png,
            }],
            || false,
        )
        .expect("write PPTX");
        let package = entries(&fs::read(output).expect("read PPTX"));
        assert_eq!(package["ppt/media/page1.png"], original);
    }

    #[test]
    fn rejects_invalid_and_mismatched_images_without_leaving_output() {
        let case = Case::new();
        let output = case.dir.join("pages.pptx");
        let mut image = case.png("page.png", 3, 2);
        let discovered =
            SlideImage::from_file(image.path.clone(), image.format).expect("PNG dimensions");
        assert_eq!((discovered.width, discovered.height), (3, 2));
        image.width = 4;
        assert!(matches!(
            write_image_pptx(&output, &[image], || false),
            Err(PptxError::InvalidImage)
        ));
        assert!(!output.exists());

        let mut image = case.jpeg("page.jpg", 3, 2);
        let discovered =
            SlideImage::from_file(image.path.clone(), image.format).expect("JPEG dimensions");
        assert_eq!((discovered.width, discovered.height), (3, 2));
        image.height = 4;
        assert!(matches!(
            write_image_pptx(&output, &[image], || false),
            Err(PptxError::InvalidImage)
        ));
        assert!(!output.exists());

        let corrupt = case.dir.join("corrupt.png");
        fs::write(&corrupt, b"not a PNG").expect("write corrupt image");
        assert!(matches!(
            SlideImage::from_file(corrupt.clone(), SlideImageFormat::Png),
            Err(PptxError::InvalidImage)
        ));
        assert!(matches!(
            write_image_pptx(
                &output,
                &[SlideImage {
                    path: corrupt,
                    width: 2,
                    height: 2,
                    format: SlideImageFormat::Png
                }],
                || false
            ),
            Err(PptxError::InvalidImage)
        ));
        assert!(!output.exists());
    }

    #[test]
    fn rejects_limits_before_creating_output() {
        let case = Case::new();
        let output = case.dir.join("pages.pptx");
        assert!(matches!(
            write_image_pptx(&output, &[], || false),
            Err(PptxError::SlideLimit)
        ));
        let image = case.png("page.png", 2, 2);
        let mut oversized = image.clone();
        oversized.width = 40_000_001;
        assert!(matches!(
            write_image_pptx(&output, &[oversized], || false),
            Err(PptxError::InvalidDimensions)
        ));
        let image_path = case.dir.join("huge.png");
        File::create(&image_path)
            .expect("create sparse image")
            .set_len(MAX_IMAGE_BYTES + 1)
            .expect("resize sparse image");
        assert!(matches!(
            write_image_pptx(
                &output,
                &[SlideImage {
                    path: image_path,
                    ..image.clone()
                }],
                || false
            ),
            Err(PptxError::PackageTooLarge)
        ));
        assert!(!output.exists());

        let entries = vec![image; MAX_SLIDES + 1];
        assert!(matches!(
            write_image_pptx(&output, &entries, || false),
            Err(PptxError::SlideLimit)
        ));
        assert!(!output.exists());
    }

    #[test]
    fn requires_an_absolute_output_in_an_existing_directory() {
        let case = Case::new();
        let image = case.png("page.png", 2, 2);
        assert!(matches!(
            SlideImage::from_file(PathBuf::from("relative.png"), SlideImageFormat::Png),
            Err(PptxError::InvalidImage)
        ));
        assert!(matches!(
            write_image_pptx(Path::new("relative.pptx"), &[image.clone()], || false),
            Err(PptxError::InvalidOutputPath)
        ));
        assert!(matches!(
            write_image_pptx(&case.dir.join("missing/pages.pptx"), &[image], || false),
            Err(PptxError::InvalidOutputPath)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symbolic_link_images() {
        let case = Case::new();
        let original = case.png("page.png", 2, 2);
        let link = case.dir.join("link.png");
        std::os::unix::fs::symlink(&original.path, &link).expect("create image link");
        let output = case.dir.join("pages.pptx");
        assert!(matches!(
            SlideImage::from_file(link.clone(), SlideImageFormat::Png),
            Err(PptxError::InvalidImage)
        ));
        let image = SlideImage {
            path: link,
            ..original
        };
        assert!(matches!(
            write_image_pptx(&output, &[image], || false),
            Err(PptxError::InvalidImage)
        ));
        assert!(!output.exists());
    }

    #[test]
    fn cancellation_removes_partial_file_and_never_overwrites_existing_output() {
        let case = Case::new();
        let output = case.dir.join("pages.pptx");
        let image = case.png("page.png", 2, 2);
        let calls = AtomicUsize::new(0);
        let result = write_image_pptx(&output, &[image.clone()], || {
            calls.fetch_add(1, Ordering::Relaxed) > 15
        });
        assert!(matches!(result, Err(PptxError::Cancelled)));
        assert!(!output.exists());

        fs::write(&output, b"keep existing").expect("write existing output");
        assert!(
            matches!(write_image_pptx(&output, &[image], || false), Err(PptxError::Io(error)) if error.kind() == io::ErrorKind::AlreadyExists)
        );
        assert_eq!(fs::read(output).expect("existing output"), b"keep existing");
    }

    #[test]
    fn zip32_rejects_offset_overflow_without_writing() {
        let case = Case::new();
        let path = case.dir.join("overflow.pptx");
        let mut zip = PackageWriter::new(File::create(&path).expect("create output"));
        zip.estimated_size = MAX_ZIP32_OFFSET - 1;
        assert!(matches!(
            zip.add_bytes("x", b"y", &|| false),
            Err(PptxError::PackageTooLarge)
        ));
        assert_eq!(fs::metadata(path).expect("output metadata").len(), 0);
    }
}
