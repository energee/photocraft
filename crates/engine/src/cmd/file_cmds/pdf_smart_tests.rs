use super::*;
use photocraft_doc::{PdfBox, PdfImportMode, PdfImportOptions};

fn pages(left: &str) -> Vec<u8> {
    let red = "1 0 0 rg 0 0 8 4 re f";
    let green_blue = format!("0 0 1 rg 0 0 8 4 re f {left} rg 0 0 2 4 re f");
    let page = |content| format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 8 4] /CropBox [2 0 6 4] /Resources << >> /Contents {content} 0 R >>");
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>".to_string(),
        page(5),
        page(6),
        format!("<< /Length {} >>\nstream\n{red}\nendstream", red.len()),
        format!("<< /Length {} >>\nstream\n{green_blue}\nendstream", green_blue.len()),
    ];
    let mut bytes = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (index, object) in objects.iter().enumerate() {
        offsets.push(bytes.len());
        bytes.extend_from_slice(format!("{} 0 obj\n{object}\nendobj\n", index + 1).as_bytes());
    }
    let xref = bytes.len();
    bytes.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes());
    for offset in offsets {
        bytes.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    bytes.extend_from_slice(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objects.len() + 1).as_bytes());
    bytes
}

fn smart(doc: &Document, id: LayerId) -> &SmartObject {
    let LayerContent::Smart(sm) = &doc.layer(id).unwrap().content else { panic!("expected smart object") };
    sm
}

fn selected() -> PdfImportOptions {
    PdfImportOptions { mode: PdfImportMode::Render, page: 2, dpi: 144.0, crop_box: PdfBox::Media }
}

fn place_pdf(s: &mut Session, bytes: Vec<u8>, linked: Option<String>) -> LayerId {
    let result = place_bytes(
        s,
        "pages.pdf",
        bytes,
        linked,
        &json!({
            "fit": false, "center": [8, 4],
            "pdf": {"mode": "render", "page": 2, "dpi": 144, "box": "media"}
        }),
    )
    .unwrap();
    LayerId(result["layer"].as_u64().unwrap())
}

fn assert_color(surface: &Surface, expected: [f32; 3]) {
    let pixel = surface.pixel(1, 1);
    for (actual, expected) in pixel.iter().zip(expected) {
        assert!((actual - expected).abs() < 0.01, "{pixel:?}");
    }
}

#[test]
fn pdf_smart_cache_distinguishes_page_resolution_box_and_mode() {
    let mut s = Session::new();
    s.execute("file.new", json!({"width": 40, "height": 24})).unwrap();
    let id = place_pdf(&mut s, pages("0 1 0"), None);
    let doc = &s.active().unwrap().doc;
    let mut sm = smart(doc, id).clone();
    assert_eq!(sm.pdf_import, Some(selected()));
    sm.cache = None;
    sm.pdf_import = None;
    let default = crate::smart_cmds::render(doc, &sm).unwrap().unwrap();
    assert_color(&default, [1.0, 0.0, 0.0]);

    for (page, dpi, crop_box, width, height, color) in [
        (2, 144.0, PdfBox::Media, 16, 8, [0.0, 1.0, 0.0]),
        (2, 72.0, PdfBox::Media, 8, 4, [0.0, 1.0, 0.0]),
        (2, 144.0, PdfBox::Crop, 8, 8, [0.0, 0.0, 1.0]),
        (1, 144.0, PdfBox::Media, 16, 8, [1.0, 0.0, 0.0]),
    ] {
        sm.pdf_import = Some(PdfImportOptions { mode: PdfImportMode::Render, page, dpi, crop_box });
        let rendered = crate::smart_cmds::render(doc, &sm).unwrap().unwrap();
        assert_eq!(rendered.content_bounds(), Rect::new(0, 0, width, height));
        assert_color(&rendered, color);
    }
    sm.pdf_import = Some(PdfImportOptions { mode: PdfImportMode::Layers, ..selected() });
    assert!(crate::smart_cmds::render(doc, &sm).is_err(), "a cached raster must not satisfy a layer-only request");
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn pdf_smart_refresh_edit_and_native_reopen_retain_the_selection() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("pages.pdf");
    let bytes = pages("0 1 0");
    std::fs::write(&source, &bytes).unwrap();
    for linked in [false, true] {
        let mut s = Session::new();
        s.execute("file.new", json!({"width": 40, "height": 24})).unwrap();
        let id = place_pdf(&mut s, bytes.clone(), linked.then(|| source.to_string_lossy().into_owned()));
        let bundle = crate::smart_cmds::encode_source(&s.active().unwrap().doc).unwrap();
        let back = crate::smart_cmds::decode_source("roundtrip.pcraft", &bundle, None).unwrap();
        assert_eq!(smart(&back, id).pdf_import, Some(selected()));
        let parent_index = s.add_document(back, None);
        let expected = if linked {
            std::fs::write(&source, pages("0 1 1")).unwrap();
            s.execute("layer.smartObjects.updateModifiedContent", json!({"layer": id.0})).unwrap();
            assert_color(smart(&s.active().unwrap().doc, id).cache.as_ref().unwrap(), [0.0, 1.0, 1.0]);
            [0.0, 1.0, 1.0]
        } else {
            [0.0, 1.0, 0.0]
        };
        let doc = &s.active().unwrap().doc;
        let mut sm = smart(doc, id).clone();
        sm.cache = None;
        assert_color(&crate::smart_cmds::render(doc, &sm).unwrap().unwrap(), expected);
        let result = s.execute("layer.smartObjects.editContents", json!({"layer": id.0})).unwrap();
        let child_index = result["document"].as_u64().unwrap() as usize;
        let child = &s.active().unwrap().doc;
        assert_eq!((child.size.width, child.size.height, child.resolution_dpi), (16, 8, 144.0));
        assert_color(child.layers[0].surface().unwrap(), expected);
        s.execute("image.imageRotation.90cw", json!({})).unwrap();
        assert!(crate::smart_cmds::commit_child(&mut s, child_index).unwrap());
        let updated = smart(&s.documents()[parent_index].doc, id);
        assert!(matches!(updated.source, SmartSource::Embedded { ref file_name, .. } if file_name.ends_with(".pcraft")));
        assert!(updated.pdf_import.is_none(), "native replacement no longer has a PDF page selection");
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn shared_pdf_contents_keep_each_instances_selection_through_replace_and_relink() {
    let directory = tempfile::tempdir().unwrap();
    let linked = directory.path().join("replacement.pdf");
    let mut s = Session::new();
    s.execute("file.new", json!({"width": 40, "height": 24})).unwrap();
    let id = place_pdf(&mut s, pages("0 1 0"), None);
    let sibling = LayerId(s.execute("layer.duplicate", json!({"layer": id.0})).unwrap()["layer"].as_u64().unwrap());
    let sibling_selection = PdfImportOptions { mode: PdfImportMode::Render, page: 1, dpi: 72.0, crop_box: PdfBox::Crop };
    s.edit("Select sibling PDF page", |doc, _| {
        let LayerContent::Smart(sm) = &mut doc.layer_mut(sibling).unwrap().content else { panic!("expected smart object") };
        sm.pdf_import = Some(sibling_selection);
        Ok(())
    })
    .unwrap();

    crate::smart_cmds::replace_contents_bytes(&mut s, id, "replacement.pdf", pages("1 0 1")).unwrap();
    let doc = &s.active().unwrap().doc;
    assert_eq!(smart(doc, id).pdf_import, Some(selected()));
    assert_eq!(smart(doc, sibling).pdf_import, Some(sibling_selection));
    assert_color(&crate::smart_cmds::render(doc, smart(doc, id)).unwrap().unwrap(), [1.0, 0.0, 1.0]);
    let sibling_image = crate::smart_cmds::render(doc, smart(doc, sibling)).unwrap().unwrap();
    assert_eq!(sibling_image.content_bounds(), Rect::new(0, 0, 4, 4));
    assert_color(&sibling_image, [1.0, 0.0, 0.0]);

    std::fs::write(&linked, pages("0 1 1")).unwrap();
    s.execute("layer.smartObjects.relinkToFile", json!({"layer": id.0, "path": linked.to_string_lossy()})).unwrap();
    let doc = &s.active().unwrap().doc;
    assert_eq!(smart(doc, id).pdf_import, Some(selected()));
    assert_eq!(smart(doc, sibling).pdf_import, Some(sibling_selection));
    assert_color(&crate::smart_cmds::render(doc, smart(doc, id)).unwrap().unwrap(), [0.0, 1.0, 1.0]);
    assert_color(&crate::smart_cmds::render(doc, smart(doc, sibling)).unwrap().unwrap(), [1.0, 0.0, 0.0]);

    crate::smart_cmds::replace_contents_bytes(&mut s, id, "plain.svg", br##"<svg xmlns="http://www.w3.org/2000/svg" width="8" height="4"/>"##.to_vec())
        .unwrap();
    let doc = &s.active().unwrap().doc;
    assert!(smart(doc, id).pdf_import.is_none());
    assert!(smart(doc, sibling).pdf_import.is_none());
}

#[test]
fn replacing_shared_pdf_contents_rolls_back_when_a_sibling_page_is_invalid() {
    let mut s = Session::new();
    s.execute("file.new", json!({"width": 40, "height": 24})).unwrap();
    let id = place_pdf(&mut s, pages("0 1 0"), None);
    let sibling = LayerId(s.execute("layer.duplicate", json!({"layer": id.0})).unwrap()["layer"].as_u64().unwrap());
    s.edit("Select unavailable sibling PDF page", |doc, _| {
        let LayerContent::Smart(sm) = &mut doc.layer_mut(sibling).unwrap().content else { panic!("expected smart object") };
        sm.pdf_import = Some(PdfImportOptions { page: 3, ..selected() });
        Ok(())
    })
    .unwrap();
    let before = smart(&s.active().unwrap().doc, id).source.clone();

    assert!(crate::smart_cmds::replace_contents_bytes(&mut s, id, "replacement.pdf", pages("1 0 1")).is_err());

    let doc = &s.active().unwrap().doc;
    assert_eq!(smart(doc, id).source, before);
    assert_eq!(smart(doc, id).pdf_import, Some(selected()));
    assert_eq!(smart(doc, sibling).pdf_import, Some(PdfImportOptions { page: 3, ..selected() }));
}

#[test]
fn editing_one_shared_pdf_projection_keeps_other_projections_open_and_pdf() {
    let mut s = Session::new();
    s.execute("file.new", json!({"width": 40, "height": 24})).unwrap();
    let id = place_pdf(&mut s, pages("0 1 0"), None);
    let sibling = LayerId(s.execute("layer.duplicate", json!({"layer": id.0})).unwrap()["layer"].as_u64().unwrap());
    let sibling_selection = PdfImportOptions { mode: PdfImportMode::Render, page: 1, dpi: 72.0, crop_box: PdfBox::Crop };
    s.edit("Select sibling PDF page", |doc, _| {
        let LayerContent::Smart(sm) = &mut doc.layer_mut(sibling).unwrap().content else { panic!("expected smart object") };
        sm.pdf_import = Some(sibling_selection);
        Ok(())
    })
    .unwrap();

    let page_two_child = s.execute("layer.smartObjects.editContents", json!({"layer": id.0})).unwrap()["document"].as_u64().unwrap() as usize;
    assert_eq!(s.active().unwrap().doc.size, photocraft_geom::Size::new(16, 8));
    s.set_active(0);
    let page_one_child = s.execute("layer.smartObjects.editContents", json!({"layer": sibling.0})).unwrap()["document"].as_u64().unwrap() as usize;
    assert_ne!(page_two_child, page_one_child);
    assert_eq!(s.active().unwrap().doc.size, photocraft_geom::Size::new(4, 4));

    s.set_active(page_two_child);
    s.execute("image.imageRotation.90cw", json!({})).unwrap();
    assert!(crate::smart_cmds::commit_child(&mut s, page_two_child).unwrap());

    s.set_active(0);
    let doc = &s.active().unwrap().doc;
    assert!(matches!(smart(doc, id).source, SmartSource::Embedded { ref file_name, .. } if file_name.ends_with(".pcraft")));
    assert!(smart(doc, id).pdf_import.is_none());
    assert!(matches!(smart(doc, sibling).source, SmartSource::Embedded { ref file_name, .. } if file_name.ends_with(".pdf")));
    assert_eq!(smart(doc, sibling).pdf_import, Some(sibling_selection));
    assert_color(&crate::smart_cmds::render(doc, smart(doc, sibling)).unwrap().unwrap(), [1.0, 0.0, 0.0]);

    assert_eq!(s.execute("layer.smartObjects.editContents", json!({"layer": id.0})).unwrap()["document"].as_u64(), Some(page_two_child as u64));
    s.set_active(0);
    assert_eq!(s.execute("layer.smartObjects.editContents", json!({"layer": sibling.0})).unwrap()["document"].as_u64(), Some(page_one_child as u64));
}
