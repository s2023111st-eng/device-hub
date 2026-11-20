# Logitech G (HID++ 2.0) — G HUB 置き換えの解析記録

対象: PRO X SUPERLIGHT (WPID 0x4093 / 受信機 046d:c547 / 有線 046d:c094) と
PRO X TKL RAPID (046d:c35b)。ドライバは `crates/devhub-hid/src/logitech/`。

## 共通: HID++ 2.0

- 設定用コレクションは vendor page `FF00`。usage 1 = short (report 0x10, 7B)、
  usage 2 = long (report 0x11, 20B)。Windows では別々の HID コレクションになる。
- 要求 `[0x11, dev, featIdx, fn<<4 | swid, params…]`。応答は `[dev, featIdx, fn|swid]` が一致。
  エラーは `[0x11, dev, 0xFF, featIdx, fn|swid, code]`。
- `dev` は有線/直結 0xFF、受信機経由はペアリングスロット (1..6)。
- 受信機経由で相手が休眠/電源オフのとき、受信機は HID++ 1.0 エラー `10 01 8F … 08`
  (unknown device) を **short コレクション**に返す。long だけ開いていればタイムアウトになる。
- 本ドライバは long だけを使う (short 用の関数も long で受け付けることを実機で確認)。
  swid は 0x0D (G HUB は 0x0A)。
- フィーチャー index は IRoot (index 0, fn0 `[featHi, featLo]`) で引く。

## 調べ方 (再現手順)

1. **受動スニッフ**: Windows の HID 入力レポートは同じコレクションを開いた全ハンドルに
   届くので、hidapi で FF00 を開いて待つと G HUB 宛の応答がすべて見える。
2. **要求の記録**: `lghub_agent.exe` は一般ユーザー権限で動くので、Frida で
   `kernelbase!WriteFile` をフックすると送信 HID++ がそのまま取れる
   (agent を `frida.spawn` すると起動時の読み出しも全部取れる)。
3. **差分**: agent は `ws://localhost:9010` (サブプロトコル `json`, Origin `file://`) で
   `{msgId, verb, path, payload}` を受け付ける。`GET /card {id}` で設定カードを取り、
   値を 1 つ変えて `SET /card` → フックで送信バイト列の差分を見る → 元のカードを `SET` で戻す。

USB の ETW (`Microsoft-Windows-USB-UCX` 等, PartialDataBusTrace) は URB の長さまでしか
残らず、ペイロードは取れなかった。

## PRO X TKL RAPID (046d:c35b, HID++ 4.2, FW U170 4.20)

フィーチャー: 0x0003, 0x0005, 0x0020, 0x0011, 0x8071, 0x8081, 0x1B10, 0x4523, 0x4540,
0x8040, 0x8101, 0x1B05, 0x8051, 0x1B08, 0x00C3, 0x00D0, …

| フィーチャー | 関数 | 意味 |
|---|---|---|
| 0x4540 | fn0 → `[layout]` | 0x0A = JIS (ANSI は metadata 側の別配置) |
| 0x1B10 | fn0 → `[count]` (=128) | コントロール数 |
| 0x1B10 | fn1 `[offset]` → CID u16 BE × 8 | **並び順 = アナログ設定のキー番号** |
| 0x1B08 | fn0 → `01 05 80 28` | 能力 (0x28 = 4.0mm 上限) |
| 0x1B08 | fn1 → `[on]` | ラピッドトリガー全体 ON/OFF の取得 |
| 0x1B08 | fn2 `[on]` | 同 設定 (G HUB の `rapid_trigger_state`) |
| 0x8101 | fn8 `[part, file, lenHi, lenLo]` + fn12 `[offHi, offLo]` → 16B | ファイル読み出し |
| 0x8101 | fn2 `[lenHi, lenLo, 0]` → fn3 16B × n → fn9 | ホスト側設定の書き込み (下記) |

### ホスト側アナログ設定 (G HUB と同じ方式)

`fn9 [0x1B, 0x08, sub, 0x02, len (3B BE), crc32 (BE)]` で確定。CRC は zlib の CRC-32。

- `sub 0` 作動点マップ `[count u16 BE][key, 値] × count`。値は 0.1mm 単位 (0.1–4.0mm)。
  キー集合は 0x00–0x4A, 0x4C–0x5C, 0x64–0x6A, 0x6E, 0x6F の 101 個 (204B)。
- `sub 1` キー別ラピッドトリガー量 (0.1–2.0mm)。G HUB は個別指定のキーだけ送るが、
  全キーを明示しても受理される (本ドライバは全キー送信)。
- `sub 3` 2B `00 00` (G HUB が毎回送る空の束。バインディング機能用と推定)。

ホスト側設定は読み戻せず (パーティション 2 は読み出しエラー)、電源断で消える。
本体はオンボード側 (パーティション 0/1 のファイル) の既定に戻るので、
**常駐エージェントが接続のたびに送り直す** (`device-hub.exe --agent`)。

キャプチャ例 (WASD = キー 0x1D/0x21/0x22/0x23 を 0.1mm、他 2.0mm):
`00 65 00 14 01 14 …` 204B, CRC32 `16517288` — G HUB の送信と一致 (`tkl.rs` のテスト)。

### オンボードのファイル (読み取りのみ確認)

`fn8 [part, 0, 0x04, 0x00]` でディレクトリ: `[crc32 4B][count][file, feat 2B, flags, len, crc32 4B, slot] × n`。
パーティション 1 / 0 とも 0x8101 プロファイル 3 個 (109B) + 0x1B08 データ 1 個 (190B)。
パーティション 0 のファイルはスロット番号 (0x08–0x0B) で開く。書き込み方式は未解析。

### キー配置

G HUB の機種メタデータ (`depots/*/metadata.json` の `inputRegions`) の矩形と CID から
`tkl_layout.rs` を生成。JIS 91 / ANSI 87 キー。CID 312/290/308/293 = W/A/S/D。

## PRO X SUPERLIGHT (WPID 0x4093)

オンボードプロファイル (0x8100) — 形式 4 (base v1)。詳細は `superlight.rs` の冒頭と
OpenMouse-Project/mouse-protocol の `docs/logitech-onboard-profiles.md`。

- 書く前に全セクタ (0x0000, 0x0001–5, 0x0101–5) を `%APPDATA%\DeviceHub\logitech\backup\` へ保存。
- ROM プロファイル (0x01xx) 使用中なら同番号のフラッシュ側へ複製し、ディレクトリ (0x0000) に登録。
- 書き込み後は読み戻して照合し、`setOnboardMode(1)` + `setCurrentProfile` で読み込ませる。
- 実機検証済 (2026-11-19, FW MPM25 2.23): getInfo `01 04 01 05 01 05 10 00 ff` = 形式 4・5 プロファイル・
  5 ボタン・16 セクタ・セクタ長 255。ディレクトリ `00 01 01 ff | 00 02 00 ff | …`、フラッシュの
  プロファイル 1 (0x0001) が使用中。ポーリング 1000→500→1000Hz の書き込み往復を読み戻しで照合
  (`examples/logie2e.rs`)。ROM 側 (0x0101) は CRC が一致しない (別形式) ので読み込みに使わない。
- 無線の応答は普段 ~27ms だが、しばらく触らないと初回だけ ~1.25s (リンク復帰)。
  タイムアウトはこれより長く (1.6s) 取る。

### ゲームごとの切替

- マウスは本体の 5 枠 (セクタ 0x0001–0x0005) を `ProfileCaps` として出す。`Change::Profile(i)` は
  `setCurrentProfile` (揮発 — フラッシュに書かない) なので、ゲームを出入りするたびの切替でも
  書き込みは発生しない。枠の中身が未使用 (CRC 不一致) なら初回だけ今の設定を複製し、
  ディレクトリで無効な枠を選べない場合は初回だけ有効化する。名前は 0xA0 の UTF-16LE (最大 23 文字)。
- キーボードはホスト側設定なので、切替 = 保存値の更新 + 再送 (フラッシュ書き込みなし)。
- ルールは「自動化」画面と同じ `prefs.json` (`profiles` / `proc_rules`)。常駐エージェントが
  1 秒ごとに前面 exe を評価して適用する (画面側と同じ `procmon::eval_rules`)。エージェント
  稼働中は画面側の評価を止める。マウス休眠中など受理されない変更は保留して再送する。
- 起動直後は全ルールの復帰先 (on_leave) を一度適用し、「どのゲームも前面でない」状態から始める。
