// Passes the architecture's linker script to the linker.
fn main() {
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let script = format!("{dir}/src/arch/{arch}/linker.ld");
    println!("cargo:rustc-link-arg=-T{script}");
    println!("cargo:rerun-if-changed={script}");
    println!("cargo:rerun-if-changed=src/arch/{arch}/boot.S");
}
