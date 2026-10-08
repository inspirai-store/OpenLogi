//! Release selection and verification shared by the GUI and update probe.

use gpui_updater::{EngineConfig, StaticManifestSource, Verification, Version};

const MANIFEST_URL: &str = match option_env!("OPENLOGI_UPDATE_MANIFEST_URL") {
    Some(url) => url,
    None => "https://github.com/inspirai-store/OpenLogi/releases/latest/download/latest.json",
};

const PUBLIC_KEY: &str = match option_env!("OPENLOGI_UPDATE_MINISIGN_PUBLIC_KEY") {
    Some(key) => key,
    None => include_str!("../../../../../packaging/windows/update.pub"),
};

pub(crate) fn new() -> StaticManifestSource {
    StaticManifestSource::new(MANIFEST_URL)
        .os(std::env::consts::OS)
        .arch(match std::env::consts::ARCH {
            "aarch64" => "arm64",
            arch => arch,
        })
        .format(match std::env::consts::OS {
            "macos" => "dmg",
            "windows" => "msi",
            _ => "tar.gz",
        })
}

pub(crate) fn config(version: Version) -> EngineConfig {
    let config = EngineConfig::new(version)
        .verification(Verification::Strict)
        .minisign_public_key(PUBLIC_KEY.trim());
    #[cfg(target_os = "windows")]
    let config = if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
        // A portable copy must relaunch the installed GUI after the MSI runs.
        config.install_root(
            std::path::PathBuf::from(local_app_data)
                .join("Programs")
                .join(openlogi_core::brand::WINDOWS_INSTALL_DIRECTORY)
                .join("OpenLogi.exe"),
        )
    } else {
        config
    };
    config
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_updater::{Asset, Error, Release, UpdateEngine, UpdateSource};

    struct Fixture(Release);
    impl UpdateSource for Fixture {
        fn fetch_latest(&self) -> Result<Release, Error> {
            Ok(self.0.clone())
        }
    }

    fn check(release: &Release) -> Result<Option<Release>, Error> {
        UpdateEngine::new(Fixture(release.clone()), config(Version::new(0, 8, 11))).check()
    }

    #[test]
    fn unsigned_release_is_refused_before_download() {
        let mut release = Release {
            version: Version::new(0, 8, 12),
            notes: None,
            asset: Asset {
                name: "update.msi".into(),
                url: "https://example.test/update.msi".into(),
                size: 1,
            },
            sha256: Some("a".repeat(64)),
            signature: None,
            signature_url: None,
        };
        check(&release).unwrap_err();
        release.signature_url = Some("https://example.test/update.msi.minisig".into());
        check(&release).unwrap().unwrap();
        release.sha256 = None;
        check(&release).unwrap_err();
    }
}
