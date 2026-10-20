//! Icon, version info and manifest for the executables.

fn main() {
    let version = env!("CARGO_PKG_VERSION");
    let mut parts = version.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
    let (major, minor, patch) = (parts.next().unwrap_or(0), parts.next().unwrap_or(0), parts.next().unwrap_or(0));
    let macros = [
        format!("VERSION_MAJOR={major}"),
        format!("VERSION_MINOR={minor}"),
        format!("VERSION_PATCH={patch}"),
        format!("VERSION_STRING=\"{version}\""),
    ];
    for (rc, bin) in [("assets/shell.rc", "custom-ui"), ("assets/settings.rc", "custom-ui-settings"), ("assets/setup.rc", "custom-ui-setup")] {
        println!("cargo:rerun-if-changed={rc}");
        embed_resource::compile_for(rc, [bin], &macros);
    }
    for file in ["assets/icon.ico", "assets/app.manifest"] {
        println!("cargo:rerun-if-changed={file}");
    }
}
