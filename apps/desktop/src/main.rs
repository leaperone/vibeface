#![forbid(unsafe_code)]

fn main() {
    if let Err(error) = vibeface_desktop::run() {
        eprintln!("Vibeface 原生桌面无法启动：{error}");
        std::process::exit(1);
    }
}
