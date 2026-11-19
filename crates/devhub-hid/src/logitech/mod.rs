//! Logitech G (HID++ 2.0) — G HUB の代わり。
//!
//! - `superlight` … PRO X SUPERLIGHT (オンボードプロファイルに保存)
//! - `tkl` … PRO X TKL RAPID (磁気キーの作動点/ラピッドトリガー、ホスト側設定を押し込む)
//!
//! プロトコルの解析記録は docs/LOGITECH_HIDPP.md。

pub mod hidpp;
pub mod superlight;
pub mod tkl;
mod tkl_layout;

use devhub_core::backend::*;
use devhub_core::model::*;
use std::sync::Arc;

/// registry の model で機種を振り分ける。
pub fn open(
    dev: hidapi::HidDevice,
    info: DeviceInfo,
    events: Option<Arc<dyn Fn(HubEvent) + Send + Sync>>,
) -> HubResult<Arc<dyn DeviceDriver>> {
    match info.model.as_str() {
        "pro-x-tkl-rapid" => Ok(Arc::new(tkl::open(dev, info)?)),
        "pro-x-superlight" => Ok(Arc::new(superlight::open(dev, info, events)?)),
        _ => Err(HubError::NotImplemented("この Logitech 機種")),
    }
}
