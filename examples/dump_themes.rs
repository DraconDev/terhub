use std::fs;
use terhub::tray::{icon_legend, IconState};

fn main() -> anyhow::Result<()> {
    let dir = std::path::Path::new("/tmp/theme_icons");
    fs::create_dir_all(dir)?;
    // Render one SVG per (theme, state) by reusing the tray's own
    // renderer through the legend's colour + a raw SVG template.
    for theme in [
        "rose-pine",
        "everforest",
        "kanagawa",
        "ayu",
        "nightfly",
        "palenight",
        "github",
        "github-light",
        "onehalf",
        "onehalf-light",
    ] {
        let rows = icon_legend(theme);
        let tdir = dir.join(theme);
        fs::create_dir_all(&tdir)?;
        for r in &rows {
            let svg = format!(
                "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"24\" height=\"24\" viewBox=\"0 0 24 24\">\
                 <path d=\"M4.6 6.4 L19.4 6.4 L12 19.4 Z\" fill=\"{}\" stroke=\"{}\" stroke-width=\"1.2\" stroke-linejoin=\"round\"/></svg>",
                r.hex, r.hex
            );
            fs::write(tdir.join(format!("{:?}.svg", r.state)), svg)?;
        }
    }
    println!("done");
    Ok(())
}
