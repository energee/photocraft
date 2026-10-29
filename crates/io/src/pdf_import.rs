//! Layer-preserving Photoshop PDF import.
//!
//! Photoshop stores its editable layer section in a page's `/PieceInfo` →
//! `/AdobePhotoshop` → `/Private` dictionary.  The bytes are TIFF-style
//! `ImageSourceData` tagged blocks, rather than a complete PSD file, so this
//! module supplies the header, image resources and composite metadata that
//! PSD's importer normally reads from the surrounding file.

use std::io::Read as _;

use flate2::read::ZlibDecoder;
use lopdf::{Dictionary, Document, Object, Stream};
use photocraft_psd::tiff::{ImageSourceData, SIGNATURE, resources_from_bytes};
use photocraft_psd::{ColorMode, Compression, Header, ImageData};
use photocraft_raster::Interrupt;

use crate::psd_import::{check_group_depth, psd_to_document_decoded_integrity};
use crate::{ImportOptions, ImportResult, IoError, PdfImportMode};

/// Largest private Photoshop stream accepted from a PDF.  This cap includes
/// the decoded layer section: it prevents a small Flate stream from creating
/// an unbounded allocation before the PSD parser gets a chance to validate it.
const MAX_PRIVATE_BYTES: usize = 512 << 20;
const MAX_PDF_BYTES: usize = 512 << 20;

/// Outcome of probing one PDF page for Photoshop's private layer section.
///
/// [`Self::Absent`] is deliberately narrow: it means the page has no
/// Photoshop preservation entry. Every other failure is terminal in
/// [`PdfImportMode::Auto`], so an unreadable or damaged private section is not
/// silently replaced with a raster image.
#[derive(Debug)]
enum LayeredPdfError {
    /// The page has no Photoshop preservation entry.
    Absent,
    /// The PDF itself cannot be read, its input/page selection is invalid, or
    /// a PDF or Photoshop-private stream exceeds an import limit.
    Pdf(String),
    Cancelled,
    /// Photoshop preservation metadata was present but is invalid or
    /// unsupported.
    Corrupt(String),
}

impl LayeredPdfError {
    fn pdf(message: impl Into<String>) -> Self {
        Self::Pdf(message.into())
    }

    fn corrupt(message: impl Into<String>) -> Self {
        Self::Corrupt(message.into())
    }

    fn into_io(self) -> IoError {
        match self {
            Self::Absent => IoError::Pdf("this PDF has no Photoshop editable layer data; choose PDF render mode".into()),
            Self::Pdf(message) => IoError::Pdf(message),
            Self::Cancelled => IoError::Cancelled,
            Self::Corrupt(message) => IoError::Pdf(format!(
                "the Photoshop editable layer data in this PDF is corrupt or unsupported ({message}); choose PDF render mode to open its page appearance"
            )),
        }
    }
}

/// PDF magic, allowing the dispatcher to select this importer before raster
/// codecs see an ordinary PDF.
pub(crate) fn is_pdf(bytes: &[u8]) -> bool {
    bytes.starts_with(b"%PDF-")
}

/// Imports a PDF according to its Photoshop-layer and page-rendering mode.
///
/// [`PdfImportMode::Auto`] rasterises only when the selected page demonstrably
/// lacks Photoshop preservation metadata. [`PdfImportMode::Layers`] reports
/// that absence, while [`PdfImportMode::Render`] always delegates directly to
/// the page renderer. Invalid PDF input, invalid page selections, source-size
/// limits, and malformed preserved data are errors in Auto and Layers modes.
pub(crate) fn import(name: &str, bytes: &[u8], options: &ImportOptions, ctl: &Interrupt) -> Result<ImportResult, IoError> {
    match options.pdf.mode {
        PdfImportMode::Render => crate::pdf_render::import(name, bytes, &options.pdf, ctl),
        PdfImportMode::Layers => layered_result(name, bytes, options.pdf.page, ctl).map_err(LayeredPdfError::into_io),
        PdfImportMode::Auto => match layered_result(name, bytes, options.pdf.page, ctl) {
            Ok(result) => Ok(result),
            Err(LayeredPdfError::Absent) => crate::pdf_render::import(name, bytes, &options.pdf, ctl),
            Err(error) => Err(error.into_io()),
        },
    }
}

/// Imports the selected Photoshop-preserved PDF page.  The dispatcher chooses
/// the ordinary renderer when this reports [`LayeredPdfError::Absent`] in Auto
/// mode; malformed preserved data is deliberately not flattened silently.
fn layered_result(name: &str, bytes: &[u8], page: u32, ctl: &Interrupt) -> Result<ImportResult, LayeredPdfError> {
    ctl.check().map_err(|_| LayeredPdfError::Cancelled)?;
    layered_file(bytes, page, ctl).map(|(mut document, warnings)| {
        document.name = name.to_owned();
        ImportResult { document, warnings, source_read_only: true, preview_only: false }
    })
}

fn layered_file(bytes: &[u8], page: u32, ctl: &Interrupt) -> Result<(photocraft_doc::Document, Vec<String>), LayeredPdfError> {
    let (pdf, page) = load_page(bytes, page)?;
    let private = photoshop_private(&pdf, page)?;
    ctl.check().map_err(|_| LayeredPdfError::Cancelled)?;

    let layer_stream = private_stream(&pdf, private, b"StandardImageFileData")?;
    let mut image_source = decode_private_stream(layer_stream, "StandardImageFileData")?;
    validate_layer_block(&image_source)?;
    image_source.try_reserve(SIGNATURE.len()).map_err(|_| LayeredPdfError::corrupt("not enough memory for Photoshop layer data"))?;
    // Add the TIFF container marker in place, rather than retaining a second complete layer stream.
    drop(image_source.splice(..0, SIGNATURE.iter().copied()));
    let (layers, mut warnings) =
        ImageSourceData::from_bytes(&image_source).map_err(|error| LayeredPdfError::corrupt(format!("could not parse StandardImageFileData: {error}")))?;
    drop(image_source);

    let mut resources = if private.get(b"ImageResources").is_ok() {
        let stream = private_stream(&pdf, private, b"ImageResources")?;
        let data = decode_private_stream(stream, "ImageResources")?;
        let (resources, resource_warnings) =
            resources_from_bytes(&data).map_err(|error| LayeredPdfError::corrupt(format!("could not parse ImageResources: {error}")))?;
        warnings.extend(resource_warnings.into_iter().map(|warning| format!("image resources: {warning}")));
        resources
    } else {
        Vec::new()
    };

    let composite = private_stream(&pdf, private, b"CompositeImage")?;
    let header = composite_header(&pdf, composite)?;
    // Photoshop PDFs can carry their only ICC profile on CompositeImage, not in ImageResources.
    if !resources.iter().any(|resource| resource.id == photocraft_psd::resources::ids::ICC_PROFILE)
        && let Ok(color) = composite.dict.get_deref(b"ColorSpace", &pdf).and_then(Object::as_array)
        && color.first().and_then(|object| object.as_name().ok()) == Some(b"ICCBased".as_slice())
    {
        let data = decode_private_stream(icc_profile(&pdf, color)?, "CompositeImage ICC profile")?;
        resources.push(photocraft_psd::ImageResource::new(photocraft_psd::resources::ids::ICC_PROFILE, data));
    }
    let file = layers.into_psd(header, resources, ImageData::new(Compression::Raw, Vec::new()));
    check_group_depth(&file).map_err(|error| LayeredPdfError::corrupt(error.to_string()))?;
    ctl.check().map_err(|_| LayeredPdfError::Cancelled)?;
    let txt2 = file.global_blocks.iter().find(|block| &block.key == b"Txt2").and_then(|block| photocraft_text::psd::parse_txt2(&block.data));
    let (mut document, psd_warnings, psd_damaged) = psd_to_document_decoded_integrity(&file, ctl, None).ok_or(LayeredPdfError::Cancelled)?;
    if psd_damaged {
        return Err(LayeredPdfError::corrupt("one or more editable layer channels could not be decoded"));
    }
    crate::text_import::prepare(&mut document, txt2.as_ref());
    warnings.extend(psd_warnings);
    Ok((document, warnings))
}

/// Validates the input size without inspecting the bytes, keeping the hard
/// allocation boundary testable without constructing a huge PDF.
fn validate_pdf_size_len(len: usize) -> Result<(), LayeredPdfError> {
    if len > MAX_PDF_BYTES {
        return Err(LayeredPdfError::pdf(format!("PDF is larger than the {} MiB import limit", MAX_PDF_BYTES >> 20)));
    }
    Ok(())
}

/// Loads a selected page only after bounding the source and rejecting page
/// zero.  This order makes oversized input terminal even when its page option
/// is invalid, and keeps parser failures distinct from private-data failures.
fn load_page(bytes: &[u8], page: u32) -> Result<(Document, lopdf::ObjectId), LayeredPdfError> {
    validate_pdf_size_len(bytes.len())?;
    if page == 0 {
        return Err(LayeredPdfError::pdf("PDF page numbers start at 1"));
    }
    let pdf = Document::load_mem(bytes).map_err(|error| LayeredPdfError::pdf(format!("could not parse PDF: {error}")))?;
    let pages = pdf.get_pages();
    let page = pages.get(&page).copied().ok_or_else(|| LayeredPdfError::pdf(format!("PDF has {} page(s); page {page} was requested", pages.len())))?;
    Ok((pdf, page))
}

/// The first Photoshop tagged block is the layer-info `Lr16`/`Layr` block.
/// Its declared payload must be bounded before the PSD parser walks records.
fn validate_layer_block(data: &[u8]) -> Result<(), LayeredPdfError> {
    let header = data.get(..12).ok_or_else(|| LayeredPdfError::corrupt("StandardImageFileData is shorter than a Photoshop tagged-block header"))?;
    if &header[..4] != b"8BIM" {
        return Err(LayeredPdfError::corrupt("StandardImageFileData does not start with a Photoshop tagged block"));
    }
    if !matches!(&header[4..8], b"Layr" | b"Lr16" | b"Lr32") {
        return Err(LayeredPdfError::corrupt("StandardImageFileData does not start with a Photoshop layer block"));
    }
    let declared = usize::try_from(u32::from_be_bytes([header[8], header[9], header[10], header[11]]))
        .map_err(|_| LayeredPdfError::corrupt("layer block length is out of range"))?;
    if declared > MAX_PRIVATE_BYTES {
        return Err(LayeredPdfError::pdf("StandardImageFileData layer block exceeds the import limit"));
    }
    if declared > data.len().saturating_sub(12) {
        return Err(LayeredPdfError::corrupt("StandardImageFileData declares a layer block outside its stream bounds"));
    }
    Ok(())
}

fn photoshop_private(pdf: &Document, page: lopdf::ObjectId) -> Result<&Dictionary, LayeredPdfError> {
    let page = pdf.get_dictionary(page).map_err(|error| LayeredPdfError::corrupt(format!("could not read page dictionary: {error}")))?;
    if page.get(b"PieceInfo").is_err() {
        return Err(LayeredPdfError::Absent);
    }
    let piece_info = page
        .get_deref(b"PieceInfo", pdf)
        .and_then(Object::as_dict)
        .map_err(|error| LayeredPdfError::corrupt(format!("invalid PieceInfo dictionary: {error}")))?;
    if piece_info.get(b"AdobePhotoshop").is_err() {
        return Err(LayeredPdfError::Absent);
    }
    let adobe = piece_info
        .get_deref(b"AdobePhotoshop", pdf)
        .and_then(Object::as_dict)
        .map_err(|error| LayeredPdfError::corrupt(format!("invalid AdobePhotoshop dictionary: {error}")))?;
    adobe
        .get_deref(b"Private", pdf)
        .and_then(Object::as_dict)
        .map_err(|error| LayeredPdfError::corrupt(format!("invalid Photoshop Private dictionary: {error}")))
}

fn private_stream<'a>(pdf: &'a Document, private: &'a Dictionary, key: &[u8]) -> Result<&'a Stream, LayeredPdfError> {
    private
        .get_deref(key, pdf)
        .map_err(|_| LayeredPdfError::corrupt(format!("missing {key:?} in Photoshop Private data")))?
        .as_stream()
        .map_err(|error| LayeredPdfError::corrupt(format!("{key:?} is not a stream: {error}")))
}

/// Decodes only the PDF stream encodings Photoshop emits for private data.
///
/// The compressed and decoded forms are both capped; callers receive a plain
/// PDF limit error before a byte slice reaches a Photoshop parser.
fn decode_private_stream(stream: &Stream, label: &str) -> Result<Vec<u8>, LayeredPdfError> {
    if stream.content.len() > MAX_PRIVATE_BYTES {
        return Err(LayeredPdfError::pdf(format!("{label} compressed data exceeds the {} MiB import limit", MAX_PRIVATE_BYTES >> 20)));
    }
    if stream.dict.get(b"Filter").is_err() {
        return Ok(stream.content.clone());
    }
    let filters = stream.filters().map_err(|error| LayeredPdfError::corrupt(format!("could not inspect {label} filters: {error}")))?;
    match filters.as_slice() {
        [] => Ok(stream.content.clone()),
        [filter] if *filter == b"FlateDecode" || *filter == b"Fl" => inflate_bounded(&stream.content, label),
        _ => Err(LayeredPdfError::corrupt(format!("{label} uses unsupported PDF stream filters"))),
    }
}

/// Inflates at most one byte beyond the private-data limit, so reaching the
/// cap proves the stream is oversized without retaining an unbounded result.
fn inflate_bounded(data: &[u8], label: &str) -> Result<Vec<u8>, LayeredPdfError> {
    let mut decoder = ZlibDecoder::new(data);
    let mut decoded = Vec::new();
    decoded.try_reserve(data.len().min(MAX_PRIVATE_BYTES)).map_err(|_| LayeredPdfError::pdf(format!("not enough memory to decode {label}")))?;
    let mut limited = decoder.by_ref().take(u64::try_from(MAX_PRIVATE_BYTES.saturating_add(1)).unwrap_or(u64::MAX));
    limited.read_to_end(&mut decoded).map_err(|error| LayeredPdfError::corrupt(format!("could not inflate {label}: {error}")))?;
    if decoded.len() > MAX_PRIVATE_BYTES {
        return Err(LayeredPdfError::pdf(format!("{label} decoded data exceeds the {} MiB import limit", MAX_PRIVATE_BYTES >> 20)));
    }
    Ok(decoded)
}

fn composite_header(pdf: &Document, composite: &Stream) -> Result<Header, LayeredPdfError> {
    let width = integer(composite, b"Width")?;
    let height = integer(composite, b"Height")?;
    let depth =
        u16::try_from(integer(composite, b"BitsPerComponent")?).map_err(|_| LayeredPdfError::corrupt("CompositeImage BitsPerComponent is out of range"))?;
    if !matches!(depth, 8 | 16 | 32) {
        return Err(LayeredPdfError::corrupt(format!("CompositeImage has unsupported {depth}-bit components")));
    }
    let channels = color_channels(pdf, composite)?;
    let mode = match channels {
        1 => ColorMode::Grayscale,
        3 => ColorMode::Rgb,
        4 => ColorMode::Cmyk,
        _ => return Err(LayeredPdfError::corrupt(format!("CompositeImage has unsupported {channels}-channel colour space"))),
    };
    let header = Header::new(photocraft_psd::Version::Psd, lopdf_u32(width, "Width")?, lopdf_u32(height, "Height")?, channels, depth, mode);
    header.validate().map_err(|error| LayeredPdfError::corrupt(error.to_string()))?;
    Ok(header)
}

fn integer(stream: &Stream, key: &[u8]) -> Result<i64, LayeredPdfError> {
    stream.dict.get(key).and_then(Object::as_i64).map_err(|error| LayeredPdfError::corrupt(format!("CompositeImage {key:?} is invalid: {error}")))
}

fn lopdf_u32(value: i64, label: &str) -> Result<u32, LayeredPdfError> {
    u32::try_from(value)
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| LayeredPdfError::corrupt(format!("CompositeImage {label} must be a positive 32-bit integer")))
}

fn color_channels(pdf: &Document, composite: &Stream) -> Result<u16, LayeredPdfError> {
    let color =
        composite.dict.get_deref(b"ColorSpace", pdf).map_err(|error| LayeredPdfError::corrupt(format!("CompositeImage ColorSpace is missing: {error}")))?;
    if let Ok(name) = color.as_name() {
        return match name {
            b"DeviceGray" => Ok(1),
            b"DeviceRGB" => Ok(3),
            b"DeviceCMYK" => Ok(4),
            _ => Err(LayeredPdfError::corrupt("CompositeImage uses an unsupported named colour space")),
        };
    }
    let array = color.as_array().map_err(|error| LayeredPdfError::corrupt(format!("CompositeImage ColorSpace is invalid: {error}")))?;
    if array.first().and_then(|object| object.as_name().ok()) != Some(b"ICCBased".as_slice()) {
        return Err(LayeredPdfError::corrupt("CompositeImage uses an unsupported colour space"));
    }
    let count = icc_profile(pdf, array)?
        .dict
        .get(b"N")
        .and_then(Object::as_i64)
        .map_err(|error| LayeredPdfError::corrupt(format!("CompositeImage ICC profile channel count is invalid: {error}")))?;
    u16::try_from(count).map_err(|_| LayeredPdfError::corrupt("CompositeImage ICC profile channel count is out of range"))
}

/// The profile stream of an `[/ICCBased <stream>]` CompositeImage colour space.
fn icc_profile<'a>(pdf: &'a Document, color: &'a [Object]) -> Result<&'a Stream, LayeredPdfError> {
    let profile = color.get(1).ok_or_else(|| LayeredPdfError::corrupt("ICCBased CompositeImage has no profile"))?;
    let (_, profile) = pdf.dereference(profile).map_err(|error| LayeredPdfError::corrupt(format!("could not read CompositeImage ICC profile: {error}")))?;
    profile.as_stream().map_err(|error| LayeredPdfError::corrupt(format!("CompositeImage ICC profile is not a stream: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use photocraft_color::{ColorMode, SampleType};
    use photocraft_doc::{Document, Layer};
    use photocraft_geom::Size;
    use photocraft_psd::tiff::{ByteOrder, ImageSourceData};

    fn object(out: &mut Vec<u8>, offsets: &mut Vec<usize>, body: &[u8]) {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", offsets.len()).as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    }

    fn ordinary_pdf() -> Vec<u8> {
        let mut out = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        object(&mut out, &mut offsets, b"<< /Type /Catalog /Pages 2 0 R >>");
        object(&mut out, &mut offsets, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>");
        object(&mut out, &mut offsets, b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 1 1] /Contents 4 0 R >>");
        let content = "0 1 0 rg 0 0 1 1 re f";
        object(&mut out, &mut offsets, format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len()).as_bytes());
        let size = offsets.len() + 1;
        let xref = out.len();
        out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", offsets.len() + 1).as_bytes());
        for offset in offsets {
            out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(format!("trailer\n<< /Size {size} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n").as_bytes());
        out
    }

    fn photoshop_pdf(layer_blocks: &[u8]) -> Vec<u8> {
        photoshop_pdf_with_depth(layer_blocks, 8)
    }

    fn photoshop_pdf_with_depth(layer_blocks: &[u8], depth: u16) -> Vec<u8> {
        photoshop_pdf_with_profile(layer_blocks, depth, None)
    }

    fn photoshop_pdf_with_profile(layer_blocks: &[u8], depth: u16, profile: Option<&[u8]>) -> Vec<u8> {
        let mut out = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        object(&mut out, &mut offsets, b"<< /Type /Catalog /Pages 2 0 R >>");
        object(&mut out, &mut offsets, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>");
        object(
            &mut out,
            &mut offsets,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 3 2] /Resources << >> /PieceInfo << /AdobePhotoshop << /Private 4 0 R >> >> >>",
        );
        object(&mut out, &mut offsets, b"<< /StandardImageFileData 5 0 R /CompositeImage 6 0 R >>");
        let mut layers = format!("<< /Length {} >>\nstream\n", layer_blocks.len()).into_bytes();
        layers.extend_from_slice(layer_blocks);
        layers.extend_from_slice(b"\nendstream");
        object(&mut out, &mut offsets, &layers);
        let color = if profile.is_some() { "[/ICCBased 7 0 R]" } else { "/DeviceRGB" };
        object(
            &mut out,
            &mut offsets,
            format!("<< /Type /XObject /Subtype /Image /Width 3 /Height 2 /BitsPerComponent {depth} /ColorSpace {color} /Length 0 >>\nstream\n\nendstream")
                .as_bytes(),
        );
        if let Some(profile) = profile {
            let mut stream = format!("<< /N 3 /Length {} >>\nstream\n", profile.len()).into_bytes();
            stream.extend_from_slice(profile);
            stream.extend_from_slice(b"\nendstream");
            object(&mut out, &mut offsets, &stream);
        }
        let size = offsets.len() + 1;
        let xref = out.len();
        out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", offsets.len() + 1).as_bytes());
        for offset in offsets {
            out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(format!("trailer\n<< /Size {size} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n").as_bytes());
        out
    }

    #[test]
    fn preserves_the_composite_icc_profile_when_resources_do_not_repeat_it() {
        let mut original = Document::new("source", Size::new(3, 2), ColorMode::Rgb, SampleType::U8);
        original.layers.push(Layer::raster("Layer", original.pixel_format()));
        let psd = crate::document_to_psd(&original);
        let (source, _) = ImageSourceData::from_psd(&psd).to_bytes(ByteOrder::Big).unwrap();
        let profile = photocraft_cms::Builtin::Srgb.profile().to_bytes();
        let pdf = photoshop_pdf_with_profile(&source[SIGNATURE.len()..], 8, Some(&profile));
        let imported = crate::import("profiled.pdf", &pdf).unwrap();
        assert_eq!(imported.document.icc_profile.as_deref().map(Vec::as_slice), Some(profile.as_slice()));
    }

    #[test]
    fn imports_preserved_layers_without_decoding_the_composite() {
        let mut original = Document::new("source", Size::new(3, 2), ColorMode::Rgb, SampleType::U8);
        original.layers.push(Layer::raster("Bottom", original.pixel_format()));
        original.layers.push(Layer::raster("Top", original.pixel_format()));
        let psd = crate::document_to_psd(&original);
        let (source, _) = ImageSourceData::from_psd(&psd).to_bytes(ByteOrder::Big).unwrap();
        let pdf = photoshop_pdf(&source[SIGNATURE.len()..]);

        let mut options = ImportOptions::default();
        options.pdf.mode = PdfImportMode::Layers;
        let imported = crate::import_with_options("synthetic.pdf", &pdf, &options).unwrap();
        assert!(imported.source_read_only);
        assert!(!imported.preview_only);
        assert_eq!(imported.document.layers.len(), 2);
        assert_eq!(imported.document.layers.iter().map(|layer| layer.name.as_str()).collect::<Vec<_>>(), ["Bottom", "Top"]);
    }

    #[test]
    fn imports_synthetic_sixteen_bit_layer_records() {
        let mut original = Document::new("source", Size::new(3, 2), ColorMode::Rgb, SampleType::U16);
        original.layers.push(Layer::raster("16-bit layer", original.pixel_format()));
        let canvas = photocraft_geom::Rect::new(0, 0, 3, 2);
        let surface = original.layers[0].surface_mut().unwrap();
        surface.write_pixel(0, 0, &[0.1234, 0.5, 0.9, 1.0]);
        surface.write_pixel(2, 1, &[0.25, 0.75, 0.0625, 0.5]);
        let expected = surface.to_interleaved(canvas);
        let psd = crate::document_to_psd(&original);
        let (source, _) = ImageSourceData::from_psd(&psd).to_bytes(ByteOrder::Big).unwrap();
        let imported = crate::import("synthetic-16.pdf", &photoshop_pdf_with_depth(&source[SIGNATURE.len()..], 16)).unwrap();
        assert_eq!(imported.document.depth, SampleType::U16);
        assert_eq!(imported.document.layers.iter().map(|layer| layer.name.as_str()).collect::<Vec<_>>(), ["16-bit layer"]);
        assert_eq!(imported.document.layers[0].surface().unwrap().to_interleaved(canvas), expected);
    }

    #[test]
    fn damaged_layer_channel_rejects_editable_pdf_imports_but_rendering_succeeds() {
        let mut original = Document::new("source", Size::new(3, 2), ColorMode::Rgb, SampleType::U8);
        original.layers.push(Layer::raster("Layer", original.pixel_format()));
        original.layers[0].surface_mut().unwrap().fill_rect(photocraft_geom::Rect::new(0, 0, 3, 2), &[1.0, 0.0, 0.0, 1.0]);
        let mut psd = crate::document_to_psd(&original);
        psd.layers_mut()[0].channels[0].data.clear();
        let (source, _) = ImageSourceData::from_psd(&psd).to_bytes(ByteOrder::Big).unwrap();
        assert!(ImageSourceData::from_bytes(&source).is_ok());
        let pdf = photoshop_pdf(&source[SIGNATURE.len()..]);

        let error = layered_result("damaged-channel.pdf", &pdf, 1, &Interrupt::NONE).unwrap_err();
        assert!(matches!(error, LayeredPdfError::Corrupt(_)), "{error:?}");

        for mode in [PdfImportMode::Auto, PdfImportMode::Layers] {
            let options = ImportOptions { pdf: crate::PdfImportOptions { mode, ..Default::default() }, ..Default::default() };
            assert!(matches!(crate::import_with_options("damaged-channel.pdf", &pdf, &options), Err(IoError::Pdf(_))));
        }

        let options = ImportOptions { pdf: crate::PdfImportOptions { mode: PdfImportMode::Render, dpi: 72.0, ..Default::default() }, ..Default::default() };
        let rendered = crate::import_with_options("damaged-channel.pdf", &pdf, &options).unwrap();
        assert_eq!(rendered.document.size, Size::new(3, 2));
        assert_eq!(rendered.document.layers[0].surface().unwrap().pixel(0, 0), [1.0, 1.0, 1.0, 1.0]);
    }

    #[test]
    fn malformed_pdf_is_a_terminal_pdf_read_error() {
        let error = layered_result("malformed.pdf", b"%PDF-not a document", 1, &Interrupt::NONE).unwrap_err();
        assert!(matches!(error, LayeredPdfError::Pdf(_)), "{error:?}");
    }

    #[test]
    fn invalid_page_selection_is_not_classified_as_corrupt_layer_data() {
        let pdf = ordinary_pdf();
        for page in [0, 2] {
            let error = layered_result("flat.pdf", &pdf, page, &Interrupt::NONE).unwrap_err();
            assert!(matches!(error, LayeredPdfError::Pdf(_)), "{error:?}");
        }
    }

    #[test]
    fn absent_preserved_data_falls_back_to_rendering() {
        let options = ImportOptions { pdf: crate::PdfImportOptions { dpi: 72.0, ..Default::default() }, ..Default::default() };
        let imported = crate::import_with_options("flat.pdf", &ordinary_pdf(), &options).unwrap();
        assert_eq!(imported.document.size, Size::new(1, 1));
        assert_eq!(imported.document.layers[0].surface().unwrap().pixel(0, 0), [0.0, 1.0, 0.0, 1.0]);
        assert!(imported.source_read_only);
    }

    #[test]
    fn pdf_size_limit_is_a_plain_pdf_error_without_a_large_allocation() {
        let error = validate_pdf_size_len(MAX_PDF_BYTES + 1).unwrap_err();
        assert!(matches!(error, LayeredPdfError::Pdf(_)), "{error:?}");
    }

    #[test]
    fn corrupt_preserved_layers_do_not_silently_fall_back_to_rendering() {
        let pdf = photoshop_pdf(b"8BIMLr16\x00\x00\x00\x10");
        let error = layered_result("damaged.pdf", &pdf, 1, &Interrupt::NONE).unwrap_err();
        assert!(matches!(error, LayeredPdfError::Corrupt(_)), "{error:?}");
        assert!(crate::import_with_options("damaged.pdf", &pdf, &ImportOptions::default()).is_err());
        let render = ImportOptions { pdf: crate::PdfImportOptions { mode: PdfImportMode::Render, dpi: 72.0, ..Default::default() }, ..Default::default() };
        let imported = crate::import_with_options("damaged.pdf", &pdf, &render).unwrap();
        assert_eq!(imported.document.size, Size::new(3, 2));
        assert_eq!(imported.document.layers[0].surface().unwrap().pixel(0, 0), [1.0, 1.0, 1.0, 1.0]);
    }
}
