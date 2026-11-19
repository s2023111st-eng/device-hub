//! Logitech (HID++ 2.0) ドライバの実機確認: 列挙 → 開く (TKL は保存設定を送信) → 読み出し。
//!
//! `cargo run -p devhub-hid --example logi`

fn main() {
    let hub = devhub_hid::connect().expect("hid");
    let list = hub.scan().expect("scan");
    for d in list
        .iter()
        .filter(|d| d.driver == devhub_core::model::DriverKind::Logitech)
    {
        println!("== {} [{}] {:?} supported={}", d.name, d.id, d.transport, d.supported);
        let drv = match hub.driver(&d.id) {
            Ok(x) => x,
            Err(e) => {
                println!("  open: {e}");
                continue;
            }
        };
        let i = drv.info();
        println!(
            "  fw={:?} battery={:?} asleep={}",
            i.firmware, i.battery, i.asleep
        );
        let caps = drv.caps();
        if let Some(k) = &caps.keyboard {
            println!("  keyboard: {} keys", k.layout.len());
        }
        match drv.read() {
            Ok(s) => {
                println!("  settings: {:?}", s.settings);
                if let Some(m) = &s.mouse {
                    println!(
                        "  dpi: {:?} active={} buttons={:?}",
                        m.stages.iter().map(|s| s.x).collect::<Vec<_>>(),
                        m.active_stage,
                        m.buttons
                    );
                }
                if let (Some(k), Some(kc)) = (&s.keyboard, &caps.keyboard) {
                    for (i, p) in k.perf.iter().enumerate() {
                        if (p.actuation - 2.0).abs() > 1e-3 || (p.rt_press - 0.5).abs() > 1e-3 {
                            println!(
                                "  key {:>8} (idx {:#04x}): AP {:.1} RT {:.1} {:?}",
                                kc.layout[i].label, kc.layout[i].id, p.actuation, p.rt_press, p.mode
                            );
                        }
                    }
                }
            }
            Err(e) => println!("  read: {e}"),
        }
    }
}
