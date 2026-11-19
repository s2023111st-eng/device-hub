//! 常駐エージェント (`device-hub.exe --agent`) — G HUB の lghub_agent の代わり。
//!
//! 窓を出さずに常駐し、ホスト側にしか設定を持てないデバイス (Logitech PRO X TKL RAPID の
//! 作動点/ラピッドトリガー) へ、保存済み設定を押し込み続ける:
//!
//! - デバイスが現れたとき (起動・差し直し) — ドライバを開くと自動で送られる
//! - スリープ復帰 (ループの経過時間が大きく飛んだ) — `resync`
//! - 本体の RT スイッチが保存値と食い違ったとき — `__agent_tick`
//!
//! 設定の編集は通常の Device HUB 画面で行う (保存先 JSON をエージェントも読む)。
//! 多重起動はロックファイルの排他オープンで防ぐ。オンボード保存のデバイス (マウス) は
//! 本体が設定を持つので何もしない。

use devhub_core::backend::{DeviceDriver, HubBackend, HubError};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// エージェントが面倒を見る機種 (registry の model)。
const HOST_MANAGED: &[&str] = &["pro-x-tkl-rapid"];
const TICK: Duration = Duration::from_secs(5);

fn lock_file() -> Option<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    let mut p = std::path::PathBuf::from(std::env::var_os("APPDATA")?);
    p.push("DeviceHub");
    std::fs::create_dir_all(&p).ok()?;
    p.push("agent.lock");
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .open(p)
        .ok()
}

fn log(msg: &str) {
    let Some(tmp) = std::env::var_os("TEMP") else {
        return;
    };
    let mut p = std::path::PathBuf::from(tmp);
    p.push("devhub_agent.log");
    // 際限なく伸びないよう 256KB を超えたら作り直す。
    if std::fs::metadata(&p).is_ok_and(|m| m.len() > 256 * 1024) {
        let _ = std::fs::remove_file(&p);
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = writeln!(f, "{t} {msg}");
    }
}

pub fn run() {
    let Some(_lock) = lock_file() else {
        return; // 既に常駐している
    };
    let hub = loop {
        match devhub_hid::connect() {
            Ok(h) => break h,
            Err(_) => std::thread::sleep(TICK),
        }
    };
    log("agent started");
    let mut open: HashMap<String, Arc<dyn DeviceDriver>> = HashMap::new();
    let mut last = Instant::now();
    loop {
        // スリープ/休止から戻ると TICK よりずっと長い時間が過ぎている。
        let resumed = last.elapsed() > TICK * 3;
        last = Instant::now();
        tick(hub.as_ref(), &mut open, resumed);
        std::thread::sleep(TICK);
    }
}

fn tick(hub: &dyn HubBackend, open: &mut HashMap<String, Arc<dyn DeviceDriver>>, resumed: bool) {
    let Ok(list) = hub.scan() else {
        return;
    };
    let present: Vec<_> = list
        .iter()
        .filter(|d| d.supported && HOST_MANAGED.contains(&d.model.as_str()))
        .collect();
    open.retain(|id, _| {
        let keep = present.iter().any(|d| &d.id.0 == id);
        if !keep {
            hub.invalidate(&devhub_core::model::DeviceId(id.clone()));
            log(&format!("gone {id}"));
        }
        keep
    });
    for d in present {
        if let Some(drv) = open.get(&d.id.0) {
            let action = if resumed { "resync" } else { "__agent_tick" };
            match drv.run_action(action, None) {
                Ok(_) => {
                    if resumed {
                        log(&format!("resync after resume {}", d.id));
                    }
                }
                Err(e) => {
                    log(&format!("{action} {} failed: {e}", d.id));
                    if matches!(e, HubError::Disconnected) {
                        hub.invalidate(&d.id);
                        open.remove(&d.id.0);
                    }
                }
            }
            continue;
        }
        // ドライバを開く = 保存済み設定の押し込み。
        match hub.driver(&d.id) {
            Ok(drv) => {
                log(&format!("applied {} ({})", d.name, d.id));
                open.insert(d.id.0.clone(), drv);
            }
            Err(e) => log(&format!("open {} failed: {e}", d.id)),
        }
    }
}
