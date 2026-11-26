use objc2_app_kit::{
    NSPasteboard, NSPasteboardTypeFileURL, NSPasteboardTypePNG, NSPasteboardTypeString,
    NSPasteboardTypeTIFF,
};
use objc2_foundation::{NSString, NSURL};
use std::path::PathBuf;

/// Rich clipboard content supporting plain text, Finder file URLs, and macOS screenshot PNG/TIFF images.
#[derive(Debug)]
pub enum ClipboardContent {
    Files(Vec<PathBuf>),
    ImagePng(Vec<u8>),
    Text(String),
}

/// Extract all local file paths from an `NSPasteboard` (works for both `Cmd+C` in Finder and drag-and-drop).
pub fn read_pasteboard_files(pb: &NSPasteboard) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    unsafe {
        if let Some(items) = pb.pasteboardItems() {
            let count = items.count();
            for i in 0..count {
                let item = items.objectAtIndex(i);
                if let Some(url_str) = item.stringForType(NSPasteboardTypeFileURL) {
                    if let Some(url) = NSURL::URLWithString(&url_str) {
                        if let Some(p) = url.path() {
                            let pb_path = PathBuf::from(p.to_string());
                            if pb_path.exists() {
                                paths.push(pb_path);
                            }
                        }
                    }
                }
            }
        }
    }
    paths
}

/// Read rich clipboard content for `Cmd+V`:
/// 1. Copied Finder files (`NSPasteboardTypeFileURL`)
/// 2. Plain text (`NSPasteboardTypeString`)
/// 3. Copied screenshot/image (`NSPasteboardTypePNG`, or `NSPasteboardTypeTIFF` converted to PNG)
pub fn read_clipboard_content() -> Option<ClipboardContent> {
    let pb = NSPasteboard::generalPasteboard();

    let files = read_pasteboard_files(&pb);
    if !files.is_empty() {
        return Some(ClipboardContent::Files(files));
    }

    unsafe {
        if let Some(text) = pb.stringForType(NSPasteboardTypeString) {
            return Some(ClipboardContent::Text(text.to_string()));
        }
        if let Some(png_data) = pb.dataForType(NSPasteboardTypePNG) {
            let bytes = png_data.to_vec();
            if !bytes.is_empty() {
                return Some(ClipboardContent::ImagePng(bytes));
            }
        }
        if let Some(tiff_data) = pb.dataForType(NSPasteboardTypeTIFF) {
            let raw = tiff_data.to_vec();
            if let Ok(dyn_img) = image::load_from_memory(&raw) {
                let mut png_buf = Vec::new();
                if dyn_img
                    .write_to(
                        &mut std::io::Cursor::new(&mut png_buf),
                        image::ImageFormat::Png,
                    )
                    .is_ok()
                {
                    return Some(ClipboardContent::ImagePng(png_buf));
                }
            }
        }
    }
    None
}

/// Read plain UTF-8 text from the macOS system clipboard (`NSPasteboard::generalPasteboard`).
pub fn read_clipboard() -> Option<String> {
    unsafe {
        let pb = NSPasteboard::generalPasteboard();
        let text = pb.stringForType(NSPasteboardTypeString)?;
        Some(text.to_string())
    }
}

/// Write plain UTF-8 text to the macOS system clipboard.
pub fn write_clipboard(text: &str) {
    unsafe {
        let pb = NSPasteboard::generalPasteboard();
        pb.clearContents();
        let ns = NSString::from_str(text);
        let _ = pb.setString_forType(&ns, NSPasteboardTypeString);
    }
}
