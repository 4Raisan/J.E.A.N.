fn main() {
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        // Building the full Clap command tree can exceed the default 1 MiB
        // Windows stack in an unoptimized build.
        println!("cargo:rustc-link-arg-bin=jean=/STACK:4194304");
    }
}
