use std::env;
use std::fs::File;
use std::io::BufWriter;
use std::io::Write;
use std::path::Path;

fn main() {
    if cfg!(target_os = "windows") {
        println!("cargo:rerun-if-changed=assets/app.ico");
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/app.ico");
        res.compile().expect("Failed to compile Windows resource");
    }

    // Decode app.png to RGBA bytes for iced window icon (avoids pulling in the
    // full `image` crate with its ~60 transitive dependencies).
    println!("cargo:rerun-if-changed=assets/app.png");
    let decoder = png::Decoder::new(File::open("assets/app.png").expect("Failed to open assets/app.png"));
    let mut reader = decoder.read_info().expect("Failed to read PNG info");
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).expect("Failed to decode PNG frame");
    assert_eq!(info.color_type, png::ColorType::Rgba, "app.png must be RGBA");

    let out_dir = env::var("OUT_DIR").unwrap();
    let dest_path = Path::new(&out_dir).join("icon_rgba.rs");
    let mut file = BufWriter::new(File::create(dest_path).unwrap());

    // Write a const byte array and helper function.
    write!(file, "const ICON_RGBA: &[u8] = &[").unwrap();
    for (i, byte) in buf.iter().enumerate() {
        if i % 256 == 0 {
            write!(file, "\n    ").unwrap();
        }
        write!(file, "{:#04x}, ", byte).unwrap();
    }
    write!(file, "\n];\n").unwrap();
    write!(
        file,
        "const ICON_WIDTH: u32 = {};\nconst ICON_HEIGHT: u32 = {};\n",
        info.width, info.height
    )
    .unwrap();
}
