use std::fs;
use std::path::PathBuf;

use resvg::tiny_skia::Pixmap;
use resvg::usvg::{Options, Transform, Tree};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let project_root = args
        .get(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join(".."));
    let assets = project_root.join("assets");

    let svg_path = assets.join("settings.svg");
    let svg = fs::read_to_string(&svg_path).expect("read svg");
    let tree = Tree::from_str(&svg, &Options::default()).expect("parse svg");
    let base_w = tree.size().width();
    let base_h = tree.size().height();

    let sizes: [u32; 7] = [256, 128, 64, 48, 32, 24, 16];

    // Render each size. The largest size is kept as PNG (fully supported by
    // Windows); smaller sizes are stored as 32bpp BMP/DIB entries because
    // PNG-compressed small icons render with a black background in Task
    // Manager and older Shell paths.
    let mut entries: Vec<(u32, Vec<u8>)> = Vec::new(); // (size, payload)
    for size in sizes {
        let scale = (size as f32) / base_w.max(base_h);
        let mut pixmap = Pixmap::new(size, size).expect("pixmap");
        let transform = Transform::from_scale(scale, scale);
        resvg::render(&tree, transform, &mut pixmap.as_mut());
        let payload = if size == 256 {
            pixmap.encode_png().expect("encode png")
        } else {
            build_dib_entry(&pixmap)
        };
        println!("rendered {size}px ({} bytes)", payload.len());
        entries.push((size, payload));
    }

    // Largest PNG is the window icon.
    let app_png = &entries[0].1;
    fs::write(assets.join("app.png"), &app_png).expect("write app.png");
    println!("wrote app.png ({} bytes)", app_png.len());

    // Compose multi-size ICO.
    let ico = build_ico(&entries);
    fs::write(assets.join("app.ico"), &ico).expect("write app.ico");
    println!("wrote app.png and app.ico ({} bytes)", ico.len());
}

/// Encode a square pixmap as a 32bpp BMP/DIB image (bottom-up BGRA rows
/// plus a zero AND mask), as required by non-256px ICO entries.
fn build_dib_entry(pixmap: &Pixmap) -> Vec<u8> {
    let w = pixmap.width();
    let h = pixmap.height();
    let row_bytes = w * 4;
    let pixels = pixmap.data();
    let mut out = Vec::with_capacity(40 + row_bytes as usize * h as usize + row_bytes as usize * h as usize / 32);

    // BITMAPINFOHEADER
    out.extend_from_slice(&40u32.to_le_bytes()); // biSize
    out.extend_from_slice(&(w as i32).to_le_bytes()); // biWidth
    out.extend_from_slice(&((h as i32) * 2).to_le_bytes()); // biHeight (double for XOR+AND)
    out.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
    out.extend_from_slice(&32u16.to_le_bytes()); // biBitCount
    out.extend_from_slice(&0u32.to_le_bytes()); // biCompression = BI_RGB
    out.extend_from_slice(&0u32.to_le_bytes()); // biSizeImage
    out.extend_from_slice(&0i32.to_le_bytes()); // biXPelsPerMeter
    out.extend_from_slice(&0i32.to_le_bytes()); // biYPelsPerMeter
    out.extend_from_slice(&0u32.to_le_bytes()); // biClrUsed
    out.extend_from_slice(&0u32.to_le_bytes()); // biClrImportant

    // XOR data: bottom-up rows, BGRA (R,G,B swapped from resvg's RGBA).
    // Premultiplied alpha from resvg is unpremultiplied per pixel so the
    // icon renders correctly over any background.
    for y in (0..h).rev() {
        for x in 0..w {
            let idx = ((y * w + x) * 4) as usize;
            let a = pixels[idx + 3] as u32;
            let (r, g, b) = if a == 0 {
                (0, 0, 0)
            } else {
                (
                    (pixels[idx] as u32 * 255 / a) as u8,
                    (pixels[idx + 1] as u32 * 255 / a) as u8,
                    (pixels[idx + 2] as u32 * 255 / a) as u8,
                )
            };
            out.push(b);
            out.push(g);
            out.push(r);
            out.push(a as u8);
        }
    }

    // AND mask: all zeros. For 32bpp entries transparency is carried by the
    // alpha channel; a non-zero mask can render as a black frame in older
    // Shell/Task Manager code paths.
    let row_mask_bytes = (w as usize + 7) / 8;
    let row_padded = (row_mask_bytes + 3) / 4 * 4;
    let and_row = vec![0u8; row_padded];
    for _ in 0..h {
        out.extend_from_slice(&and_row);
    }
    out
}

fn build_ico(images: &[(u32, Vec<u8>)]) -> Vec<u8> {
    // ICONDIR header
    let mut out = Vec::new();
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved
    out.extend_from_slice(&1u16.to_le_bytes()); // type: 1 = icon
    out.extend_from_slice(&(images.len() as u16).to_le_bytes());

    let header_len = 6u32 + (16u32 * images.len() as u32);
    let mut offset = header_len;
    for (size, data) in images {
        let enc = if *size == 256 { 0 } else { *size as u8 };
        out.extend_from_slice(&[enc]);
        out.extend_from_slice(&[enc]);
        out.push(0); // palette
        out.push(0); // reserved
        out.extend_from_slice(&1u16.to_le_bytes()); // planes
        out.extend_from_slice(&32u16.to_le_bytes()); // bpp
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += data.len() as u32;
    }
    for (_, data) in images {
        out.extend_from_slice(data);
    }
    out
}