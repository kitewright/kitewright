//! Browser-free typesetting backend: compile a Typst template (with JSON data
//! injected via `sys.inputs.data`) straight to PDF bytes. No process is
//! spawned. Compiled only with the `typst` feature.
//!
//! Data injection: the request's `data` JSON is serialized to a string and
//! placed in the standard-library inputs dictionary under the key `data`, so a
//! template reads it with, e.g.:
//!
//! ```typst
//! #let data = json(bytes(sys.inputs.data))
//! ```
//!
//! Fonts are bundled at compile time from `typst-assets` (Libertinus Serif —
//! Typst's default — plus New Computer Modern and DejaVu Sans Mono), so a
//! template that specifies no font renders without touching the filesystem.

use std::sync::OnceLock;

use typst::diag::{FileError, FileResult, SourceDiagnostic, Warned};
use typst::foundations::{Bytes, Datetime, Dict, Duration, Str, Value};
use typst::syntax::{FileId, Source};
use typst::text::{Font, FontBook};
use typst::utils::LazyHash;
use typst::{Library, LibraryExt, World};

use crate::request::{RenderError, RenderRequest};

/// Bundled fonts, parsed once and shared across all renders (both `Font` and
/// `FontBook` clones are cheap / reference-counted).
struct FontCache {
    book: FontBook,
    fonts: Vec<Font>,
}

fn font_cache() -> &'static FontCache {
    static CACHE: OnceLock<FontCache> = OnceLock::new();
    CACHE.get_or_init(|| {
        let mut book = FontBook::new();
        let mut fonts = Vec::new();
        for data in typst_assets::fonts() {
            let bytes = Bytes::new(data);
            for font in Font::iter(bytes) {
                book.push(font.info().clone());
                fonts.push(font);
            }
        }
        FontCache { book, fonts }
    })
}

/// A minimal [`World`] backing a single in-memory compilation: one main source,
/// bundled fonts, no external files.
struct PdfWorld {
    library: LazyHash<Library>,
    book: LazyHash<FontBook>,
    fonts: &'static [Font],
    main: Source,
}

impl PdfWorld {
    fn new(template: String, data_json: String) -> Self {
        // Expose the request's JSON (as a string) via `sys.inputs.data`.
        let mut inputs = Dict::new();
        inputs.insert(Str::from("data"), Value::Str(Str::from(data_json)));
        let library = Library::builder().with_inputs(inputs).build();

        let cache = font_cache();
        PdfWorld {
            library: LazyHash::new(library),
            book: LazyHash::new(cache.book.clone()),
            fonts: &cache.fonts,
            main: Source::detached(template),
        }
    }
}

impl World for PdfWorld {
    fn library(&self) -> &LazyHash<Library> {
        &self.library
    }

    fn book(&self) -> &LazyHash<FontBook> {
        &self.book
    }

    fn main(&self) -> FileId {
        self.main.id()
    }

    fn source(&self, id: FileId) -> FileResult<Source> {
        if id == self.main.id() {
            Ok(self.main.clone())
        } else {
            Err(FileError::NotFound(id.vpath().get_without_slash().into()))
        }
    }

    fn file(&self, id: FileId) -> FileResult<Bytes> {
        Err(FileError::NotFound(id.vpath().get_without_slash().into()))
    }

    fn font(&self, index: usize) -> Option<Font> {
        self.fonts.get(index).cloned()
    }

    fn today(&self, _offset: Option<Duration>) -> Option<Datetime> {
        None
    }
}

/// Format Typst diagnostics into a single human-readable string.
fn format_diagnostics(diags: &[SourceDiagnostic]) -> String {
    if diags.is_empty() {
        return "typst compilation failed".to_string();
    }
    diags
        .iter()
        .map(|d| d.message.to_string())
        .collect::<Vec<_>>()
        .join("; ")
}

/// Compile `template` (with `data_json` injected as `sys.inputs.data`) to PDF.
/// Pure/synchronous and CPU-bound — callers should run it off the async runtime
/// (see [`render`]).
pub fn compile_to_pdf(template: String, data_json: String) -> Result<Vec<u8>, RenderError> {
    let world = PdfWorld::new(template, data_json);
    let Warned { output, warnings } = typst::compile(&world);
    let document = output.map_err(|errs| {
        RenderError::bad_request(format!(
            "typst compile error: {}",
            format_diagnostics(&errs)
        ))
    })?;
    if !warnings.is_empty() {
        tracing::debug!("typst warnings: {}", format_diagnostics(&warnings));
    }
    // `document` infers to `typst_layout::PagedDocument` from this call.
    typst_pdf::pdf(&document, &typst_pdf::PdfOptions::default()).map_err(|errs| {
        RenderError::internal(format!("typst pdf export: {}", format_diagnostics(&errs)))
    })
}

/// Async wrapper: validate the request and run the CPU-bound compile on a
/// blocking thread so it never stalls the async runtime.
pub async fn render(req: &RenderRequest) -> Result<Vec<u8>, RenderError> {
    let template = req
        .template
        .clone()
        .ok_or_else(|| RenderError::bad_request("typst backend requires `template`"))?;
    let data_json = match &req.data {
        Some(v) => serde_json::to_string(v)
            .map_err(|e| RenderError::bad_request(format!("`data` is not serializable: {e}")))?,
        None => "null".to_string(),
    };
    tokio::task::spawn_blocking(move || compile_to_pdf(template, data_json))
        .await
        .map_err(|e| RenderError::internal(format!("typst render task failed: {e}")))?
}
