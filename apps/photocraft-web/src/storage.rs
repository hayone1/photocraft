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
//! ([`craft_web_host::is_host_path`]): a document opened from one keeps it, so
//! File › Save writes back there without asking. Without `window.craftStorage` nothing changes.
//!
//! Browsers open a file picker only during a click: when the user picks "This device" in the host's
//! own dialog, `chooseOpen` should resolve with "device" right from that click, or the browser's
//! picker can't open.
//!
//! The contract and its rules are the same for every Craft app and live in `craft-web-host`; this
//! module connects them to PhotoCraft's file dialog and writer.

use std::cell::RefCell;
use std::rc::Rc;

use craft_web_host::is_host_path;
use craft_web_host::storage::{self, OpenChoice, SaveChoice, SaveQueue};
use photocraft_ui_egui::{FileDialogAnswer, FileDialogRequest, Services};

thread_local! {
    /// Writes to the host, one at a time in the order the user saved (`craft_web_host`).
    static WRITES: SaveQueue<Result<(), String>> = SaveQueue::new();
}

/// The app's egui context, so the host's answers reach the app on the next frame.
pub fn set_context(ctx: egui::Context) {
    WRITES.with(|w| w.on_done(move || ctx.request_repaint()));
}

/// The writes to the host that finished since the last call, for
/// [`photocraft_ui_egui::PhotocraftApp::host_write_settled`]: (id, result).
pub fn take_settled() -> Vec<(u64, Result<(), String>)> {
    WRITES.with(SaveQueue::take_done)
}

/// Wrap the file dialog and the writer in `services` so they offer the host's storage when the
/// page has one. The device's own dialog and download stay one choice away.
pub fn install(services: &mut Services) {
    if let Some(device_dialog) = services.file_dialog.take() {
        let device_dialog = Rc::new(RefCell::new(device_dialog));
        services.file_dialog = Some(Box::new(move |request, parent, reply| {
            if !storage::available() {
                return (device_dialog.borrow_mut())(request, parent, reply);
            }
            let device_dialog = device_dialog.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let answer = match &request {
                    FileDialogRequest::Open { extensions, .. } => {
                        let exts: Option<Vec<&str>> = extensions.as_ref().map(|e| e.iter().map(String::as_str).collect());
                        storage::choose_open(exts.as_deref()).await.map(|choice| match choice {
                            OpenChoice::File { path, bytes } => Some(Some(FileDialogAnswer::Contents(path, bytes))),
                            OpenChoice::Device => None,
                            OpenChoice::Cancelled => Some(None),
                        })
                    }
                    FileDialogRequest::Save { suggested } => storage::choose_save(suggested).await.map(|choice| match choice {
                        SaveChoice::Host(path) => Some(Some(FileDialogAnswer::SaveTo(path))),
                        SaveChoice::Device => None,
                        SaveChoice::Cancelled => Some(None),
                    }),
                    // A host's storage has no folder paths for engine commands: the device answers.
                    FileDialogRequest::Folder { .. } => Ok(None),
                };
                match answer {
                    Ok(Some(answer)) => reply.send(answer),
                    // The user's own choice: the browser's picker or download.
                    Ok(None) => (device_dialog.borrow_mut())(request, None, reply),
                    // A host that failed or answered something else: nothing was chosen. A device
                    // save here would also leave a document from the host bound to a download.
                    Err(e) => {
                        log::error!("photocraft-web: {e}");
                        reply.send(None);
                    }
                }
            });
        }));
    }
    if let Some(mut device_write) = services.write.take() {
        services.host_write_pending = Some(Box::new(|| WRITES.with(SaveQueue::latest)));
        services.write = Some(Box::new(move |path: &str, bytes: &[u8]| {
            if !is_host_path(path) {
                return device_write(path, bytes);
            }
            // A document from a host whose storage is gone: a download would lose its place there.
            if !storage::available() {
                return Err(format!("{path} is in the page's own storage, which isn't available any more"));
            }
            let (path, bytes) = (path.to_string(), bytes.to_vec());
            WRITES.with(|w| {
                w.push(
                    async move { storage::write(&path, &bytes).await.inspect_err(|e| log::error!("photocraft-web: craftStorage.write of {path} failed: {e}")) },
                )
            });
            Ok(())
        }));
    }
}
