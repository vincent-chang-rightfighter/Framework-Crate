# icon_gen

One-shot asset generator. Not part of the root build or CI; run manually
only when `assets/settings.svg` changes.

```powershell
cargo run --manifest-path tools/icon_gen/Cargo.toml -- <project-root>
```

Renders `assets/settings.svg` via resvg and writes `assets/app.png`
(256px window icon) and `assets/app.ico` (multi-size, 32bpp BMP entries
for older Shell paths).
