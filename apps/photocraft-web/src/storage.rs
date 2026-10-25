//! Host storage: a page that serves PhotoCraft can offer its own file storage (a cloud drive, a
//! team folder) next to this device, through `window.craftStorage`:
//!
//! ```js
//! window.craftStorage = {
//!   // File › Open: {path, bytes} for a file in the host's storage, "device" for the browser's
//!   // own file picker, or null when the user cancelled.
//!   async chooseOpen(extensions /* ["psd", …] or null */) { … },
//!   // File › Save As: a path in the host's storage, "device" for a download, or null.
//!   async chooseSave(suggested /* a file name, or the document's host path */) { … },
//!   // Writes a file the user saved to a host path; the host reports progress and errors itself.
//!   async write(path, bytes /* Uint8Array */) { … },
//! };
//! ```
//!
//! Host paths have a scheme of their own, such as `cloud:home/art/photo.psd`
//! ([`photocraft_ui_egui::file_dialog::is_host_path`]): a document opened from one keeps it, so
//! File › Save writes back there without asking. Without `window.craftStorage` nothing changes.
//!
//! Browsers open a file picker only during a click: when the user picks "This device" in the host's
//! own dialog, `chooseOpen` should resolve with "device" right from that click, or the browser's
//! picker can't open.

use std::cell::RefCell;
use std::rc::Rc;

use photocraft_ui_egui::file_dialog::is_host_path;
use photocraft_ui_egui::{FileDialogAnswer, FileDialogRequest, Services};
use wasm_bindgen::JsCast as _;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;

fn storage() -> Option<JsValue> {
    let window: JsValue = web_sys::window()?.into();
    let storage = js_sys::Reflect::get(&window, &"craftStorage".into()).ok()?;
    (!storage.is_undefined() && !storage.is_null()).then_some(storage)
}

async fn call(method: &str, args: &[JsValue]) -> Result<JsValue, JsValue> {
    let storage = storage().ok_or(JsValue::NULL)?;
    let f: js_sys::Function = js_sys::Reflect::get(&storage, &method.into())?.dyn_into()?;
    let promise: js_sys::Promise = f.apply(&storage, &args.iter().collect::<js_sys::Array>())?.dyn_into()?;
    JsFuture::from(promise).await
}

/// Wrap the file dialog and the writer in `services` so they offer the host's storage when the
/// page has one. The device's own dialog and download stay one choice away.
pub fn install(services: &mut Services) {
    if let Some(device_dialog) = services.file_dialog.take() {
        let device_dialog = Rc::new(RefCell::new(device_dialog));
        services.file_dialog = Some(Box::new(move |request, parent, reply| {
            if storage().is_none() {
                return (device_dialog.borrow_mut())(request, parent, reply);
            }
            let device_dialog = device_dialog.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let choice = match &request {
                    FileDialogRequest::Open { extensions, .. } => {
                        let exts = extensions.as_ref().map_or(JsValue::NULL, |e| e.iter().map(JsValue::from).collect::<js_sys::Array>().into());
                        call("chooseOpen", &[exts]).await
                    }
                    FileDialogRequest::Save { suggested } => call("chooseSave", &[suggested.into()]).await,
                    // A host's storage has no folder paths for engine commands: the device answers.
                    FileDialogRequest::Folder { .. } => return (device_dialog.borrow_mut())(request, None, reply),
                };
                match choice {
                    Ok(c) if c.is_null() => reply.send(None),
                    Ok(c) if c.is_object() => {
                        let path = js_sys::Reflect::get(&c, &"path".into()).ok().and_then(|p| p.as_string()).unwrap_or_default();
                        let bytes = js_sys::Reflect::get(&c, &"bytes".into()).map(|b| js_sys::Uint8Array::new(&b).to_vec()).unwrap_or_default();
                        reply.send(Some(FileDialogAnswer::Contents(path, bytes)));
                    }
                    Ok(c) if c.as_string().is_some_and(|p| is_host_path(&p)) => reply.send(c.as_string().map(FileDialogAnswer::SaveTo)),
                    // "device", nothing, or a failed host: the browser's own picker or download.
                    _ => (device_dialog.borrow_mut())(request, None, reply),
                }
            });
        }));
    }
    if let Some(mut device_write) = services.write.take() {
        services.write = Some(Box::new(move |path: &str, bytes: &[u8]| {
            if !is_host_path(path) || storage().is_none() {
                return device_write(path, bytes);
            }
            let args = [JsValue::from(path), js_sys::Uint8Array::from(bytes).into()];
            wasm_bindgen_futures::spawn_local(async move {
                if let Err(e) = call("write", &args).await {
                    log::error!("photocraft-web: craftStorage.write failed: {e:?}");
                }
            });
            Ok(())
        }));
    }
}
