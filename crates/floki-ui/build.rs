//! Embeds the Floki icon in `floki.exe` (Explorer, taskbar, Alt+Tab), drawn
//! by `src/icon_art.rs` and written as a multi-size `.ico` at build time,
//! plus version and author details (Properties → Details).

#[path = "src/icon_art.rs"]
mod icon_art;
#[path = "../../build-support/version_rc.rs"]
mod version_rc;

use std::path::PathBuf;

/// Sizes Windows asks for across DPI settings and Explorer views.
const SIZES: [u32; 8] = [16, 20, 24, 32, 40, 48, 64, 256];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/icon_art.rs");
    println!("cargo:rerun-if-changed=../../build-support/version_rc.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    let ico = out.join("floki.ico");
    std::fs::write(&ico, ico_file(&SIZES)).expect("write floki.ico");
    let rc = out.join("floki.rc");
    let ico_path = ico.to_string_lossy().replace('\\', "\\\\");
    let version = version_rc::version_rc("floki", "Floki search window");
    std::fs::write(&rc, format!("1 ICON \"{ico_path}\"\n{version}")).expect("write floki.rc");
    embed_resource::compile(&rc, embed_resource::NONE)
        .manifest_optional()
        .expect("compile the icon resource");
}

/// An `.ico` holding one 32-bit BMP image per size.
fn ico_file(sizes: &[u32]) -> Vec<u8> {
    let images: Vec<Vec<u8>> = sizes
        .iter()
        .map(|&s| bmp_image(s, &icon_art::render(s)))
        .collect();
    let mut out = Vec::new();
    out.extend(0u16.to_le_bytes()); // reserved
    out.extend(1u16.to_le_bytes()); // type: icon
    out.extend((sizes.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * sizes.len() as u32;
    for (&s, img) in sizes.iter().zip(&images) {
        let dim = if s >= 256 { 0 } else { s as u8 }; // 0 means 256
        out.extend([dim, dim, 0, 0]);
        out.extend(1u16.to_le_bytes()); // planes
        out.extend(32u16.to_le_bytes()); // bits per pixel
        out.extend((img.len() as u32).to_le_bytes());
        out.extend(offset.to_le_bytes());
        offset += img.len() as u32;
    }
    for img in images {
        out.extend(img);
    }
    out
}

/// ICO-flavoured BMP: header with doubled height, bottom-up BGRA rows, then
/// a 1-bit AND mask (set where fully transparent).
fn bmp_image(s: u32, rgba: &[u8]) -> Vec<u8> {
    let mask_row = s.div_ceil(32) * 4;
    let mut b = Vec::new();
    b.extend(40u32.to_le_bytes());
    b.extend((s as i32).to_le_bytes());
    b.extend((2 * s as i32).to_le_bytes());
    b.extend(1u16.to_le_bytes());
    b.extend(32u16.to_le_bytes());
    b.extend(0u32.to_le_bytes()); // BI_RGB
    b.extend((s * s * 4 + mask_row * s).to_le_bytes());
    b.extend([0u8; 16]); // resolution, palette counts
    for y in (0..s).rev() {
        for x in 0..s {
            let i = ((y * s + x) * 4) as usize;
            b.extend([rgba[i + 2], rgba[i + 1], rgba[i], rgba[i + 3]]);
        }
    }
    for y in (0..s).rev() {
        let mut row = vec![0u8; mask_row as usize];
        for x in 0..s {
            if rgba[((y * s + x) * 4 + 3) as usize] == 0 {
                row[(x / 8) as usize] |= 0x80 >> (x % 8);
            }
        }
        b.extend(row);
    }
    b
}
