//! HID++ 2.0 の最小トランスポート。
//!
//! - 要求は常に long report (ID 0x11, 20B): `[0x11, dev, featIdx, fn<<4 | swid, params…16]`。
//!   HID++ 2.0 機器は short 用の関数も long で受け付ける (実機で確認)。
//! - 応答は同じ `[dev, featIdx, fn|swid]` で一致判定する。swid は G HUB (0x0A) と
//!   重ならない値を使い、他アプリの応答や通知を取り違えないようにする。
//! - エラー: `[0x11, dev, 0xFF, featIdx, fn|swid, code]`。受信機が返す HID++ 1.0 の
//!   0x8F (デバイス不在) は short コレクションに来るので、long 側ではタイムアウトになる
//!   → 呼び側は「休眠/電源オフ」として扱う。
//! - フィーチャー番号 → index は IRoot (index 0, fn0) で引いてキャッシュする。

use devhub_core::backend::{HubError, HubResult};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const LONG: u8 = 0x11;
const SWID: u8 = 0x0D;

/// HID++ 2.0 のエラーコード。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fail {
    /// 応答が来ない (無線の休眠・電源オフ)。
    Timeout,
    /// 機器が返したエラーコード。
    Code(u8),
}

pub struct Hidpp {
    dev: Mutex<hidapi::HidDevice>,
    /// デバイス番号: 有線/直結 0xFF、レシーバー経由はペアリングスロット 1..6。
    index: Mutex<u8>,
    features: Mutex<HashMap<u16, Option<u8>>>,
    timeout: Duration,
}

impl Hidpp {
    pub fn new(dev: hidapi::HidDevice, index: u8, timeout: Duration) -> Self {
        dev.set_blocking_mode(false).ok();
        Self {
            dev: Mutex::new(dev),
            index: Mutex::new(index),
            features: Mutex::new(HashMap::new()),
            timeout,
        }
    }

    pub fn index(&self) -> u8 {
        *self.index.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn set_index(&self, i: u8) {
        *self.index.lock().unwrap_or_else(|p| p.into_inner()) = i;
        self.forget_features();
    }

    /// 再接続 (ファーム更新・ペアリングし直し) でフィーチャー表が変わり得るので捨てる。
    pub fn forget_features(&self) {
        self.features
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clear();
    }

    /// 生の関数呼び出し。返り値は応答のパラメータ部 16B。
    pub fn call_raw(&self, feat_idx: u8, func: u8, params: &[u8]) -> Result<[u8; 16], Fail> {
        self.call_timeout(feat_idx, func, params, self.timeout)
    }

    pub fn call_timeout(
        &self,
        feat_idx: u8,
        func: u8,
        params: &[u8],
        timeout: Duration,
    ) -> Result<[u8; 16], Fail> {
        let dev_idx = self.index();
        let fsw = (func << 4) | SWID;
        let mut pkt = [0u8; 20];
        pkt[0] = LONG;
        pkt[1] = dev_idx;
        pkt[2] = feat_idx;
        pkt[3] = fsw;
        let n = params.len().min(16);
        pkt[4..4 + n].copy_from_slice(&params[..n]);
        let dev = self.dev.lock().unwrap_or_else(|p| p.into_inner());
        // 前の要求の取り残し (遅れて来た応答・通知) を捨てる。
        let mut buf = [0u8; 64];
        while matches!(dev.read_timeout(&mut buf, 0), Ok(n) if n > 0) {}
        dev.write(&pkt).map_err(|_| Fail::Timeout)?;
        let end = Instant::now() + timeout;
        loop {
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(Fail::Timeout);
            }
            let n = match dev.read_timeout(&mut buf, left.as_millis().max(1) as i32) {
                Ok(n) => n,
                Err(_) => return Err(Fail::Timeout),
            };
            if n < 7 || buf[0] != LONG || buf[1] != dev_idx {
                continue;
            }
            if buf[2] == 0xFF && buf[3] == feat_idx && buf[4] == fsw {
                return Err(Fail::Code(buf[5]));
            }
            if buf[2] == feat_idx && buf[3] == fsw {
                let mut out = [0u8; 16];
                let m = (n - 4).min(16);
                out[..m].copy_from_slice(&buf[4..4 + m]);
                return Ok(out);
            }
        }
    }

    /// フィーチャー ID → index (無ければ None)。
    pub fn feature(&self, id: u16) -> Result<Option<u8>, Fail> {
        if let Some(v) = self
            .features
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&id)
        {
            return Ok(*v);
        }
        let r = self.call_raw(0, 0, &[(id >> 8) as u8, id as u8])?;
        let v = (r[0] != 0).then_some(r[0]);
        self.features
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id, v);
        Ok(v)
    }

    /// フィーチャー `id` の関数 `func` を呼ぶ。フィーチャーが無ければ Unsupported。
    pub fn call(&self, id: u16, func: u8, params: &[u8]) -> HubResult<[u8; 16]> {
        let idx = self
            .feature(id)
            .map_err(to_err)?
            .ok_or(HubError::Unsupported("HID++ feature"))?;
        self.call_raw(idx, func, params).map_err(to_err)
    }

    pub fn has(&self, id: u16) -> bool {
        matches!(self.feature(id), Ok(Some(_)))
    }

    /// IRoot ping。応答すれば HID++ のプロトコル版 (major, minor)。
    pub fn ping(&self, timeout: Duration) -> Option<(u8, u8)> {
        self.call_timeout(0, 1, &[0, 0, 0x5A], timeout)
            .ok()
            .map(|r| (r[0], r[1]))
    }
}

pub fn to_err(f: Fail) -> HubError {
    match f {
        Fail::Timeout => HubError::Disconnected,
        Fail::Code(c) => HubError::Protocol(format!("HID++ エラー 0x{c:02x} ({})", code_name(c))),
    }
}

fn code_name(c: u8) -> &'static str {
    match c {
        1 => "unknown",
        2 => "invalid argument",
        3 => "out of range",
        4 => "hardware error",
        5 => "logitech internal",
        6 => "invalid feature index",
        7 => "invalid function",
        8 => "busy",
        9 => "unsupported",
        _ => "?",
    }
}

/// CRC-16/CCITT-FALSE (poly 0x1021, init 0xFFFF)。オンボードプロファイルのセクタ末尾。
pub fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &b in data {
        crc ^= (b as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// CRC-32 (IEEE, zlib と同じ)。0x8101 ファイル転送の確定に使う。
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// 0x0003 DeviceFwVersion — 本体アプリ (type 0) のエンティティを "U170 4.20" 形式で。
/// エンティティ 0 はブートローダ (type 1) のことが多いので種別で選ぶ。
pub fn firmware(h: &Hidpp) -> Option<String> {
    let n = h.call(0x0003, 0, &[]).ok()?[0].max(1);
    let mut first = None;
    for e in 0..n {
        let Ok(r) = h.call(0x0003, 1, &[e]) else {
            continue;
        };
        let prefix: String = r[1..4]
            .iter()
            .filter(|c| c.is_ascii_graphic())
            .map(|&c| c as char)
            .collect();
        let build = u16::from_be_bytes([r[6], r[7]]);
        let s = format!("{prefix}{:02x} {:x}.{:x}", r[4], r[5], build);
        if r[0] == 0 {
            return Some(s);
        }
        first.get_or_insert(s);
    }
    first
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_matches_ghub_capture() {
        // G HUB → PRO X TKL RAPID のキー別 RT ファイル (WASD 0.1mm) と fn9 の CRC。
        let rt = [0x00, 0x04, 0x1d, 0x01, 0x21, 0x01, 0x22, 0x01, 0x23, 0x01];
        assert_eq!(crc32(&rt), 0xcc68_a362);
        assert_eq!(crc32(&[0, 0]), 0x41d9_12ff);
    }

    #[test]
    fn crc16_ccitt_false() {
        assert_eq!(crc16(b"123456789"), 0x29B1);
    }
}
