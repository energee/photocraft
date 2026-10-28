use super::*;
use photocraft_io::{ImportOptions, PdfImportMode};

fn pdf() -> Vec<u8> {
    let mut s = Session::new();
    s.execute("file.new", json!({"width": 4, "height": 2, "background": "white"})).unwrap();
    let mut doc = s.active().unwrap().doc.as_ref().clone();
    doc.resolution_dpi = 72.0;
    photocraft_io::export(&doc, "page.pdf", &Default::default()).unwrap().bytes
}

#[test]
fn pdf_render_options_change_dimensions_without_acquiring_source_save_path() {
    let bytes = pdf();
    let mut s = Session::new();
    let opts = crate::jobs::pdf_import_options(ImportOptions::default(), &json!({"pdf": {"mode": "render", "dpi": 144}})).unwrap();
    open_bytes_as_with_options(&mut s, "page.pdf", &bytes, None, Some("/private/page.pdf".into()), &opts).unwrap();
    let st = s.active().unwrap();
    assert_eq!((st.doc.size.width, st.doc.size.height), (8, 4));
    assert_eq!(st.doc.resolution_dpi, 144.0);
    assert!(st.path.is_none(), "Save must not flatten over the imported PDF");
    assert!(st.source_read_only);
    assert!(!saves_in_place("page.pdf"));
}

#[test]
fn pdf_layers_request_does_not_silently_rasterize_an_ordinary_pdf() {
    let mut s = Session::new();
    let mut opts = ImportOptions::default();
    opts.pdf.mode = PdfImportMode::Layers;
    let err = open_bytes_as_with_options(&mut s, "page.pdf", &pdf(), None, None, &opts).unwrap_err();
    assert!(err.to_string().contains("Photoshop"), "{err}");
    assert!(s.documents().is_empty());
}

#[test]
fn invalid_pdf_import_params_leave_session_unchanged() {
    let mut s = Session::new();
    for pdf in [
        json!(null),
        json!(true),
        json!({"page": 0}),
        json!({"page": -1}),
        json!({"page": 1.5}),
        json!({"page": u64::MAX}),
        json!({"dpi": 0}),
        json!({"dpi": 2401}),
        json!({"dpi": "300"}),
        json!({"mode": "preview"}),
        json!({"box": "unknown"}),
        json!({"pages": 1}),
    ] {
        let err = s.execute("file.openAs", json!({"path": "not-read.pdf", "pdf": pdf})).unwrap_err();
        assert!(matches!(err, EngineError::BadParams { .. }), "{pdf}: {err}");
        assert!(s.documents().is_empty());
    }
}
