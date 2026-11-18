//! An incremental Typst compiler for one project, fed file by file.
//!
//! [`LiveCompiler`] is the interface the browser preview and the server export
//! share. A host writes each project file once and then only the files that
//! change. The compiler owns its Typst world for its whole life, so Typst's
//! memoized results survive between compiles. A compile keeps the laid-out
//! document and reports a fingerprint per page, so a preview re-renders only
//! the pages whose fingerprint changed.
//!
//! The compiler holds no font data of its own. The host passes the catalogue
//! of fonts it can supply, and the compiler reports in
//! [`Compiled::missing_fonts`] each font a compile asked for that the host has
//! not yet given it with [`LiveCompiler::add_font`]. The host loads those
//! fonts and compiles again.
//!
//! Packages work the same way. A compile that imports a package the host
//! has not given with [`LiveCompiler::add_package`] fails with a problem
//! that names it, and lists it in [`Compiled::missing_packages`].

use std::collections::{BTreeSet, HashMap};
use std::sync::Mutex;

use typst::diag::{FileError, FileResult, PackageError, Severity, SourceDiagnostic};
use typst::foundations::{Bytes, Datetime};
use typst::syntax::package::PackageSpec;
use typst::syntax::{FileId, RootedPath, Source, VirtualPath, VirtualRoot};
use typst::text::{Font, FontBook};
use typst::utils::LazyHash;
use typst::{Library, LibraryExt, World, WorldExt};
use typst_layout::PagedDocument;

pub use typst::text::FontInfo;

/// One file of a project: Typst or other text, or binary bytes (images, data).
pub enum FileContents {
    /// UTF-8 text, such as a `.typ`, `.bib` or `.yml` file.
    Text(String),
    /// Raw bytes, such as a PNG figure.
    Bytes(Vec<u8>),
}

/// One font face the host can supply, as the build lists it in `fonts.json`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FontEntry {
    /// The file that holds the face, relative to the fonts directory.
    pub file: String,
    /// The face's index inside the file. Not zero only for collections.
    pub index: u32,
    /// What Typst needs to pick the face without loading it.
    pub info: FontInfo,
}

impl FontEntry {
    /// Lists every face in one font file.
    #[must_use]
    pub fn read(file: &str, data: &[u8]) -> Vec<Self> {
        (0..)
            .map_while(|index| {
                Some(Self {
                    file: file.to_owned(),
                    index,
                    info: FontInfo::new(data, index)?,
                })
            })
            .collect()
    }
}

/// A compile problem, with a range into the named file when Typst gave one.
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
    /// Catalogue indices of fonts this compile wanted and did not have. When
    /// this is not empty, the host adds those fonts and compiles again.
    pub missing_fonts: Vec<usize>,
    /// Packages this compile imported and did not have, as `@namespace/name:version`.
    pub missing_packages: Vec<String>,
}

/// The kind of PDF [`LiveCompiler::pdf`] writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PdfKind {
    /// Typst's normal output.
    #[default]
    Plain,
    /// PDF/A-2b, for archiving.
    A2b,
}

/// A long-lived compiler for one project.
pub struct LiveCompiler {
    world: ProjectWorld,
    entry: String,
    document: Option<PagedDocument>,
}

impl LiveCompiler {
    /// Creates a compiler rooted at `entry`, with no files yet. `fonts` is the
    /// catalogue of faces the host can supply; none of them is loaded.
    pub fn new(entry: &str, fonts: &[FontEntry]) -> Result<Self, String> {
        let main = file_id(entry)?;
        let book = FontBook::from_infos(fonts.iter().map(|f| f.info.clone()));
        Ok(Self {
            world: ProjectWorld {
                library: LazyHash::new(Library::builder().build()),
                book: LazyHash::new(book),
                main,
                files: HashMap::new(),
                fonts: fonts.iter().map(|_| None).collect(),
                faces: fonts.iter().map(|f| f.index).collect(),
                infos: fonts.iter().map(|f| f.info.clone()).collect(),
                missing: Mutex::new(BTreeSet::new()),
                packages: HashMap::new(),
                missing_packages: Mutex::new(BTreeSet::new()),
                today: None,
            },
            entry: entry.to_owned(),
            document: None,
        })
    }

    /// Writes or replaces one file. Writing identical text is free.
    pub fn write(&mut self, path: &str, contents: FileContents) {
        let Ok(id) = file_id(path) else { return };
        match contents {
            FileContents::Text(text) => {
                if let Some(Entry::Text(source, bytes)) = self.world.files.get_mut(&id) {
                    if source.text() != text {
                        source.replace(&text);
                        *bytes = Bytes::from_string(text);
                    }
                } else {
                    let source = Source::new(id, text.clone());
                    self.world
                        .files
                        .insert(id, Entry::Text(source, Bytes::from_string(text)));
                }
            }
            FileContents::Bytes(bytes) => {
                self.world.files.insert(id, Entry::Bytes(Bytes::new(bytes)));
            }
        }
    }

    /// Removes a file. Removing a missing file does nothing.
    pub fn remove(&mut self, path: &str) {
        if let Ok(id) = file_id(path) {
            self.world.files.remove(&id);
        }
    }

    /// Gives the compiler the bytes of catalogue font `index`.
    pub fn add_font(&mut self, index: usize, data: Vec<u8>) -> Result<(), String> {
        let face = *self.world.faces.get(index).ok_or("no such font")?;
        let font = Font::new(Bytes::new(data), face).ok_or("not a font")?;
        self.world.fonts[index] = Some(font);
        Ok(())
    }

    /// Adds every face of a font file that is not in the catalogue, such as
    /// a font file of the project or one the server administrator added,
    /// and loads it. Returns how many faces it added; 0 when the bytes are
    /// not a font Typst reads (TrueType or OpenType, or a collection).
    pub fn add_font_file(&mut self, data: Vec<u8>) -> usize {
        let bytes = Bytes::new(data);
        let mut added = 0;
        for font in Font::iter(bytes) {
            self.world.infos.push(font.info().clone());
            self.world.faces.push(font.index());
            self.world.fonts.push(Some(font));
            added += 1;
        }
        if added > 0 {
            self.world.book = LazyHash::new(FontBook::from_infos(self.world.infos.iter().cloned()));
        }
        added
    }

    /// Gives the compiler the files of a package, as paths inside the
    /// package and their bytes. `spec` is `@namespace/name:version`.
    pub fn add_package(&mut self, spec: &str, files: Vec<(String, Vec<u8>)>) -> Result<(), String> {
        let spec: PackageSpec = spec.parse().map_err(|e| format!("{spec}: {e}"))?;
        let mut entries = HashMap::new();
        for (path, data) in files {
            let vpath = VirtualPath::new(&path).map_err(|e| format!("{path}: {e}"))?;
            let id = RootedPath::new(VirtualRoot::Package(spec.clone()), vpath).intern();
            let entry = match String::from_utf8(data) {
                Ok(text)
                    if std::path::Path::new(&path)
                        .extension()
                        .is_some_and(|e| e.eq_ignore_ascii_case("typ")) =>
                {
                    Entry::Text(Source::new(id, text.clone()), Bytes::from_string(text))
                }
                Ok(text) => Entry::Bytes(Bytes::from_string(text)),
                Err(e) => Entry::Bytes(Bytes::new(e.into_bytes())),
            };
            entries.insert(id, entry);
        }
        self.world.packages.insert(spec, entries);
        Ok(())
    }

    /// Sets the date `datetime.today()` returns. Without it, that call fails.
    pub fn set_today(&mut self, year: i32, month: u8, day: u8) {
        self.world.today = Datetime::from_ymd(year, month, day);
    }

    /// Compiles the entry file. On failure the previous layout is kept, so a
    /// preview can go on showing the last good pages next to the problems.
    pub fn compile(&mut self) -> Compiled {
        self.world.missing.lock().expect("missing fonts").clear();
        self.world
            .missing_packages
            .lock()
            .expect("missing packages")
            .clear();
        let result = typst::compile::<PagedDocument>(&self.world);
        typst::comemo::evict(10);
        let world = &self.world;
        let mut problems: Vec<Problem> = result
            .warnings
            .iter()
            .map(|diag| world.problem(diag))
            .collect();
        let ok = match result.output {
            Ok(document) => {
                self.document = Some(document);
                true
            }
            Err(errors) => {
                problems.extend(errors.iter().map(|diag| world.problem(diag)));
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
        let missing_fonts = std::mem::take(&mut *world.missing.lock().expect("missing fonts"))
            .into_iter()
            .collect();
        let missing_packages =
            std::mem::take(&mut *world.missing_packages.lock().expect("missing packages"))
                .into_iter()
                .collect();
        Compiled {
            pages,
            sizes,
            problems,
            ok,
            missing_fonts,
            missing_packages,
        }
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

    /// Exports the last good layout as PDF of the given kind. A PDF/A file
    /// is dated with the date set by [`LiveCompiler::set_today`], at
    /// midnight UTC, so the same files on the same day give the same bytes.
    pub fn pdf(&self, kind: PdfKind) -> Result<Vec<u8>, String> {
        let document = self.document.as_ref().ok_or("nothing compiled yet")?;
        let join = |errors: &[_]| {
            errors
                .iter()
                .map(|error: &SourceDiagnostic| error.message.to_string())
                .collect::<Vec<_>>()
                .join("; ")
        };
        let mut options = typst_pdf::PdfOptions::default();
        if kind == PdfKind::A2b {
            options.standards = typst_pdf::PdfStandards::new(&[typst_pdf::PdfStandard::A_2b])
                .map_err(|e| e.message().to_string())?;
            options.timestamp = self
                .world
                .today
                .and_then(|d| Datetime::from_ymd_hms(d.year()?, d.month()?, d.day()?, 0, 0, 0))
                .map(typst_pdf::Timestamp::new_utc);
        }
        typst_pdf::pdf(document, &options).map_err(|errors| join(&errors))
    }

    /// The entry file this compiler is rooted at.
    #[must_use]
    pub fn entry(&self) -> &str {
        &self.entry
    }
}

fn file_id(path: &str) -> Result<FileId, String> {
    let vpath = VirtualPath::new(path).map_err(|e| format!("{path}: {e}"))?;
    Ok(RootedPath::new(VirtualRoot::Project, vpath).intern())
}

enum Entry {
    /// A text file: parsed for `#include` and `#import`, raw for `read()`.
    Text(Source, Bytes),
    Bytes(Bytes),
}

/// The Typst world: the project's files in memory and the fonts loaded so far.
struct ProjectWorld {
    library: LazyHash<Library>,
    book: LazyHash<FontBook>,
    main: FileId,
    files: HashMap<FileId, Entry>,
    fonts: Vec<Option<Font>>,
    faces: Vec<u32>,
    infos: Vec<FontInfo>,
    missing: Mutex<BTreeSet<usize>>,
    packages: HashMap<PackageSpec, HashMap<FileId, Entry>>,
    missing_packages: Mutex<BTreeSet<String>>,
    today: Option<Datetime>,
}

impl ProjectWorld {
    fn entry(&self, id: FileId) -> FileResult<&Entry> {
        let rooted = id.get();
        let not_found = || FileError::NotFound(rooted.vpath().get_without_slash().into());
        if let VirtualRoot::Package(spec) = rooted.root() {
            let Some(files) = self.packages.get(spec) else {
                self.missing_packages
                    .lock()
                    .expect("missing packages")
                    .insert(spec.to_string());
                return Err(FileError::Package(PackageError::Other(Some(
                    format!(
                        "package {spec} is not available: the server could not find or \
                         download it, or this device has no copy of it while offline"
                    )
                    .into(),
                ))));
            };
            return files.get(&id).ok_or_else(not_found);
        }
        self.files.get(&id).ok_or_else(not_found)
    }

    fn problem(&self, diag: &SourceDiagnostic) -> Problem {
        let severity = match diag.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
        };
        let id = diag.span.id();
        let text = id.and_then(|id| match self.entry(id) {
            Ok(Entry::Text(source, _)) => Some(source.text()),
            _ => None,
        });
        let range = self.range(diag.span);
        let to_utf16 =
            |byte: usize| text.map(|text| text[..byte.min(text.len())].encode_utf16().count());
        Problem {
            severity,
            message: diag.message.to_string(),
            path: id.map(|id| {
                let rooted = id.get();
                let path = rooted.vpath().get_without_slash();
                match rooted.root() {
                    VirtualRoot::Project => path.to_owned(),
                    VirtualRoot::Package(spec) => format!("{spec}/{path}"),
                }
            }),
            start: range.as_ref().and_then(|r| to_utf16(r.start)),
            end: range.as_ref().and_then(|r| to_utf16(r.end)),
        }
    }
}

impl World for ProjectWorld {
    fn library(&self) -> &LazyHash<Library> {
        &self.library
    }

    fn book(&self) -> &LazyHash<FontBook> {
        &self.book
    }

    fn main(&self) -> FileId {
        self.main
    }

    fn source(&self, id: FileId) -> FileResult<Source> {
        match self.entry(id)? {
            Entry::Text(source, _) => Ok(source.clone()),
            Entry::Bytes(_) => Err(FileError::NotSource),
        }
    }

    fn file(&self, id: FileId) -> FileResult<Bytes> {
        match self.entry(id)? {
            Entry::Text(_, bytes) | Entry::Bytes(bytes) => Ok(bytes.clone()),
        }
    }

    fn font(&self, index: usize) -> Option<Font> {
        let font = self.fonts.get(index)?.clone();
        if font.is_none() {
            self.missing.lock().expect("missing fonts").insert(index);
        }
        font
    }

    fn today(&self, _offset: Option<typst::foundations::Duration>) -> Option<Datetime> {
        self.today
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalogue() -> (Vec<FontEntry>, Vec<&'static [u8]>) {
        let mut entries = Vec::new();
        let mut data = Vec::new();
        for (n, font) in typst_assets::fonts().enumerate() {
            for entry in FontEntry::read(&format!("{n}"), font) {
                entries.push(entry);
                data.push(font);
            }
        }
        (entries, data)
    }

    /// A compiler that loads each font the first time a compile asks for it,
    /// the way the browser worker does.
    struct Host {
        compiler: LiveCompiler,
        data: Vec<&'static [u8]>,
        loaded: usize,
    }

    impl Host {
        fn new() -> Self {
            let (entries, data) = catalogue();
            Self {
                compiler: LiveCompiler::new("main.typ", &entries).unwrap(),
                data,
                loaded: 0,
            }
        }

        fn text(&mut self, path: &str, text: &str) {
            self.compiler
                .write(path, FileContents::Text(text.to_owned()));
        }

        fn compile(&mut self) -> Compiled {
            loop {
                let result = self.compiler.compile();
                if result.missing_fonts.is_empty() {
                    return result;
                }
                for index in result.missing_fonts {
                    self.compiler
                        .add_font(index, self.data[index].to_vec())
                        .unwrap();
                    self.loaded += 1;
                }
            }
        }
    }

    #[test]
    fn fonts_load_only_when_asked_for() {
        let mut host = Host::new();
        host.text("main.typ", "Hello");
        let first = host.compiler.compile();
        assert!(!first.missing_fonts.is_empty());
        assert!(first.missing_fonts.len() < host.data.len());
        assert!(host.compile().ok);
        assert!(host.loaded < host.data.len(), "loaded every font");
        assert!(host.compiler.compile().missing_fonts.is_empty());
    }

    #[test]
    fn unchanged_pages_keep_their_fingerprints() {
        let mut host = Host::new();
        host.text("main.typ", "= One\nfirst\n#pagebreak()\n= Two\nsecond");
        let before = host.compile();
        assert!(before.ok);
        assert_eq!(before.pages.len(), 2);
        host.text(
            "main.typ",
            "= One\nfirst\n#pagebreak()\n= Two\nsecond, edited",
        );
        let after = host.compile();
        assert_eq!(before.pages[0], after.pages[0]);
        assert_ne!(before.pages[1], after.pages[1]);
        let (width, height, pixels) = host.compiler.page_rgba(1, 1.0).unwrap();
        assert_eq!(pixels.len(), (width * height * 4) as usize);
    }

    #[test]
    fn a_failed_compile_keeps_the_last_good_pages() {
        let mut host = Host::new();
        host.text("main.typ", "hello");
        assert!(host.compile().ok);
        host.text("main.typ", "a\n#let x = (");
        let failed = host.compile();
        assert!(!failed.ok);
        assert_eq!(failed.pages.len(), 1);
        let problem = &failed.problems[0];
        assert_eq!(problem.path.as_deref(), Some("main.typ"));
        assert!(problem.start.is_some());
    }

    #[test]
    fn reads_bibliography_and_images_from_written_files() {
        let mut host = Host::new();
        host.text(
            "refs.bib",
            "@book{knuth1984, title={The TeXbook}, author={Knuth, Donald}, year={1984}, publisher={Addison-Wesley}}",
        );
        host.compiler.write(
            "fig.svg",
            FileContents::Bytes(
                br#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><rect width="10" height="10"/></svg>"#.to_vec(),
            ),
        );
        host.text(
            "main.typ",
            "#image(\"fig.svg\", width: 2cm)\nSee @knuth1984.\n#bibliography(\"refs.bib\")",
        );
        let result = host.compile();
        assert!(result.ok, "{:?}", result.problems);
        assert!(
            host.compiler
                .pdf(PdfKind::Plain)
                .unwrap()
                .starts_with(b"%PDF")
        );
    }

    #[test]
    fn today_is_what_the_host_set() {
        let mut host = Host::new();
        host.text("main.typ", "#datetime.today().display()");
        assert!(!host.compile().ok);
        host.compiler.set_today(2026, 1, 2);
        assert!(host.compile().ok);
    }

    #[test]
    fn removed_files_are_gone() {
        let mut host = Host::new();
        host.text("part.typ", "part");
        host.text("main.typ", "#include \"part.typ\"");
        assert!(host.compile().ok);
        host.compiler.remove("part.typ");
        assert!(!host.compile().ok);
    }
}

#[cfg(test)]
mod packages_fonts_and_archival_pdf {
    use super::*;

    /// A compiler with every bundled font loaded.
    fn compiler() -> LiveCompiler {
        let mut fonts = Vec::new();
        let mut data = Vec::new();
        for (i, bytes) in typst_assets::fonts().enumerate() {
            for entry in FontEntry::read(&format!("{i}.ttf"), bytes) {
                fonts.push(entry);
                data.push(bytes.to_vec());
            }
        }
        let mut c = LiveCompiler::new("main.typ", &fonts).unwrap();
        for (i, bytes) in data.into_iter().enumerate() {
            c.add_font(i, bytes).unwrap();
        }
        c.set_today(2026, 11, 18);
        c
    }

    #[test]
    fn a_missing_package_is_named_and_a_given_one_is_used() {
        let mut c = compiler();
        c.write(
            "main.typ",
            FileContents::Text("#import \"@preview/shells:0.1.0\": whelk\n#whelk".into()),
        );
        let first = c.compile();
        assert!(!first.ok);
        assert_eq!(first.missing_packages, ["@preview/shells:0.1.0"]);
        assert!(
            first.problems[0].message.contains("@preview/shells:0.1.0"),
            "{:?}",
            first.problems
        );
        c.add_package(
            "@preview/shells:0.1.0",
            vec![
                (
                    "typst.toml".into(),
                    b"[package]\nname = \"shells\"\nversion = \"0.1.0\"\nentrypoint = \"lib.typ\"\n"
                        .to_vec(),
                ),
                ("lib.typ".into(), b"#let whelk = [Whelk]".to_vec()),
            ],
        )
        .unwrap();
        let second = c.compile();
        assert!(second.ok, "{:?}", second.problems);
        assert!(second.missing_packages.is_empty());
        assert!(c.add_package("not a spec", vec![]).is_err());
    }

    #[test]
    fn a_font_file_adds_its_faces() {
        let mut c = LiveCompiler::new("main.typ", &[]).unwrap();
        assert_eq!(c.add_font_file(b"not a font".to_vec()), 0);
        let font = typst_assets::fonts().next().unwrap().to_vec();
        assert!(c.add_font_file(font) >= 1);
        c.write("main.typ", FileContents::Text("Periwinkle".into()));
        let result = c.compile();
        assert!(result.ok, "{:?}", result.problems);
        assert!(result.missing_fonts.is_empty());
    }

    #[test]
    fn a_pdf_a_is_dated_by_today_and_repeatable() {
        let mut c = compiler();
        c.write("main.typ", FileContents::Text("= Rock pools".into()));
        assert!(c.compile().ok);
        let a = c.pdf(PdfKind::A2b).unwrap();
        assert_eq!(a, c.pdf(PdfKind::A2b).unwrap());
        let text = String::from_utf8_lossy(&a);
        assert!(text.contains("pdfaid:part"), "no PDF/A identification");
        assert!(text.contains("2026-11-18"), "not dated by today");
        let plain = c.pdf(PdfKind::Plain).unwrap();
        assert!(!String::from_utf8_lossy(&plain).contains("pdfaid:part"));
    }
}
