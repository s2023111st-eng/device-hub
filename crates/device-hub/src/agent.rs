//! 常駐エージェント (`device-hub.exe --agent`) — G HUB の lghub_agent の代わり。
//!
//! 窓を出さずに常駐し、次の 2 つを受け持つ:
//!
//! 1. **ホスト側設定の維持** — 本体に保存できないデバイス (Logitech PRO X TKL RAPID の
//!    作動点/ラピッドトリガー) へ保存済み設定を押し込み続ける:
//!    デバイスが現れたとき (ドライバを開くと自動送信) / スリープ復帰 (`resync`) /
//!    本体の RT スイッチが保存値と食い違ったとき (`__agent_tick`)。
//! 2. **プロセス連動 (ゲームごとの切替)** — 「自動化」画面のルール (`prefs.json` の
//!    `proc_rules` / `profiles`) を、窓を閉じていても評価して適用する。判定は画面側と
//!    同じ `procmon::eval_rules`。エージェント稼働中は画面側の評価を止める (二重適用しない)。
//!    適用できなかった変更 (マウス休眠中など) は保留して、受理されるまで毎 tick 再送する。
//!
//! 多重起動はロックファイルの排他オープンで防ぐ (`is_running` も同じファイルで判定)。

use crate::prefs::Prefs;
use crate::procmon::{self, ProcApplied};
use devhub_core::backend::{DeviceDriver, HubBackend, HubError};
use devhub_core::model::{Change, DeviceId};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// ホスト側設定を維持する機種 (registry の model)。
const HOST_MANAGED: &[&str] = &["pro-x-tkl-rapid"];
/// 前面アプリの評価間隔。デバイス列挙と設定の維持は `SCAN_EVERY` tick ごと。
const TICK: Duration = Duration::from_secs(1);
const SCAN_EVERY: u32 = 5;
/// `prefs.json` (画面で編集されたルール) を読み直す間隔 (tick)。
const PREFS_EVERY: u32 = 3;

fn lock_path() -> Option<std::path::PathBuf> {
    let mut p = std::path::PathBuf::from(std::env::var_os("APPDATA")?);
    p.push("DeviceHub");
    p.push("agent.lock");
    Some(p)
}

fn open_lock() -> std::io::Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    let p = lock_path().ok_or_else(|| std::io::Error::other("APPDATA"))?;
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .open(p)
}

/// エージェントが常駐中か (ロックファイルを排他で開けなければ誰かが握っている)。
pub fn is_running() -> bool {
    matches!(open_lock(), Err(e) if e.raw_os_error() == Some(32))
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

struct Agent {
    hub: Arc<dyn HubBackend>,
    /// ホスト側設定を維持中のドライバ。
    managed: HashMap<String, Arc<dyn DeviceDriver>>,
    prefs: Prefs,
    /// 前面アプリ (2 tick 連続で同じときだけ評価する — 高速 alt-tab の中間を無視)。
    fg: Option<String>,
    states: HashMap<String, ProcApplied>,
    /// デバイス → まだ受理されていない変更 (先頭から順に送る)。
    pending: HashMap<String, (String, Vec<Change>)>,
}

pub fn run() {
    let Ok(_lock) = open_lock() else {
        return; // 既に常駐している
    };
    let hub = loop {
        match devhub_hid::connect() {
            Ok(h) => break h,
            Err(_) => std::thread::sleep(TICK * SCAN_EVERY),
        }
    };
    log("agent started");
    let mut a = Agent {
        hub,
        managed: HashMap::new(),
        prefs: Prefs::load(),
        fg: None,
        states: HashMap::new(),
        pending: HashMap::new(),
    };
    // 起動直後は「どのゲームも前面でない」状態に揃える: 各ルールの復帰先を一度適用する
    // (ゲーム中に電源を落とした場合などでも、デスクトップ用の設定から始める)。
    a.queue_baseline();
    let mut n: u32 = 0;
    let mut last = Instant::now();
    loop {
        // スリープ/休止から戻ると TICK よりずっと長い時間が過ぎている。
        let resumed = last.elapsed() > TICK * SCAN_EVERY * 3;
        last = Instant::now();
        if n.is_multiple_of(PREFS_EVERY) {
            a.prefs = Prefs::load();
        }
        if n.is_multiple_of(SCAN_EVERY) || resumed {
            a.maintain(resumed);
        }
        a.rules();
        a.drain();
        n = n.wrapping_add(1);
        std::thread::sleep(TICK);
    }
}

impl Agent {
    fn queue_baseline(&mut self) {
        let mut seen = std::collections::HashSet::new();
        let targets: Vec<(String, String)> = self
            .prefs
            .proc_rules
            .iter()
            .filter(|r| r.enabled && !r.on_leave.is_empty())
            .filter(|r| seen.insert(r.on_leave.clone()))
            .map(|r| (r.device.clone(), r.on_leave.clone()))
            .collect();
        for (device, name) in targets {
            self.queue_profile(&device, &name);
        }
    }

    /// プロファイル `name` の全対象デバイスへの変更を保留キューへ。
    /// 同じデバイスへの未送信分は新しいプロファイルで置き換える (最後に決まったものが勝つ)。
    fn queue_profile(&mut self, device: &str, name: &str) {
        let Some(p) = self
            .prefs
            .profiles
            .iter()
            .find(|p| p.name == name && p.devices.iter().any(|t| t.device == device))
        else {
            log(&format!("profile '{name}' not found"));
            return;
        };
        for t in &p.devices {
            self.pending
                .insert(t.device.clone(), (name.to_string(), t.changes.clone()));
        }
    }

    /// 前面アプリの遷移を評価する。
    fn rules(&mut self) {
        let fg = procmon::foreground_app().filter(|s| !s.is_empty());
        if fg != self.fg {
            self.fg = fg;
            return;
        }
        let Some(fg) = fg else { return };
        let ev = procmon::eval_rules(&self.prefs.proc_rules, &fg, &self.states);
        for (device, name) in &ev.leaves {
            log(&format!("leave -> '{name}' ({fg})"));
            self.queue_profile(device, name);
        }
        for (device, names) in &ev.enters {
            for name in names {
                log(&format!("enter '{name}' ({fg})"));
                self.queue_profile(device, name);
            }
        }
        self.states = ev.now;
    }

    /// 保留中の変更を送る。受理されなかったら残りを次の tick に回す。
    fn drain(&mut self) {
        let devices: Vec<String> = self.pending.keys().cloned().collect();
        for dev in devices {
            let Some((name, changes)) = self.pending.remove(&dev) else {
                continue;
            };
            let id = DeviceId(dev.clone());
            let drv = match self.hub.driver(&id) {
                Ok(d) => d,
                Err(_) => {
                    // 未接続/未列挙 — 次の列挙後に再試行。
                    self.pending.insert(dev, (name, changes));
                    continue;
                }
            };
            let mut rest = changes.into_iter();
            let mut failed: Option<Change> = None;
            for c in rest.by_ref() {
                if let Err(e) = drv.apply(&c) {
                    if matches!(e, HubError::Disconnected) {
                        self.hub.invalidate(&id);
                    }
                    failed = Some(c);
                    break;
                }
            }
            match failed {
                Some(c) => {
                    let mut left = vec![c];
                    left.extend(rest);
                    self.pending.insert(dev, (name, left));
                }
                None => log(&format!("applied '{name}' to {dev}")),
            }
        }
    }

    /// デバイス列挙とホスト側設定の維持。
    fn maintain(&mut self, resumed: bool) {
        let Ok(list) = self.hub.scan() else {
            return;
        };
        let present: Vec<_> = list
            .iter()
            .filter(|d| d.supported && HOST_MANAGED.contains(&d.model.as_str()))
            .collect();
        let hub = self.hub.clone();
        self.managed.retain(|id, _| {
            let keep = present.iter().any(|d| &d.id.0 == id);
            if !keep {
                hub.invalidate(&DeviceId(id.clone()));
                log(&format!("gone {id}"));
            }
            keep
        });
        for d in present {
            if let Some(drv) = self.managed.get(&d.id.0) {
                let action = if resumed { "resync" } else { "__agent_tick" };
                if let Err(e) = drv.run_action(action, None) {
                    log(&format!("{action} {} failed: {e}", d.id));
                    if matches!(e, HubError::Disconnected) {
                        self.hub.invalidate(&d.id);
                        self.managed.remove(&d.id.0);
                    }
                } else if resumed {
                    log(&format!("resync after resume {}", d.id));
                }
                continue;
            }
            // ドライバを開く = 保存済み設定の押し込み。
            match self.hub.driver(&d.id) {
                Ok(drv) => {
                    log(&format!("applied stored settings {} ({})", d.name, d.id));
                    self.managed.insert(d.id.0.clone(), drv);
                }
                Err(e) => log(&format!("open {} failed: {e}", d.id)),
            }
        }
    }
}
