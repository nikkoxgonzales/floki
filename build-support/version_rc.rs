//! Windows VERSIONINFO resource shared by the build scripts of `floki`,
//! `flokid` and `flk`: what Explorer shows under Properties → Details
//! (product, version, author, copyright, project URL).

/// Year in the copyright line.
const COPYRIGHT_YEAR: u16 = 2026;

/// `.rc` source for a VERSIONINFO block describing `exe`, filled from the
/// package metadata Cargo passes to build scripts.
pub fn version_rc(exe: &str, description: &str) -> String {
    let version = env("CARGO_PKG_VERSION");
    let author = env("CARGO_PKG_AUTHORS");
    // "Name <email>" → "Name"; the email stays in Cargo.toml.
    let author = author.split('<').next().unwrap_or("").trim().to_owned();
    let repo = env("CARGO_PKG_REPOSITORY");
    let mut parts = version
        .split(['.', '-', '+'])
        .map(|p| p.parse::<u16>().unwrap_or(0));
    let numeric = format!(
        "{},{},{},0",
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0)
    );
    format!(
        r#"1 VERSIONINFO
FILEVERSION {numeric}
PRODUCTVERSION {numeric}
FILEOS 0x40004
FILETYPE 0x1
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904b0"
    BEGIN
      VALUE "CompanyName", "{author}"
      VALUE "FileDescription", "{description}"
      VALUE "FileVersion", "{version}"
      VALUE "InternalName", "{exe}"
      VALUE "LegalCopyright", "Copyright (C) {COPYRIGHT_YEAR} {author}. MIT License."
      VALUE "OriginalFilename", "{exe}.exe"
      VALUE "ProductName", "Floki"
      VALUE "ProductVersion", "{version}"
      VALUE "Comments", "{repo}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#
    )
}

fn env(key: &str) -> String {
    std::env::var(key).unwrap_or_default()
}
