//! Scanner build identity shared by runtime evidence and cache fingerprints.

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EngineIdentity {
    pub(crate) engine_version: String,
    pub(crate) engine_build: String,
    pub(crate) target_triple: String,
}

pub(crate) fn engine_identity() -> EngineIdentity {
    EngineIdentity {
        engine_version: env!("CARGO_PKG_VERSION").to_string(),
        engine_build: crate::ENGINE_BUILD_IDENTITY.to_string(),
        target_triple: target_triple(),
    }
}

fn target_triple() -> String {
    let arch = std::env::consts::ARCH;
    if cfg!(all(target_os = "windows", target_env = "msvc")) {
        format!("{arch}-pc-windows-msvc")
    } else if cfg!(target_os = "windows") {
        format!("{arch}-pc-windows-gnu")
    } else if cfg!(target_os = "macos") {
        format!("{arch}-apple-darwin")
    } else {
        format!("{arch}-unknown-linux-gnu")
    }
}
