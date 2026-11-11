use super::platform::NativeAction;
use anyhow::{Context, Result, ensure};
use std::io::Cursor;
use tray_icon::{
    Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent,
    menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem},
};

/// Own on GPUI's main thread for the lifetime of the app. Linux KSNI uses its own D-Bus
/// worker; macOS/Windows share GPUI's native OS event loop.
pub struct NativeTray {
    icon: TrayIcon,
    settings: MenuId,
    focus: MenuId,
    quit: MenuId,
}
impl NativeTray {
    pub fn new() -> Result<Self> {
        let menu = Menu::new();
        let settings = MenuItem::new("本机节点…", true, None);
        let focus = MenuItem::new("显示 Vibeface", true, None);
        let quit = MenuItem::new("退出 Vibeface", true, None);
        let separator = PredefinedMenuItem::separator();
        menu.append_items(&[&settings, &focus, &separator, &quit])?;
        let icon = tray_icon()?;
        let builder = TrayIconBuilder::new().with_menu(Box::new(menu));
        #[cfg(target_os = "macos")]
        let builder = builder.with_icon_templated(icon);
        #[cfg(not(target_os = "macos"))]
        let builder = builder.with_icon(icon);
        let icon = builder
            .with_menu_on_left_click(false)
            .with_tooltip(format!("Vibeface {}", env!("CARGO_PKG_VERSION")))
            .build()
            .context("无法创建系统托盘。")?;
        Ok(Self {
            icon,
            settings: settings.id().clone(),
            focus: focus.id().clone(),
            quit: quit.id().clone(),
        })
    }
    pub fn poll(&self) -> Vec<NativeAction> {
        let mut actions = Vec::new();
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            if event.id == self.settings {
                actions.push(NativeAction::NodeSettings);
            } else if event.id == self.focus {
                actions.push(NativeAction::Focus);
            } else if event.id == self.quit {
                actions.push(NativeAction::Quit);
            }
        }
        while let Ok(event) = TrayIconEvent::receiver().try_recv() {
            match event {
                TrayIconEvent::Click {
                    id,
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                } if id == self.icon.id() => actions.push(NativeAction::Focus),
                TrayIconEvent::DoubleClick { id, .. } if id == self.icon.id() => {
                    actions.push(NativeAction::Focus)
                }
                _ => {}
            }
        }
        actions
    }
}
fn tray_icon() -> Result<Icon> {
    #[cfg(target_os = "macos")]
    let bytes = include_bytes!("../../assets/trayTemplate@2x.png").as_slice();
    #[cfg(not(target_os = "macos"))]
    let bytes = include_bytes!("../../assets/tray@2x.png").as_slice();
    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info()?;
    let mut data = vec![0; reader.output_buffer_size().context("托盘图像过大。")?];
    let info = reader.next_frame(&mut data)?;
    ensure!(info.width <= 256 && info.height <= 256, "托盘图像过大。");
    let pixels = &data[..info.buffer_size()];
    let rgba = match info.color_type {
        png::ColorType::Rgba => pixels.to_vec(),
        png::ColorType::Rgb => pixels
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        png::ColorType::GrayscaleAlpha => pixels
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|p| [p[0], p[0], p[0], p[1]])
            .collect(),
        png::ColorType::Grayscale => pixels.iter().flat_map(|p| [*p, *p, *p, 255]).collect(),
        png::ColorType::Indexed => anyhow::bail!("托盘图像未展开。"),
    };
    Ok(Icon::from_rgba(rgba, info.width, info.height)?)
}
