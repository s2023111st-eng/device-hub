//! PRO X SUPERLIGHT の書き込み往復確認 (オンボードプロファイル)。
//!
//! ポーリングを 500Hz に書いて読み戻し、元の値へ戻して読み戻す。
//! ボタン/DPI は触らない。書き込み前にドライバが全セクタをバックアップする。
//!
//! `cargo run -p devhub-hid --example logie2e`

use devhub_core::model::*;

fn main() {
    let hub = devhub_hid::connect().expect("hid");
    let d = hub
        .scan()
        .expect("scan")
        .into_iter()
        .find(|d| d.model == "pro-x-superlight")
        .expect("PRO X SUPERLIGHT が見つかりません");
    let drv = hub.driver(&d.id).expect("open");
    let rate = |s: &DeviceState| s.settings.get("polling").map(|v| v.as_choice());
    let before = drv.read().expect("read");
    let m = before.mouse.as_ref().unwrap();
    println!(
        "before: polling={:?} dpi={:?} active={} buttons={:?}",
        rate(&before),
        m.stages.iter().map(|s| s.x).collect::<Vec<_>>(),
        m.active_stage,
        m.buttons
    );
    let orig = rate(&before).unwrap_or(1000);
    let test = if orig == 500 { 1000 } else { 500 };
    for want in [test, orig] {
        drv.apply(&Change::Setting {
            id: "polling".into(),
            value: SettingValue::Choice(want),
        })
        .expect("apply");
        let s = drv.read().expect("read back");
        println!("wrote {want} Hz -> read back {:?}", rate(&s));
        assert_eq!(rate(&s), Some(want));
    }
    let after = drv.read().expect("read");
    assert_eq!(after.mouse, before.mouse, "DPI/ボタンが変わっていないこと");
    println!("OK: round trip complete, DPI/buttons unchanged");
}
