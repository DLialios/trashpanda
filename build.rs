//! Embeds the program icon in the Windows .exe, where Explorer and shortcuts
//! show it. Builds for other systems skip it.

fn main() {
    println!("cargo:rerun-if-changed=assets/icon.rc");
    println!("cargo:rerun-if-changed=assets/icon.ico");
    // Required rather than optional: a Windows build that cannot find the
    // resource compiler (x86_64-w64-mingw32-windres when cross-compiling)
    // fails instead of quietly producing an .exe without an icon.
    embed_resource::compile("assets/icon.rc", embed_resource::NONE)
        .manifest_required()
        .unwrap();
}
