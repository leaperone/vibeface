use anyhow::{Context, Result, ensure};
use gpui_kit::{
    App, ClipboardItem, KeyBinding, Menu, MenuItem, OsAction, PathPromptOptions, SystemNotification,
};
use std::{
    collections::{HashMap, VecDeque},
    io::Write,
    path::{Path, PathBuf},
};
use tokio::sync::mpsc;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NativeAction {
    About,
    NodeSettings,
    Focus,
    Quit,
    Reload,
    ResetZoom,
    ZoomIn,
    ZoomOut,
    Fullscreen,
    Minimize,
    CloseWindow,
    FrontAll,
    Notification { route: String },
}
gpui_kit::actions!(
    desktop,
    [
        About,
        NodeSettings,
        Focus,
        Quit,
        Reload,
        ResetZoom,
        ZoomIn,
        ZoomOut,
        Fullscreen,
        Minimize,
        CloseWindow,
        FrontAll,
        Hide,
        HideOthers,
        UnhideOthers,
        Docs,
        Releases
    ]
);

/// Installs native menus and keyboard equivalents. The receiver carries user intent only;
/// the UI owns window state and must await host.shutdown() before quitting.
pub fn install_menus(cx: &mut App) -> mpsc::UnboundedReceiver<NativeAction> {
    use gpui_kit::component::input::{Copy, Cut, Paste, Redo, SelectAll, Undo};
    let (sender, receiver) = mpsc::unbounded_channel();
    macro_rules! forward {
        ($action:ident, $value:ident) => {{
            let sender = sender.clone();
            cx.on_action(move |_: &$action, _| {
                let _ = sender.send(NativeAction::$value);
            });
        }};
    }
    forward!(About, About);
    forward!(NodeSettings, NodeSettings);
    forward!(Focus, Focus);
    forward!(Quit, Quit);
    forward!(Reload, Reload);
    forward!(ResetZoom, ResetZoom);
    forward!(ZoomIn, ZoomIn);
    forward!(ZoomOut, ZoomOut);
    forward!(Fullscreen, Fullscreen);
    forward!(Minimize, Minimize);
    forward!(CloseWindow, CloseWindow);
    forward!(FrontAll, FrontAll);
    cx.on_action(|_: &Hide, cx| cx.hide());
    cx.on_action(|_: &HideOthers, cx| cx.hide_other_apps());
    cx.on_action(|_: &UnhideOthers, cx| cx.unhide_other_apps());
    cx.on_action(|_: &Docs, cx| {
        cx.open_url("https://github.com/leaperone/vibeface/tree/main/packages/docs")
    });
    cx.on_action(|_: &Releases, cx| cx.open_url("https://github.com/leaperone/vibeface/releases"));
    let mut app_items = vec![
        MenuItem::action("关于 Vibeface", About),
        MenuItem::separator(),
        MenuItem::action("本机节点…", NodeSettings),
        MenuItem::action("显示 Vibeface", Focus),
        MenuItem::separator(),
    ];
    if cfg!(target_os = "macos") {
        app_items.extend([
            MenuItem::action("隐藏 Vibeface", Hide),
            MenuItem::action("隐藏其他应用", HideOthers),
            MenuItem::action("显示全部", UnhideOthers),
            MenuItem::separator(),
        ]);
    }
    app_items.push(MenuItem::action("退出 Vibeface", Quit));
    let mut window_items = vec![
        MenuItem::action("最小化", Minimize),
        MenuItem::action("关闭窗口", CloseWindow),
    ];
    if cfg!(target_os = "macos") {
        window_items.extend([
            MenuItem::separator(),
            MenuItem::action("前置全部窗口", FrontAll),
        ]);
    }
    cx.set_menus([
        Menu::new("Vibeface").items(app_items),
        Menu::new("编辑").items([
            MenuItem::os_action("撤销", Undo, OsAction::Undo),
            MenuItem::os_action("重做", Redo, OsAction::Redo),
            MenuItem::separator(),
            MenuItem::os_action("剪切", Cut, OsAction::Cut),
            MenuItem::os_action("复制", Copy, OsAction::Copy),
            MenuItem::os_action("粘贴", Paste, OsAction::Paste),
            MenuItem::os_action("全选", SelectAll, OsAction::SelectAll),
        ]),
        Menu::new("视图").items([
            MenuItem::action("重新载入", Reload),
            MenuItem::action("实际大小", ResetZoom),
            MenuItem::action("放大", ZoomIn),
            MenuItem::action("缩小", ZoomOut),
            MenuItem::separator(),
            MenuItem::action("全屏", Fullscreen),
        ]),
        Menu::new("窗口").items(window_items),
        Menu::new("帮助").items([
            MenuItem::action("使用文档", Docs),
            MenuItem::action("发行版本", Releases),
            MenuItem::separator(),
            MenuItem::Action {
                name: format!("版本 {}", env!("CARGO_PKG_VERSION")).into(),
                action: Box::new(About),
                os_action: None,
                checked: false,
                disabled: true,
            },
        ]),
    ]);
    let key = |mac: &'static str, other: &'static str| {
        if cfg!(target_os = "macos") {
            mac
        } else {
            other
        }
    };
    cx.bind_keys([
        KeyBinding::new(key("cmd-,", "ctrl-,"), NodeSettings, None),
        KeyBinding::new(key("cmd-shift-o", "ctrl-shift-o"), Focus, None),
        KeyBinding::new(key("cmd-q", "ctrl-q"), Quit, None),
        KeyBinding::new(key("cmd-r", "ctrl-r"), Reload, None),
        KeyBinding::new(key("cmd-0", "ctrl-0"), ResetZoom, None),
        KeyBinding::new(key("cmd-=", "ctrl-="), ZoomIn, None),
        KeyBinding::new(key("cmd--", "ctrl--"), ZoomOut, None),
        KeyBinding::new("f11", Fullscreen, None),
        KeyBinding::new(key("cmd-m", "ctrl-m"), Minimize, None),
        KeyBinding::new(key("cmd-w", "ctrl-w"), CloseWindow, None),
    ]);
    if cfg!(target_os = "macos") {
        cx.bind_keys([
            KeyBinding::new("cmd-h", Hide, None),
            KeyBinding::new("cmd-alt-h", HideOthers, None),
            KeyBinding::new("cmd-ctrl-f", Fullscreen, None),
        ]);
    }
    if cfg!(target_os = "windows") {
        cx.bind_keys([KeyBinding::new("alt-f4", Quit, None)]);
    }
    receiver
}
pub fn choose_files(
    cx: &App,
    multiple: bool,
) -> impl std::future::Future<Output = Result<Option<Vec<PathBuf>>>> + use<> {
    let receiver = cx.prompt_for_paths(PathPromptOptions {
        files: true,
        directories: false,
        multiple,
        prompt: Some("选择文件".into()),
    });
    async move { receiver.await.context("文件对话框未返回结果。")? }
}
pub fn choose_directory(
    cx: &App,
) -> impl std::future::Future<Output = Result<Option<PathBuf>>> + use<> {
    let receiver = cx.prompt_for_paths(PathPromptOptions {
        files: false,
        directories: true,
        multiple: false,
        prompt: Some("选择已有节点目录".into()),
    });
    async move {
        Ok(receiver
            .await
            .context("目录对话框未返回结果。")??
            .and_then(|paths| paths.into_iter().next()))
    }
}
pub fn choose_save_path(
    cx: &App,
    directory: &Path,
    filename: &str,
) -> impl std::future::Future<Output = Result<Option<PathBuf>>> + use<> {
    let receiver = cx.prompt_for_new_path(directory, Some(filename));
    async move { receiver.await.context("保存对话框未返回结果。")? }
}
pub fn copy_text(cx: &App, text: String) {
    cx.write_to_clipboard(ClipboardItem::new_string(text));
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SaveResult {
    Cancelled,
    Saved(PathBuf),
}
/// Call on the background executor only after the native save dialog returns.
pub fn save_selected(path: Option<PathBuf>, bytes: &[u8]) -> Result<SaveResult> {
    let Some(path) = path else {
        return Ok(SaveResult::Cancelled);
    };
    super::model::safe_path(&path)?;
    if let Ok(metadata) = std::fs::symlink_metadata(&path) {
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "保存位置不是普通文件。"
        );
    }
    let directory = path.parent().context("保存位置缺少父目录。")?;
    let temporary = directory.join(format!(".vibeface-save-{}", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = vibeface_bridge::platform::privatefs::create_new(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        vibeface_bridge::platform::privatefs::durable_rename(&temporary, &path)?;
        Ok::<_, anyhow::Error>(SaveResult::Saved(path.clone()))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}

/// A submitted notification is not proof that the OS displayed it or granted permission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotificationSubmission {
    Disabled,
    SubmittedToOs,
}
#[derive(Default)]
pub struct Notifications {
    routes: HashMap<String, String>,
    order: VecDeque<String>,
}
impl Notifications {
    pub fn submit(
        &mut self,
        cx: &App,
        enabled: bool,
        title: &str,
        body: &str,
        route: &str,
    ) -> Result<NotificationSubmission> {
        if !enabled {
            return Ok(NotificationSubmission::Disabled);
        }
        ensure!(
            title.chars().count() <= 128
                && body.chars().count() <= 1024
                && route.len() <= 512
                && route.starts_with('/')
                && !route.starts_with("//")
                && !route.contains(['?', '#', '\\'])
                && !route.chars().any(char::is_control),
            "通知内容无效。"
        );
        while self.routes.len() >= 128 {
            if let Some(oldest) = self.order.pop_front() {
                self.routes.remove(&oldest);
                cx.dismiss_system_notification(&oldest);
            } else {
                break;
            }
        }
        let tag = format!("vibeface-{}", uuid::Uuid::new_v4());
        self.routes.insert(tag.clone(), route.into());
        self.order.push_back(tag.clone());
        cx.show_system_notification(SystemNotification {
            tag: tag.into(),
            title: title.to_owned().into(),
            body: body.to_owned().into(),
            actions: Vec::new(),
        });
        Ok(NotificationSubmission::SubmittedToOs)
    }
    pub fn respond(&mut self, tag: &str) -> Option<NativeAction> {
        self.order.retain(|queued| queued != tag);
        self.routes
            .remove(tag)
            .map(|route| NativeAction::Notification { route })
    }
    pub fn dismiss(&mut self, cx: &App, tag: &str) {
        self.order.retain(|queued| queued != tag);
        self.routes.remove(tag);
        cx.dismiss_system_notification(tag);
    }
}
