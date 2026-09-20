mod commands;
mod config;
mod context_menu;
mod file_icons;
mod highlight;
mod i18n;
mod indexing_status;
mod logging;
mod perf;
mod rebuild;
mod state;
mod tray;
mod watcher;
mod window_fx;

use std::sync::Mutex;

use tauri::Manager;
use tauri_plugin_autostart::ManagerExt as AutostartManagerExt;
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};

use config::ConfigState;
use context_menu::ContextMenuTarget;
use file_icons::FileIconCache;
use indexing_status::IndexingStatus;
use perf::HotkeyPerfState;
use rebuild::RebuildGuard;
use state::SearchState;
use watcher::WatchController;
use window_fx::EffectLevelState;

/// 当前生效的全局呼出快捷键。启动时初始化成配置里的键，设置面板"改键"
/// （`commands::set_hotkey`）注册成功后更新它——全局快捷键回调据此判断收到
/// 的到底是不是我们这一个呼出键。之所以要一份可更新的共享态、而不是像从前
/// 那样把 `toggle` 常量 move 进回调：改键后新键跟旧常量必然失配，回调会认不
/// 出新键；把"当前键"放进 State，改键时更新一处，回调每次现读，就不会失配。
pub struct HotkeyState(pub Mutex<Shortcut>);

/// 默认全局呼出快捷键：Alt+`（反引号）。原先是 Alt+Space，跟部分用户机器上的
/// PowerToys Run 冲突，改成配置项后这个只是兜底默认值和解析失败时的回退。
fn default_shortcut() -> Shortcut {
    Shortcut::new(Some(Modifiers::ALT), Code::Backquote)
}

/// 从配置里的字符串解析快捷键，解析失败（比如手改配置文件写错了格式）
/// 就回退到默认值，不能让整个应用起不来。
fn parse_shortcut(hotkey: &str) -> Shortcut {
    hotkey.parse().unwrap_or_else(|err| {
        eprintln!("解析快捷键配置 \"{hotkey}\" 失败，回退到默认值: {err}");
        default_shortcut()
    })
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // 越早越好：这一步之后所有的 eprintln!/println!（包括 dowse 内部
    // 各处排障日志）才会落进 `%LOCALAPPDATA%\dowse-personal\logs\dowse.log`，
    // 而不是消失在 GUI 子系统没有控制台的黑洞里（见 logging.rs 的文档）。
    logging::init();

    let toggle = parse_shortcut(&config::load().hotkey);

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, shortcut, event| {
                    // 全程只注册一个快捷键（改键时先退旧再注册新），但仍跟当前
                    // 生效的呼出键（HotkeyState）比一次：既防御未来注册多个快捷键
                    // 的情况，也明确"只认呼出键"。当前键改键后会变，所以现读 State
                    // 而不是比一个启动时定死的常量。
                    if event.state() != ShortcutState::Pressed {
                        return;
                    }
                    let current = *app
                        .state::<HotkeyState>()
                        .0
                        .lock()
                        .expect("hotkey mutex poisoned");
                    if *shortcut != current {
                        return;
                    }
                    if let Some(window) = app.get_webview_window("main") {
                        // 呼出延迟埋点的起点：热键回调刚进来的这一刻。只在窗口当前
                        // 不可见（即将变成"显示"而不是"隐藏"）时标记——toggle 同时
                        // 承担两种职责，隐藏路径不该污染下一次呼出的计时基准。
                        if !window.is_visible().unwrap_or(false) {
                            app.state::<HotkeyPerfState>().mark_hotkey_show();
                        }
                        window_fx::toggle_window(&window);
                    }
                })
                .build(),
        )
        .manage(ConfigState::new())
        .manage(HotkeyState(Mutex::new(toggle)))
        .manage(SearchState::load_initial())
        .manage(WatchController::new())
        .manage(FileIconCache::new())
        .manage(ContextMenuTarget::new())
        .manage(IndexingStatus::new())
        .manage(RebuildGuard::new())
        .manage(HotkeyPerfState::new())
        .invoke_handler(tauri::generate_handler![
            commands::index_status,
            commands::indexing_status,
            commands::index_usage,
            commands::search,
            commands::preview,
            commands::read_content_chunk,
            commands::find_in_doc,
            commands::open_file,
            commands::reveal_in_folder,
            commands::rebuild_index,
            commands::add_root,
            commands::get_effect_level,
            commands::get_glass_alpha,
            commands::get_hotkey,
            commands::get_rules,
            commands::set_rules,
            commands::get_config,
            commands::set_hotkey,
            commands::set_transparency_enabled,
            commands::set_transparency_tier,
            commands::set_autostart,
            commands::set_lang,
            commands::file_icon,
            commands::set_pinned,
            commands::hide_window,
            commands::report_shown_perf,
            commands::report_search_perf,
            context_menu::show_result_context_menu,
        ])
        .setup(move |app| {
            // 快捷键抢注册失败（常见原因：被输入法或别的常驻工具占用了）
            // 不该让整个应用起不来——托盘的"呼出"菜单项还能用，把错误打到日志就行。
            match app.global_shortcut().register(toggle) {
                Ok(()) => logging::log_line("startup", &format!("已注册全局呼出快捷键: {toggle}")),
                Err(err) => logging::log_line(
                    "startup",
                    &format!("注册 {toggle} 全局快捷键失败，可能被别的程序占用了: {err}"),
                ),
            }

            let window = app
                .get_webview_window("main")
                .expect("tauri.conf.json 里定义的 main 窗口应该存在");

            // 结果行右键菜单（context_menu::show_result_context_menu）在这个窗口上
            // popup，选中项通过这里回调；托盘菜单是另一套独立的事件注册，见 tray.rs。
            window.on_menu_event(context_menu::handle_context_menu_event);

            let cfg = app.state::<ConfigState>().get();
            let level = window_fx::apply_with_fallback(
                &window,
                cfg.transparency_enabled,
                cfg.transparency_tier,
            );
            app.manage(EffectLevelState::new(level));
            // 初始位置交给 tauri.conf.json 的 `center: true`：自用版的窗口是
            // 常驻文档窗口，位置一旦定下就不再被程序挪动（见 window_fx::show_window）。

            // 自用版：**默认不开机自启**。上游默认帮用户把自启项打开，对
            // 文档阅读工具来说是干扰——用户想开机自启时自己到设置面板里打开，
            // 这里不再做任何"默认帮用户打开"的动作（`set_autostart` 命令照常
            // 可用，用户开的自启项也不会被启动流程改回去）。
            {
                let mgr = app.autolaunch();
                logging::log_line(
                    "startup",
                    &format!("开机自启当前状态（自用版默认不主动开启）: {}", mgr.is_enabled().unwrap_or(false)),
                );
            }

            tray::build(app.handle())?;

            // 常驻监听：读索引里注册的根，先对账补齐停机期间的变更、再挂实时监听。
            // 索引不存在或 schema 需重建时读不到根，直接跳过——等用户重建后由
            // rebuild 流程把监听挂上。
            if let Ok(index_dir) = config::index_dir()
                && let Ok(roots) = dowse::registered_roots(&index_dir)
            {
                app.state::<WatchController>()
                    .start(app.handle().clone(), index_dir, roots);
            }

            Ok(())
        })
        // 自用版：**不做"失焦即隐藏"**。上游是 Spotlight/Raycast 那种浮窗，
        // 切走就自动收起；而自用场景的主线是"搜到一篇文档后接着在全文阅读里
        // 读"，一失焦就消失会让阅读和文内查找没法用。现在窗口只在两种情况下
        // 收起：用户按 Esc、或按全局呼出快捷键切换（两者都走 window_fx 的
        // hide 路径）。窗口位置也不再每次呼出都重置（见 window_fx::show_window），
        // 关掉再打开会停在原处，符合"常驻文档窗口"的直觉。
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
