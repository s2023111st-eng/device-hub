//! Device HUB エントリポイント。
//!
//! 参照実装 (`src/main.rs`) と同じ構成:
//!   gpui_kit::application().with_assets(AllAssets) → gpui_kit::init →
//!   TitleBar::window_options → gpui_kit::open_window → Shell。
//!
//! 環境変数:
//!   DEVHUB_DEMO=1  常にデモデバイス
//!   DEVHUB_FAST=1  GPU ガラスを切る (フォールバック確認)

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

// mimalloc: Windows の既定ヒープは free 済み領域を OS に返さず、
// 長時間稼働でワーキングセットが膨らむ。mimalloc は未使用領域を
// 解放し、断片化も抑える (Zed も同じ構成)。
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use device_hub::app::{clear_component_chrome, Shell};
use device_hub::{prefs::Prefs, theme};
use gpui_kit::component::{Theme, ThemeMode, TitleBar};
use gpui_kit::*;

fn main() {
    // 常駐エージェント (窓なし): ホスト側設定のデバイスへ設定を押し込み続ける。
    if std::env::args().any(|a| a == "--agent") {
        device_hub::agent::run();
        return;
    }
    device_hub::rawinput::start(); // 実測ポーリングレート計測 (Raw Input)
    // USB 計測ワーカーの診断ログ — 未設定なら %TEMP% に既定で出す
    // (切断の OS コード採取用。ワーカー遷移のみで容量は微小)。
    if std::env::var_os("DEVHUB_USB_LOG").is_none() {
        if let Some(tmp) = std::env::var_os("TEMP") {
            let mut p = std::path::PathBuf::from(tmp);
            p.push("devhub_usb_worker.log");
            unsafe {
                std::env::set_var("DEVHUB_USB_LOG", p);
            }
        }
    }
    gpui_kit::application()
        .with_assets(gpui_kit::assets::AllAssets)
        .run(|cx| {
            gpui_kit::init(cx);
            // フォント登録は選択の後 — 使う書体だけ載せて常駐メモリを削る。
            let prefs = Prefs::load();
            theme::set_font_id(&prefs.font);
            theme::install_fonts(cx);
            let light = prefs.light();
            theme::set_light(light);
            Theme::change(
                if light {
                    ThemeMode::Light
                } else {
                    ThemeMode::Dark
                },
                None,
                cx,
            );
            clear_component_chrome(cx);

            let mut options = TitleBar::window_options();
            options.window_bounds = Some(WindowBounds::Windowed(Bounds::centered(
                None,
                size(px(1320.), px(840.)),
                cx,
            )));
            options.window_min_size = Some(size(px(1040.), px(640.)));
            // 背面に回ったら描画を間引く (ゲームの邪魔をしない)。
            options.inactive_frame_interval = Some(std::time::Duration::from_millis(250));
            options.app_id = Some("devicehub".into());
            // 窓キャプション — タスクマネージャーと Alt+Tab の表示名になる。
            if let Some(tb) = options.titlebar.as_mut() {
                tb.title = Some("Device HUB".into());
            }

            gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| Shell::new(window, cx))
            })
            .expect("failed to open window");
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
        });
}
