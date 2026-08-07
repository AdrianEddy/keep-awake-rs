use std::env;

fn main() {
    // src/macos.rs declares the IOPMAssertion functions itself, so the framework
    // that defines them has to be linked here.
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-lib=framework=IOKit");
    }
}
