fn main() {
    assert!(
        std::env::var_os("RISC0_SKIP_BUILD").is_none(),
        "skipping the delivery guest build is forbidden"
    );
    risc0_build::embed_methods();
}
