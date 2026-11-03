use std::{env, fs, path::Path};

fn main() {
    let manifest = Path::new(&env::var("CARGO_MANIFEST_DIR").unwrap()).to_path_buf();
    delivery_abi(&manifest);
}

fn delivery_abi(manifest: &Path) {
    let schema = manifest.join("../../crates/delivery/abi/Statement.sol");
    let interface = manifest.join("src/delivery/abi.sol");
    println!("cargo:rerun-if-changed={}", schema.display());
    println!("cargo:rerun-if-changed={}", interface.display());
    let source = format!(
        "{}\n{}",
        fs::read_to_string(schema).unwrap(),
        fs::read_to_string(interface).unwrap()
    )
    .replace("struct ", "#[sol(all_derives)]\nstruct ");
    fs::write(
        Path::new(&env::var("OUT_DIR").unwrap()).join("delivery_abi.rs"),
        format!("alloy_sol_types::sol! {{ {source} }}"),
    )
    .unwrap();
}
