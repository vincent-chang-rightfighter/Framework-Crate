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

        // Windows PE metadata shown in Explorer / file properties.
        res.set("ProductName", "Framework Crate");
        res.set("FileDescription", "Framework Crate");
        res.set("CompanyName", "Vincent Chang");
        res.set("LegalCopyright", "Copyright (c) 2026 Vincent Chang");
        res.set("ProductVersion", env!("CARGO_PKG_VERSION"));
        res.set("FileVersion", env!("CARGO_PKG_VERSION"));
        res.set("OriginalFilename", "framework-crate.exe");

        // Request administrator privileges via UAC manifest (release only).
        // Debug builds use asInvoker so `cargo test` works without elevation.
        let exec_level = if env::var("PROFILE").unwrap_or_default() == "release" {
            "requireAdministrator"
        } else {
            "asInvoker"
        };
        let manifest = format!(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="{}" uiAccess="false"/>
      </requestedPrivileges>
    </security>
  </trustInfo>
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
    <application>
      <supportedOS Id="{{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}}"/>
    </application>
  </compatibility>
</assembly>
"#, exec_level);
        res.set_manifest(&manifest);

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
