use std::env;
use std::path::PathBuf;

fn main() {
    let config = slint_build::CompilerConfiguration::new().with_style("fluent".into());
    slint_build::compile_with_config("ui/main.slint", config).expect("failed to compile the Slint UI");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_windows_resources();
    }
}

/// The application manifest: run as the invoking user (elevation is an explicit choice), per-monitor
/// DPI awareness, long paths, UTF-8 for any ANSI API, and the hardened segment heap.
const MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <assemblyIdentity type="win32" name="FreeFlite.Ferrotop" version="@VERSION@" processorArchitecture="*"/>
  <description>Ferrotop process explorer</description>
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="asInvoker" uiAccess="false"/>
      </requestedPrivileges>
    </security>
  </trustInfo>
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
    <application>
      <!-- Windows 10 and 11 -->
      <supportedOS Id="{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}"/>
    </application>
  </compatibility>
  <application xmlns="urn:schemas-microsoft-com:asm.v3">
    <windowsSettings>
      <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true/pm</dpiAware>
      <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2</dpiAwareness>
      <longPathAware xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">true</longPathAware>
      <activeCodePage xmlns="http://schemas.microsoft.com/SMI/2019/WindowsSettings">UTF-8</activeCodePage>
      <heapType xmlns="http://schemas.microsoft.com/SMI/2020/WindowsSettings">SegmentHeap</heapType>
    </windowsSettings>
  </application>
</assembly>
"#;

/// Embeds the app icon (shown by Explorer and the taskbar), the application manifest, and a
/// version resource (shown in the file's Properties and by process explorers, Ferrotop included).
fn embed_windows_resources() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let icon = manifest_dir.join("assets").join("ferrotop.ico");
    println!("cargo:rerun-if-changed={}", icon.display());

    let version = env::var("CARGO_PKG_VERSION").unwrap();
    let numeric =
        ["MAJOR", "MINOR", "PATCH"].map(|part| env::var(format!("CARGO_PKG_VERSION_{part}")).unwrap()).join(",");
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let manifest = out.join("ferrotop.manifest");
    let dotted = format!("{}.0", numeric.replace(',', "."));
    std::fs::write(&manifest, MANIFEST.replace("@VERSION@", &dotted)).expect("failed to write the manifest");
    // RC string literals need doubled backslashes.
    let rc_path = |path: &PathBuf| path.display().to_string().replace('\\', "\\\\");
    let (icon_path, manifest_path) = (rc_path(&icon), rc_path(&manifest));
    let resource = format!(
        r#"1 ICON "{icon_path}"
1 24 "{manifest_path}"

1 VERSIONINFO
FILEVERSION {numeric},0
PRODUCTVERSION {numeric},0
FILEFLAGSMASK 0x3f
FILEFLAGS 0x0
FILEOS 0x40004
FILETYPE 0x1
FILESUBTYPE 0x0
BEGIN
    BLOCK "StringFileInfo"
    BEGIN
        BLOCK "040904b0"
        BEGIN
            VALUE "CompanyName", "FreeFlite"
            VALUE "FileDescription", "Ferrotop process explorer"
            VALUE "FileVersion", "{version}"
            VALUE "InternalName", "ferrotop"
            VALUE "LegalCopyright", "Copyright (c) 2026 FreeFlite. MIT License."
            VALUE "OriginalFilename", "ferrotop.exe"
            VALUE "ProductName", "Ferrotop"
            VALUE "ProductVersion", "{version}"
        END
    END
    BLOCK "VarFileInfo"
    BEGIN
        VALUE "Translation", 0x409, 1200
    END
END
"#
    );
    let rc = out.join("ferrotop.rc");
    std::fs::write(&rc, resource).expect("failed to write ferrotop.rc");
    embed_resource::compile(&rc, embed_resource::NONE).manifest_optional().expect("failed to compile ferrotop.rc");
}
