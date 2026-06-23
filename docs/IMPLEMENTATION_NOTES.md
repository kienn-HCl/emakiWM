# emakiWM 実装メモ

開発中に判明した非自明な設計判断・Win32 の罠・試行錯誤の記録。
REQUIREMENTS.md の要件定義を補完する。

---

## 1. アニメーション・レンダリング設計

### visual マップによる「論理状態」と「画面上の現在位置」の分離

`strip` は常に最終ターゲット（論理状態）を保持し、画面上の現在位置は
`visual: HashMap<u64, Rect>` で別管理する。イベントが来るたびにターゲット
集合を再計算し、**前回と差分があるときだけ** `visual` から新ターゲットへ
向け直す。差分がなければ進行中のアニメーションを乱さない。

これをしないと、イベントノイズ（LOCATIONCHANGE 等が連打される）で補間が
毎回リスタートして減速し続ける問題が起きる（§8-8）。

### 画面外→画面外の移動は即時反映

左退避・右退避の切り替えなど「画面外→画面外」の移動を補間すると、
Viewport を横切って飛ぶアニメーションが見えてしまう。
そのため `start_transition` 内でスナップ対象を検出して即時反映し、
`visual` を更新してから補間対象のみアニメーションする。

### 退避方向の決定

Viewport 外の Column を退避させるとき、射影上の `Rect.x` が
作業領域より小さければ左外、大きければ右外へ退避する。
これにより「左の Column へフォーカスしたとき、その Column が右から
滑り込んで見える」という違和感が解消された。

### アニメーション方式の変遷

初期は `offset_x` のみ補間していたが、列の詰め・挿入時シフト・
起動時流し込みをすべてアニメーション化するため、
各ウィンドウ `Rect` の `from → to` 補間方式に変更した。
時間は 140ms → 180ms（体感調整）。

### Core ループのアイドル CPU 設計

アニメーション中とアイドル時でイベント待ちの方法を切り替える。

- **アニメーション中**: `recv_timeout(8ms)` でタイムアウトのたびに起きてフレームを描く
- **アイドル時**: `recv()` でブロックし、イベントが来るまで CPU を使わない（NFR-2）

この切り替えをしないとアイドル中も 8ms ごとに空ループが回り続けて CPU 使用率が上がる。

---

## 2. Win32 API の罠・実装判断

### DWMWA_CLOAK は他プロセスに使えない

`DwmSetWindowAttribute(DWMWA_CLOAK)` は **自プロセスのウィンドウにのみ** 有効で、
他プロセスのウィンドウには `E_ACCESSDENIED (0x80070005)` が返る。
非公開シェル COM の `IApplicationViewCollection::GetViewForHwnd →
IApplicationView::SetCloak` で代替する（`com.rs` 参照）。

COM はスレッドごとに初期化が必要なため（復元は Ctrl ハンドラの
別スレッドからも走る）、ビューコレクションは `thread_local` に保持する。

### SetShowInSwitchers は Win11 25H2 (build 26200) で E_NOTIMPL

Alt+Tab・タスクビューの一覧からウィンドウを消すための
`IApplicationView::SetShowInSwitchers` は Win11 25H2 (build 26200) 時点で
`E_NOTIMPL` のため使用不可。代わりに `WS_EX_TOOLWINDOW` を一時付与する
（文書化された挙動：このフラグがあると Alt+Tab・タスクバーの一覧から消える）。

管理対象は adopt 時点で `WS_EX_TOOLWINDOW` を持たない（`decide()` が
float に落とすため）ので、解除時に無条件でビットをクリアしても
元のスタイルを壊さない。

### cloak / TOOLWINDOW の操作順序が重要

`WS_EX_TOOLWINDOW` が付与されている間、シェルは application view を
破棄するため `GetViewForHwnd` が失敗する。

- **隠すとき**: `SetCloak(true)` → `TOOLWINDOW` 付与（view がある間に cloak）
- **戻すとき**: `TOOLWINDOW` 解除 → `SetCloak(false)`（view が復活してから uncloak）

順序が逆だと uncloak が `TYPE_E_ELEMENTNOTFOUND` で失敗する。

### TYPE_E_ELEMENTNOTFOUND on SetCloak(false) は実質成功

`TOOLWINDOW` 解除直後はシェルの view 再作成が **非同期** のため、
`SetCloak(false)` が `TYPE_E_ELEMENTNOTFOUND` で返ることがある。
しかし再作成された view は cloak が外れた状態で戻るため、実質的には成功。
`warn` ではなく `debug` に格下げし、「画面内なのに cloak が残っている窓」を
アニメーション完了後の同期処理で検出して強制解除する保険を追加している。

### WM_DISPLAYCHANGE / WM_SETTINGCHANGE の受け方

これらのブロードキャストは **message-only ウィンドウでは受け取れない**。
不可視トップレベルウィンドウ（`WS_POPUP`・サイズ 0）を作成して受ける。

### kill 後の WS_EX_LAYERED 残留問題

強制終了後に `WS_EX_LAYERED + LWA_ALPHA (alpha < 255)` が残ると、
再起動後は「アプリ自身が layered を使っている窓」と誤認されて
`undim` できず半透明が固定化する。

`adopt` 時と `--uncloak-all` で `GetLayeredWindowAttributes` を確認し、
`LWA_ALPHA かつ alpha < 255` の窓は dim の取り残しとみなして不透明へ戻す。

### SetForegroundWindow 失敗時のリトライ

フォアグラウンドロックにより `SetForegroundWindow` が失敗することがある。
失敗時はフォアグラウンドスレッドへ `AttachThreadInput` してからリトライし、
完了後に必ず `AttachThreadInput(false)` でデタッチする（§8-5）。

### DeferWindowPos 失敗時のフォールバック

UIPI 等で `DeferWindowPos` が途中失敗するとバッチ全体が継続不能になる。
この場合は個別の `SetWindowPos` に切り替えてバッチ内の残りウィンドウを処理する。
失敗した 1 枚だけ飛ばして続行するより安全。

### IPC のマルチクライアント対応

`subscribe` コマンドは応答を返さずに接続を維持し、状態変化のたびに JSON を
push し続ける。この間も他のクライアント（`state` や操作コマンド）を
受け付けられるよう、`PIPE_UNLIMITED_INSTANCES` + 接続ごとのスレッドで実装する。
接続ごとにスレッドを立てることで、subscribe 中のクライアントがいても
新規接続がブロックされない。

---

## 3. 枠オーバーレイの設計変遷（FR-3.7）

3 段階の試行錯誤を経て現在の方式に落ち着いた。

#### 第 1 案: topmost ウィンドウ + GDI リージョン（額縁形の切り抜き）
- **問題**: topmost のためフローティングダイアログよりも上に枠が乗った
- **問題**: GDI リージョンの角丸はギザつく（アンチエイリアスなし）

#### 第 2 案: フォーカスウィンドウ直下の Z + DWM 角丸のベタ塗り矩形
- topmost を廃止し、対象ウィンドウの直下に Z 挿入することで解決
- 対象本体が中央を覆い隠すため「額縁形の切り抜き」が不要になる
- DWM 角丸 (`DWMWCP_ROUND`) を付けたベタ塗りで対象の角丸に沿った枠に見える
- **問題**: `FR-3.8` の半透明化（非フォーカスウィンドウ）と組み合わせると、
  半透明越しにベタ塗り矩形が透けて見えた

#### 第 3 案（現行）: per-pixel alpha の角丸リング（UpdateLayeredWindow + GDI+）
- GDI+ で角丸リングを **アンチエイリアス描画** した ARGB ビットマップを
  `UpdateLayeredWindow` で貼る
- リングの内側は完全透明のため、半透明化した非フォーカスウィンドウの
  背後に枠色が透けない
- 内縁の角丸半径は Win11 標準（約 8px @ 96dpi）に合わせる
- 再描画はサイズ・太さ・色の変化時のみ。移動は `SetWindowPos` だけで済む

Z 挿入先（対象直下）はそのままなので、上位にあるフローティング
ダイアログには枠が乗らない。トレードオフとして太さを gap より大きくすると
隣のタイルに隠れ得る（config ドキュメントに注記済み）。

---

## 4. レイアウトエンジン設計

### Offscreen も射影 Rect を保持する

Viewport 外の Column を退避させるとき、タイルの `Rect` はサイズを変えない。
`project()` は可視・不可視を問わず全タイルの「論理位置」を返し、
`wm.rs` 側で画面外座標へ移動させる。これにより FR-3.1 の
「既存ウィンドウは決してリサイズされない」が退避中も保証される。

### 不可視フレーム補正（§8-2）

`adopt` 時に `GetWindowRect` と `DWMWA_EXTENDED_FRAME_BOUNDS` の差
`(left, top, right, bottom)` を記録する。配置時にこの差分で Rect を
展開してから `SetWindowPos` に渡すことで、gap が見た目どおり均一になる。

差が取れなかった場合は補正 0（実害は gap のわずかなズレのみ）。

### ワークスペースの不変条件（FR-5.4）

`Stack` の不変条件: 末尾に常に空の `Strip` が 1 つあり、それ以外に空はない。
ウィンドウの増減・移動後に `normalize()` を呼ぶことで回復する。

`normalize` は active インデックスを「消えた active が存在した位置」へ
追従させる。アクティブな `Strip` が空で消滅した場合は次の位置（末尾の空）
へ落とす。

### Alt+J/K のワークスペース切替フォールバック（FR-4.1）

Column 内の上下端（または `neighbor` が `None`）に当たった場合、
J/K はワークスペース切替にフォールバックする。
左右端は停止のまま（ラップしない）。

---

## 5. 開発環境・ビルド

- **クロスビルド**: WSL2 + Nix devShell（`flake.nix`）で
  `cargo build --target x86_64-pc-windows-gnu` を実行
- **実行確認**: WSL2 binfmt interop により `.exe` を WSL から直接起動できる
  （既存の WM が稼働中の Windows 上で動作確認済み）
- **DPI 仮想化の罠**: Per-Monitor v2 マニフェスト（`build.rs`）を入れないと
  `GetWindowRect` 等の座標が DPI 仮想化で狂う。Phase 0 で先行対応済み
- **cargo test の分離**: `emakiwm-core` は Win32 非依存のため、
  Linux ネイティブの `cargo test` でレイアウトロジックの単体テストを実行できる
