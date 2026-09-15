//! DHDesktop —— DeepSeek Harness 桌面客户端。
//!
//! 架构(详见 README 与 `docs/recon-dsh.md`):
//!
//! * Rust 侧负责 dsh 进程的生命周期(定位 / 启动 / 就绪检测 / 停止 / 退出清理);
//! * 主窗口先显示我们自己的启动页,后端就绪后**导航**到 dsh 自带界面;
//! * 控制台(后端状态、日志、插件管理)是独立窗口,通过原生菜单打开。

mod dsh;

use std::sync::Arc;

use tauri::menu::{MenuBuilder, MenuItem, SubmenuBuilder};
use tauri::{AppHandle, Manager, RunEvent, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_opener::OpenerExt;

use dsh::locate::EnvReport;
use dsh::profile::PluginSnapshot;
use dsh::{BackendManager, BackendStatus, LogLine};

const CONSOLE_WINDOW: &str = "console";

// ---------------------------------------------------------------- commands

#[tauri::command]
async fn backend_status(state: tauri::State<'_, Arc<BackendManager>>) -> Result<BackendStatus, String> {
    Ok(state.status().await)
}

#[tauri::command]
async fn backend_logs(state: tauri::State<'_, Arc<BackendManager>>) -> Result<Vec<LogLine>, String> {
    Ok(state.logs().await)
}

#[tauri::command]
async fn backend_start(state: tauri::State<'_, Arc<BackendManager>>) -> Result<BackendStatus, String> {
    let manager = Arc::clone(&state);
    Ok(manager.start().await)
}

#[tauri::command]
async fn backend_stop(state: tauri::State<'_, Arc<BackendManager>>) -> Result<BackendStatus, String> {
    Ok(state.stop().await)
}

#[tauri::command]
async fn backend_restart(state: tauri::State<'_, Arc<BackendManager>>) -> Result<BackendStatus, String> {
    let manager = Arc::clone(&state);
    Ok(manager.restart().await)
}

/// 把主窗口切到已就绪的 dsh 界面。
#[tauri::command]
async fn open_dsh_ui(state: tauri::State<'_, Arc<BackendManager>>) -> Result<(), String> {
    state.open_dsh_ui().await;
    Ok(())
}

#[tauri::command]
fn env_report() -> EnvReport {
    dsh::locate::env_report()
}

#[tauri::command]
fn plugin_snapshot() -> Result<PluginSnapshot, String> {
    dsh::profile::snapshot()
}

#[tauri::command]
fn reveal_in_finder(app: AppHandle, path: String) -> Result<(), String> {
    app.opener()
        .open_path(path, None::<&str>)
        .map_err(|e| e.to_string())
}

// ---------------------------------------------------------------- windows

/// 打开(或聚焦)控制台窗口 —— 后端状态、日志、插件管理都在这里。
fn open_console(app: &AppHandle) -> tauri::Result<()> {
    if let Some(window) = app.get_webview_window(CONSOLE_WINDOW) {
        window.show()?;
        window.set_focus()?;
        return Ok(());
    }
    WebviewWindowBuilder::new(
        app,
        CONSOLE_WINDOW,
        WebviewUrl::App("index.html".into()),
    )
    .title("DHDesktop 控制台")
    .inner_size(1080.0, 760.0)
    .min_inner_size(880.0, 560.0)
    .build()?;
    Ok(())
}

fn build_menu(app: &AppHandle) -> tauri::Result<tauri::menu::Menu<tauri::Wry>> {
    let dsh_ui = MenuItem::with_id(app, "open_dsh", "打开 dsh 界面", true, None::<&str>)?;
    let console = MenuItem::with_id(
        app,
        "open_console",
        "控制台 / 插件管理",
        true,
        Some("CmdOrCtrl+Shift+P"),
    )?;
    let restart = MenuItem::with_id(
        app,
        "restart_backend",
        "重启 dsh 后端",
        true,
        Some("CmdOrCtrl+Shift+R"),
    )?;

    let app_menu = SubmenuBuilder::new(app, "DHDesktop")
        .item(&dsh_ui)
        .separator()
        .item(&console)
        .item(&restart)
        .separator()
        .services()
        .separator()
        .hide()
        .hide_others()
        .show_all()
        .separator()
        .quit()
        .build()?;

    let edit_menu = SubmenuBuilder::new(app, "编辑")
        .undo()
        .redo()
        .separator()
        .cut()
        .copy()
        .paste()
        .select_all()
        .build()?;

    MenuBuilder::new(app)
        .items(&[&app_menu, &edit_menu])
        .build()
}

// ---------------------------------------------------------------- entry

pub fn run() {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let handle = app.handle().clone();
            let manager = Arc::new(BackendManager::new(handle.clone()));
            app.manage(Arc::clone(&manager));

            // 后端由 Rust 自己拉起:
            // 不依赖前端加载、也不显示我们的窗口 —— 「打开客户端就是 dsh」。
            // 启动中用户看不到窗口是故意的(启动页只在出错时自动出现)。
            {
                let manager = Arc::clone(&manager);
                tauri::async_runtime::spawn(async move {
                    manager.start().await;
                });
            }

            let menu = build_menu(&handle)?;
            app.set_menu(menu)?;
            app.on_menu_event(move |app, event| match event.id().as_ref() {
                "open_console" => {
                    let _ = open_console(app);
                }
                "open_dsh" => {
                    if let Some(state) = app.try_state::<Arc<BackendManager>>() {
                        let manager = Arc::clone(&state);
                        tauri::async_runtime::spawn(async move {
                            manager.open_dsh_ui().await;
                        });
                    }
                }
                "restart_backend" => {
                    if let Some(state) = app.try_state::<Arc<BackendManager>>() {
                        let manager = Arc::clone(&state);
                        tauri::async_runtime::spawn(async move {
                            manager.restart().await;
                        });
                    }
                }
                _ => {}
            });

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            backend_status,
            backend_logs,
            backend_start,
            backend_stop,
            backend_restart,
            open_dsh_ui,
            env_report,
            plugin_snapshot,
            reveal_in_finder,
        ])
        // 统一策略:**关窗 = 隐藏,永不销毁**。
        //
        // 曾经的行为是让窗口被销毁,于是“再打开”必须重建同 label 的窗口,
        // 而 dsh 窗口重建还要重做一次鉴权 —— 途中任何一环失败,用户看到的就是
        // “关掉之后再也打不开”,而且错误信息还打在隐藏着的状态页里,完全看不到。
        // 隐藏/显示就没有这类失败模式:窗口对象一直在,重开只是 show()。
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application");

    app.run(|app_handle, event| match event {
        RunEvent::ExitRequested { .. } | RunEvent::Exit => {
            if let Some(state) = app_handle.try_state::<Arc<BackendManager>>() {
                // 同步清理子进程,避免退出后留下游离的 dsh。
                state.kill_now();
            }
        }
        // 点 Dock 图标且当前没有可见窗口时 —— 用户要的是 dsh 界面,不是我们的状态页。
        // 后端没在跑才把启动页露出来(否则用户会以为应用卡死了)。
        RunEvent::Reopen {
            has_visible_windows: false,
            ..
        } => {
            if let Some(state) = app_handle.try_state::<Arc<BackendManager>>() {
                let manager = Arc::clone(&state);
                tauri::async_runtime::spawn(async move {
                    if manager.status().await.is_ready() {
                        manager.open_dsh_ui().await;
                    } else {
                        manager.show_main_window();
                    }
                });
            }
        }
        _ => {}
    });
}
