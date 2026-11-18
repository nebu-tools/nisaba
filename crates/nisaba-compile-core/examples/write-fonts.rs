//! Writes Typst's bundled fonts as one file each, plus the catalogue
//! `fonts.json`, into the directory given as the only argument.
//!
//! Each file is named by a hash of its bytes, so the server can let browsers
//! cache it for a year. The build runs this before the web build.

use std::path::PathBuf;

use nisaba_compile_core::FontEntry;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = PathBuf::from(std::env::args().nth(1).ok_or("usage: write-fonts DIR")?);
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    std::fs::create_dir_all(&dir)?;
    let mut catalogue = Vec::new();
    for data in typst_assets::fonts() {
        let ext = if data.starts_with(b"OTTO") {
            "otf"
        } else {
            "ttf"
        };
        let file = format!("{:032x}.{ext}", typst::utils::hash128(data));
        std::fs::write(dir.join(&file), data)?;
        catalogue.extend(FontEntry::read(&file, data));
    }
    std::fs::write(dir.join("fonts.json"), serde_json::to_vec(&catalogue)?)?;
    println!("{} faces in {}", catalogue.len(), dir.display());
    Ok(())
}
