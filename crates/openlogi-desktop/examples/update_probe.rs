//! Exercise the GUI's exact release source without launching or installing it.
//! `update_probe 0.8.11 --download` verifies and stages the newer MSI only.

#[path = "../src/platform/updater/source.rs"]
mod source;

use gpui_updater::{UpdateEngine, Version};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let version = std::env::args()
        .nth(1)
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").into());
    let engine = UpdateEngine::new(source::new(), source::config(Version::parse(&version)?));
    if let Some(release) = engine.check()? {
        println!("Available: {} {}", release.version, release.asset.url);
        if std::env::args().any(|arg| arg == "--download") {
            let artifact = engine.download(&release, |_, _| {})?;
            let installed = engine.install(&artifact)?;
            println!("Verified: {}", artifact.display());
            println!("Staged: {:?}", installed.restart_path);
        }
    } else {
        println!("Up to date: {version}");
    }
    Ok(())
}
