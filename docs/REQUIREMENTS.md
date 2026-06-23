# emakiWM — niri風スクロール型タイリングWM for Windows 要件定義・設計書

- 版: 0.1 (2026-06-11)
- 対象OS: Windows 10 21H2+ / Windows 11
- 実装言語: Rust (windows crate / windows-rs)
- ライセンス方針: MIT (商用利用制限なし)

## 1. 背景と目的

niri (https://github.com/YaLTeR/niri) の「スクロール型タイリング」ワークフローを
Windows 上で実現する。Windows では DWM (Desktop Window Manager) を置換できないため、
komorebi / GlazeWM と同様に **外部プロセスから既存ウィンドウを操作するオーバーレイ型WM**
として実装する。

### niri モデルの核心仕様(本プロジェクトで再現するもの)

1. 各モニタは右方向に無限に伸びる仮想的な「ストリップ」を持つ
2. ウィンドウは「列 (Column)」単位でストリップ上に水平に並ぶ
3. **新しいウィンドウを開いても既存ウィンドウは決してリサイズされない**
4. モニタは「ビューポート」としてストリップの一部を表示する
5. フォーカス移動に追従してビューポートがスクロールする

### 非ゴール (Out of Scope)

- DWM の置換、独自コンポジタの実装
- ウィンドウ装飾(タイトルバー)の描画変更
- Linux/macOS 対応
- GUI 設定画面 (設定はテキストファイルのみ)
- Windows 仮想デスクトップ連携 (単一デスクトップ前提。他デスクトップのウィンドウは cloaked として無視し、デスクトップ切替時の uncloak 一斉発火への追従は保証しない)

## 2. 用語定義

| 用語 | 定義 |
|------|------|
| Strip | モニタごとに存在する無限幅の仮想水平空間。原点 x=0、右方向に伸びる |
| Column | Strip 上に並ぶ縦の区画。幅を持ち、1つ以上の Tile を縦に積む |
| Tile | Column 内の1ウィンドウ枠。実ウィンドウ (HWND) と1:1対応 |
| Viewport | モニタの作業領域 (タスクバー除く) が Strip 上のどこを映しているか。`offset_x` で表現 |
| Workspace | Strip + Viewport の組。モニタごとに複数持ち、縦方向に切替 (v1では1モニタ1個でも可) |
| Managed Window | 本WMが配置管理する HWND。除外ルールに該当しないトップレベル可視ウィンドウ |

## 3. 機能要件

### FR-1 ウィンドウ管理ライフサイクル
- FR-1.1 起動時に `EnumWindows` で既存のトップレベルウィンドウをスキャンし、管理対象を Strip に取り込む
- FR-1.2 `SetWinEventHook` で以下を購読し、リアルタイムに状態へ反映する:
  - `EVENT_OBJECT_SHOW` (新規ウィンドウ。`EVENT_OBJECT_CREATE` はスタイル未確定の段階で発火するため SHOW を主とする)
  - `EVENT_OBJECT_DESTROY` / `EVENT_OBJECT_HIDE` (消滅)
  - `EVENT_OBJECT_CLOAKED` / `EVENT_OBJECT_UNCLOAKED` (UWP の遅延実体化検出。§8-3)
  - `EVENT_OBJECT_LOCATIONCHANGE` (自己発生イベントの抑制・実サイズ読み戻しに使用。§8-1, §8-4)
  - `EVENT_SYSTEM_FOREGROUND` (フォーカス変更)
  - `EVENT_SYSTEM_MINIMIZESTART` / `MINIMIZEEND`
  - `EVENT_SYSTEM_MOVESIZEEND` (ユーザーによる手動移動・リサイズ)
- FR-1.2.1 WinEvent コールバックは `idObject == OBJID_WINDOW && idChild == CHILDID_SELF` かつトップレベル HWND のみ処理する (DESTROY / LOCATIONCHANGE 等は子 UI 要素でも大量発火するため必須のフィルタ)
- FR-1.3 新規ウィンドウは「フォーカス中の Column の右隣」に新しい Column として挿入する。既存 Column の幅・位置 (Strip座標) は変更しない
- FR-1.3.1 挿入後、新規ウィンドウへフォーカスを移し、Viewport を追従させる (niri 挙動)
- FR-1.4 ウィンドウ消滅時は Column から除去し、空になった Column は詰める (右側の Column が左へシフト)
- FR-1.5 WM 終了時 (正常終了・パニック時とも) に全 Managed Window を可視領域内へ復元する (必須。画面外に取り残さない)
- FR-1.6 panic ハンドラはプロセス kill・電源断をカバーできないため、`restore_rect` を `%LOCALAPPDATA%\emakiwm\state.json` へ永続化し (管理ウィンドウの増減時に書き出し、正常終了時に削除)、`emakiwm --restore` で事後復元できるレスキューコマンドを提供する

### FR-2 管理対象の判定
- FR-2.1 以下は管理対象外 (フローティング扱い、位置を触らない):
  - `WS_EX_TOOLWINDOW`、オーナー付きウィンドウ (ダイアログ)、`WS_POPUP` のみのウィンドウ
  - cloaked ウィンドウ (`DwmGetWindowAttribute(DWMWA_CLOAKED)` ≠ 0) — UWPゴースト対策。他仮想デスクトップのウィンドウもここに含まれる (非ゴール参照)
  - リサイズ不可 (`WS_THICKFRAME` なし) のウィンドウ
  - 自プロセスのウィンドウ
- FR-2.2 設定ファイルで exe名 / ウィンドウクラス / タイトルの正規表現により ignore / force-manage / force-float ルールを定義できる
- FR-2.3 管理者権限プロセスのウィンドウは操作失敗 (UIPI) を握りつぶし、untracked としてマークする (クラッシュ・無限リトライ禁止)

### FR-3 レイアウトと配置
- FR-3.1 Column は Strip 座標 `(x, width)` を持つ。Tile は Column 内で等分割 (v1)、weight 指定は v2
- FR-3.2 配置時、Viewport 内に見えている Column のみ実座標へ `DeferWindowPos` バッチで配置する
- FR-3.3 Viewport 外の Column の隠蔽方式は設定 (`hide`) で選択可能:
  - `offscreen` (デフォルト): モニタ外座標へ移動 (Alt+Tab・タスクバーに残る)
  - `cloak`: shell cloak + `WS_EX_TOOLWINDOW` で完全に隠す (Alt+Tab・タスクバーからも消える)。スライドイン前に戻し、アニメーション完了後に隠す。自分が発生させた CLOAKED イベントは無視する
    - 注: `DwmSetWindowAttribute(DWMWA_CLOAK)` は他プロセスには E_ACCESSDENIED で使えないため、非公開シェル COM の `IApplicationView::SetCloak` を使う
    - 注: cloak は描画停止のみで Alt+Tab の一覧には残る (仮想デスクトップの一覧フィルタは別機構)。一覧から消す `SetShowInSwitchers` は Win11 25H2 (26200) で E_NOTIMPL のため、文書化された WS_EX_TOOLWINDOW の一時付与で行う (管理対象は adopt 時点で TOOLWINDOW を持たないため復元時の無条件クリアが安全)
- FR-3.4 Column 幅プリセット: 画面幅の 1/3, 1/2, 2/3 をキーでサイクル。任意 px/% リサイズも可
- FR-3.5 gap (Column 間・画面端) を設定可能 (デフォルト 8px)
- FR-3.6 ウィンドウ最小/最大サイズ制約 (`WM_GETMINMAXINFO`) により要求サイズ通りにならない場合、実サイズを受け入れて Strip 座標のみ論理値を維持する
- FR-3.7 フォーカス中 / 非フォーカスのウィンドウ枠色を設定可能 (`DWMWA_BORDER_COLOR`、Win11)。未指定なら枠色には触らない。終了・レスキュー時は OS 既定色へ戻す。`border_thickness` > 0 で太枠オーバーレイ (クリックスルー・DWM 角丸・対象直下の Z) を管理ウィンドウに敷く
- FR-3.8 非フォーカスウィンドウの半透明化を設定可能 (`unfocused_opacity`、WS_EX_LAYERED + alpha)。アプリ自身が layered を使う窓は触らない。終了・レスキュー時は不透明へ戻す。`toggle-opacity` コマンドでウィンドウ単位の opacity ピン (非フォーカスでも `pinned_opacity` を維持) をトグルできる

### FR-4 ナビゲーションとスクロール
- FR-4.1 フォーカス移動: 左右 Column / Column 内上下 Tile。左右の端では停止 (ラップしない)。上下の端ではさらに押すと Workspace 切替へ続く (FR-5.4)
- FR-4.2 フォーカス先 Column が Viewport 外なら、その Column 全体が見える最小移動量で `offset_x` を自動調整する
- FR-4.3 明示的スクロール操作 (Viewport を1 Column / 半画面ぶん左右へ)
- FR-4.4 Column の並べ替え (フォーカス Column を左右へ移動)
- FR-4.5 Tile の Column 間移動 (右隣へ押し出して独立 / 右隣を取り込んで縦スタック化)。キーは Alt+Shift+Period / Comma (J/K は Workspace 移動に使用)
- FR-4.6 maximize-column トグル: フォーカス Column を一時的に Viewport 全幅へ。解除で元の幅へ復帰
- FR-4.7 fullscreen トグル: フォーカスウィンドウをモニタ全面 (タスクバー含む) へ
- FR-4.8 アニメーション: レイアウト変化 (スクロール・列の詰め・挿入) を各ウィンドウ Rect の約 180ms / ease-out 補間で滑らかに反映する (60fps 保証はしない)。設定で無効化・時間調整可能 (Phase 4)

### FR-5 マルチモニタ・Workspace
- FR-5.1 モニタごとに独立した Strip を持つ。ウィンドウのモニタ間移動コマンドを提供する
- FR-5.2 モニタの接続/切断 (`WM_DISPLAYCHANGE`) で、消えたモニタの Column を他モニタの Strip 末尾へ退避する
- FR-5.3 Per-Monitor DPI v2 対応 (マニフェストで宣言)。座標計算は物理ピクセルで統一する
- FR-5.5 作業領域の変更 (解像度・タスクバー) に WM_DISPLAYCHANGE / WM_SETTINGCHANGE で追従して relayout する。AppBar 登録しないバー (Zebar 等) のために画面余白 (`margin = [上,右,下,左]`) を設定できる
- FR-5.4 モニタごとに複数 Workspace を縦に持つ (niri の動的モデル、v2 から前倒し):
  - 末尾に常に空の Workspace が 1 つあり、空になった中間 Workspace は自動消滅する
  - Alt+Shift+J/K でフォーカスウィンドウを下/上の Workspace へ移動 (フォーカス追従、下端では動的作成)
  - Alt+U/I で Workspace 切替。切替・移動は縦スライドのアニメーション

### FR-6 入力・操作系
- FR-6.1 グローバルホットキーは `RegisterHotKey` で登録 (低レベルフックは使わない。AV誤検知・遅延リスク回避)
- FR-6.2 デフォルトの modifier は `Alt`。設定で `Win` 等へ変更可能
- FR-6.3 全キーバインドは設定ファイルで再定義可能
- FR-6.4 既知の制約 (ドキュメントに明記する):
  - 非昇格プロセスの `RegisterHotKey` は、管理者権限ウィンドウがフォアグラウンドの間は発火しない (UIPI)
  - Alt+Shift 系バインドは Windows のキーボードレイアウト切替 (Alt+Shift) と衝突し得る。切替ホットキーの無効化手順を README で案内する

### FR-7 設定・IPC
- FR-7.1 設定ファイル: TOML。場所は `%USERPROFILE%\.config\emakiwm\config.toml`
- FR-7.2 設定ファイルのホットリロード (ファイル監視 or リロードコマンド)
- FR-7.3 名前付きパイプによる IPC サーバーを持ち、CLI (`emakiwmc`) から全操作・状態取得 (JSON) ができる
- FR-7.4 状態取得 API はステータスバー連携 (Zebar / yasb 等) を想定した購読 (subscribe) モードを持つ
- FR-7.5 webview 系バー (Zebar) 向けに WebSocket (`ws_port`、localhost のみ) でも購読と同じ state JSON を配信する。サンプルウィジェットを examples/zebar/ に同梱

## 4. 非機能要件

- NFR-1 操作レイテンシ: キー入力から配置完了まで 50ms 以内 (アニメーション除く)
- NFR-2 アイドル時 CPU 使用率 ≈ 0% (ポーリング禁止、完全イベント駆動)
- NFR-3 メモリ常駐 50MB 以下
- NFR-4 クラッシュ耐性: panic ハンドラで FR-1.5 のウィンドウ復元を必ず実行する。強制終了に備え FR-1.6 の永続化 + `--restore` も提供する
- NFR-5 単一実行ファイル配布 (emakiwm.exe + emakiwmc.exe)。インストーラ不要
- NFR-6 ログ: `tracing` クレートで構造化ログ。`--verbose` でイベントダンプ

## 5. アーキテクチャ

```
┌────────────────────────────────────────────────────────┐
│ emakiwm.exe                                            │
│                                                        │
│  [Win32 スレッド]                                       │
│   ├ メッセージループ (RegisterHotKey, WM_DISPLAYCHANGE) │
│   └ SetWinEventHook コールバック                        │
│        ↓ Event enum を mpsc チャネルへ送信              │
│  [Core スレッド]                                        │
│   ├ State: Vec<Monitor> → Vec<Workspace> → Strip       │
│   ├ reducer: (State, Event) -> (State, Vec<Cmd>)       │
│   │   ※ 純粋関数として実装し単体テスト可能にする        │
│   └ animator: offset_x 補間タイマー                     │
│        ↓ Cmd                                           │
│  [Renderer]                                            │
│   ├ DeferWindowPos バッチ適用                           │
│   ├ cloak / uncloak                                    │
│   └ 自己発生イベントの抑制 (下記 8.1 参照)              │
│  [IPC スレッド] 名前付きパイプ \\.\pipe\emakiwm         │
└────────────────────────────────────────────────────────┘
   emakiwmc.exe (CLI) ──JSON──▶ IPC
```

### 5.1 状態モデル (Rust スケッチ)

```rust
struct State {
    monitors: Vec<Monitor>,
    focused: Option<WindowId>,
}
struct Monitor {
    handle: HMONITOR,
    work_area: Rect,      // 物理px
    workspace: Workspace, // v2 で Vec<Workspace>
}
struct Workspace {
    columns: Vec<Column>,
    offset_x: i32,        // Viewport 左端の Strip 座標
    anim: Option<ScrollAnim>,
}
struct Column {
    x: i32,               // Strip 座標
    width: ColumnWidth,   // Proportion(f32) | Fixed(i32)
    tiles: Vec<Tile>,
}
struct Tile {
    hwnd: HWND,
    state: TileState,     // Managed | Untracked | Fullscreen
    restore_rect: Rect,   // WM 終了時の復元用
}
```

### 5.2 主要 Win32 API

| 目的 | API |
|------|-----|
| ウィンドウ列挙 | `EnumWindows`, `IsWindowVisible`, `GetWindowLongPtrW` |
| イベント購読 | `SetWinEventHook` (WINEVENT_OUTOFCONTEXT) |
| 配置 | `BeginDeferWindowPos` / `DeferWindowPos` / `EndDeferWindowPos` |
| 実フレーム取得 | `DwmGetWindowAttribute(DWMWA_EXTENDED_FRAME_BOUNDS)` ※下記 8.2 |
| cloak | `DwmSetWindowAttribute(DWMWA_CLOAK)` (非公開寄り、要検証) |
| フォーカス | `SetForegroundWindow` + `AllowSetForegroundWindow` 対策 |
| モニタ | `EnumDisplayMonitors`, `GetMonitorInfoW`, `GetDpiForMonitor` |
| ホットキー | `RegisterHotKey` |

## 6. レイアウトアルゴリズム

```
project(workspace, work_area):
  visible = work_area.width
  for col in columns:
    screen_x = col.x - offset_x + work_area.left + gap調整
    if col が [offset_x, offset_x + visible] と交差:
      col 内の各 tile を縦等分割して DeferWindowPos に積む
    else:
      隠蔽方式に応じて offscreen 移動 or cloak
focus_follow(col):
  if col.x < offset_x:                 offset_x = col.x
  elif col.right > offset_x + visible: offset_x = col.right - visible
```

- 挿入: `columns.insert(focused_idx + 1, new_col)`、以降の Column の x を new_col.width ぶん右へシフト
- 削除: 逆操作。offset_x が末尾を超えたらクランプ

## 7. デフォルトキーバインド

| キー | 動作 |
|------|------|
| Alt+H / Alt+L | 左 / 右の Column へフォーカス |
| Alt+J / Alt+K | Column 内 下 / 上の Tile へフォーカス (端ではさらに Workspace 切替) |
| Alt+Shift+H / L | Column を左 / 右へ移動 |
| Alt+Shift+J / K | ウィンドウを下 / 上の Workspace へ移動 (下端で動的作成) |
| Alt+U / Alt+I | Workspace を下 / 上へ切替 |
| Alt+Shift+Period / Comma | Tile を隣 Column へ押し出し / 取り込み |
| Alt+R | Column 幅プリセットをサイクル (1/3 → 1/2 → 2/3) |
| Alt+F | maximize-column トグル |
| Alt+Shift+F | fullscreen トグル |
| Alt+Comma / Period | Viewport を左 / 右へスクロール |
| Alt+Shift+Q | フォーカスウィンドウを閉じる |
| Alt+Shift+E | WM 終了 (全ウィンドウ復元) |

## 8. 既知の罠・エッジケース (実装時必読)

1. **自己発生イベントのループ**: 自分の SetWindowPos が EVENT_OBJECT_LOCATIONCHANGE 等を発火させる。配置中フラグ + HWND ごとの「期待 Rect」を持ち、一致するイベントは無視する
2. **不可視フレーム**: Win10/11 のウィンドウは `GetWindowRect` に影付き不可視ボーダーを含む。`DWMWA_EXTENDED_FRAME_BOUNDS` との差分をオフセットとして補正しないと gap がガタつく
3. **UWP / ApplicationFrameWindow**: 起動直後は cloaked で、後から実体化する。EVENT_OBJECT_UNCLOAKED も監視する
4. **最小サイズ制約**: 要求幅より大きくなるウィンドウ (例: 一部 Electron アプリ) は隣と重なる。実サイズを読み戻して Strip 上の論理幅を更新する
5. **SetForegroundWindow 制限**: フォアグラウンドロックにより失敗することがある。`keybd_event` での Alt 空打ちワークアラウンド等は最終手段とし、まず AttachThreadInput を検討
6. **DPI 混在**: モニタ間でウィンドウを移動すると WM_DPICHANGED で自動リサイズが走る。移動→配置を 2 段階に分ける
7. **Explorer 再起動**: タスクバー再生成で work_area が変わる。WM_SETTINGCHANGE / WM_DISPLAYCHANGE で再計算
8. **アニメーション中の新イベント**: 進行中の補間は中断し、最新ターゲットへ向け直す (キューに溜めない)

## 9. 実装フェーズ計画 (Claude Code 向け)

各フェーズは独立にビルド・動作確認可能であること。受入条件を満たしてから次へ進む。

### Phase 0: 骨組み
- cargo workspace: `crates/emakiwm` (本体 bin) / `crates/emakiwmc` (CLI bin、空スケルトン) / `crates/emakiwm-core` (純粋ロジック lib。※ crate 名 `core` は Rust 組み込みクレートと衝突するため不可)
- windows crate (0.62) セットアップ、tracing 導入
- Per-Monitor v2 DPI マニフェスト埋め込み (`embed-manifest`)。これがないと dry-run の座標出力が DPI 仮想化で狂うため Phase 0 で行う (FR-5.3 の前倒し)
- 管理対象判定は純粋関数として emakiwm-core に実装し単体テストを付ける: `WindowInfo { style, ex_style, cloaked, has_owner, ... } -> Decision { Manage | Float(理由) | Ignore(理由) }`
- `emakiwm --dry-run`: 全トップレベルウィンドウの hwnd / exe / class / title / 判定+理由 / 昇格状態 (FR-2.3) を表出力する。`GetWindowRect` と `DWMWA_EXTENDED_FRAME_BOUNDS` の両方を出力し、§8-2 の不可視フレーム差分を先行検証する
- 開発環境: WSL2 + Nix (flake.nix)。`cargo build --target x86_64-pc-windows-gnu` でクロスビルドし、WSL interop で .exe を直接実行して確認する
- 受入: メモ帳 / ブラウザ / UWP 設定アプリ / 管理者 cmd / ツールウィンドウ各種に対し、期待判定 (manage / float / ignore) と dry-run 出力が一致する

### Phase 1: 静的タイリング (スクロールなし)
- 単一モニタ、Strip に Column を並べ、Viewport 内のみ配置 (はみ出しは offscreen)
- WinEventHook で開閉に追従。終了時復元 (FR-1.5)
- 受入: ウィンドウを 5 個開閉しても既存ウィンドウがリサイズされず、終了で全部戻る

### Phase 2: フォーカスナビゲーション + 自動スクロール
- Alt+H/L/J/K、フォーカス追従スクロール (FR-4.2)、EVENT_SYSTEM_FOREGROUND 同期
- 新規ウィンドウへのフォーカス移動 (FR-1.3.1)、スクロールアニメーション (FR-4.8 を Phase 5 から前倒し)
- 受入: 画面外の Column へフォーカスすると Viewport が追従する

### Phase 3: 列操作
- 幅プリセット、Column/Tile 移動、maximize-column、fullscreen、不可視フレーム補正
- 受入: 全デフォルトキーバインドが仕様通り動く

### Phase 4: 設定 + IPC
- TOML 設定、ルール (FR-2.2)、ホットリロード、名前付きパイプ + emakiwmc
- 受入: `emakiwmc state` が JSON を返し、`emakiwmc focus left` が効く

### Phase 5: マルチモニタ + 仕上げ
- 実装済み: 状態永続化 + `--restore` (FR-1.6)、cloak モード (FR-3.3)、subscribe (FR-7.4)、アニメーションの設定化 (Phase 4 で実装)
- 未着手: モニタ間移動、切断退避、DPI 混在 (FR-5.1/5.2) — 2 モニタ環境での検証が必要
- 受入: 2 モニタ + DPI 混在環境で破綻しない

## 10. テスト戦略

- emakiwm-core クレートの reducer / レイアウト計算は HWND に依存させず、純粋関数として単体テスト (挿入・削除・スクロール・クランプの境界値)
- Win32 層は手動チェックリスト (メモ帳 / ブラウザ / Electron / UWP / 管理者 cmd で各操作)
- CI: `cargo test` + `cargo clippy -- -D warnings` (GitHub Actions, windows-latest)

## 11. 参考プロジェクト

- niri (本家・設計思想): https://github.com/YaLTeR/niri
- komorebi (Win32 オーバーレイ型WM、ライセンスは PolyForm Non-Commercial): https://github.com/LGUG2Z/komorebi
- GlazeWM (MIT、Rust 実装の参考に可): https://github.com/glzr-io/glazewm
- OpenNiri-Windows (同コンセプトの先行例): https://github.com/AdEx-Partners-DE/OpenNiri-Windows
- PaperWM (スクロール型の元祖): https://github.com/paperwm/PaperWM
- windows-rs: https://github.com/microsoft/windows-rs
