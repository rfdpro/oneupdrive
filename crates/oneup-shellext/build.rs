//! COM's entry points are exported as `PRIVATE` (not for linking against),
//! as the linker expects for these two names.

fn main() {
    println!("cargo:rerun-if-changed=oneup_shell.def");
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        let def =
            std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default()).join("oneup_shell.def");
        println!("cargo:rustc-cdylib-link-arg=/DEF:{}", def.display());
    }
}
