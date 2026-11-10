//! Bounds retained async state size, which does not measure generated poll-function stack use.
//! Real actor form-boundary and cold-offline tests exercise the default worker stack.
#[path = "../src/app.rs"]
#[allow(dead_code, unused_imports)]
mod app;
#[path = "../src/host/mod.rs"]
#[allow(dead_code, unused_imports)]
mod host;
#[path = "../src/theme.rs"]
#[allow(dead_code)]
mod theme;
#[path = "../src/ui.rs"]
#[allow(dead_code)]
mod ui;
#[path = "../src/views/mod.rs"]
#[allow(dead_code, unused_imports)]
mod views;
#[path = "../src/widgets/mod.rs"]
#[allow(dead_code)]
mod widgets;
#[test]
fn native_actor_generators_fit_the_debug_stack_budget() {
    let frames = views::backend::Backend::frame_sizes_for_tests();
    for (name, size) in &frames {
        eprintln!("native async frame {name}: {size} bytes");
    }
    assert!(
        frames.iter().all(|(_, size)| *size <= 256 * 1024),
        "native composed futures exceed the256KiB generator budget: {frames:?}"
    );
}
