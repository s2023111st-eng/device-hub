//! プロセス監視 — フォアグラウンドの exe 名を見て、プロファイルを自動適用する。
//!
//! `prefs.proc_rules` の各ルールについて「exe 名がフォアグラウンドか」を ~1 秒おきに
//! 評価し、立ち上がり/立ち下がりの遷移で `on_fg`/`on_leave` のプロファイルを
//! `hub.apply` で順に流す。最小化中も 33ms ティッカー経由で動く。
//!
//! 遷移の評価ルール:
//! - 前面 exe が解決できない tick は遷移しない (権限の高い窓で誤って復帰しない)。
//! - 前面が 2 tick 連続で同じときだけ評価 (高速 alt-tab の中間状態へ書かない)。
//! - 復帰 (leave) を全部先に、新規 (enter) を後に適用 — 管理下のアプリ間を
//!   直接移っても新しい側が必ず最後に書く。
//! - 同一 process+device のルールはキーで集約し、複数一致時は並び順に適用
//!   (下のルールほど優先)。
//! - 適用中にルールが削除/無効化/編集されても、記録した on_leave で復帰する。
//!
//! デバイスが書き込みを受理できない (読み込み中・構成切替中・スリープ中) 時点の
//! 変更は `proc_pending` に積み、受理可能になってから drain する — 無言で消えない。

use crate::app::Shell;
use crate::prefs::ProcRule;
use devhub_core::{Change, DeviceId};
use gpui_kit::prelude::*;

/// 前面ウィンドウの exe 名 (小文字・拡張子つき)。
#[cfg(windows)]
pub fn foreground_app() -> Option<String> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            return None;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 {
            return None;
        }
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 520];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(
            h,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
        .is_ok();
        let _ = CloseHandle(h);
        if !ok || len == 0 {
            return None;
        }
        String::from_utf16_lossy(&buf[..len as usize])
            .rsplit(['\\', '/'])
            .next()
            .map(|s| s.to_lowercase())
    }
}

#[cfg(not(windows))]
pub fn foreground_app() -> Option<String> {
    None
}

/// 可視ウィンドウを持つプロセスの一覧 `(exe 名, 代表ウィンドウタイトル)`。
/// フォアグラウンドになり得るプロセスだけ = ルールが実際に発火し得るものだけ。
/// 先頭は現在フォアグラウンドのプロセス。
#[cfg(windows)]
pub fn running_apps() -> Vec<(String, String)> {
    use std::collections::BTreeMap;
    use windows::core::BOOL;
    use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM};
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible,
    };
    unsafe {
        // 可視窓の (pid, タイトル) を集める。
        struct Acc {
            v: Vec<(u32, String)>,
        }
        unsafe extern "system" fn cb(h: HWND, l: LPARAM) -> BOOL {
            let acc = &mut *(l.0 as *mut Acc);
            if IsWindowVisible(h).as_bool() {
                let mut t = [0u16; 256];
                let n = GetWindowTextW(h, &mut t) as usize;
                let mut pid = 0u32;
                GetWindowThreadProcessId(h, Some(&mut pid));
                if pid != 0 {
                    acc.v.push((
                        pid,
                        if n > 0 {
                            String::from_utf16_lossy(&t[..n])
                        } else {
                            String::new()
                        },
                    ));
                }
            }
            BOOL(1)
        }
        let mut acc = Acc { v: vec![] };
        let _ = EnumWindows(Some(cb), LPARAM(&mut acc as *mut _ as isize));
        // pid → exe 名を引き、exe でまとめる (代表タイトルは最初の非空のもの)。
        let mut map: BTreeMap<String, String> = BTreeMap::new();
        for (pid, title) in acc.v {
            let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
                continue;
            };
            let mut buf = [0u16; 520];
            let mut len = buf.len() as u32;
            let ok = QueryFullProcessImageNameW(
                h,
                PROCESS_NAME_WIN32,
                windows::core::PWSTR(buf.as_mut_ptr()),
                &mut len,
            )
            .is_ok();
            let _ = CloseHandle(h);
            if !ok || len == 0 {
                continue;
            }
            let exe = String::from_utf16_lossy(&buf[..len as usize])
                .rsplit(['\\', '/'])
                .next()
                .unwrap_or_default()
                .to_lowercase();
            if exe.is_empty() {
                continue;
            }
            let e = map.entry(exe).or_default();
            if e.is_empty() && !title.is_empty() {
                *e = title;
            }
        }
        let fg = foreground_app().unwrap_or_default();
        let mut v: Vec<(String, String)> = map.into_iter().collect();
        v.sort_by(|a, b| (b.0 == fg).cmp(&(a.0 == fg)).then_with(|| a.0.cmp(&b.0)));
        v
    }
}

#[cfg(not(windows))]
pub fn running_apps() -> Vec<(String, String)> {
    vec![]
}

/// exe 名がルールの process に一致するか (小文字・部分一致)。
fn rule_matches(rule: &ProcRule, fg: &str) -> bool {
    let pat = rule.process.trim().to_lowercase();
    !pat.is_empty() && fg.contains(&pat)
}

/// ルールの識別キー (index ではなく内容で持つ — 削除/並び替えに強い)。
pub(crate) fn rule_key(rule: &ProcRule) -> String {
    format!("{}|{}", rule.process.trim().to_lowercase(), rule.device)
}

/// 適用中のルール 1 件分の記録 (離脱時の復帰に必要な情報)。
#[derive(Clone, Debug, Default)]
pub struct ProcApplied {
    /// ルールの対象デバイス (profile 検索のフィルタに使う)。
    pub device: String,
    /// 離脱時に適用するプロファイル名 (空 = 復帰しない)。
    pub on_leave: String,
}

/// 1 tick 分の評価結果。適用順は leaves (復帰) → enters (新規) 固定 —
/// 管理下のアプリ A から管理下の B へ直接切り替えたとき、A の復帰が
/// B の適用を上書きしないようにするため。
#[derive(Default)]
pub(crate) struct ProcEval {
    /// 現在アクティブなキー → 次の離脱で使う記録。
    pub(crate) now: std::collections::HashMap<String, ProcApplied>,
    /// (デバイス, プロファイル名) — ルールの並び順に発火。
    pub(crate) leaves: Vec<(String, String)>,
    /// (デバイス, on_fg プロファイル名の列) — ルールの並び順に発火。
    pub(crate) enters: Vec<(String, Vec<String>)>,
}

/// ルール集合と前面 exe から今 tick の遷移を決める (純粋関数 — 副作用なし)。
///
/// キー (process|device) ごとに集約するので、同一 process+device のルールが
/// 複数あっても 1 遷移として扱う。`states` は前回アクティブだったキーの記録で、
/// ルールが削除・編集されて実体が無くなっても記録値で復帰できる。
pub(crate) fn eval_rules(
    rules: &[ProcRule],
    fg: &str,
    states: &std::collections::HashMap<String, ProcApplied>,
) -> ProcEval {
    // 有効 & マッチするルールをキーで集約 (出現順を保つ)。
    let mut order: Vec<String> = vec![];
    let mut contributors: std::collections::HashMap<String, Vec<&ProcRule>> =
        std::collections::HashMap::new();
    for r in rules {
        if r.enabled && rule_matches(r, fg) {
            let k = rule_key(r);
            contributors.entry(k.clone()).or_default().push(r);
            if !order.contains(&k) {
                order.push(k);
            }
        }
    }

    let mut ev = ProcEval::default();
    let mut emitted: std::collections::HashSet<String> = Default::default();

    // 離脱 (以前アクティブだったが今は contributors 無し)。
    // 実体が残っているキーはルールの並び順で発火し、on_leave は
    // 同キーの最新の定義 (最後尾の非空) を優先 — 無ければ記録値。
    for r in rules {
        let k = rule_key(r);
        if !states.contains_key(&k)
            || contributors.contains_key(&k)
            || !emitted.insert(k.clone())
        {
            continue;
        }
        let name = rules
            .iter()
            .rev()
            .find(|r2| rule_key(r2) == k && !r2.on_leave.is_empty())
            .map(|r2| r2.on_leave.clone())
            .unwrap_or_else(|| states[&k].on_leave.clone());
        if !name.is_empty() {
            ev.leaves.push((states[&k].device.clone(), name));
        }
    }
    // 実体の無くなったキー (ルール削除) は最後に — 記録値で復帰。
    let mut orphans: Vec<&String> = states
        .keys()
        .filter(|k| !emitted.contains(*k) && !contributors.contains_key(*k))
        .collect();
    orphans.sort();
    for k in orphans {
        if !states[k].on_leave.is_empty() {
            ev.leaves
                .push((states[k].device.clone(), states[k].on_leave.clone()));
        }
    }

    // 遷入/継続。on_leave の記録は寄与ルールの最後尾の非空。
    for k in order {
        let rs = &contributors[&k];
        let leave = rs
            .iter()
            .rev()
            .find(|r| !r.on_leave.is_empty())
            .map(|r| r.on_leave.clone())
            .unwrap_or_default();
        let device = rs[0].device.clone();
        ev.now.insert(
            k.clone(),
            ProcApplied {
                device,
                on_leave: leave,
            },
        );
        if states.contains_key(&k) {
            continue; // 継続 — 再適用しない。
        }
        // 新規: 寄与ルールの on_fg を順に適用 (連続する同名はまとめる)。
        let mut fgs: Vec<String> = vec![];
        for r in rs {
            if !r.on_fg.is_empty() && fgs.last() != Some(&r.on_fg) {
                fgs.push(r.on_fg.clone());
            }
        }
        if !fgs.is_empty() {
            ev.enters.push((rs[0].device.clone(), fgs));
        }
    }
    ev
}

impl Shell {
    /// デバイスが今書き込みを受理できるか (読み込み済み・切替中でない・起きている)。
    fn device_applyable(&self, id: &DeviceId) -> bool {
        let Some(s) = self.hub.session(id) else {
            return false;
        };
        if !s.ready() || s.switching_profile {
            return false;
        }
        // スリープ中の実機は書き込みが応答タイムアウトする (mf8 系) → 覚醒を待つ。
        !self.hub.devices.iter().any(|d| d.id == *id && d.asleep)
    }

    /// `changes` を先頭から受理できるだけ `hub.apply` で送り、残りを `proc_pending`
    /// に積む。戻り値は (送れた件数, 保留した件数)。
    ///
    /// `Change::Profile` (構成切替) を途中に含んでいても、切替が始まった時点で
    /// 以降を保留に回すので順序が守られる。
    pub fn apply_changes(&mut self, id: &DeviceId, changes: Vec<Change>) -> (usize, usize) {
        let n = changes.len();
        let mut i = 0;
        while i < n && self.device_applyable(id) {
            self.hub.apply(id, changes[i].clone());
            i += 1;
        }
        if i < n {
            // 同じデバイスの滞留があれば末尾のキューへ追記 (順序を保ち、
            // 未接続のまま遷移を繰り返してもエントリ数が増えない)。
            let rest = changes[i..].to_vec();
            if let Some((_, q)) = self.proc_pending.iter_mut().find(|(d, _)| *d == id.0) {
                q.extend(rest);
            } else {
                self.proc_pending.push((id.0.clone(), rest));
            }
        }
        (i, n - i)
    }

    /// 保留分を、受理可能になったデバイスへ流す (proc_tick から毎回呼ぶ)。
    fn proc_drain(&mut self, cx: &mut Context<Self>) {
        if self.proc_pending.is_empty() {
            return;
        }
        let mut pending = std::mem::take(&mut self.proc_pending);
        let mut progress = false;
        for (did, q) in pending.iter_mut() {
            let id = DeviceId(did.clone());
            // 未接続だったデバイスが後から挿された → セッションを開いて読み込み開始。
            if self.hub.session(&id).is_none()
                && self.hub.devices.iter().any(|d| d.id == id && d.supported)
            {
                self.hub.open(&id, false);
            }
            // 開き損ねたセッション (Failed) は 10 秒おきに開き直す。
            if let Some(s) = self.hub.session(&id) {
                if matches!(s.status, crate::store::Status::Failed(_)) {
                    let due = self
                        .proc_retry
                        .get(did)
                        .is_none_or(|t| t.elapsed() > std::time::Duration::from_secs(10));
                    if due {
                        self.proc_retry
                            .insert(did.clone(), std::time::Instant::now());
                        self.hub.reload(&id);
                    }
                }
            }
            let had = q.len();
            while !q.is_empty() && self.device_applyable(&id) {
                let c = q.remove(0);
                self.hub.apply(&id, c);
                progress = true;
            }
            if had > 0 && q.is_empty() {
                self.toast(
                    format!("予約していたプロファイル分を適用しました ({did})"),
                    false,
                    cx,
                );
            }
        }
        pending.retain(|(_, q)| !q.is_empty());
        self.proc_pending = pending;
        if progress {
            cx.notify();
        }
    }

    /// プロファイル名で適用 (rule の device でフィルタしてバンドル全体を流す)。
    /// 戻り値は (送れた件数, 保留した件数, 全件数)。見つからなければ None。
    fn apply_profile_named(&mut self, device: &str, name: &str) -> Option<(usize, usize, usize)> {
        let profile = self
            .prefs
            .profiles
            .iter()
            .find(|p| p.name == name && p.devices.iter().any(|t| t.device == device))
            .cloned()?;
        let (mut sent, mut held, mut n) = (0usize, 0usize, 0usize);
        for t in &profile.devices {
            let id = DeviceId(t.device.clone());
            // セッションを開けるなら開く。未接続/未 ready でも諦めず、変更は
            // apply_changes が保留キューへ積む (接続・覚醒・読み込み後に drain)。
            if self.hub.session(&id).is_none()
                && self.hub.devices.iter().any(|d| d.id == id && d.supported)
            {
                self.hub.open(&id, false);
            }
            n += t.changes.len();
            let (s, h) = self.apply_changes(&id, t.changes.clone());
            sent += s;
            held += h;
        }
        Some((sent, held, n))
    }

    /// ~1 秒おきに呼ぶ。フォアグラウンド遷移でプロファイルを適用する。
    ///
    /// - 前面 exe の解決に失敗した tick は遷移評価しない (権限の高い窓などで
    ///   None になると全ルールが「離脱」と誤判定されて復帰を書き込むのを防ぐ)。
    /// - 前面が 2 tick 連続で同じときだけ評価する (alt-tab を速く往復した
    ///   中間状態へ EEPROM 書き込みをしない = フラッシュ保護)。
    /// - 遷移は復帰→新規の順で、管理下のアプリ間を直接移っても新しい側が勝つ。
    pub fn proc_tick(&mut self, cx: &mut Context<Self>) {
        // 常駐エージェントが動いていればルールはエージェントが適用する (二重適用しない)。
        if crate::agent::is_running() {
            self.proc_fg = None;
            return;
        }
        self.proc_drain(cx);
        if self.prefs.proc_rules.is_empty() {
            // ルールが全削除 = 管理下から外れた。適用中だった分は記録値で復帰する。
            let states = std::mem::take(&mut self.proc_states);
            if !states.is_empty() {
                let mut restored = 0;
                for a in states.values() {
                    if !a.on_leave.is_empty()
                        && self
                            .apply_profile_named(&a.device, &a.on_leave)
                            .is_some()
                    {
                        restored += 1;
                    }
                }
                if restored > 0 {
                    self.toast(
                        format!("復帰: ルール解除に伴い {restored} 件のプロファイルを適用"),
                        false,
                        cx,
                    );
                    cx.notify();
                }
            }
            self.proc_fg = None;
            return;
        }
        let fg = foreground_app().filter(|s| !s.is_empty());
        if fg != self.proc_fg {
            self.proc_fg = fg;
            return;
        }
        let Some(fg) = fg else { return };
        let rules = self.prefs.proc_rules.clone();
        let ev = eval_rules(&rules, &fg, &self.proc_states);
        let changed = !ev.leaves.is_empty() || !ev.enters.is_empty();
        let mut msgs: Vec<String> = vec![];
        let detail = |res: Option<(usize, usize, usize)>| match res {
            None => "未検出".to_string(),
            Some((sent, held, n)) => {
                if held > 0 {
                    if sent > 0 {
                        format!("{sent}/{n} 件・残り予約")
                    } else {
                        format!("{n} 件を予約")
                    }
                } else {
                    format!("{n} 件")
                }
            }
        };
        for (device, name) in &ev.leaves {
            let res = self.apply_profile_named(device, name);
            msgs.push(format!("復帰「{name}」{}", detail(res)));
        }
        for (device, names) in &ev.enters {
            for name in names {
                let res = self.apply_profile_named(device, name);
                msgs.push(format!("適用「{name}」{}", detail(res)));
            }
        }
        self.proc_states = ev.now;
        if !msgs.is_empty() {
            self.toast(format!("{} — {fg}", msgs.join(" → ")), false, cx);
        }
        if changed {
            cx.notify();
        }
    }

    /// ルールに今マッチしているか (UI の状態表示用)。
    pub fn proc_rule_active(&self, idx: usize) -> bool {
        self.prefs
            .proc_rules
            .get(idx)
            .is_some_and(|r| self.proc_states.contains_key(&rule_key(r)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prefs::{Prefs, ProcRule, SavedProfile};

    fn rule(process: &str) -> ProcRule {
        ProcRule {
            process: process.into(),
            device: "dev".into(),
            on_fg: "fps".into(),
            on_leave: String::new(),
            enabled: true,
        }
    }

    #[test]
    fn rule_match_is_case_insensitive_substring() {
        assert!(rule_matches(
            &rule("valorant"),
            "valorant-win64-shipping.exe"
        ));
        assert!(rule_matches(&rule("VALORANT"), "valorant.exe"));
        assert!(rule_matches(&rule("notepad"), "notepad.exe"));
        assert!(!rule_matches(&rule("valorant"), "chrome.exe"));
        assert!(!rule_matches(&rule("  "), "anything.exe"));
        assert!(!rule_matches(&rule(""), ""));
    }

    #[test]
    fn prefs_profiles_roundtrip() {
        let mut p = Prefs::default();
        p.profiles.push(SavedProfile {
            name: "fps".into(),
            devices: vec![
                crate::prefs::ProfileTarget {
                    device: "dev0".into(),
                    changes: vec![devhub_core::Change::Profile(1)],
                },
                crate::prefs::ProfileTarget {
                    device: "dev1".into(),
                    changes: vec![],
                },
            ],
        });
        p.proc_rules.push(rule("game.exe"));
        let s = serde_json::to_string(&p).unwrap();
        let q: Prefs = serde_json::from_str(&s).unwrap();
        assert_eq!(q.profiles.len(), 1);
        assert_eq!(q.profiles[0].name, "fps");
        assert_eq!(q.profiles[0].devices.len(), 2);
        assert_eq!(q.profiles[0].devices[0].changes.len(), 1);
        assert_eq!(q.proc_rules[0].process, "game.exe");
        assert_eq!(q.proc_rules[0].on_leave, "");
    }

    #[test]
    fn prefs_old_single_device_profile_migrates() {
        // 旧形式 (device + changes 直書き) のプロファイルが devices に移行されること。
        let q: Prefs = serde_json::from_str(
            r#"{"profiles":[{"name":"fps","device":"dev0","changes":[{"Profile":1}]}]}"#,
        )
        .unwrap();
        assert_eq!(q.profiles.len(), 1);
        assert_eq!(q.profiles[0].devices.len(), 1);
        assert_eq!(q.profiles[0].devices[0].device, "dev0");
        assert_eq!(q.profiles[0].devices[0].changes.len(), 1);
    }

    #[test]
    fn prefs_old_file_without_new_fields_loads() {
        // 旧 prefs.json (profiles/proc_rules 無し) が読めること。
        let q: Prefs = serde_json::from_str(r#"{"theme":"dark"}"#).unwrap();
        assert!(q.profiles.is_empty());
        assert!(q.proc_rules.is_empty());
    }

    // ---------------- eval_rules ----------------

    fn states_of(entries: &[(&str, &str, &str)]) -> std::collections::HashMap<String, ProcApplied> {
        entries
            .iter()
            .map(|(k, d, l)| {
                (
                    (*k).to_string(),
                    ProcApplied {
                        device: (*d).into(),
                        on_leave: (*l).into(),
                    },
                )
            })
            .collect()
    }

    fn rule_full(process: &str, device: &str, on_fg: &str, on_leave: &str) -> ProcRule {
        ProcRule {
            process: process.into(),
            device: device.into(),
            on_fg: on_fg.into(),
            on_leave: on_leave.into(),
            enabled: true,
        }
    }

    #[test]
    fn eval_enter_then_leave() {
        let rules = vec![rule_full("valorant", "mouse", "fps", "desk")];
        // 遷入: states 空 → on_fg が enters に、on_leave が記録される。
        let ev = eval_rules(&rules, "valorant.exe", &Default::default());
        assert_eq!(ev.enters, vec![("mouse".into(), vec!["fps".into()])]);
        assert!(ev.leaves.is_empty());
        assert_eq!(ev.now["valorant|mouse"].on_leave, "desk");
        // 継続: 同じ状態なら何も起きない。
        let ev = eval_rules(&rules, "valorant.exe", &ev.now);
        assert!(ev.enters.is_empty() && ev.leaves.is_empty());
        // 離脱: fg が変わると on_leave が leaves に。
        let ev = eval_rules(&rules, "chrome.exe", &ev.now);
        assert_eq!(ev.leaves, vec![("mouse".into(), "desk".into())]);
        assert!(ev.now.is_empty());
    }

    #[test]
    fn eval_leave_before_enter_new_app_wins() {
        // valorant→mouse(A) と apex→mouse(B)。valorant から apex へ直接切替。
        let rules = vec![
            rule_full("apex", "mouse", "b_prof", "desk_b"),
            rule_full("valorant", "mouse", "a_prof", "desk_a"),
        ];
        let states = states_of(&[("valorant|mouse", "mouse", "desk_a")]);
        let ev = eval_rules(&rules, "apex.exe", &states);
        // leaves が先、enters が後 → apex (B) が最後に書いて勝つ。
        assert_eq!(ev.leaves, vec![("mouse".into(), "desk_a".into())]);
        assert_eq!(ev.enters, vec![("mouse".into(), vec!["b_prof".into()])]);
        assert_eq!(ev.now["apex|mouse"].on_leave, "desk_b");
    }

    #[test]
    fn eval_deleted_rule_restores_stored_leave() {
        // ルールを消しても適用中だった分は記録値で復帰する。
        let rules = vec![rule_full("other", "mouse", "x", "")];
        let states = states_of(&[("valorant|mouse", "mouse", "desk")]);
        let ev = eval_rules(&rules, "other.exe", &states);
        assert_eq!(ev.leaves, vec![("mouse".into(), "desk".into())]);
    }

    #[test]
    fn eval_disabled_rule_leaves_with_live_on_leave() {
        // 無効化でも離脱扱い。on_leave が編集されていたら最新値を使う。
        let mut r = rule_full("valorant", "mouse", "fps", "desk_new");
        r.enabled = false;
        let states = states_of(&[("valorant|mouse", "mouse", "desk_old")]);
        let ev = eval_rules(&[r], "valorant.exe", &states);
        assert_eq!(ev.leaves, vec![("mouse".into(), "desk_new".into())]);
    }

    #[test]
    fn eval_same_key_rules_aggregate() {
        // 同一 process+device のルール 2 件 → 1 遷移に集約、on_fg は順に適用。
        let rules = vec![
            rule_full("valorant", "mouse", "a", "desk"),
            rule_full("valorant", "mouse", "b", "desk2"),
        ];
        let ev = eval_rules(&rules, "valorant.exe", &Default::default());
        assert_eq!(ev.enters.len(), 1);
        assert_eq!(ev.enters[0].1, vec!["a".to_string(), "b".to_string()]);
        // 片方を無効化してもキーは生きている → 誤って復帰しない。
        let mut r2 = rules.clone();
        r2[0].enabled = false;
        let ev = eval_rules(&r2, "valorant.exe", &ev.now);
        assert!(ev.leaves.is_empty());
        assert!(ev.enters.is_empty());
        // 両方無効 → 離脱。on_leave は最後尾の非空 "desk2"。
        r2[1].enabled = false;
        let ev = eval_rules(&r2, "valorant.exe", &ev.now);
        assert_eq!(ev.leaves, vec![("mouse".into(), "desk2".into())]);
    }

    #[test]
    fn eval_empty_on_leave_no_restore() {
        // on_leave 空のルールは離脱時に何も出さない。
        let rules = vec![rule_full("valorant", "mouse", "fps", "")];
        let ev = eval_rules(&rules, "valorant.exe", &Default::default());
        let ev = eval_rules(&rules, "chrome.exe", &ev.now);
        assert!(ev.leaves.is_empty());
    }
}
