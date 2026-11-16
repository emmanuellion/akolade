// Windows: embeds packaging/windows/akolade.rc (the app icon) into akolade.exe.
fn main() {
    println!("cargo:rerun-if-changed=packaging/windows/akolade.rc");
    println!("cargo:rerun-if-changed=packaging/windows/akolade.ico");
    #[cfg(windows)]
    embed_resource::compile("packaging/windows/akolade.rc", embed_resource::NONE).manifest_optional().unwrap();
}
