//! Logitech PRO X SUPERLIGHT (LIGHTSPEED 受信機 046d:c547 / 有線 046d:c094, WPID 0x4093)。
//!
//! 設定はすべて **オンボードプロファイル (HID++ 0x8100)** に書く。マウス本体に保存されるので
//! G HUB もこのアプリも常駐不要で、電源を入れ直しても保持される。
//!
//! - 0x8100 fn0 getInfo `[memModel, format, macroFmt, profiles, _, buttons, sectors, sectorSize u16 BE]`
//!   fn1 setMode [1=onboard, 2=host] / fn2 getMode / fn3 setCurrentProfile [sector BE] /
//!   fn4 getCurrentProfile → sector BE / fn5 memoryRead [sector, offset] → 16B /
//!   fn6 memoryAddrWrite [sector, offset, len] / fn7 memoryWrite 16B / fn8 memoryWriteEnd /
//!   fn12 setCurrentDpiIndex [i]
//! - セクタ 0x0000 = プロファイル一覧 `[sector BE, enabled, 0xFF] × n` + 0xFFFF 終端。
//!   0x01xx は ROM (工場出荷値)。フラッシュ未使用ならディレクトリは CRC 不一致 (全 0xFF)。
//! - プロファイル (format 1–5 = base v1 — SUPERLIGHT は format 4):
//!   0x00 レポート間隔 ms / 0x01 既定 DPI index / 0x02 シフト DPI index /
//!   0x03 + 2n DPI u16 LE ×5 (0 = 未使用) / 0x20 + 4n ボタン ×16 /
//!   末尾 2B = CRC-16/CCITT-FALSE (BE)。
//! - ボタン 4B: `80 01 mask(BE16)` マウス / `80 02 mods key` キー / `80 03 usage(BE16)` コンシューマ /
//!   `90 xx 00 00` 特殊 (03 DPI+, 04 DPI-, 05 DPI 循環, 07 DPI シフト, 0A プロファイル循環) /
//!   `FF FF FF FF` 無効。
//!
//! 出典: OpenMouse-Project/mouse-protocol (docs/logitech-onboard-profiles.md)、libratbag の
//! hidpp20 実装、G HUB の機種定義 (pro_x_wireless_mouse)。書き込み前に全セクタを
//! %APPDATA%\DeviceHub\logitech\backup\ へ保存し、書き込み後は読み戻して照合する。

use super::hidpp::{crc16, firmware, Hidpp};
use devhub_core::backend::*;
use devhub_core::model::*;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const F_PROFILES: u16 = 0x8100;
const F_UNIFIED_BATTERY: u16 = 0x1004;
const F_BATTERY: u16 = 0x1000;
const F_REPORT_RATE: u16 = 0x8060;

const DPI_MIN: u32 = 100;
const DPI_MAX: u32 = 25600;
const STAGES: usize = 5;
const BUTTONS: usize = 5;
const RATES: [u32; 4] = [125, 250, 500, 1000];
/// 無線の応答待ち上限 (ms)。リンク復帰の初回応答 ~1.25s を待てる長さ。
const WAKE_MS: u64 = 1600;
/// オンボードのプロファイル枠 (セクタ 0x0001–0x0005)。
const PROFILES: usize = 5;
const NAME_AT: usize = 0xA0;
const NAME_LEN: usize = 0x30;

type Events = Option<Arc<dyn Fn(HubEvent) + Send + Sync>>;

#[derive(Clone, Copy, Debug)]
struct OnboardInfo {
    format: u8,
    sector_size: usize,
}

pub struct Superlight {
    h: Hidpp,
    info: Mutex<DeviceInfo>,
    caps: Capabilities,
    events: Events,
    onboard: Mutex<Option<OnboardInfo>>,
    /// 直近に読んだプロファイル (セクタ番号, 中身)。apply はこれを書き換えて保存する。
    profile: Mutex<Option<(u16, Vec<u8>)>>,
    names: Mutex<Option<Vec<String>>>,
    backed_up: Mutex<bool>,
}

pub fn open(dev: hidapi::HidDevice, info: DeviceInfo, events: Events) -> HubResult<Superlight> {
    let wireless = info.transport == Transport::Wireless24G;
    // 無線は普段 ~27ms で応答するが、しばらく触らずにいると最初の 1 回だけ
    // リンク復帰で ~1.25s かかる (実機計測)。それより短いと休眠と誤判定する。
    let h = Hidpp::new(
        dev,
        if wireless { 1 } else { 0xFF },
        Duration::from_millis(if wireless { WAKE_MS } else { 800 }),
    );
    if wireless && h.ping(Duration::from_millis(WAKE_MS)).is_none() {
        // スロット 1 が黙っていれば他のペアリングスロットを探す (既定 1)。
        let found = (2..=6u8).find(|&i| {
            h.set_index(i);
            h.ping(Duration::from_millis(300)).is_some()
        });
        h.set_index(found.unwrap_or(1));
    }
    let d = Superlight {
        h,
        info: Mutex::new(info),
        caps: build_caps(),
        events,
        onboard: Mutex::new(None),
        profile: Mutex::new(None),
        names: Mutex::new(None),
        backed_up: Mutex::new(false),
    };
    d.refresh_info();
    Ok(d)
}

fn build_caps() -> Capabilities {
    let mut c = Capabilities::empty();
    c.battery = true;
    // 本体の 5 枠。切替は揮発 (setCurrentProfile) なのでゲームごとの自動切替に使える。
    c.profiles = Some(ProfileCaps {
        count: PROFILES as u8,
        renamable: true,
    });
    c.mouse = Some(MouseCaps {
        dpi_min: DPI_MIN,
        dpi_max: DPI_MAX,
        dpi_step: 50,
        min_stages: 1,
        max_stages: STAGES,
        xy_split: false,
        stage_colors: false,
        stage_toggle: false,
        buttons: ["左ボタン", "右ボタン", "ホイールクリック", "戻る (サイド後)", "進む (サイド前)"]
            .iter()
            .map(|n| ButtonSlot { name: (*n).into() })
            .collect(),
        button_kinds: vec![
            ButtonActionKind::Mouse,
            ButtonActionKind::Key,
            ButtonActionKind::Media,
            ButtonActionKind::Dpi,
            ButtonActionKind::ProfileCycle,
            ButtonActionKind::Disabled,
        ],
        require_left_click: true,
        macros: None,
        live_buttons: vec![],
        adaptive_dpi: None,
    });
    c.settings = vec![SettingSpec::choice(
        "polling",
        section::PERFORMANCE,
        "ポーリングレート",
        RATES
            .iter()
            .map(|r| ChoiceOpt::new(*r as i64, format!("{r} Hz")))
            .collect(),
    )
    .help("オンボードプロファイルに保存 (G HUB 不要で保持)")];
    c.actions = vec![ActionSpec::new(
        "backup_onboard",
        section::SYSTEM,
        "オンボードメモリをバックアップ",
    )
    .help("全セクタを %APPDATA%\\DeviceHub\\logitech\\backup へ保存")];
    c
}

impl Superlight {
    fn wireless(&self) -> bool {
        self.info.lock().unwrap_or_else(|p| p.into_inner()).transport == Transport::Wireless24G
    }

    /// 無線で応答が無いのは休眠 — ドライバを捨てさせない (Disconnected にしない)。
    fn err(&self, e: HubError) -> HubError {
        if matches!(e, HubError::Disconnected) && self.wireless() {
            self.set_asleep(true);
            HubError::Protocol("マウスが応答しません — 動かして起こしてください".into())
        } else {
            e
        }
    }

    fn call(&self, id: u16, func: u8, params: &[u8]) -> HubResult<[u8; 16]> {
        let r = self.h.call(id, func, params).map_err(|e| self.err(e))?;
        self.set_asleep(false);
        Ok(r)
    }

    fn set_asleep(&self, asleep: bool) {
        let mut i = self.info.lock().unwrap_or_else(|p| p.into_inner());
        if i.asleep == asleep || i.transport != Transport::Wireless24G {
            return;
        }
        i.asleep = asleep;
        if asleep {
            // 再接続でフィーチャー表が変わり得る (ファーム更新後など)。
            self.h.forget_features();
        }
        let snap = i.clone();
        drop(i);
        if let Some(ev) = &self.events {
            ev(HubEvent::Info(snap.id.clone(), Box::new(snap)));
        }
    }

    fn refresh_info(&self) {
        let alive = self.h.ping(Duration::from_millis(WAKE_MS)).is_some();
        let mut i = self.info.lock().unwrap_or_else(|p| p.into_inner());
        i.supported = true;
        i.asleep = !alive && i.transport == Transport::Wireless24G;
        if alive {
            if let Some(fw) = firmware(&self.h) {
                i.firmware = Some(fw);
            }
            drop(i);
            if let Ok(b) = self.battery() {
                self.info.lock().unwrap_or_else(|p| p.into_inner()).battery = Some(b);
            }
        }
    }

    fn onboard_info(&self) -> HubResult<OnboardInfo> {
        if let Some(o) = *self.onboard.lock().unwrap_or_else(|p| p.into_inner()) {
            return Ok(o);
        }
        let r = self.call(F_PROFILES, 0, &[])?;
        let o = OnboardInfo {
            format: r[1],
            sector_size: u16::from_be_bytes([r[7], r[8]]) as usize,
        };
        if !(1..=5).contains(&o.format) {
            return Err(HubError::Protocol(format!(
                "未対応のプロファイル形式 {} (base v1 = 1..5 のみ)",
                o.format
            )));
        }
        if !(64..=1024).contains(&o.sector_size) {
            return Err(HubError::Protocol(format!(
                "セクタ長が不正です ({})",
                o.sector_size
            )));
        }
        *self.onboard.lock().unwrap_or_else(|p| p.into_inner()) = Some(o);
        Ok(o)
    }

    fn read_sector(&self, sector: u16, size: usize) -> HubResult<Vec<u8>> {
        let mut out = vec![0u8; size];
        let mut off = 0usize;
        while off < size {
            // セクタ末尾を越える読みは拒否されるので最後は size-16 から。
            let at = off.min(size - 16);
            let r = self.call(
                F_PROFILES,
                5,
                &[
                    (sector >> 8) as u8,
                    sector as u8,
                    (at >> 8) as u8,
                    at as u8,
                ],
            )?;
            out[at..at + 16].copy_from_slice(&r);
            off += 16;
        }
        Ok(out)
    }

    fn write_sector(&self, sector: u16, data: &[u8]) -> HubResult<()> {
        let len = data.len();
        self.call(
            F_PROFILES,
            6,
            &[
                (sector >> 8) as u8,
                sector as u8,
                0,
                0,
                (len >> 8) as u8,
                len as u8,
            ],
        )?;
        for chunk in data.chunks(16) {
            let mut c = [0xFFu8; 16];
            c[..chunk.len()].copy_from_slice(chunk);
            self.call(F_PROFILES, 7, &c)?;
        }
        self.call(F_PROFILES, 8, &[])?;
        let back = self.read_sector(sector, len)?;
        if back != data {
            return Err(HubError::Protocol(format!(
                "セクタ 0x{sector:04x} の読み戻しが一致しません"
            )));
        }
        Ok(())
    }

    fn crc_ok(b: &[u8]) -> bool {
        let n = b.len();
        n > 2 && crc16(&b[..n - 2]) == u16::from_be_bytes([b[n - 2], b[n - 1]])
    }

    fn seal(b: &mut [u8]) {
        let n = b.len();
        let c = crc16(&b[..n - 2]).to_be_bytes();
        b[n - 2] = c[0];
        b[n - 1] = c[1];
    }

    /// 現在のプロファイル (セクタ番号, 中身) を読む。
    fn load_profile(&self) -> HubResult<(u16, Vec<u8>)> {
        let o = self.onboard_info()?;
        let r = self.call(F_PROFILES, 4, &[])?;
        let mut sector = u16::from_be_bytes([r[0], r[1]]);
        if sector == 0 || sector == 0xFFFF {
            sector = 0x0101;
        }
        let data = self.read_sector(sector, o.sector_size)?;
        if !Self::crc_ok(&data) {
            return Err(HubError::Protocol(format!(
                "プロファイル 0x{sector:04x} の CRC が不正です"
            )));
        }
        *self.profile.lock().unwrap_or_else(|p| p.into_inner()) = Some((sector, data.clone()));
        Ok((sector, data))
    }

    /// 書き込み前の保険: ディレクトリと全プロファイル (フラッシュ/ROM) を保存。
    fn backup(&self) -> HubResult<std::path::PathBuf> {
        let o = self.onboard_info()?;
        let mut blob = Vec::new();
        let mut sectors: Vec<u16> = vec![0x0000];
        sectors.extend(1..=5u16);
        sectors.extend(0x0101..=0x0105u16);
        for s in sectors {
            if let Ok(b) = self.read_sector(s, o.sector_size) {
                blob.extend_from_slice(&s.to_be_bytes());
                blob.extend_from_slice(&(b.len() as u16).to_be_bytes());
                blob.extend_from_slice(&b);
            }
        }
        let mut p = std::path::PathBuf::from(std::env::var_os("APPDATA").unwrap_or_default());
        p.push("DeviceHub");
        p.push("logitech");
        p.push("backup");
        std::fs::create_dir_all(&p).map_err(|e| HubError::Io(e.to_string()))?;
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        p.push(format!("superlight-onboard-{ts}.bin"));
        std::fs::write(&p, &blob).map_err(|e| HubError::Io(e.to_string()))?;
        Ok(p)
    }

    /// プロファイルを書き換えて保存し、本体に読み込ませる。
    fn edit(&self, f: impl FnOnce(&mut Vec<u8>) -> HubResult<()>) -> HubResult<()> {
        let o = self.onboard_info()?;
        let (sector, mut data) = match self.profile.lock().unwrap_or_else(|p| p.into_inner()).clone() {
            Some(p) => p,
            None => {
                drop(self.load_profile()?);
                self.profile
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .clone()
                    .ok_or(HubError::Protocol("プロファイル未読込".into()))?
            }
        };
        self.ensure_backup()?;
        f(&mut data)?;
        Self::seal(&mut data);
        // ROM プロファイル使用中なら同じ番号のフラッシュ側へ複製し、ディレクトリで有効化。
        let target = if sector >= 0x0100 { sector & 0x00FF } else { sector };
        self.write_sector(target, &data)?;
        if sector >= 0x0100 {
            let mut dir = vec![0xFFu8; o.sector_size];
            dir[..4].copy_from_slice(&[(target >> 8) as u8, target as u8, 0x01, 0xFF]);
            Self::seal(&mut dir);
            self.write_sector(0x0000, &dir)?;
        }
        // オンボードモードにして、書いたプロファイルを読み込み直させる。
        let _ = self.call(F_PROFILES, 1, &[0x01]);
        self.call(F_PROFILES, 3, &[(target >> 8) as u8, target as u8])?;
        *self.profile.lock().unwrap_or_else(|p| p.into_inner()) = Some((target, data));
        Ok(())
    }

    fn ensure_backup(&self) -> HubResult<()> {
        let mut b = self.backed_up.lock().unwrap_or_else(|p| p.into_inner());
        if !*b {
            self.backup()?;
            *b = true;
        }
        Ok(())
    }

    /// 現在使っているフラッシュ側のプロファイル枠 (0 始まり)。ROM 使用中は 0。
    fn active_slot(&self) -> HubResult<usize> {
        let r = self.call(F_PROFILES, 4, &[])?;
        let s = u16::from_be_bytes([r[0], r[1]]) & 0x00FF;
        Ok((s as usize).clamp(1, PROFILES) - 1)
    }

    /// 枠の名前 (セクタ 0xA0 からの UTF-16LE)。一度読んだらキャッシュする。
    fn profile_names(&self) -> Vec<String> {
        if let Some(n) = self.names.lock().unwrap_or_else(|p| p.into_inner()).clone() {
            return n;
        }
        let mut out = Vec::with_capacity(PROFILES);
        for slot in 0..PROFILES {
            let sector = slot as u16 + 1;
            let mut raw = Vec::with_capacity(NAME_LEN);
            for off in (NAME_AT..NAME_AT + NAME_LEN).step_by(16) {
                match self.call(
                    F_PROFILES,
                    5,
                    &[(sector >> 8) as u8, sector as u8, (off >> 8) as u8, off as u8],
                ) {
                    Ok(r) => raw.extend_from_slice(&r),
                    Err(_) => return vec![String::new(); PROFILES],
                }
            }
            out.push(decode_name(&raw));
        }
        *self.names.lock().unwrap_or_else(|p| p.into_inner()) = Some(out.clone());
        out
    }

    /// 枠 `slot` に切り替える。setCurrentProfile は揮発 (フラッシュに書かない) なので
    /// ゲームごとの切替に使える。枠の中身が壊れて (未使用で) いれば今の設定を複製してから。
    fn switch_profile(&self, slot: usize) -> HubResult<()> {
        if slot >= PROFILES {
            return Err(HubError::Invalid("プロファイル番号が範囲外です".into()));
        }
        let o = self.onboard_info()?;
        let sector = slot as u16 + 1;
        let data = self.read_sector(sector, o.sector_size)?;
        if !Self::crc_ok(&data) {
            let (_, cur) = self.load_profile()?;
            self.ensure_backup()?;
            self.write_sector(sector, &cur)?;
        }
        let set = |s: u16| self.call(F_PROFILES, 3, &[(s >> 8) as u8, s as u8]);
        if set(sector).is_err() {
            // ディレクトリで無効な枠は選べない機種向け: 有効にしてから選び直す (初回だけ書く)。
            let mut dir = self.read_sector(0x0000, o.sector_size)?;
            let mut changed = false;
            for e in dir[..o.sector_size - 2].chunks_mut(4) {
                if e[0] == 0xFF && e[1] == 0xFF {
                    break;
                }
                if u16::from_be_bytes([e[0], e[1]]) == sector && e[2] != 1 {
                    e[2] = 1;
                    changed = true;
                }
            }
            if !changed {
                return Err(HubError::Protocol(format!(
                    "プロファイル {} を選べません",
                    slot + 1
                )));
            }
            self.ensure_backup()?;
            Self::seal(&mut dir);
            self.write_sector(0x0000, &dir)?;
            set(sector)?;
        }
        *self.profile.lock().unwrap_or_else(|p| p.into_inner()) = None;
        Ok(())
    }

    /// 枠 `slot` の名前を書き換える (その枠のセクタを 1 回書く)。
    fn rename_profile(&self, slot: usize, name: &str) -> HubResult<()> {
        if slot >= PROFILES {
            return Err(HubError::Invalid("プロファイル番号が範囲外です".into()));
        }
        let o = self.onboard_info()?;
        let sector = slot as u16 + 1;
        let mut data = self.read_sector(sector, o.sector_size)?;
        if !Self::crc_ok(&data) {
            return Err(HubError::Protocol(
                "未使用のプロファイルです — 先に切り替えて作成してください".into(),
            ));
        }
        self.ensure_backup()?;
        data[NAME_AT..NAME_AT + NAME_LEN].copy_from_slice(&encode_name(name));
        Self::seal(&mut data);
        self.write_sector(sector, &data)?;
        if let Some(n) = self.names.lock().unwrap_or_else(|p| p.into_inner()).as_mut() {
            n[slot] = decode_name(&data[NAME_AT..NAME_AT + NAME_LEN]);
        }
        if let Some((s, d)) = self.profile.lock().unwrap_or_else(|p| p.into_inner()).as_mut() {
            if *s == sector {
                *d = data;
            }
        }
        Ok(())
    }
}

/// プロファイル名 0x30B (UTF-16LE、最大 23 文字 + 終端)。未設定は 0xFF 埋め。
pub fn decode_name(raw: &[u8]) -> String {
    let units: Vec<u16> = raw
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|u| *u != 0 && *u != 0xFFFF)
        .collect();
    String::from_utf16_lossy(&units)
}

pub fn encode_name(name: &str) -> [u8; NAME_LEN] {
    let mut out = [0u8; NAME_LEN];
    for (i, u) in name.encode_utf16().take(NAME_LEN / 2 - 1).enumerate() {
        out[2 * i..2 * i + 2].copy_from_slice(&u.to_le_bytes());
    }
    out
}

// ---------------------------------------------------------------- ボタン符号

fn media_code(m: MediaFn) -> u16 {
    match m {
        MediaFn::PlayPause => 0xCD,
        MediaFn::Stop => 0xB7,
        MediaFn::Next => 0xB5,
        MediaFn::Prev => 0xB6,
        MediaFn::VolumeUp => 0xE9,
        MediaFn::VolumeDown => 0xEA,
        MediaFn::Mute => 0xE2,
        MediaFn::Calculator => 0x192,
        MediaFn::MyComputer => 0x194,
        MediaFn::Browser => 0x196,
        MediaFn::Mail => 0x18A,
        MediaFn::BrightnessUp => 0x6F,
        MediaFn::BrightnessDown => 0x70,
        MediaFn::Refresh => 0x227,
    }
}

fn media_of(c: u16) -> Option<MediaFn> {
    [
        MediaFn::PlayPause,
        MediaFn::Stop,
        MediaFn::Next,
        MediaFn::Prev,
        MediaFn::VolumeUp,
        MediaFn::VolumeDown,
        MediaFn::Mute,
        MediaFn::Calculator,
        MediaFn::MyComputer,
        MediaFn::Browser,
        MediaFn::Mail,
        MediaFn::BrightnessUp,
        MediaFn::BrightnessDown,
        MediaFn::Refresh,
    ]
    .into_iter()
    .find(|m| media_code(*m) == c)
}

fn mouse_mask(m: MouseFn) -> Option<u16> {
    Some(match m {
        MouseFn::Left => 0x01,
        MouseFn::Right => 0x02,
        MouseFn::Middle => 0x04,
        MouseFn::Back => 0x08,
        MouseFn::Forward => 0x10,
        _ => return None,
    })
}

pub fn decode_button(b: &[u8]) -> ButtonAction {
    match b[0] {
        0xFF if b.iter().all(|x| *x == 0xFF) => ButtonAction::Disabled,
        0x80 => match b[1] {
            0x01 => {
                let mask = u16::from_be_bytes([b[2], b[3]]);
                let f = match mask {
                    0x01 => Some(MouseFn::Left),
                    0x02 => Some(MouseFn::Right),
                    0x04 => Some(MouseFn::Middle),
                    0x08 => Some(MouseFn::Back),
                    0x10 => Some(MouseFn::Forward),
                    _ => None,
                };
                f.map(ButtonAction::Mouse).unwrap_or(ButtonAction::Unsupported {
                    ty: b[0],
                    params: b[1..4].to_vec(),
                })
            }
            0x02 => ButtonAction::Key(KeyStroke {
                mods: b[2],
                usage: b[3] as u16,
            }),
            0x03 => match media_of(u16::from_be_bytes([b[2], b[3]])) {
                Some(m) => ButtonAction::Media(m),
                None => ButtonAction::Unsupported {
                    ty: b[0],
                    params: b[1..4].to_vec(),
                },
            },
            _ => ButtonAction::Unsupported {
                ty: b[0],
                params: b[1..4].to_vec(),
            },
        },
        0x90 => match b[1] {
            0x03 => ButtonAction::Dpi(DpiFn::Up),
            0x04 => ButtonAction::Dpi(DpiFn::Down),
            0x05 => ButtonAction::Dpi(DpiFn::Cycle),
            0x0A => ButtonAction::ProfileCycle,
            _ => ButtonAction::Unsupported {
                ty: b[0],
                params: b[1..4].to_vec(),
            },
        },
        t => ButtonAction::Unsupported {
            ty: t,
            params: b[1..4].to_vec(),
        },
    }
}

pub fn encode_button(a: &ButtonAction) -> HubResult<[u8; 4]> {
    Ok(match a {
        ButtonAction::Disabled => [0xFF; 4],
        ButtonAction::Mouse(m) => {
            let mask = mouse_mask(*m).ok_or(HubError::Unsupported("このマウス動作"))?;
            let m = mask.to_be_bytes();
            [0x80, 0x01, m[0], m[1]]
        }
        ButtonAction::Key(k) => {
            if k.usage > 0xFF {
                return Err(HubError::Unsupported("このキー"));
            }
            [0x80, 0x02, k.mods, k.usage as u8]
        }
        ButtonAction::Media(m) => {
            let c = media_code(*m).to_be_bytes();
            [0x80, 0x03, c[0], c[1]]
        }
        ButtonAction::Dpi(DpiFn::Up) => [0x90, 0x03, 0, 0],
        ButtonAction::Dpi(DpiFn::Down) => [0x90, 0x04, 0, 0],
        ButtonAction::Dpi(DpiFn::Cycle) => [0x90, 0x05, 0, 0],
        ButtonAction::Dpi(DpiFn::Shift(_)) => [0x90, 0x07, 0, 0],
        ButtonAction::ProfileCycle => [0x90, 0x0A, 0, 0],
        ButtonAction::Unsupported { ty, params } if params.len() == 3 => {
            [*ty, params[0], params[1], params[2]]
        }
        _ => return Err(HubError::Unsupported("この割り当て")),
    })
}

// ---------------------------------------------------------------- プロファイル (base v1)

pub fn parse_profile(b: &[u8]) -> (MouseState, u32) {
    let ms = b[0].max(1) as u32;
    let rate = 1000 / ms;
    let mut stages = Vec::new();
    for n in 0..STAGES {
        let v = u16::from_le_bytes([b[3 + 2 * n], b[4 + 2 * n]]);
        if v == 0 || v == 0xFFFF {
            continue;
        }
        stages.push(DpiStage {
            x: v as u32,
            y: None,
            color: Rgb::default(),
            enabled: true,
        });
    }
    if stages.is_empty() {
        stages.push(DpiStage {
            x: 800,
            y: None,
            color: Rgb::default(),
            enabled: true,
        });
    }
    let active = (b[1] as usize).min(stages.len() - 1);
    let buttons = (0..BUTTONS)
        .map(|i| decode_button(&b[0x20 + 4 * i..0x24 + 4 * i]))
        .collect();
    (
        MouseState {
            stages,
            active_stage: active,
            buttons,
            macros: vec![],
            adaptive: None,
        },
        rate,
    )
}

pub fn write_stages(b: &mut [u8], stages: &[DpiStage], active: usize) -> HubResult<()> {
    if stages.is_empty() || stages.len() > STAGES {
        return Err(HubError::Invalid(format!("DPI ステージは 1〜{STAGES} 段です")));
    }
    for n in 0..STAGES {
        let v = stages
            .get(n)
            .map(|s| (s.x.clamp(DPI_MIN, DPI_MAX) / 50 * 50) as u16)
            .unwrap_or(0);
        b[3 + 2 * n..5 + 2 * n].copy_from_slice(&v.to_le_bytes());
    }
    let a = active.min(stages.len() - 1) as u8;
    b[1] = a;
    if b[2] as usize >= stages.len() {
        b[2] = a;
    }
    Ok(())
}

impl DeviceDriver for Superlight {
    fn info(&self) -> DeviceInfo {
        self.refresh_info();
        self.info.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    fn caps(&self) -> Capabilities {
        self.caps.clone()
    }

    fn read(&self) -> HubResult<DeviceState> {
        let (_, data) = self.load_profile()?;
        let (mouse, rate) = parse_profile(&data);
        let mut settings = BTreeMap::new();
        let rate = RATES
            .iter()
            .copied()
            .min_by_key(|r| r.abs_diff(rate))
            .unwrap_or(1000);
        settings.insert("polling".into(), SettingValue::Choice(rate as i64));
        Ok(DeviceState {
            profile: ProfileState {
                active: self.active_slot().unwrap_or(0) as u8,
                names: self.profile_names(),
            },
            settings,
            mouse: Some(mouse),
            keyboard: None,
            lighting: BTreeMap::new(),
            peq: None,
        })
    }

    fn apply(&self, change: &Change) -> HubResult<()> {
        match change {
            Change::DpiStages { stages, active } => {
                self.edit(|b| write_stages(b, stages, *active))?;
                let _ = self.call(F_PROFILES, 12, &[*active as u8]);
                Ok(())
            }
            Change::ActiveStage(i) => {
                self.edit(|b| {
                    let n = (0..STAGES)
                        .filter(|n| {
                            let v = u16::from_le_bytes([b[3 + 2 * n], b[4 + 2 * n]]);
                            v != 0 && v != 0xFFFF
                        })
                        .count()
                        .max(1);
                    b[1] = (*i).min(n - 1) as u8;
                    Ok(())
                })?;
                let _ = self.call(F_PROFILES, 12, &[*i as u8]);
                Ok(())
            }
            Change::Button { slot, action } => {
                if *slot >= BUTTONS {
                    return Err(HubError::Invalid("ボタン番号が範囲外です".into()));
                }
                let enc = encode_button(action)?;
                self.edit(|b| {
                    // 左クリックを失わない (UI でも検査するが二重に守る)。
                    let mut left = false;
                    for i in 0..BUTTONS {
                        let cur = if i == *slot {
                            enc
                        } else {
                            [b[0x20 + 4 * i], b[0x21 + 4 * i], b[0x22 + 4 * i], b[0x23 + 4 * i]]
                        };
                        left |= cur == [0x80, 0x01, 0x00, 0x01];
                    }
                    if !left {
                        return Err(HubError::Invalid("左クリックが 1 つは必要です".into()));
                    }
                    b[0x20 + 4 * slot..0x24 + 4 * slot].copy_from_slice(&enc);
                    Ok(())
                })
            }
            Change::Profile(i) => self.switch_profile(*i as usize),
            Change::ProfileName { index, name } => self.rename_profile(*index as usize, name),
            Change::Setting { id, value } if id == "polling" => {
                let hz = value.as_choice() as u32;
                if !RATES.contains(&hz) {
                    return Err(HubError::Invalid(format!("{hz} Hz は選べません")));
                }
                self.edit(|b| {
                    b[0] = (1000 / hz) as u8;
                    Ok(())
                })?;
                // ホストモードの機器にも即時反映 (オンボードでは無視されても害はない)。
                let _ = self.call(F_REPORT_RATE, 2, &[(1000 / hz) as u8]);
                Ok(())
            }
            _ => Err(HubError::Unsupported("change")),
        }
    }

    fn run_action(&self, id: &str, _file: Option<&std::path::Path>) -> HubResult<ActionOutcome> {
        match id {
            "backup_onboard" => {
                let p = self.backup()?;
                Ok(ActionOutcome {
                    message: Some(format!("保存しました: {}", p.display())),
                    reload: false,
                })
            }
            _ => Err(HubError::Unsupported("action")),
        }
    }

    fn battery(&self) -> HubResult<Battery> {
        if self.h.has(F_UNIFIED_BATTERY) {
            let r = self.call(F_UNIFIED_BATTERY, 1, &[])?;
            return Ok(Battery {
                percent: (r[0] <= 100 && r[0] > 0).then_some(r[0]),
                charging: matches!(r[2], 1 | 2),
            });
        }
        match self.h.call(F_BATTERY, 0, &[]) {
            Ok(r) => Ok(Battery {
                percent: (r[0] <= 100 && r[0] > 0).then_some(r[0]),
                charging: matches!(r[2], 1..=3),
            }),
            Err(e) => Err(self.err(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn factory() -> Vec<u8> {
        // base v1: 1ms、既定 index 1、DPI 400/800/1600/3200/6400、ボタン L R M B F。
        let mut b = vec![0xFFu8; 255];
        b[0] = 1;
        b[1] = 1;
        b[2] = 0;
        for (n, d) in [400u16, 800, 1600, 3200, 6400].iter().enumerate() {
            b[3 + 2 * n..5 + 2 * n].copy_from_slice(&d.to_le_bytes());
        }
        for (i, m) in [1u8, 2, 4, 8, 0x10].iter().enumerate() {
            b[0x20 + 4 * i..0x24 + 4 * i].copy_from_slice(&[0x80, 0x01, 0x00, *m]);
        }
        Superlight::seal(&mut b);
        b
    }

    #[test]
    fn parse_and_roundtrip_profile() {
        let mut b = factory();
        assert!(Superlight::crc_ok(&b));
        let (m, rate) = parse_profile(&b);
        assert_eq!(rate, 1000);
        assert_eq!(m.stages.iter().map(|s| s.x).collect::<Vec<_>>(), [400, 800, 1600, 3200, 6400]);
        assert_eq!(m.active_stage, 1);
        assert_eq!(m.buttons[3], ButtonAction::Mouse(MouseFn::Back));
        let st: Vec<DpiStage> = m.stages[..3].to_vec();
        write_stages(&mut b, &st, 2).unwrap();
        Superlight::seal(&mut b);
        let (m2, _) = parse_profile(&b);
        assert_eq!(m2.stages.len(), 3);
        assert_eq!(m2.active_stage, 2);
    }

    #[test]
    fn profile_name_codec() {
        let n = encode_name("StarRail");
        assert_eq!(&n[..4], &[b'S', 0, b't', 0]);
        assert_eq!(decode_name(&n), "StarRail");
        assert_eq!(decode_name(&[0xFF; NAME_LEN]), "");
        let long: String = "あ".repeat(40);
        assert_eq!(decode_name(&encode_name(&long)).chars().count(), 23);
    }

    #[test]
    fn button_codec() {
        for a in [
            ButtonAction::Disabled,
            ButtonAction::Mouse(MouseFn::Forward),
            ButtonAction::Key(KeyStroke { mods: 0, usage: 0xE2 }),
            ButtonAction::Key(KeyStroke { mods: 0x01, usage: 0x1D }),
            ButtonAction::Media(MediaFn::VolumeUp),
            ButtonAction::Dpi(DpiFn::Cycle),
            ButtonAction::ProfileCycle,
        ] {
            assert_eq!(decode_button(&encode_button(&a).unwrap()), a);
        }
    }
}
