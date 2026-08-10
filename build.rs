fn main() {
    if cfg!(target_os = "windows") {
        println!("cargo:rerun-if-changed=assets/app.ico");
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/app.ico");
        res.compile().expect("Failed to compile Windows resource");
    }
}
