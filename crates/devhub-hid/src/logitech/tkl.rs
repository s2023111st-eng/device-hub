//! Logitech PRO X TKL RAPID (046d:c35b) — 磁気 (アナログ) キーの作動点とラピッドトリガー。
//!
//! 解析: G HUB の lghub_agent を Frida でフックして送信 HID++ を記録し、
//! agent の WebSocket (ws://localhost:9010) から設定カードを 1 項目ずつ変えて差分を取った
//! (docs/LOGITECH_HIDPP.md)。
//!
//! - 0x1B10 ControlList: fn0 → 件数、fn1(offset) → CID (u16 BE) × 8。
//!   **リストの並び = アナログ設定のキー番号**。CID は配置表 (tkl_layout.rs) と突き合わせる。
//! - 0x1B08 AnalogKeys: fn1 → ラピッドトリガー全体 ON/OFF、fn2 [on] → 設定。
//! - 0x8101 FileSystem (ホスト側の揮発設定):
//!   fn2 `[lenHi, lenLo, 0]` → fn3 16B チャンク × n → fn9
//!   `[0x1B, 0x08, sub, 0x02, len(3B BE), crc32(BE)]`。
//!   sub 0 = 作動点マップ、sub 1 = キー別 RT 量、sub 3 = 2B (`00 00`、G HUB と同じ空の束)。
//!   マップは `[count u16 BE][key, 値(0.1mm 単位)] × count`。
//!
//! ホスト側設定は読み戻せず、電源断で消える (本体はオンボード既定に戻る)。そのため
//! 設定はこのドライバが JSON に保存し、ドライバを開くたび (= アプリ/常駐エージェントが
//! キーボードを見つけるたび) に押し込む。G HUB の「ホストモード」と同じ動き。

use super::hidpp::{crc32, firmware, Hidpp};
use super::tkl_layout;
use devhub_core::backend::*;
use devhub_core::model::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

const F_ANALOG: u16 = 0x1B08;
const F_CONTROLS: u16 = 0x1B10;
const F_FS: u16 = 0x8101;
const F_LAYOUT: u16 = 0x4540;

/// アナログ設定を持つキー番号 (G HUB が送る作動点マップと同じ集合)。
/// 0x4B と 0x5D..0x63 (ゲームモード/メディアボタン) は含まない。
fn analog_indices() -> Vec<u8> {
    (0x00..=0x4A)
        .chain(0x4C..=0x5C)
        .chain(0x64..=0x6A)
        .chain([0x6E, 0x6F])
        .collect()
}

/// 0.1mm 単位。公式 G HUB の範囲 (keyTravelConfig)。
const AP_MIN: f32 = 0.1;
const AP_MAX: f32 = 4.0;
const RT_MIN: f32 = 0.1;
const RT_MAX: f32 = 2.0;
/// G HUB の既定プリセット (ANALOG_DEFAULT)。
const AP_DEFAULT: f32 = 2.0;
const RT_DEFAULT: f32 = 0.5;

/// 保存形式 (%APPDATA%\DeviceHub\logitech\pro-x-tkl-rapid.json)。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct AnalogState {
    pub rapid_trigger: bool,
    pub global_actuation: f32,
    pub global_rapid: f32,
    /// キー番号 → 作動点 (mm)。無いキーは global。
    #[serde(default)]
    pub actuation: BTreeMap<u8, f32>,
    /// キー番号 → RT 量 (mm)。無いキーは global。
    #[serde(default)]
    pub rapid: BTreeMap<u8, f32>,
}

impl Default for AnalogState {
    fn default() -> Self {
        Self {
            rapid_trigger: false,
            global_actuation: AP_DEFAULT,
            global_rapid: RT_DEFAULT,
            actuation: BTreeMap::new(),
            rapid: BTreeMap::new(),
        }
    }
}

impl AnalogState {
    fn ap(&self, k: u8) -> f32 {
        self.actuation
            .get(&k)
            .copied()
            .unwrap_or(self.global_actuation)
    }
    fn rt(&self, k: u8) -> f32 {
        self.rapid.get(&k).copied().unwrap_or(self.global_rapid)
    }
}

pub fn state_path() -> Option<PathBuf> {
    let base = std::env::var_os("APPDATA")?;
    let mut p = PathBuf::from(base);
    p.push("DeviceHub");
    p.push("logitech");
    p.push("pro-x-tkl-rapid.json");
    Some(p)
}

fn load_state() -> AnalogState {
    state_path()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save_state(s: &AnalogState) -> HubResult<()> {
    let Some(p) = state_path() else {
        return Ok(());
    };
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir).map_err(|e| HubError::Io(e.to_string()))?;
    }
    let tmp = p.with_extension("json.tmp");
    let body = serde_json::to_vec_pretty(s).map_err(|e| HubError::Io(e.to_string()))?;
    std::fs::write(&tmp, body).map_err(|e| HubError::Io(e.to_string()))?;
    std::fs::rename(&tmp, &p).map_err(|e| HubError::Io(e.to_string()))
}

/// mm → 0.1mm 単位のワイヤ値 (範囲で丸める)。
fn tenth(v: f32, min: f32, max: f32) -> u8 {
    (v.clamp(min, max) * 10.).round() as u8
}

/// `[count u16 BE][key, value] × n`
pub fn encode_map(pairs: &[(u8, u8)]) -> Vec<u8> {
    let mut b = Vec::with_capacity(2 + pairs.len() * 2);
    b.extend_from_slice(&(pairs.len() as u16).to_be_bytes());
    for (k, v) in pairs {
        b.push(*k);
        b.push(*v);
    }
    b
}

pub struct TklRapid {
    h: Hidpp,
    info: Mutex<DeviceInfo>,
    caps: Capabilities,
    /// layout index → キー番号。
    keys: Vec<u8>,
    state: Mutex<AnalogState>,
}

pub fn open(dev: hidapi::HidDevice, mut info: DeviceInfo) -> HubResult<TklRapid> {
    let h = Hidpp::new(dev, 0xFF, Duration::from_millis(800));
    h.ping(Duration::from_millis(800))
        .ok_or(HubError::Disconnected)?;
    info.firmware = firmware(&h);
    info.supported = true;
    let jis = h
        .call(F_LAYOUT, 0, &[])
        .map(|r| r[0] == 0x0A)
        .unwrap_or(true);
    let cids = control_list(&h)?;
    let table = if jis {
        tkl_layout::JIS
    } else {
        tkl_layout::ANSI
    };
    let analog = analog_indices();
    let mut layout = Vec::new();
    let mut keys = Vec::new();
    for &(cid, x, y, w, hh, label) in table {
        let Some(idx) = cids.iter().position(|c| *c == cid) else {
            continue;
        };
        let idx = idx as u8;
        if !analog.contains(&idx) {
            continue;
        }
        layout.push(LayoutKey {
            id: idx as u16,
            x,
            y,
            w,
            h: hh,
            label: label.into(),
        });
        keys.push(idx);
    }
    if layout.is_empty() {
        return Err(HubError::Protocol(
            "キー配置を特定できません (コントロールリスト不一致)".into(),
        ));
    }
    let caps = build_caps(layout);
    let d = TklRapid {
        h,
        info: Mutex::new(info),
        caps,
        keys,
        state: Mutex::new(load_state()),
    };
    // 開いた時点で保存済み設定を押し込む (起動時・差し直し時・常駐エージェント)。
    d.push()?;
    Ok(d)
}

fn control_list(h: &Hidpp) -> HubResult<Vec<u16>> {
    let n = h.call(F_CONTROLS, 0, &[])?[0] as usize;
    let mut out = Vec::with_capacity(n);
    let mut off = 0usize;
    while off < n {
        let r = h.call(F_CONTROLS, 1, &[off as u8])?;
        for i in 0..8 {
            out.push(u16::from_be_bytes([r[2 * i], r[2 * i + 1]]));
        }
        off += 8;
    }
    out.truncate(n);
    Ok(out)
}

fn build_caps(layout: Vec<LayoutKey>) -> Capabilities {
    let mut c = Capabilities::empty();
    c.keyboard = Some(KeyboardCaps {
        layout,
        // キー割り当て (0x1B05) は未解析 — 層を宣言しないので UI はキーマップ画面を出さない。
        layers: vec![],
        catalog: vec![],
        combos: false,
        perf: Some(PerfCaps {
            travel: AP_MAX,
            min: AP_MIN,
            step: 0.1,
            rapid_trigger: true,
            separate_release: false,
            rt_first: true,
            deadzones: false,
            continuous_rt: false,
            switch_types: vec![],
            switch_ids: vec![],
            per_key: true,
            live_travel: false,
        }),
        advanced: vec![],
        macros: None,
    });
    c.settings = vec![
        SettingSpec::toggle("rapid_trigger", section::KEYBOARD, "ラピッドトリガー")
            .help("全キー共通の ON/OFF (G HUB と同じく本体はキーボード単位で切り替え)"),
        SettingSpec::slider(
            "global_actuation",
            section::KEYBOARD,
            "既定の作動点",
            AP_MIN,
            AP_MAX,
            0.1,
            "mm",
        )
        .help("個別設定の無いキーの作動点"),
        SettingSpec::slider(
            "global_rapid",
            section::KEYBOARD,
            "既定のラピッドトリガー量",
            RT_MIN,
            RT_MAX,
            0.1,
            "mm",
        )
        .help("個別設定の無いキーの再作動/リセット量"),
    ];
    c.actions = vec![
        ActionSpec::new("reset_analog", section::KEYBOARD, "アナログ設定を既定に戻す")
            .danger("全キーの作動点 2.0mm / RT 0.5mm / ラピッドトリガー OFF に戻します"),
        ActionSpec::new("resync", section::KEYBOARD, "キーボードへ再送信")
            .help("保存済みの設定を本体へ送り直す (スリープ復帰後などに)"),
    ];
    c
}

impl TklRapid {
    /// 保存済み設定を本体へ送る (G HUB と同じ 3 ファイル + RT スイッチ)。
    fn push(&self) -> HubResult<()> {
        let s = self.state.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let idx = analog_indices();
        let ap: Vec<(u8, u8)> = idx
            .iter()
            .map(|&k| (k, tenth(s.ap(k), AP_MIN, AP_MAX)))
            .collect();
        let rt: Vec<(u8, u8)> = idx
            .iter()
            .map(|&k| (k, tenth(s.rt(k), RT_MIN, RT_MAX)))
            .collect();
        self.send_file(0, &encode_map(&ap))?;
        self.send_file(1, &encode_map(&rt))?;
        self.send_file(3, &[0, 0])?;
        self.h.call(F_ANALOG, 2, &[s.rapid_trigger as u8])?;
        Ok(())
    }

    fn send_file(&self, sub: u8, data: &[u8]) -> HubResult<()> {
        let len = data.len();
        self.h
            .call(F_FS, 2, &[(len >> 8) as u8, len as u8, 0])?;
        for chunk in data.chunks(16) {
            self.h.call(F_FS, 3, chunk)?;
        }
        let crc = crc32(data).to_be_bytes();
        let p = [
            0x1B,
            0x08,
            sub,
            0x02,
            (len >> 16) as u8,
            (len >> 8) as u8,
            len as u8,
            crc[0],
            crc[1],
            crc[2],
            crc[3],
        ];
        self.h.call(F_FS, 9, &p)?;
        Ok(())
    }

    fn update(&self, f: impl FnOnce(&mut AnalogState)) -> HubResult<()> {
        let snapshot = {
            let mut s = self.state.lock().unwrap_or_else(|p| p.into_inner());
            f(&mut s);
            s.clone()
        };
        save_state(&snapshot)?;
        self.push()
    }

    fn perf_of(&self, s: &AnalogState, k: u8) -> KeyPerf {
        let ap = s.ap(k);
        let rt = s.rt(k);
        KeyPerf {
            mode: if s.rapid_trigger {
                TriggerMode::Rapid
            } else {
                TriggerMode::Normal
            },
            actuation: ap,
            release: None,
            rt_first: ap,
            rt_press: rt,
            rt_release: rt,
            top_deadzone: 0.,
            bottom_deadzone: 0.,
            continuous: false,
            switch_type: 0,
        }
    }
}

impl DeviceDriver for TklRapid {
    fn info(&self) -> DeviceInfo {
        self.info.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    fn caps(&self) -> Capabilities {
        self.caps.clone()
    }

    fn read(&self) -> HubResult<DeviceState> {
        // 生存確認 (抜去を UI に伝える)。RT スイッチは本体から読める唯一の値。
        let on = self.h.call(F_ANALOG, 1, &[])?[0] != 0;
        let s = {
            let mut s = self.state.lock().unwrap_or_else(|p| p.into_inner());
            if s.rapid_trigger != on {
                // 本体側が既定に戻っている (スリープ復帰・別アプリ) — 保存値を送り直す。
                drop(s);
                self.push()?;
                s = self.state.lock().unwrap_or_else(|p| p.into_inner());
            }
            s.clone()
        };
        let mut settings = BTreeMap::new();
        settings.insert("rapid_trigger".into(), SettingValue::Bool(s.rapid_trigger));
        settings.insert(
            "global_actuation".into(),
            SettingValue::Num(s.global_actuation),
        );
        settings.insert("global_rapid".into(), SettingValue::Num(s.global_rapid));
        let perf = self.keys.iter().map(|&k| self.perf_of(&s, k)).collect();
        Ok(DeviceState {
            profile: ProfileState::default(),
            settings,
            mouse: None,
            keyboard: Some(KeyboardState {
                keymap: vec![],
                perf,
                advanced: vec![],
                macros: vec![],
            }),
            lighting: BTreeMap::new(),
            peq: None,
        })
    }

    fn apply(&self, change: &Change) -> HubResult<()> {
        match change {
            Change::Setting { id, value } => match id.as_str() {
                "rapid_trigger" => self.update(|s| s.rapid_trigger = value.as_bool()),
                "global_actuation" => {
                    let v = value.as_num().clamp(AP_MIN, AP_MAX);
                    self.update(|s| s.global_actuation = v)
                }
                "global_rapid" => {
                    let v = value.as_num().clamp(RT_MIN, RT_MAX);
                    self.update(|s| s.global_rapid = v)
                }
                _ => Err(HubError::Unsupported("setting")),
            },
            Change::KeyPerf { keys, perf } => {
                let ids: Vec<u8> = keys
                    .iter()
                    .filter_map(|i| self.keys.get(*i).copied())
                    .collect();
                if ids.is_empty() {
                    return Err(HubError::Invalid("キーが選ばれていません".into()));
                }
                // UI は RT 時に rt_first、通常時に actuation を編集する。どちらが動いたかを
                // 先頭キーの現在値との差で判断する (片方だけ動かした操作の意図を保つ)。
                self.update(|s| {
                    let cur = self.perf_of(s, ids[0]);
                    let ap = if (perf.rt_first - cur.rt_first).abs() > 1e-4 {
                        perf.rt_first
                    } else {
                        perf.actuation
                    };
                    let rt = if (perf.rt_press - cur.rt_press).abs() > 1e-4 {
                        perf.rt_press
                    } else {
                        perf.rt_release
                    };
                    for k in &ids {
                        s.actuation.insert(*k, ap.clamp(AP_MIN, AP_MAX));
                        s.rapid.insert(*k, rt.clamp(RT_MIN, RT_MAX));
                    }
                    // 方式の切替は本体仕様上キーボード全体に効く。
                    s.rapid_trigger = perf.mode == TriggerMode::Rapid;
                })
            }
            _ => Err(HubError::Unsupported("change")),
        }
    }

    fn run_action(&self, id: &str, _file: Option<&std::path::Path>) -> HubResult<ActionOutcome> {
        match id {
            "reset_analog" => {
                self.update(|s| *s = AnalogState::default())?;
                Ok(ActionOutcome {
                    message: Some("アナログ設定を既定に戻しました".into()),
                    reload: true,
                })
            }
            "resync" => {
                self.push()?;
                Ok(ActionOutcome {
                    message: Some("キーボードへ設定を送り直しました".into()),
                    reload: false,
                })
            }
            // 常駐エージェントの定期確認 (UI には出さない)。本体の RT スイッチが保存値と
            // 食い違っていたら (スリープ復帰で既定に戻った等) 全設定を送り直す。
            "__agent_tick" => {
                let on = self
                    .h
                    .call(F_ANALOG, 1, &[])
                    .map_err(|_| HubError::Disconnected)?[0]
                    != 0;
                let want = self
                    .state
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .rapid_trigger;
                if on != want {
                    self.push()?;
                }
                Ok(ActionOutcome::default())
            }
            _ => Err(HubError::Unsupported("action")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ap_map_matches_ghub_capture() {
        // G HUB が送った作動点マップ (全キー 2.0mm、WASD = 0x1D/0x21/0x22/0x23 は 0.1mm)
        // と同じバイト列・CRC になること (実機キャプチャ 2026-11-19)。
        let mut s = AnalogState {
            rapid_trigger: true,
            global_actuation: 2.0,
            global_rapid: 0.5,
            ..Default::default()
        };
        for k in [0x1D, 0x21, 0x22, 0x23] {
            s.actuation.insert(k, 0.1);
        }
        let ap: Vec<(u8, u8)> = analog_indices()
            .iter()
            .map(|&k| (k, tenth(s.ap(k), AP_MIN, AP_MAX)))
            .collect();
        let b = encode_map(&ap);
        assert_eq!(b.len(), 0xCC);
        assert_eq!(&b[..6], &[0x00, 0x65, 0x00, 0x14, 0x01, 0x14]);
        assert_eq!(crc32(&b), 0x1651_7288);
    }

    #[test]
    fn layout_tables_have_unique_cids() {
        for t in [tkl_layout::JIS, tkl_layout::ANSI] {
            let mut c: Vec<u16> = t.iter().map(|k| k.0).collect();
            c.sort();
            let n = c.len();
            c.dedup();
            assert_eq!(c.len(), n);
        }
    }
}
