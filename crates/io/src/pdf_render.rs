//! Raster fallback for PDF pages that do not contain importable Photoshop layers.
//!
//! Hayro renders into an sRGB, premultiplied RGBA8 pixmap.  PDF has no dependable
//! document-wide ICC profile, so the result is deliberately tagged as ordinary sRGB
//! rather than copying an image XObject's profile out of context.

use std::sync::Arc;

use hayro::hayro_interpret::InterpreterSettings;
use hayro::hayro_interpret::util::TransformExt;
use hayro::hayro_syntax::{Pdf, PdfData, page::Rotation};
use hayro::kurbo::Affine;
use hayro::vello_cpu::color::palette::css::WHITE;
use hayro::vello_cpu::{Pixmap, RasterizerSettings, RenderContext, Resources, TargetInit};
use hayro::{RenderCache, RenderSettings, render_into};
use photocraft_color::{ColorMode, PixelFormat, SampleType};
use photocraft_doc::{Document, Layer, LayerContent};
use photocraft_doc::{PdfBox, PdfImportOptions};
use photocraft_geom::{Rect, Size};
use photocraft_raster::{Interrupt, Surface};

use crate::{ImportResult, IoError};

/// PDF input is kept below this limit before the parser gets it. This does not attempt to make
/// an arbitrary PDF renderer allocation-free; it rejects inputs too large for an interactive open.
const MAX_PDF_BYTES: usize = 128 << 20;
/// Hayro's CPU pixmap dimensions are `u16`; keeping a lower limit makes room for its intermediates.
const MAX_SIDE: u32 = 16_384;
/// Four RGBA8 bytes per output pixel. This caps the retained source raster at 256 MiB.
const MAX_PIXELS: u64 = 64 << 20;
const MIN_DPI: f64 = 1.0;
const MAX_DPI: f64 = 2_400.0;

/// Render one PDF page as a single, opaque sRGB background layer.
///
/// The caller selects this for [`crate::PdfImportMode::Render`] or after its Photoshop-data probe proves
/// that no preserved data is present. Encrypted PDFs, unsupported PDF features, and requests that
/// exceed the source or pixel limits fail instead of silently opening a different image.
pub(crate) fn import(name: &str, bytes: &[u8], options: &PdfImportOptions, ctl: &Interrupt<'_>) -> Result<ImportResult, IoError> {
    ctl.check().map_err(|_| IoError::Cancelled)?;
    validate_options(bytes, options)?;
    ctl.progress(0.05);

    let data: PdfData = Arc::new(page_data(bytes, options)?).into();
    let pdf = Pdf::new(data).map_err(|e| IoError::Pdf(format!("could not read PDF: {e:?}")))?;
    let page_index = usize::try_from(options.page - 1).map_err(|_| IoError::Pdf("PDF page number is not representable on this platform".into()))?;
    let page =
        pdf.pages().get(page_index).ok_or_else(|| IoError::Pdf(format!("PDF has {} page(s); page {} was requested", pdf.pages().len(), options.page)))?;
    let (width, height, transform) = render_geometry(page, options)?;
    ctl.check().map_err(|_| IoError::Cancelled)?;
    ctl.progress(0.1);

    // Hayro does not offer an interrupt callback within interpretation/rasterisation. Check at
    // each externally observable boundary, before allocating the destination and before publish.
    let cache = RenderCache::new();
    let mut context = RenderContext::new(width, height);
    let scale = Affine::scale(options.dpi / 72.0);
    render_into(page, &cache, &InterpreterSettings::default(), &RenderSettings::default(), &mut context, scale * transform);
    context.flush();
    ctl.check().map_err(|_| IoError::Cancelled)?;
    ctl.progress(0.85);

    let mut pixmap = Pixmap::new(width, height);
    context.render_with(&mut pixmap, &mut Resources::default(), RasterizerSettings { target_init: TargetInit::Clear(WHITE), ..Default::default() });
    ctl.check().map_err(|_| IoError::Cancelled)?;

    // The white target makes every output pixel opaque, so Hayro's premultiplied RGBA8 bytes are
    // also straight RGBA8. Taking the pixmap moves its storage instead of making another full copy.
    let pixels = pixmap.take_rgba8(hayro::vello_cpu::peniko::ImageAlphaType::AlphaPremultiplied);
    let size = Size::new(u32::from(width), u32::from(height));
    let mut surface = Surface::new(PixelFormat::new(ColorMode::Rgb, SampleType::U8, true));
    surface.write_interleaved(Rect::new(0, 0, i32::from(width), i32::from(height)), &pixels);
    surface.prune();

    let mut document = Document::new(name, size, ColorMode::Rgb, SampleType::U8);
    document.icc_profile = Some(photocraft_cms::Builtin::Srgb.profile().to_bytes());
    document.resolution_dpi = options.dpi as f32;
    let mut background = Layer::new("Background", LayerContent::Raster(surface));
    background.locks.transparency = true;
    background.locks.position = true;
    document.layers.push(background);
    ctl.progress(0.98);
    Ok(ImportResult {
        document,
        warnings: vec![
            "PDF page was rasterised to opaque sRGB; editable PDF objects, transparency, spot colours, and PDF ICC/output-intent colour management are not preserved".into(),
            "the PDF rasteriser does not yet faithfully support every blend, isolation, knockout, or colour-key-masked PDF construct".into(),
        ],
        source_read_only: true,
        preview_only: false,
    })
}

/// Reject invalid page numbers/resolutions and source sizes before parsing or allocating a canvas.
fn validate_options(bytes: &[u8], options: &PdfImportOptions) -> Result<(), IoError> {
    if bytes.len() > MAX_PDF_BYTES {
        return Err(IoError::Pdf(format!("PDF is {} MiB, above the {} MiB import limit", bytes.len() >> 20, MAX_PDF_BYTES >> 20)));
    }
    if options.page == 0 {
        return Err(IoError::Pdf("PDF page numbers start at 1".into()));
    }
    if !options.dpi.is_finite() || !(MIN_DPI..=MAX_DPI).contains(&options.dpi) {
        return Err(IoError::Pdf(format!("PDF raster resolution must be between {MIN_DPI:.0} and {MAX_DPI:.0} ppi")));
    }
    Ok(())
}

/// Derive the rotated page canvas and source transform, rejecting oversized pixel allocations.
fn render_geometry(page: &hayro::hayro_syntax::page::Page<'_>, options: &PdfImportOptions) -> Result<(u16, u16, Affine), IoError> {
    let (page_width, page_height) = box_dimensions(page.intersected_crop_box())?;
    let (page_width, page_height) = match page.rotation() {
        Rotation::Horizontal | Rotation::FlippedHorizontal => (page_height, page_width),
        Rotation::None | Rotation::Flipped => (page_width, page_height),
    };
    let transform = page.initial_transform(true).to_kurbo();
    let width = scaled_dimension(page_width, options.dpi, "width")?;
    let height = scaled_dimension(page_height, options.dpi, "height")?;
    let pixels = u64::from(width) * u64::from(height);
    if pixels > MAX_PIXELS {
        return Err(IoError::Pdf(format!(
            "PDF page at {:.0} ppi is {width}×{height} pixels, above the {} megapixel import limit",
            options.dpi,
            MAX_PIXELS >> 20
        )));
    }
    Ok((width, height, transform))
}

/// Round a physical page extent outward without introducing a row at exact DPI boundaries.
fn scaled_dimension(points: f64, dpi: f64, axis: &str) -> Result<u16, IoError> {
    // Multiply before dividing: 960 * (300 / 72) rounds above 4000 and adds a spurious row.
    let pixels = (points * dpi / 72.0).ceil();
    if !pixels.is_finite() || pixels < 1.0 || pixels > f64::from(MAX_SIDE) {
        return Err(IoError::Pdf(format!("PDF page {axis} at this resolution must be between 1 and {MAX_SIDE} pixels")));
    }
    // The range check above establishes an exact, nonzero u16 conversion.
    Ok(pixels as u16)
}

/// Prepare an owned parser buffer. Hayro always clips to CropBox, so Media Box overrides only the
/// chosen page's CropBox in a transient PDF to reveal content outside the original crop.
fn page_data(bytes: &[u8], options: &PdfImportOptions) -> Result<Vec<u8>, IoError> {
    if options.crop_box == PdfBox::Crop {
        return Ok(bytes.to_vec());
    }
    let error = |error: lopdf::Error| IoError::Pdf(format!("could not select Media Box: {error}"));
    let mut pdf = lopdf::Document::load_mem(bytes).map_err(error)?;
    let page = pdf.get_pages().get(&options.page).copied().ok_or_else(|| IoError::Pdf(format!("PDF page {} does not exist", options.page)))?;
    let media = inherited_media_box(&pdf, page)?;
    pdf.get_object_mut(page).and_then(lopdf::Object::as_dict_mut).map_err(error)?.set("CropBox", media);
    let mut data = Vec::new();
    pdf.save_to(&mut data).map_err(|error| IoError::Pdf(format!("could not prepare Media Box rendering: {error}")))?;
    Ok(data)
}

/// Resolve inherited MediaBox data with bounded parent traversal, including cyclic page trees.
fn inherited_media_box(pdf: &lopdf::Document, mut page: lopdf::ObjectId) -> Result<lopdf::Object, IoError> {
    for _ in 0..128 {
        let dict = pdf.get_dictionary(page).map_err(|error| IoError::Pdf(format!("invalid page tree: {error}")))?;
        if let Ok(media) = dict.get_deref(b"MediaBox", pdf) {
            return Ok(media.clone());
        }
        page = dict.get(b"Parent").and_then(lopdf::Object::as_reference).map_err(|_| IoError::Pdf("page has no MediaBox".into()))?;
    }
    Err(IoError::Pdf("page tree is cyclic or exceeds 128 levels".into()))
}

/// Validate finite positive page geometry before pixel arithmetic or renderer transforms.
fn box_dimensions(box_: hayro::hayro_syntax::object::Rect) -> Result<(f64, f64), IoError> {
    // ponytail: width = x1 - x0 is finite only when both corners are, so this also rejects NaN/inf corners.
    let (width, height) = (box_.width(), box_.height());
    if !(width.is_finite() && height.is_finite() && width > 0.0 && height > 0.0) {
        return Err(IoError::Pdf("PDF page box must have finite, positive dimensions".into()));
    }
    Ok((width, height))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pdf_with_boxes(rotation: Option<u16>) -> Vec<u8> {
        let rotate = rotation.map_or_else(String::new, |degrees| format!(" /Rotate {degrees}"));
        let content = "q 1 0 0 rg 10 20 7 5 re f 0 0 1 rg 50 45 10 7 re f Q";
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
            format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 60] /CropBox [10 20 30 40]{rotate} /Resources << >> /Contents 4 0 R >>"),
            format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len()),
        ];
        let mut out = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (index, object) in objects.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n{object}\nendobj\n", index + 1).as_bytes());
        }
        let xref = out.len();
        out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes());
        for offset in offsets {
            out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objects.len() + 1).as_bytes());
        out
    }

    fn is_red(surface: &Surface, x: i32, y: i32) -> bool {
        let pixel = surface.read_region(Rect::new(x, y, x + 1, y + 1));
        pixel[0] > 0.9 && pixel[1] < 0.1 && pixel[2] < 0.1
    }

    #[test]
    fn rasterises_visible_content_at_selected_box_and_resolution() {
        let bytes = pdf_with_boxes(None);
        let crop = import("page.pdf", &bytes, &PdfImportOptions { dpi: 72.0, ..Default::default() }, &Interrupt::NONE).unwrap();
        assert_eq!((crop.document.size.width, crop.document.size.height), (20, 20));
        let crop_surface = crop.document.layers[0].surface().unwrap();
        assert!(is_red(crop_surface, 2, 17), "CropBox origin must translate the asymmetric mark");

        let media = import("page.pdf", &bytes, &PdfImportOptions { dpi: 72.0, crop_box: PdfBox::Media, ..Default::default() }, &Interrupt::NONE).unwrap();
        assert_eq!((media.document.size.width, media.document.size.height), (100, 60));
        let surface = media.document.layers[0].surface().unwrap();
        assert!(is_red(surface, 12, 37), "MediaBox origin must retain the asymmetric mark position");
        let outside_crop = surface.read_region(Rect::new(52, 12, 53, 13));
        assert!(outside_crop[0] < 0.1 && outside_crop[2] > 0.9, "Media Box must reveal the blue mark outside CropBox");
    }

    #[test]
    fn fractional_dpi_scale_does_not_add_an_extra_pixel_row() {
        let bytes = pdf_with_boxes(None);
        let result = import("page.pdf", &bytes, &PdfImportOptions { dpi: 300.0, crop_box: PdfBox::Media, ..Default::default() }, &Interrupt::NONE).unwrap();
        assert_eq!((result.document.size.width, result.document.size.height), (417, 250));
        assert!(is_red(result.document.layers[0].surface().unwrap(), 50, 155));
    }

    #[test]
    fn rotates_the_page_appearance_with_its_media_box() {
        for (rotation, width, height, x, y) in [(90, 60, 100, 23, 12), (180, 100, 60, 88, 23), (270, 60, 100, 37, 88)] {
            let bytes = pdf_with_boxes(Some(rotation));
            let rotated = import("page.pdf", &bytes, &PdfImportOptions { dpi: 72.0, crop_box: PdfBox::Media, ..Default::default() }, &Interrupt::NONE).unwrap();
            assert_eq!((rotated.document.size.width, rotated.document.size.height), (width, height), "{rotation}");
            assert!(is_red(rotated.document.layers[0].surface().unwrap(), x, y), "{rotation}: mark must rotate with the page");
        }
    }

    #[test]
    fn rejects_invalid_options_and_cancellation() {
        let bytes = pdf_with_boxes(None);
        assert!(import("page.pdf", &bytes, &PdfImportOptions { page: 0, ..Default::default() }, &Interrupt::NONE).is_err());
        let cancelled = || true;
        assert!(matches!(import("page.pdf", &bytes, &PdfImportOptions::default(), &Interrupt::cancel_only(&cancelled)), Err(IoError::Cancelled)));
    }
}
