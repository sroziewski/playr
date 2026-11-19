fn main() {
    println!("cargo:rerun-if-changed=assets/playr.rc");
    println!("cargo:rerun-if-changed=assets/playr.ico");
    // The icon is cosmetic: a Windows build machine without RC.EXE (no Windows SDK) still gets
    // a working playr.exe, just with the generic one, and a warning says so. A resource
    // compiler that is there but fails on the script is a real error.
    match embed_resource::compile("assets/playr.rc", embed_resource::NONE) {
        embed_resource::CompilationResult::NotAttempted(why) => {
            println!("cargo:warning=playr.exe gets no icon: no resource compiler ({why})")
        }
        result => result.manifest_optional().expect("compiling assets/playr.rc"),
    }
}
