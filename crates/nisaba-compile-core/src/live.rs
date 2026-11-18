//! An incremental compiler for one project, fed file by file.
//!
//! [`LiveCompiler`] is the interface the browser preview and the server export
//! share. A host writes each project file once and then only the files that
//! change; Typst's `comemo` caches survive between compiles because the
//! universe lives as long as the compiler. A compile keeps the laid-out
//! document and reports a fingerprint per page, so a preview re-renders only
//! the pages whose fingerprint changed.

use std::collections::HashMap;

use tinymist_vfs::mock::MockWorkspace;
use tinymist_world::mock::{MockUniverse, MockWorkspaceWorldExt, MockWorldChangeExt};
use typst::diag::Severity;
use typst::{World, WorldExt};
use typst_layout::PagedDocument;

/// One file of a project: Typst or other text, or binary bytes (images, data).
pub enum FileContents {
    /// UTF-8 text, such as a `.typ`, `.bib` or `.yml` file.
    Text(String),
    /// Raw bytes, such as a PNG figure.
    Bytes(Vec<u8>),
}

/// A compile problem, with a byte range into the named file when Typst gave one.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Problem {
    /// `"error"` or `"warning"`.
    pub severity: &'static str,
    /// Typst's message.
    pub message: String,
    /// Project-relative path of the file the problem points into.
    pub path: Option<String>,
    /// Start of the range, as a UTF-16 offset, matching the editor.
    pub start: Option<usize>,
    /// End of the range, as a UTF-16 offset.
    pub end: Option<usize>,
}

/// What one compile produced.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Compiled {
    /// One fingerprint per page, in page order. Equal fingerprints mean equal pages.
    pub pages: Vec<String>,
    /// Page sizes in points, `[width, height]`, in page order.
    pub sizes: Vec<[f64; 2]>,
    /// Errors and warnings. When there are errors, `pages` is the last good layout.
    pub problems: Vec<Problem>,
    /// Whether this compile succeeded.
    pub ok: bool,
}

/// A long-lived compiler for one project.
pub struct LiveCompiler {
    workspace: MockWorkspace,
    universe: MockUniverse,
    entry: String,
    text: HashMap<String, String>,
    document: Option<PagedDocument>,
}

impl LiveCompiler {
    /// Creates a compiler rooted at `entry`, with no files yet.
    pub fn new(entry: &str) -> Result<Self, String> {
        let workspace = MockWorkspace::default_builder().file(entry, "").build();
        let universe = workspace
            .world(entry)
            .build_universe()
            .map_err(|error| format!("create Typst world: {error:?}"))?;
        Ok(Self {
            workspace,
            universe,
            entry: entry.to_owned(),
            text: HashMap::new(),
            document: None,
        })
    }

    /// Writes or replaces one file. Writing identical text is free.
    pub fn write(&mut self, path: &str, contents: FileContents) {
        let change = match contents {
            FileContents::Text(text) => {
                if self.text.get(path) == Some(&text) {
                    return;
                }
                let change = self.workspace.write_source(path, text.clone());
                self.text.insert(path.to_owned(), text);
                change
            }
            FileContents::Bytes(bytes) => {
                self.text.remove(path);
                self.workspace
                    .write_bytes(path, typst::foundations::Bytes::new(bytes))
            }
        };
        change.apply_to_universe(&mut self.universe);
    }

    /// Removes a file. Removing a missing file does nothing.
    pub fn remove(&mut self, path: &str) {
        self.text.remove(path);
        if let Ok(change) = self.workspace.remove(path) {
            change.apply_to_universe(&mut self.universe);
        }
    }

    /// Compiles the entry file. On failure the previous layout is kept, so a
    /// preview can go on showing the last good pages next to the problems.
    pub fn compile(&mut self) -> Compiled {
        let world = self.universe.snapshot();
        let result = typst::compile::<PagedDocument>(&world);
        let mut problems: Vec<Problem> = result
            .warnings
            .iter()
            .map(|diag| problem(&world, &self.text, diag))
            .collect();
        let ok = match result.output {
            Ok(document) => {
                self.document = Some(document);
                true
            }
            Err(errors) => {
                problems.extend(errors.iter().map(|diag| problem(&world, &self.text, diag)));
                false
            }
        };
        let (pages, sizes) = self.document.as_ref().map_or_else(Default::default, |doc| {
            doc.pages()
                .iter()
                .map(|page| {
                    let size = page.frame.size();
                    (
                        format!("{:032x}", typst::utils::hash128(page)),
                        [size.x.to_pt(), size.y.to_pt()],
                    )
                })
                .unzip()
        });
        Compiled {
            pages,
            sizes,
            problems,
            ok,
        }
    }

    /// Renders one page of the last good layout as SVG.
    #[must_use]
    pub fn page_svg(&self, index: usize) -> Option<String> {
        let page = self.document.as_ref()?.pages().get(index)?;
        Some(typst_svg::svg(page, &typst_svg::SvgOptions::default()))
    }

    /// Renders one page of the last good layout as RGBA pixels at `pixel_per_pt`.
    #[must_use]
    pub fn page_rgba(&self, index: usize, pixel_per_pt: f32) -> Option<(u32, u32, Vec<u8>)> {
        let page = self.document.as_ref()?.pages().get(index)?;
        let pixmap = typst_render::render(
            page,
            &typst_render::RenderOptions {
                pixel_per_pt: typst::utils::Scalar::new(f64::from(pixel_per_pt)),
                ..Default::default()
            },
        );
        Some((pixmap.width(), pixmap.height(), pixmap.take()))
    }

    /// Exports the last good layout as PDF.
    pub fn pdf(&self) -> Result<Vec<u8>, String> {
        let document = self.document.as_ref().ok_or("nothing compiled yet")?;
        typst_pdf::pdf(document, &typst_pdf::PdfOptions::default()).map_err(|errors| {
            errors
                .iter()
                .map(|error| error.message.to_string())
                .collect::<Vec<_>>()
                .join("; ")
        })
    }

    /// The entry file this compiler is rooted at.
    #[must_use]
    pub fn entry(&self) -> &str {
        &self.entry
    }
}

fn problem(
    world: &dyn World,
    text: &HashMap<String, String>,
    diag: &typst::diag::SourceDiagnostic,
) -> Problem {
    let severity = match diag.severity {
        Severity::Error => "error",
        Severity::Warning => "warning",
    };
    let id = diag.span.id();
    let path = id.map(|id| id.vpath().get_without_slash().to_owned());
    let range = world.range(diag.span);
    let to_utf16 = |byte: usize| {
        path.as_ref()
            .and_then(|p| text.get(p))
            .map(|source| source[..byte.min(source.len())].encode_utf16().count())
    };
    Problem {
        severity,
        message: diag.message.to_string(),
        start: range.as_ref().and_then(|r| to_utf16(r.start)),
        end: range.as_ref().and_then(|r| to_utf16(r.end)),
        path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unchanged_pages_keep_their_fingerprints() {
        let mut compiler = LiveCompiler::new("main.typ").unwrap();
        compiler.write(
            "main.typ",
            FileContents::Text("= One\nfirst\n#pagebreak()\n= Two\nsecond".into()),
        );
        let before = compiler.compile();
        assert!(before.ok);
        assert_eq!(before.pages.len(), 2);
        compiler.write(
            "main.typ",
            FileContents::Text("= One\nfirst\n#pagebreak()\n= Two\nsecond, edited".into()),
        );
        let after = compiler.compile();
        assert_eq!(before.pages[0], after.pages[0]);
        assert_ne!(before.pages[1], after.pages[1]);
        assert!(compiler.page_svg(1).unwrap().starts_with("<svg"));
    }

    #[test]
    fn a_failed_compile_keeps_the_last_good_pages() {
        let mut compiler = LiveCompiler::new("main.typ").unwrap();
        compiler.write("main.typ", FileContents::Text("hello".into()));
        assert!(compiler.compile().ok);
        compiler.write("main.typ", FileContents::Text("#let x = (".into()));
        let failed = compiler.compile();
        assert!(!failed.ok);
        assert_eq!(failed.pages.len(), 1);
        assert_eq!(failed.problems[0].path.as_deref(), Some("main.typ"));
    }

    #[test]
    fn reads_bibliography_and_images_from_written_files() {
        let mut compiler = LiveCompiler::new("main.typ").unwrap();
        compiler.write(
            "refs.bib",
            FileContents::Text(
                "@book{knuth1984, title={The TeXbook}, author={Knuth, Donald}, year={1984}, publisher={Addison-Wesley}}".into(),
            ),
        );
        compiler.write(
            "fig.svg",
            FileContents::Bytes(
                br#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><rect width="10" height="10"/></svg>"#.to_vec(),
            ),
        );
        compiler.write(
            "main.typ",
            FileContents::Text(
                "#image(\"fig.svg\", width: 2cm)\nSee @knuth1984.\n#bibliography(\"refs.bib\")"
                    .into(),
            ),
        );
        let result = compiler.compile();
        assert!(result.ok, "{:?}", result.problems);
        assert!(!compiler.pdf().unwrap().is_empty());
    }
}
