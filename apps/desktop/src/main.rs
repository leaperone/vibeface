#![forbid(unsafe_code)]
mod app;
mod host;
mod theme;
mod ui;
mod views;
mod widgets;
use sha2::Digest;
use anyhow::{Context,Result,ensure};
use gpui_kit::*;
use std::{path::PathBuf,sync::Arc,time::Duration};
use host::{activation::{SingleInstance,InstanceClaim,Activation},platform::NativeAction};

struct Options{profile:PathBuf,runtime:PathBuf,report:Option<PathBuf>,fixture:bool,args:Vec<String>}
impl Options{
 fn parse()->Result<Self>{let args=std::env::args().skip(1).collect::<Vec<_>>();let mut profile=None;let mut runtime=None;let mut report=None;let mut fixture=false;let mut index=0;while index<args.len(){match args[index].as_str(){"--profile-dir"|"--runtime-dir"|"--acceptance-report"=>{let option=args[index].clone();index+=1;let path=PathBuf::from(args.get(index).context("参数缺少路径")?);ensure!(path.is_absolute(),"路径必须是绝对路径");match option.as_str(){"--profile-dir"=>profile=Some(path),"--runtime-dir"=>runtime=Some(path),_=>report=Some(path)}},"--controlled-fixture"=>fixture=true,value if value.starts_with("vibeface:")=>{},_=>anyhow::bail!("未知桌面参数")};index+=1;}
 let profile=match profile{Some(path)=>path,None=>default_profile()?};if runtime.is_some(){ensure!(fixture,"--runtime-dir 仅允许受控验收夹具使用");}let executable=std::env::current_exe()?;let parent=executable.parent().context("程序路径缺少父目录")?;let runtime=runtime.unwrap_or_else(||if cfg!(target_os="macos"){parent.parent().unwrap_or(parent).join("Resources/runtime")}else{parent.join("resources/runtime")});if let Some(path)=&report{ensure!(fixture,"验收报告仅在 --controlled-fixture 模式提供");ensure!(path.starts_with(&profile),"验收报告必须位于受控 profile 目录内");}Ok(Self{profile,runtime,report,fixture,args})}
}
fn default_profile()->Result<PathBuf>{
 #[cfg(target_os="macos")]let base=PathBuf::from(std::env::var_os("HOME").context("无法定位当前用户目录")?).join("Library/Application Support");
 #[cfg(target_os="windows")]let base=PathBuf::from(std::env::var_os("LOCALAPPDATA").context("无法定位当前用户应用目录")?);
 #[cfg(not(any(target_os="macos",target_os="windows")))]let base=std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).unwrap_or(PathBuf::from(std::env::var_os("HOME").context("无法定位当前用户目录")?).join(".local/share"));
 Ok(base.join("Vibeface/native-desktop"))
}
fn main(){if let Err(error)=run(){eprintln!("Vibeface 原生桌面无法启动：{error}");std::process::exit(1);}}
fn run() -> Result<()> {
    let options = Options::parse()?;
    let runtime = Arc::new(tokio::runtime::Builder::new_multi_thread().enable_all().thread_name("vibeface-client").build()?);
    let instance = runtime.block_on(SingleInstance::claim(&options.profile, &options.args))?;
    let InstanceClaim::Primary(mut instance) = instance else { return Ok(()); };
    let desktop_host = host::DesktopHost::open(options.profile.clone(), options.runtime, options.fixture)?;
    let (tx, events, snapshots, stop, backend_task) = views::backend::Backend::spawn(desktop_host, runtime.handle());
    let shutdown = stop.clone();
    let finish_runtime = runtime.clone();
    let (activation_tx, activation_rx) = std::sync::mpsc::sync_channel(32);
    let application = gpui_kit::application().with_assets(gpui_kit::assets::Assets);
    let urls_tx = activation_tx.clone();
    application.on_open_urls(move |urls| {
        for url in urls {
            if let Some(activation) = Activation::parse(&url) { let _ = urls_tx.try_send(activation); }
        }
    });
    application.on_reopen(move |cx| { cx.activate(true); let _ = activation_tx.try_send(Activation::Focus); });
    application.run(move |cx| {
        gpui_kit::init(cx);
        cx.set_quit_mode(QuitMode::Explicit);
        cx.set_app_identity("one.leaper.vibeface", "Vibeface");
        let mut menu = host::platform::install_menus(cx);
        let (mut tray, tray_error) = match host::tray::NativeTray::new() {
            Ok(tray) => (Some(tray), None),
            Err(error) => (None, Some(format!("系统托盘不可用：{error}"))),
        };
        let fixture = options.fixture;
        let credential_scope = format!("vibeface-native://{}", hex::encode(sha2::Sha256::digest(options.profile.to_string_lossy().as_bytes())));
        let app_tx = tx.clone();let app_stop=stop.clone();
        let (visible, view) = gpui_kit::open_window(WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, size(px(1200.), px(820.)), cx))),
            titlebar: Some(TitlebarOptions { title: Some("Vibeface".into()), ..Default::default() }),
            ..Default::default()
        }, cx, move |window, cx| {
            window.on_window_should_close(cx, |_, cx| { cx.hide(); false });
            cx.new(|cx| {
                let mut app = app::Desktop::new(app_tx, app_stop, events, snapshots, window, cx);
                app.credential_scope = credential_scope;
                app.notice = tray_error;
                if fixture { app.configure_fixture(window, cx); }
                app
            })
        }).expect("无法创建原生窗口");
        cx.activate(true);
        let notifications = std::rc::Rc::new(std::cell::RefCell::new(host::platform::Notifications::default()));
        let notification_state = notifications.clone();
        let notification_view = view.downgrade();
        cx.on_system_notification_response(move |response, cx| {
            if notification_state.borrow_mut().respond(response.tag.as_ref()).is_some() {
                let _ = notification_view.update(cx, |app, cx| app.navigate(ui::Route::Inbox, cx));
                cx.activate(true);
            }
        });
        let report = options.report;
        let runtime_for_loop = runtime.clone();
        cx.spawn(async move |cx| {
            let mut report_opened = false;
            loop {
                cx.background_executor().timer(Duration::from_millis(100)).await;
                let mut actions = Vec::new();
                while let Ok(action) = menu.try_recv() { actions.push(action); }
                if let Some(tray) = tray.as_mut() { actions.extend(tray.poll()); }
                let mut activations = Vec::new();
                while let Some(activation) = instance.try_recv() { activations.push(activation); }
                while let Ok(activation) = activation_rx.try_recv() { activations.push(activation); }
                for activation in activations {
                    match activation {
                        Activation::Focus => actions.push(NativeAction::Focus),
                        Activation::Join { ticket } => {
                            let _ = visible.update(cx, |_, window, cx| {
                                view.update(cx, |app, cx| app.edit(views::forms::Editor::Join, std::collections::BTreeMap::from([("ticket", ticket)]), window, cx));
                                window.activate_window();
                            });
                        }
                    }
                }
                for action in actions {
                    if matches!(action, NativeAction::Quit) {
                        stop.send_replace(true);
                        continue;
                    }
                    let _ = visible.update(cx, |_, window, cx| {
                        view.update(cx, |app, cx| match action {
                            NativeAction::NodeSettings => app.navigate(ui::Route::Node, cx),
                            NativeAction::Reload => app.send(views::backend::Command::Refresh, cx),
                            NativeAction::About => { app.notice = Some(format!("Vibeface {} · GPUI 原生桌面", env!("CARGO_PKG_VERSION"))); cx.notify(); }
                            NativeAction::Focus | NativeAction::FrontAll => window.activate_window(),
                            NativeAction::Fullscreen => window.toggle_fullscreen(),
                            NativeAction::Minimize => window.minimize_window(),
                            NativeAction::CloseWindow => cx.hide(),
                            NativeAction::ZoomIn => window.set_rem_size((window.rem_size() + px(1.)).min(px(24.))),
                            NativeAction::ZoomOut => window.set_rem_size((window.rem_size() - px(1.)).max(px(12.))),
                            NativeAction::ResetZoom => window.set_rem_size(px(16.)),
                            NativeAction::Notification { .. } => app.navigate(ui::Route::Inbox, cx),
                            NativeAction::Quit => {}
                        });
                    });
                }
                if let Some(path) = &report {
                    if !report_opened {
                        let payload = serde_json::json!({"runtime_ui":"gpui","phase":"opened","window_opened":true,"pid":std::process::id(),"node_started":false,"controlled_fixture":true,"ime_verified":false});
                        let save_path = path.clone();
                        let saved = runtime_for_loop.spawn_blocking(move || host::platform::save_selected(Some(save_path), &serde_json::to_vec(&payload)?)).await;
                        if !matches!(saved, Ok(Ok(_))) { eprintln!("原生验收报告保存失败"); }
                        report_opened = true;
                    }
                    let continue_path = PathBuf::from(format!("{}.continue", path.display()));
                    let continued = runtime_for_loop.spawn_blocking(move || continue_path.try_exists()).await;
                    if matches!(continued, Ok(Ok(true))) {
                        let payload = visible.update(cx, |_, window, cx| view.read(cx).fixture_report(window, cx));
                        if let Ok(payload) = payload {
                            let save_path = path.clone();
                            let saved = runtime_for_loop.spawn_blocking(move || host::platform::save_selected(Some(save_path), &serde_json::to_vec(&payload)?)).await;
                            if !matches!(saved, Ok(Ok(_))) { eprintln!("原生验收最终报告保存失败"); }
                        }
                        stop.send_replace(true);
                        return;
                    }
                }
            }
        }).detach();
    });
    // GPUI only gives OS quit callbacks 200ms. Retain the runtime after its event loop
    // returns so EOF/graceful Child cleanup finishes before the process exits.
    shutdown.send_replace(true);
    finish_runtime.block_on(backend_task).context("原生客户端后台退出失败")?;
    Ok(())
}
