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
//!   // Writes a file the user saved to a host path. The document counts as saved once it
//!   // resolves; a rejection leaves it unsaved and shows the error. Writes come one at a time,
//!   // in the order the user saved, so each must settle: a host whose storage can hang should
//!   // reject after its own timeout (only the host can tell a slow upload from a stuck one).
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

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
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

thread_local! {
    /// Writes waiting for the host, made one at a time in the order the app asked, so a later
    /// save of a file is never overwritten by an earlier one: (id, host path, bytes).
    static QUEUE: RefCell<VecDeque<(u64, String, Vec<u8>)>> = const { RefCell::new(VecDeque::new()) };
    /// The write the host is making now.
    static WRITING: Cell<Option<u64>> = const { Cell::new(None) };
    static LAST_ID: Cell<u64> = const { Cell::new(0) };
    /// Writes that finished since the web shell last looked: (id, result), for
    /// [`photocraft_ui_egui::PhotocraftApp::host_write_settled`] ([`take_settled`]).
    static SETTLED: RefCell<Vec<(u64, Result<(), String>)>> = const { RefCell::new(Vec::new()) };
    /// The app's egui context, asked for a frame when the host answers a write.
    static CTX: RefCell<Option<egui::Context>> = const { RefCell::new(None) };
}

/// The app's egui context, so the host's answers reach the app on the next frame.
pub fn set_context(ctx: egui::Context) {
    CTX.with(|c| *c.borrow_mut() = Some(ctx));
}

/// The id of the latest write to the host that hasn't finished.
fn pending() -> Option<u64> {
    QUEUE.with(|q| q.borrow().back().map(|w| w.0)).or_else(|| WRITING.with(Cell::get))
}

/// The writes to the host that finished since the last call.
pub fn take_settled() -> Vec<(u64, Result<(), String>)> {
    SETTLED.with(|s| std::mem::take(&mut *s.borrow_mut()))
}

/// Queue a write to the host; the first one starts the writer, which works through the queue.
fn enqueue(path: String, bytes: Vec<u8>) {
    let id = LAST_ID.with(|n| {
        n.set(n.get() + 1);
        n.get()
    });
    let start = pending().is_none();
    QUEUE.with(|q| q.borrow_mut().push_back((id, path, bytes)));
    if !start {
        return;
    }
    wasm_bindgen_futures::spawn_local(async move {
        while let Some((id, path, bytes)) = QUEUE.with(|q| q.borrow_mut().pop_front()) {
            WRITING.with(|w| w.set(Some(id)));
            let args = [JsValue::from(&path), js_sys::Uint8Array::from(bytes.as_slice()).into()];
            let result = call("write", &args).await.map(|_| ()).map_err(|e| {
                log::error!("photocraft-web: craftStorage.write of {path} failed: {e:?}");
                e.dyn_ref::<js_sys::Error>().map_or_else(|| e.as_string().unwrap_or_else(|| "the host couldn't store the file".into()), |e| e.message().into())
            });
            WRITING.with(|w| w.set(None));
            SETTLED.with(|s| s.borrow_mut().push((id, result)));
            CTX.with(|c| c.borrow().as_ref().map(egui::Context::request_repaint));
        }
    });
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
                    // Only the user's own choice opens the browser's picker or download.
                    Ok(c) if c.as_string().as_deref() == Some("device") => (device_dialog.borrow_mut())(request, None, reply),
                    // A host that failed or answered something else: nothing was chosen. A device
                    // save here would also leave a document from the host bound to a download.
                    other => {
                        log::error!("photocraft-web: craftStorage didn't answer with a choice: {other:?}");
                        reply.send(None);
                    }
                }
            });
        }));
    }
    if let Some(mut device_write) = services.write.take() {
        services.host_write_pending = Some(Box::new(pending));
        services.write = Some(Box::new(move |path: &str, bytes: &[u8]| {
            if !is_host_path(path) || storage().is_none() {
                return device_write(path, bytes);
            }
            enqueue(path.to_string(), bytes.to_vec());
            Ok(())
        }));
    }
}
