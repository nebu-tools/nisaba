//! The browser binding of [`nisaba_compile_core::LiveCompiler`].
//!
//! The preview worker writes every file once, then only the file that
//! changed, and calls [`JsLiveCompiler::compile`]. The result lists a
//! fingerprint per page, so the worker renders only the pages that changed,
//! and the fonts the compile asked for and did not have, so the worker can
//! fetch them and compile again.

use nisaba_compile_core::{FileContents, FontEntry, LiveCompiler};
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
pub struct JsLiveCompiler {
    inner: LiveCompiler,
}

#[wasm_bindgen]
impl JsLiveCompiler {
    /// Creates a compiler rooted at `entry`. `fonts` is the text of the
    /// build's `fonts.json`; no font is loaded yet.
    #[wasm_bindgen(constructor)]
    pub fn new(entry: &str, fonts: &str) -> Result<JsLiveCompiler, JsError> {
        let fonts: Vec<FontEntry> =
            serde_json::from_str(fonts).map_err(|e| JsError::new(&e.to_string()))?;
        LiveCompiler::new(entry, &fonts)
            .map(|inner| Self { inner })
            .map_err(|e| JsError::new(&e))
    }

    pub fn write_text(&mut self, path: &str, text: String) {
        self.inner.write(path, FileContents::Text(text));
    }

    pub fn write_bytes(&mut self, path: &str, bytes: Vec<u8>) {
        self.inner.write(path, FileContents::Bytes(bytes));
    }

    pub fn remove(&mut self, path: &str) {
        self.inner.remove(path);
    }

    /// Gives the compiler the bytes of catalogue font `index`.
    pub fn add_font(&mut self, index: usize, bytes: Vec<u8>) -> Result<(), JsError> {
        self.inner
            .add_font(index, bytes)
            .map_err(|e| JsError::new(&e))
    }

    pub fn set_today(&mut self, year: i32, month: u8, day: u8) {
        self.inner.set_today(year, month, day);
    }

    /// Compiles and returns the `Compiled` result as JSON.
    pub fn compile(&mut self) -> Result<String, JsError> {
        serde_json::to_string(&self.inner.compile()).map_err(|e| JsError::new(&e.to_string()))
    }

    /// Renders one page as RGBA pixels at `pixel_per_pt`. The first eight
    /// bytes are the width and height as little-endian `u32`; pixels follow.
    pub fn page_rgba(&self, index: usize, pixel_per_pt: f32) -> Option<Vec<u8>> {
        let (width, height, pixels) = self.inner.page_rgba(index, pixel_per_pt)?;
        let mut out = Vec::with_capacity(8 + pixels.len());
        out.extend_from_slice(&width.to_le_bytes());
        out.extend_from_slice(&height.to_le_bytes());
        out.extend_from_slice(&pixels);
        Some(out)
    }

    pub fn pdf(&self) -> Result<Vec<u8>, JsError> {
        self.inner.pdf().map_err(|e| JsError::new(&e))
    }
}
