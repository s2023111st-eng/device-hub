//! 自動化 — 設定プロファイルの録画/適用と、プロセス連動ルール。
//!
//! - プロファイル: デバイス画面での変更を `Change` 列として録画し、ワンクリック or
//!   プロセス監視で順に再生する。
//! - プロセス監視: exe 名がフォアグラウンドの間 `on_fg` プロファイルを適用し、
//!   外れたら `on_leave` を適用する (procmon.rs が ~1 秒おきに評価)。

use crate::app::Shell;
use crate::prefs::SavedProfile;
use crate::ui::{self, Btn};
use crate::{procmon, theme};
use devhub_core::DeviceId;
use gpui_kit::prelude::*;
use gpui_kit::*;
use std::rc::Rc;

/// 新規プロファイル/ルール作成フォームの選択状態。
pub struct AutoForm {
    /// 新規プロファイルの対象デバイス (対応デバイス一覧の index、複数選択)。
    pub new_devs: std::collections::BTreeSet<usize>,
    /// ルールの対象デバイス。
    pub rule_dev: usize,
    /// ルールの on_fg プロファイル (usize::MAX = 未選択)。
    pub rule_fg: usize,
    /// ルールの on_leave プロファイル (usize::MAX = なし)。
    pub rule_leave: usize,
    /// 編集中のルール (Some ならフォームは更新モード)。
    pub edit_rule: Option<usize>,
}

impl Default for AutoForm {
    fn default() -> Self {
        Self {
            new_devs: std::collections::BTreeSet::new(),
            rule_dev: 0,
            rule_fg: usize::MAX,
            rule_leave: usize::MAX,
            edit_rule: None,
        }
    }
}

/// 対応デバイスの一覧 (サイドバーと同じ並び・表示名)。
fn dev_list(shell: &Shell) -> Vec<(DeviceId, String)> {
    let mut devices = shell.hub.devices.clone();
    devices.sort_by_key(|d| {
        (
            !d.supported,
            d.kind != devhub_core::DeviceKind::Mouse,
            d.name.clone(),
        )
    });
    devices
        .into_iter()
        .filter(|d| d.supported)
        .map(|d| {
            let name = shell
                .prefs
                .nicknames
                .get(&d.id.0)
                .cloned()
                .unwrap_or_else(|| d.name.clone());
            (d.id, name)
        })
        .collect()
}

/// 選択チップの行 (デバイス/プロファイル選択用)。
fn chips_row(
    id_prefix: &str,
    items: &[String],
    sel: usize,
    cx: &mut Context<Shell>,
    on: Rc<dyn Fn(&mut Shell, usize, &mut Context<Shell>)>,
) -> Div {
    chips_row_impl(id_prefix, items, |i| i == sel, cx, on)
}

/// 複数選択チップの行 (新規プロファイルの対象デバイス用)。
fn chips_row_multi(
    id_prefix: &str,
    items: &[String],
    sel: &std::collections::BTreeSet<usize>,
    cx: &mut Context<Shell>,
    on: Rc<dyn Fn(&mut Shell, usize, &mut Context<Shell>)>,
) -> Div {
    chips_row_impl(id_prefix, items, |i| sel.contains(&i), cx, on)
}

fn chips_row_impl(
    id_prefix: &str,
    items: &[String],
    is_sel: impl Fn(usize) -> bool,
    cx: &mut Context<Shell>,
    on: Rc<dyn Fn(&mut Shell, usize, &mut Context<Shell>)>,
) -> Div {
    let p = theme::pal();
    let mut row = div().flex().flex_wrap().gap(px(6.));
    for (i, label) in items.iter().enumerate() {
        let on_i = is_sel(i);
        let on2 = on.clone();
        row = row.child(
            div()
                .id(ElementId::Name(format!("{id_prefix}-{i}").into()))
                .px(px(10.))
                .py(px(5.))
                .rounded(px(8.))
                .cursor_pointer()
                .text_size(px(12.))
                .font_weight(if on_i {
                    FontWeight::SEMIBOLD
                } else {
                    FontWeight::NORMAL
                })
                .text_color(if on_i { p.on_accent } else { p.sub })
                .bg(if on_i {
                    theme::accent_grad()
                } else {
                    theme::alpha(p.faint, 0.08).into()
                })
                .border_1()
                .border_color(if on_i { p.accent } else { p.hair })
                .hover(|s| s.bg(theme::alpha(p.faint, 0.16)))
                .child(label.clone())
                .on_click(cx.listener(move |this, _, _, cx| on2(this, i, cx))),
        );
    }
    row
}

impl Shell {
    /// 保存プロファイルを今すぐ適用する (受理できない分は保留キューへ)。
    /// 対象デバイスが複数あるバンドルは全デバイスへ流す。
    pub fn apply_profile(&mut self, pi: usize, cx: &mut Context<Self>) {
        let Some(p) = self.prefs.profiles.get(pi).cloned() else {
            return;
        };
        if p.devices.is_empty() {
            self.toast("対象デバイスがありません", true, cx);
            return;
        }
        let (mut sent, mut held, mut n) = (0usize, 0usize, 0usize);
        for t in &p.devices {
            let id = DeviceId(t.device.clone());
            if self.hub.session(&id).is_none()
                && self.hub.devices.iter().any(|d| d.id == id && d.supported)
            {
                self.hub.open(&id, false);
            }
            n += t.changes.len();
            // 未接続・未読込の分は保留キューへ積まれる (再接続後に drain)。
            let (s, h) = self.apply_changes(&id, t.changes.clone());
            sent += s;
            held += h;
        }
        let msg = if held > 0 {
            format!(
                "プロファイル「{}」: {sent}/{n} 件を適用、{held} 件を予約",
                p.name
            )
        } else {
            format!("プロファイル「{}」を適用 ({n} 件)", p.name)
        };
        self.toast(msg, false, cx);
        cx.notify();
    }

    /// デバイスの現在の設定をプロファイルへ記録する (スナップショット)。
    /// 読み込み済みの対象デバイスそれぞれの現在値を、対応する変更列へ入れる。
    /// キー割り当て/ボタン/マクロ等の構造系は含まない (snapshot_changes 参照)。
    pub fn capture_profile(&mut self, pi: usize, cx: &mut Context<Self>) {
        let Some(prof) = self.prefs.profiles.get(pi).cloned() else {
            return;
        };
        let (mut got, mut total, mut skipped) = (0usize, 0usize, 0usize);
        for t in &prof.devices {
            let Some(s) = self.hub.session(&DeviceId(t.device.clone())) else {
                skipped += 1;
                continue;
            };
            let (Some(caps), Some(state)) = (s.caps.as_ref(), s.state.as_ref()) else {
                skipped += 1;
                continue;
            };
            let changes = crate::store::snapshot_changes(caps, state);
            total += changes.len();
            got += 1;
            if let Some(p) = self.prefs.profiles.get_mut(pi) {
                if let Some(t2) = p.devices.iter_mut().find(|x| x.device == t.device) {
                    t2.changes = changes;
                }
            }
        }
        if got == 0 {
            self.toast("対象デバイスを開いて読み込んでから使えます", true, cx);
            return;
        }
        self.prefs.save();
        let msg = if skipped > 0 {
            format!(
                "「{}」に現在の設定を記録 ({got} 台・{total} 件・{skipped} 台は未読込で据え置き)",
                prof.name
            )
        } else {
            format!("「{}」に現在の設定を記録 ({total} 件)", prof.name)
        };
        self.toast(msg, false, cx);
        cx.notify();
    }

    /// ルールフォーム用: 指定デバイスのプロファイル名一覧 (先頭に「なし」)。
    fn profile_names(
        &self,
        dev_idx: usize,
        devs: &[(DeviceId, String)],
        allow_none: bool,
    ) -> Vec<String> {
        let Some((id, _)) = devs.get(dev_idx) else {
            return vec![];
        };
        let mut v: Vec<String> = self
            .prefs
            .profiles
            .iter()
            .filter(|p| p.devices.iter().any(|t| t.device == id.0))
            .map(|p| p.name.clone())
            .collect();
        if allow_none {
            v.insert(0, "なし".into());
        }
        v
    }

    pub fn page_auto(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = theme::pal();
        let devs = dev_list(self);
        let fg_now = procmon::foreground_app().unwrap_or_else(|| "(不明)".into());
        let running = procmon::running_apps();
        let running_set: std::collections::HashSet<String> =
            running.iter().map(|(e, _)| e.clone()).collect();
        let mut page = div().flex().flex_col().gap(px(14.)).w_full();
        if crate::agent::is_running() {
            page = page.child(ui::faint(
                "常駐エージェントが実行中です — プロセス連動ルールはエージェントが適用します (この画面を閉じても切り替わります)。",
            ));
        }

        // ================= プロファイル =================
        let mut prof_body = ui::body();
        if self.prefs.profiles.is_empty() {
            prof_body = prof_body.child(ui::faint(
                "プロファイルはまだありません。下のフォームで作成するか、デバイス画面で「録画」を開始してください。",
            ));
        }
        for (i, prof) in self.prefs.profiles.clone().iter().enumerate() {
            let dev_name = prof
                .devices
                .iter()
                .map(|t| {
                    devs.iter()
                        .find(|(id, _)| id.0 == t.device)
                        .map(|(_, n)| n.clone())
                        .unwrap_or_else(|| t.device.clone())
                })
                .collect::<Vec<_>>()
                .join("・");
            let recording = self.rec_profile == Some(i);
            let mut btns = div().flex().items_center().gap(px(8.));
            if recording {
                btns = btns.child(ui::chip("録画中", p.warn));
            }
            let help_txt = if recording {
                "録画中: 対象デバイスの画面で設定を変更すると自動で記録されます".to_string()
            } else {
                format!(
                    "{} 件の変更",
                    prof.devices.iter().map(|t| t.changes.len()).sum::<usize>()
                )
            };
            let row = ui::row_first(
                format!("{} ({dev_name})", prof.name).as_str(),
                Some(help_txt.as_str()),
                btns.child(ui::button(
                    format!("prof-apply-{i}"),
                    "適用",
                    Some("play"),
                    Btn::Glass,
                    cx,
                    move |this, _, cx| this.apply_profile(i, cx),
                ))
                .child(ui::button(
                    format!("prof-cap-{i}"),
                    "今の状態",
                    Some("save"),
                    Btn::Ghost,
                    cx,
                    move |this, _, cx| this.capture_profile(i, cx),
                ))
                .child(ui::button(
                    format!("prof-ren-{i}"),
                    "名前",
                    Some("pencil"),
                    Btn::Ghost,
                    cx,
                    move |this, w, cx| {
                        let Some(old) = this
                            .prefs
                            .profiles
                            .get(i)
                            .map(|p| p.name.clone())
                        else {
                            return;
                        };
                        let old_name = old.clone();
                        this.modal = Some(crate::app::Modal::Text {
                            title: "プロファイル名を変更".into(),
                            on_ok: Rc::new(move |this, v, _, cx| {
                                let v = v.trim().to_string();
                                if v.is_empty() || v == old {
                                    return;
                                }
                                if this.prefs.profiles.iter().any(|p| p.name == v) {
                                    this.toast("同名のプロファイルがあります", true, cx);
                                    return;
                                }
                                // ルールの参照も追従する (プロファイル名は一意)。
                                for r in this.prefs.proc_rules.iter_mut() {
                                    if r.on_fg == old {
                                        r.on_fg = v.clone();
                                    }
                                    if r.on_leave == old {
                                        r.on_leave = v.clone();
                                    }
                                }
                                // 適用中のルールが記録した復帰先も追従する。
                                for a in this.proc_states.values_mut() {
                                    if a.on_leave == old {
                                        a.on_leave = v.clone();
                                    }
                                }
                                if let Some(p) = this.prefs.profiles.get_mut(i) {
                                    p.name = v;
                                }
                                this.prefs.save();
                                cx.notify();
                            }),
                        });
                        // 現在値を入力欄へ (未作成なら現在値で作る)。
                        this.input("modal", &old_name, "", w, cx);
                        this.set_input("modal", &old_name, w, cx);
                        cx.notify();
                    },
                ))
                .child(ui::button(
                    format!("prof-rec-{i}"),
                    if recording { "停止" } else { "録画" },
                    Some("circle"),
                    if recording { Btn::Danger } else { Btn::Ghost },
                    cx,
                    move |this, _, cx| {
                        if this.rec_profile == Some(i) {
                            this.rec_profile = None;
                            let n = this
                                .prefs
                                .profiles
                                .get(i)
                                .map(|p| p.devices.iter().map(|t| t.changes.len()).sum::<usize>())
                                .unwrap_or(0);
                            this.toast(format!("録画終了: {n} 件を記録"), false, cx);
                        } else {
                            this.rec_profile = Some(i);
                            let name = this
                                .prefs
                                .profiles
                                .get(i)
                                .map(|p| p.name.clone())
                                .unwrap_or_default();
                            this.toast(
                                format!("録画開始: 「{name}」— デバイス画面での変更を記録します"),
                                false,
                                cx,
                            );
                        }
                        cx.notify();
                    },
                ))
                .child(ui::button(
                    format!("prof-del-{i}"),
                    "削除",
                    Some("trash-2"),
                    Btn::Ghost,
                    cx,
                    move |this, _, cx| {
                        this.prefs.profiles.remove(i);
                        if this.rec_profile == Some(i) {
                            this.rec_profile = None;
                        } else if let Some(r) = this.rec_profile {
                            if r > i {
                                this.rec_profile = Some(r - 1);
                            }
                        }
                        this.prefs.save();
                        cx.notify();
                    },
                )),
            );
            prof_body = prof_body.child(row);
        }
        // 新規作成フォーム — 対象デバイスは複数選べ、全デバイスへまとめて
        // 適用するバンドルになる (録画は対象内の各デバイスの変更を振り分ける)。
        let dev_names: Vec<String> = devs.iter().map(|(_, n)| n.clone()).collect();
        let new_sel = self.auto_form.new_devs.clone();
        prof_body = prof_body.child(ui::row(
            "対象デバイス",
            Some("複数選べます — 全デバイスへまとめて適用します"),
            div().child(chips_row_multi(
                "prof-dev",
                &dev_names,
                &new_sel,
                cx,
                Rc::new(|this, i, cx| {
                    let s = &mut this.auto_form.new_devs;
                    if !s.remove(&i) {
                        s.insert(i);
                    }
                    cx.notify();
                }),
            )),
        ));
        let name_in = self.input(
            "auto-prof-name",
            "",
            "プロファイル名 (例: FPS / 動画編集)",
            window,
            cx,
        );
        prof_body = prof_body.child(ui::row_first(
            "新規プロファイル",
            Some("名前を付けて作成 → 録画で変更を集める"),
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .child(
                    div()
                        .w(px(220.))
                        .child(gpui_kit::component::input::Input::new(&name_in)),
                )
                .child(ui::button(
                    "prof-new",
                    "作成",
                    Some("plus"),
                    Btn::Primary,
                    cx,
                    |this, w, cx| {
                        let name = this.input_value("auto-prof-name", cx).unwrap_or_default();
                        let name = name.trim().to_string();
                        if name.is_empty() {
                            this.toast("プロファイル名を入力してください", true, cx);
                            return;
                        }
                        if this.prefs.profiles.iter().any(|p| p.name == name) {
                            this.toast("同名のプロファイルがあります", true, cx);
                            return;
                        }
                        let devs = dev_list(this);
                        let targets: Vec<crate::prefs::ProfileTarget> = this
                            .auto_form
                            .new_devs
                            .iter()
                            .filter_map(|&i| devs.get(i))
                            .map(|(id, _)| crate::prefs::ProfileTarget {
                                device: id.0.clone(),
                                changes: vec![],
                            })
                            .collect();
                        if targets.is_empty() {
                            this.toast("対象デバイスを選んでください", true, cx);
                            return;
                        }
                        this.prefs.profiles.push(SavedProfile {
                            name,
                            devices: targets,
                        });
                        this.prefs.save();
                        this.set_input("auto-prof-name", "", w, cx);
                        this.auto_form.new_devs.clear();
                        cx.notify();
                    },
                )),
        ));
        page = page.child(ui::card_titled("設定プロファイル", "layers", None).child(prof_body));

        // ================= プロセス監視 =================
        let mut rule_body = ui::body().child(ui::row(
            "現在の前面アプリ",
            Some("exe 名の部分一致 · 複数一致は下のルールが優先 · 前面が安定してから切替 (~2 秒)"),
            div().child(ui::chip(fg_now.clone(), p.accent)),
        ));
        if self.prefs.proc_rules.is_empty() {
            rule_body = rule_body.child(ui::faint(
                "ルールはまだありません。ゲーム等の exe 名を登録すると、そのアプリが前面の間だけプロファイルを自動適用します。",
            ));
        }
        for (i, rule) in self.prefs.proc_rules.clone().iter().enumerate() {
            let active = self.proc_rule_active(i);
            let is_running = running_set.contains(&rule.process)
                || running_set
                    .iter()
                    .any(|e| e.contains(rule.process.as_str()) && !rule.process.is_empty());
            let editing = self.auto_form.edit_rule == Some(i);
            let dev_name = devs
                .iter()
                .find(|(id, _)| id.0 == rule.device)
                .map(|(_, n)| n.clone())
                .unwrap_or_else(|| rule.device.clone());
            let fg_name = self
                .prefs
                .profiles
                .iter()
                .find(|p| {
                    p.name == rule.on_fg && p.devices.iter().any(|t| t.device == rule.device)
                })
                .map(|_| rule.on_fg.clone())
                .unwrap_or_else(|| format!("{} (未検出)", rule.on_fg));
            let leave_name = if rule.on_leave.is_empty() {
                "なし".to_string()
            } else {
                self.prefs
                    .profiles
                    .iter()
                    .find(|p| {
                        p.name == rule.on_leave
                            && p.devices.iter().any(|t| t.device == rule.device)
                    })
                    .map(|_| rule.on_leave.clone())
                    .unwrap_or_else(|| format!("{} (未検出)", rule.on_leave))
            };
            rule_body = rule_body.child(ui::row_first(
                format!("{} → {}", rule.process, dev_name).as_str(),
                Some(format!("前面: {fg_name} / 離脱: {leave_name}").as_str()),
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(if active {
                        ui::chip("適用中", p.ok)
                    } else if !rule.enabled {
                        ui::chip("無効", p.faint)
                    } else if is_running {
                        ui::chip("実行中", p.accent)
                    } else {
                        ui::chip("待機", p.faint)
                    })
                    .child(ui::toggle(
                        format!("rule-en-{i}"),
                        rule.enabled,
                        cx,
                        move |this, v, cx| {
                            if let Some(r) = this.prefs.proc_rules.get_mut(i) {
                                r.enabled = v;
                            }
                            this.prefs.save();
                            cx.notify();
                        },
                    ))
                    .child(ui::button(
                        format!("rule-edit-{i}"),
                        if editing { "閉じる" } else { "編集" },
                        Some("pencil"),
                        if editing { Btn::Glass } else { Btn::Ghost },
                        cx,
                        move |this, w, cx| {
                            if this.auto_form.edit_rule == Some(i) {
                                this.auto_form.edit_rule = None;
                                this.set_input("auto-proc-name", "", w, cx);
                                cx.notify();
                                return;
                            }
                            let Some(rule) = this.prefs.proc_rules.get(i).cloned() else {
                                return;
                            };
                            let devs = dev_list(this);
                            if let Some(di) = devs.iter().position(|(id, _)| id.0 == rule.device) {
                                this.auto_form.rule_dev = di;
                            }
                            this.auto_form.rule_fg = usize::MAX;
                            this.auto_form.rule_leave = usize::MAX;
                            // プロファイル選択を名前から解決 (form の index に合わせる)。
                            let dev_idx = this.auto_form.rule_dev.min(devs.len().saturating_sub(1));
                            let fg_items = this.profile_names(dev_idx, &devs, false);
                            if let Some(fi) = fg_items.iter().position(|n| *n == rule.on_fg) {
                                this.auto_form.rule_fg = fi;
                            }
                            let leave_items = this.profile_names(dev_idx, &devs, true);
                            if let Some(li) = leave_items.iter().position(|n| *n == rule.on_leave) {
                                this.auto_form.rule_leave = li;
                            }
                            this.input("auto-proc-name", &rule.process, "", w, cx);
                            this.set_input("auto-proc-name", &rule.process, w, cx);
                            this.auto_form.edit_rule = Some(i);
                            cx.notify();
                        },
                    ))
                    .child(ui::button(
                        format!("rule-del-{i}"),
                        "削除",
                        Some("trash-2"),
                        Btn::Ghost,
                        cx,
                        move |this, _, cx| {
                            this.prefs.proc_rules.remove(i);
                            if this.auto_form.edit_rule == Some(i) {
                                this.auto_form.edit_rule = None;
                            } else if let Some(e) = this.auto_form.edit_rule {
                                if e > i {
                                    this.auto_form.edit_rule = Some(e - 1);
                                }
                            }
                            this.prefs.save();
                            cx.notify();
                        },
                    )),
            ));
        }
        // ルール追加フォーム。
        let rule_dev_sel = self
            .auto_form
            .rule_dev
            .min(dev_names.len().saturating_sub(1));
        let fg_items = self.profile_names(rule_dev_sel, &devs, false);
        let leave_items = self.profile_names(rule_dev_sel, &devs, true);
        // 未選択 (usize::MAX) の既定: on_fg = 先頭のプロファイル、on_leave = なし(0)。
        let fg_sel = if fg_items.is_empty() {
            usize::MAX
        } else if self.auto_form.rule_fg == usize::MAX {
            0
        } else {
            self.auto_form.rule_fg.min(fg_items.len() - 1)
        };
        let leave_sel = if leave_items.is_empty() || self.auto_form.rule_leave == usize::MAX {
            0
        } else {
            self.auto_form.rule_leave.min(leave_items.len() - 1)
        };
        let proc_in = self.input(
            "auto-proc-name",
            "",
            "exe 名 (例: valorant-win64-shipping / 部分一致可)",
            window,
            cx,
        );
        // 実行中のアプリ一覧 → ドロップダウンで選ぶと入力欄へ入る。
        let proc_items: Vec<crate::app::PopItem> = running
            .iter()
            .map(|(exe, title)| {
                let mut it = crate::app::PopItem::new(exe.clone(), false);
                if !title.is_empty() {
                    it.sub = Some(title.clone());
                }
                it.icon = Some("app-window");
                it
            })
            .collect();
        let editing = self.auto_form.edit_rule;
        let exe_list: Vec<String> = running.iter().map(|(e, _)| e.clone()).collect();
        rule_body = rule_body
            .child(ui::row(
                "監視するプロセス",
                Some("実行中のアプリから選ぶか、exe 名を直接入力 (部分一致)"),
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .w(px(260.))
                            .child(gpui_kit::component::input::Input::new(&proc_in)),
                    )
                    .child(ui::select(
                        "auto-proc-pick",
                        proc_items,
                        "実行中のアプリ…",
                        220.,
                        cx,
                        move |this, i, w, cx| {
                            if let Some(exe) = exe_list.get(i).cloned() {
                                this.set_input("auto-proc-name", &exe, w, cx);
                            }
                        },
                    ))
                    .child(ui::button(
                        "proc-capture",
                        "前面を取得",
                        Some("scan"),
                        Btn::Ghost,
                        cx,
                        move |this, w, cx| {
                            if let Some(exe) = procmon::foreground_app() {
                                this.set_input("auto-proc-name", &exe, w, cx);
                            } else {
                                this.toast("前面アプリを取得できません", true, cx);
                            }
                        },
                    )),
            ))
            .child(ui::row(
                "対象デバイス",
                None,
                div().child(chips_row(
                    "rule-dev",
                    &dev_names,
                    rule_dev_sel,
                    cx,
                    Rc::new(|this, i, cx| {
                        this.auto_form.rule_dev = i;
                        // デバイスが変わったらプロファイル選択をリセット。
                        this.auto_form.rule_fg = usize::MAX;
                        this.auto_form.rule_leave = usize::MAX;
                        cx.notify();
                    }),
                )),
            ))
            .child(ui::row(
                "前面になったら適用",
                None,
                div().child(chips_row(
                    "rule-fg",
                    &fg_items,
                    fg_sel,
                    cx,
                    Rc::new(|this, i, cx| {
                        this.auto_form.rule_fg = i;
                        cx.notify();
                    }),
                )),
            ))
            .child(ui::row(
                "前面を外れたら適用",
                Some("「なし」なら何もしない (手動で戻す)"),
                div().child(chips_row(
                    "rule-leave",
                    &leave_items,
                    leave_sel,
                    cx,
                    Rc::new(|this, i, cx| {
                        this.auto_form.rule_leave = i;
                        cx.notify();
                    }),
                )),
            ))
            .child(ui::row_first(
                if editing.is_some() {
                    "ルールを更新"
                } else {
                    "ルールを追加"
                },
                Some(if editing.is_some() {
                    "フォームの内容で上書きします"
                } else {
                    "約1秒ごとに前面アプリを確認します (最小化中も動作)"
                }),
                {
                    let commit = Rc::new(
                        |this: &mut Shell, w: &mut Window, cx: &mut Context<Shell>| {
                            let proc = this.input_value("auto-proc-name", cx).unwrap_or_default();
                            let proc = proc.trim().to_lowercase();
                            if proc.is_empty() {
                                this.toast("プロセス名を入力してください", true, cx);
                                return;
                            }
                            let devs = dev_list(this);
                            let dev_idx = this.auto_form.rule_dev.min(devs.len().saturating_sub(1));
                            let Some((id, _)) = devs.get(dev_idx) else {
                                return;
                            };
                            let fg_items = this.profile_names(dev_idx, &devs, false);
                            if fg_items.is_empty() {
                                this.toast("先にプロファイルを作成してください", true, cx);
                                return;
                            }
                            let fg_sel = if this.auto_form.rule_fg == usize::MAX {
                                0
                            } else {
                                this.auto_form.rule_fg.min(fg_items.len() - 1)
                            };
                            let leave_items = this.profile_names(dev_idx, &devs, true);
                            let leave_sel = if this.auto_form.rule_leave == usize::MAX {
                                0
                            } else {
                                this.auto_form
                                    .rule_leave
                                    .min(leave_items.len().saturating_sub(1))
                            };
                            let on_fg = fg_items[fg_sel].clone();
                            let on_leave = if leave_items.is_empty() || leave_sel == 0 {
                                String::new()
                            } else {
                                leave_items[leave_sel].clone()
                            };
                            if let Some(ei) = this.auto_form.edit_rule {
                                if let Some(r) = this.prefs.proc_rules.get_mut(ei) {
                                    r.process = proc;
                                    r.device = id.0.clone();
                                    r.on_fg = on_fg;
                                    r.on_leave = on_leave;
                                }
                                this.auto_form.edit_rule = None;
                            } else {
                                this.prefs.proc_rules.push(crate::prefs::ProcRule {
                                    process: proc,
                                    device: id.0.clone(),
                                    on_fg,
                                    on_leave,
                                    enabled: true,
                                });
                            }
                            this.prefs.save();
                            this.set_input("auto-proc-name", "", w, cx);
                            cx.notify();
                        },
                    );
                    let mut b = div().flex().items_center().gap(px(8.)).child(ui::button(
                        if editing.is_some() {
                            "rule-update"
                        } else {
                            "rule-add"
                        },
                        if editing.is_some() {
                            "更新"
                        } else {
                            "追加"
                        },
                        Some(if editing.is_some() { "check" } else { "plus" }),
                        Btn::Primary,
                        cx,
                        move |this, w, cx| commit(this, w, cx),
                    ));
                    if editing.is_some() {
                        b = b.child(ui::button(
                            "rule-cancel",
                            "キャンセル",
                            None,
                            Btn::Ghost,
                            cx,
                            |this, w, cx| {
                                this.auto_form.edit_rule = None;
                                this.set_input("auto-proc-name", "", w, cx);
                                cx.notify();
                            },
                        ));
                    }
                    b
                },
            ));
        let pending_n: usize = self.proc_pending.iter().map(|(_, q)| q.len()).sum();
        let proc_trailing = (pending_n > 0)
            .then(|| ui::chip(format!("保留 {pending_n} 件"), p.warn).into_any_element());
        page = page.child(
            ui::card_titled("プロセス連動", "monitor", proc_trailing).child(rule_body),
        );

        div()
            .id("auto-scroll")
            .size_full()
            .overflow_y_scroll()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(theme::PAGE_GAP))
                    .p(px(theme::PAGE_PAD))
                    .pt(px(theme::PAGE_TOP))
                    .pb(px(theme::PAGE_BOT))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(6.))
                            .child(ui::title("自動化"))
                            .child(ui::sub(
                                "プロファイルはデバイスへ保存される設定のまとまりです。プロセス連動は「アプリが前面にある間だけ」適用し、離れると復帰プロファイルを適用します。",
                            )),
                    )
                    .child(page),
            )
    }
}
