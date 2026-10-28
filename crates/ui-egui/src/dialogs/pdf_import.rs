//! PDF import options, held as an ordinary UI dialog so control clients can inspect and drive it.

use std::sync::Arc;

use serde_json::{Map, Value, json};

use crate::PhotocraftApp;
use crate::state::DialogKind;

const MARK: &str = "__pdfImport";

/// A PDF waiting for the user to choose its rasterization settings.
pub(crate) enum Source {
    Bytes(Arc<Vec<u8>>),
    #[cfg(not(target_arch = "wasm32"))]
    Path(String),
}

pub(crate) struct Pending {
    pub name: String,
    pub path: Option<String>,
    pub source: Source,
}

pub fn owns(fields: &Map<String, Value>) -> bool {
    fields.contains_key(MARK)
}

/// Park an ordinary PDF behind its import-options dialog.
pub(crate) fn open(app: &mut PhotocraftApp, pending: Pending) -> u64 {
    let mut fields = Map::new();
    fields.insert("__label".into(), json!("Import PDF…"));
    fields.insert("mode".into(), json!("auto"));
    fields.insert("page".into(), json!(1));
    fields.insert("dpi".into(), json!(300.0));
    fields.insert("box".into(), json!("crop"));
    let id = app.ui.open_dialog(DialogKind::Command, fields);
    if let Some(dialog) = app.ui.dialog_mut(id) {
        dialog.fields.insert(MARK.into(), json!(id));
    }
    app.pdf_import.push((id, pending));
    id
}
pub fn body(ui: &mut egui::Ui, fields: &mut Map<String, Value>) {
    let mut mode = fields.get("mode").and_then(Value::as_str).filter(|v| matches!(*v, "auto" | "layers" | "render")).unwrap_or("auto").to_string();
    let mut page = fields.get("page").and_then(Value::as_u64).and_then(|v| u32::try_from(v).ok()).filter(|v| *v > 0).unwrap_or(1);
    let mut dpi = fields.get("dpi").and_then(Value::as_f64).filter(|v| v.is_finite() && *v > 0.0).unwrap_or(300.0);
    let mut crop_box = fields.get("box").and_then(Value::as_str).filter(|v| matches!(*v, "crop" | "media")).unwrap_or("crop").to_string();

    ui.label(tl!("Choose how to import this PDF."));
    ui.add_space(6.0);
    egui::Grid::new("pdf-import-options").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
        ui.label(tl!("Import As"));
        ui.vertical(|ui| {
            ui.radio_value(&mut mode, "auto".to_string(), tl!("Photoshop Layers (when available)"));
            ui.radio_value(&mut mode, "layers".to_string(), tl!("Photoshop Layers"));
            ui.radio_value(&mut mode, "render".to_string(), tl!("Rasterized Image"));
        });
        ui.end_row();
        let page_label = ui.label(tl!("Page"));
        ui.add(egui::DragValue::new(&mut page).range(1..=u32::MAX).speed(1.0)).labelled_by(page_label.id);
        ui.end_row();
        let dpi_label = ui.label(tl!("Resolution"));
        ui.horizontal(|ui| {
            ui.add(egui::DragValue::new(&mut dpi).range(1.0..=2400.0).speed(1.0).suffix(" ppi")).labelled_by(dpi_label.id);
        });
        ui.end_row();
        ui.label(tl!("Crop To"));
        ui.horizontal(|ui| {
            ui.radio_value(&mut crop_box, "crop".to_string(), tl!("Crop Box"));
            ui.radio_value(&mut crop_box, "media".to_string(), tl!("Media Box"));
        });
        ui.end_row();
    });
    match mode.as_str() {
        "auto" => ui.weak(tl!("Photoshop layer data is preserved when this PDF contains it.")),
        "layers" => ui.weak(tl!("Open only the Photoshop layer data in this PDF.")),
        _ => ui.weak(tl!("Rasterizing ignores Photoshop layer data.")),
    };
    fields.insert("mode".into(), json!(mode));
    fields.insert("page".into(), json!(page));
    fields.insert("dpi".into(), json!(dpi));
    fields.insert("box".into(), json!(crop_box));
}

fn options(app: &PhotocraftApp, fields: &Map<String, Value>) -> Result<photocraft_io::ImportOptions, String> {
    let pdf = json!({
        "mode": fields.get("mode").cloned().unwrap_or_else(|| json!("auto")),
        "page": fields.get("page").cloned().unwrap_or_else(|| json!(1)),
        "dpi": fields.get("dpi").cloned().unwrap_or_else(|| json!(300.0)),
        "box": fields.get("box").cloned().unwrap_or_else(|| json!("crop")),
    });
    photocraft_engine::jobs::pdf_import_options(photocraft_engine::jobs::import_options(app.session.prefs()), &json!({"pdf": pdf}))
        .map_err(|error| error.to_string())
}

pub fn confirm(app: &mut PhotocraftApp, fields: &Map<String, Value>) -> Result<Value, String> {
    let opts = options(app, fields)?;
    let id = fields.get(MARK).and_then(Value::as_u64).ok_or("no PDF import is pending")?;
    let pending =
        app.pdf_import.iter().position(|(pending_id, _)| *pending_id == id).map(|i| app.pdf_import.swap_remove(i).1).ok_or("no PDF import is pending")?;
    match pending.source {
        Source::Bytes(bytes) => {
            crate::jobs_ui::start_open_with_options(app, &pending.name, pending.path, photocraft_engine::jobs::OpenSource::Bytes(bytes), opts)?
        }
        #[cfg(not(target_arch = "wasm32"))]
        Source::Path(path) => crate::jobs_ui::start_open_with_options(app, &pending.name, pending.path, photocraft_engine::jobs::OpenSource::Path(path), opts)?,
    }
    Ok(app.jobs.last_started.map_or(Value::Null, |job| json!({"job": job.0, "pending": true})))
}

pub fn cancel(app: &mut PhotocraftApp, fields: &Map<String, Value>) {
    if let Some(id) = fields.get(MARK).and_then(Value::as_u64)
        && let Some(i) = app.pdf_import.iter().position(|(pending_id, _)| *pending_id == id)
    {
        app.pdf_import.swap_remove(i);
    }
    app.ui.status = tl!("Open cancelled").into();
    app.ui.status_error = false;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pdf() -> Arc<Vec<u8>> {
        let mut session = photocraft_engine::Session::new();
        session.execute("file.new", json!({"width": 4, "height": 2, "background": "white"})).unwrap();
        let mut doc = session.active().unwrap().doc.as_ref().clone();
        doc.resolution_dpi = 72.0;
        Arc::new(photocraft_io::export(&doc, "page.pdf", &Default::default()).unwrap().bytes)
    }

    #[test]
    fn cancelling_one_pdf_does_not_replace_another_pending_source() {
        let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), Default::default());
        let bytes = pdf();
        let first = open(&mut app, Pending { name: "first.pdf".into(), path: None, source: Source::Bytes(Arc::clone(&bytes)) });
        let second = open(&mut app, Pending { name: "second.pdf".into(), path: Some("second.pdf".into()), source: Source::Bytes(bytes) });
        crate::dialogs::cancel(&mut app, first).unwrap();
        assert!(app.session.documents().is_empty());
        let dialog = app.ui.dialog_mut(second).unwrap();
        dialog.fields.insert("mode".into(), json!("render"));
        dialog.fields.insert("dpi".into(), json!(144));
        crate::dialogs::confirm(&mut app, second).unwrap();
        if let Some(job) = app.jobs.opens.last().map(|tab| tab.job) {
            app.session.wait_job(job).unwrap();
        }
        let st = app.session.active().unwrap();
        assert_eq!(st.doc.name, "second.pdf");
        assert_eq!((st.doc.size.width, st.doc.size.height), (8, 4));
        assert!(st.source_read_only);
        assert!(st.path.is_none());
        assert!(app.pdf_import.is_empty());
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn automation_cannot_confirm_an_ambient_pdf_path() {
        let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), Default::default());
        let id = open(
            &mut app,
            Pending { name: "private.pdf".into(), path: Some("/private/private.pdf".into()), source: Source::Path("/private/private.pdf".into()) },
        );
        app.automation_input = true;
        app.services.automation_command = Some(Box::new(|command, _| if command == "file.openAs" { Err("ambient PDF read denied".into()) } else { Ok(()) }));
        let error = crate::dialogs::confirm(&mut app, id).unwrap_err();
        assert_eq!(error, "ambient PDF read denied");
        assert!(app.session.documents().is_empty());
        assert_eq!(app.pdf_import.len(), 1);
    }
}
