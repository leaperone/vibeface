#![forbid(unsafe_code)]

#[cfg(all(test, feature = "native-ui-tests"))]
extern crate gpui_kit as gpui;

mod app;
mod application;
pub mod host;
mod theme;
pub mod ui;
pub mod views;
pub mod widgets;

pub use application::run;
