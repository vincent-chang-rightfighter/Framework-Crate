use std::env;
use std::fs::File;
use std::io::BufWriter;
use std::io::Write;
use std::path::Path;

fn main() {
    // Expose the pinned framework_lib version so the UI (views/cpu_power.rs /
    // app/cpu_power.rs) never shows a stale hardcoded version after a dependency bump.
    println!("cargo:rerun-if-changed=Cargo.lock");
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let lock_path = Path::new(&manifest_dir).join("Cargo.lock");
    let lock = std::fs::read_to_string(&lock_path).expect("Failed to read Cargo.lock");
    let mut current_name = String::new();
    let mut found = false;
    for line in lock.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("name = ") {
            current_name = rest.trim_matches('"').to_string();
        } else if let Some(rest) = line.strip_prefix("version = ")
            && current_name == "framework_lib"
        {
            println!(
                "cargo:rustc-env=FRAMEWORK_LIB_VERSION={}",
                rest.trim_matches('"')
            );
            found = true;
            break;
        }
    }
    if !found {
        println!("cargo:warning=framework_lib version not found in Cargo.lock");
        println!("cargo:rustc-env=FRAMEWORK_LIB_VERSION=unknown");
    }

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
        let manifest = format!(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
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
"#,
            exec_level
        );
        res.set_manifest(&manifest);

        res.compile().expect("Failed to compile Windows resource");
    }

    // Decode app.png to RGBA bytes for iced window icon (avoids pulling in the
    // full `image` crate with its ~60 transitive dependencies).
    // Anchored at CARGO_MANIFEST_DIR so --manifest-path builds from elsewhere work.
    println!("cargo:rerun-if-changed=assets/app.png");
    let png_path = Path::new(&manifest_dir).join("assets/app.png");
    let decoder = png::Decoder::new(File::open(&png_path).expect("Failed to open assets/app.png"));
    let mut reader = decoder.read_info().expect("Failed to read PNG info");
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader
        .next_frame(&mut buf)
        .expect("Failed to decode PNG frame");
    // Only 8-bit images are handled below. info.bit_depth is the *output* depth,
    // and the png crate reports ColorType::Rgb for 16-bit RGB too, where a pixel
    // is 6 bytes rather than 3. The expansion below would then run at double the
    // pixel count and emit a garbled icon instead of failing the build.
    // Interlaced input needs no check: next_frame loops every Adam7 pass and
    // expands them into the buffer itself.
    assert_eq!(
        info.bit_depth,
        png::BitDepth::Eight,
        "app.png must be 8-bit, got {:?}",
        info.bit_depth
    );
    // gracefully handle non-RGBA (e.g. RGB) by expanding to RGBA instead of assert-failing build
    let buf = if info.color_type != png::ColorType::Rgba {
        if info.color_type == png::ColorType::Rgb {
            // Expand RGB -> RGBA (alpha=0xFF)
            let mut rgba = Vec::with_capacity((info.width * info.height * 4) as usize);
            for chunk in buf.as_chunks::<3>().0 {
                rgba.extend_from_slice(chunk);
                rgba.push(0xFF);
            }
            rgba
        } else {
            panic!(
                "app.png must be RGBA or RGB, got {:?} — convert to RGBA",
                info.color_type
            );
        }
    } else {
        buf
    };
    // Catches any mismatch between the decoded layout and the width/height that
    // gets written into the generated constants.
    assert_eq!(
        buf.len(),
        (info.width * info.height * 4) as usize,
        "app.png decoded to {} bytes, expected width*height*4",
        buf.len()
    );

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
