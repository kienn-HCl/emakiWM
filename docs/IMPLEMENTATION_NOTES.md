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

### 論理状態 (State) と Win32 層の分離

モニター・ワークスペース・フォーカス・fullscreen と、イベントに対する状態遷移は
`emakiwm_core::state::State` が持つ（REQUIREMENTS §5 の reducer）。
`wm.rs` は OS への問い合わせ結果（例: フォーカス窓が既に消滅しているか）を
引数で渡し、戻り値の「前面化すべきウィンドウ」を実行するだけ。時刻も単調 ms を
引数で渡すので、モニター跨ぎのフォーカス追従の巻き戻し（800ms）や閉じ直後の
防衛（500ms）といった経験則を Linux の `cargo test` で検証できる。

Win32 側に残る状態は、画面上の現在位置（`visual`）・不可視フレーム・cloak /
半透明 / 枠の実状態・復元 Rect のみ。

### 列幅は viewport 比で追従する

解像度・gap・margin の変更で viewport 幅が変わると、全 Column の幅を比率を保って
引き直す（`Strip::rescale`）。maximize 中の Column は新しい全幅へ追従する。
モニター間移動でも移動元での viewport 比を保つ。

### 手動リサイズと最小サイズ制約（FR-1.2 / FR-3.6）

- `EVENT_SYSTEM_MOVESIZEEND`: ユーザーがタイルをドラッグ・リサイズし終えたら、
  幅の変化は Column 幅として採用し、位置はタイルの定位置へアニメーションで戻す。
- 配置完了時に可視ウィンドウの実フレーム幅を読み、要求幅より広い（最小サイズ制約）
  場合は Column 幅を実幅へ広げて隣との重なりを解消する。広げる方向のみ・
  1 回のレイアウト変化につき回数上限つきで判定するので、読み戻しがループすることはない。
- Firefox は要求幅を一旦受け入れ、リサイズが止まってから 40〜120ms 後に自分で
  最小幅へ広がり直す（実測）。配置完了の瞬間だけ見ると取りこぼすため、完了後も
  40 / 80 / 180 / 400ms 間隔で再確認する。判明した最小幅はウィンドウごとに記録し、
  次回以降は配置前に Column を広げておく（重なりが一瞬も見えない）。記録は
  ウィンドウの消滅時と、ユーザーが手動リサイズしたときに破棄する。
  `LOCATIONCHANGE` は購読しない（自己発生イベントの抑制が不要になる）。
- DPI の異なるモニターへ移すと、アプリが WM_DPICHANGED に応じて自分でリサイズし、
  こちらの配置を上書きする（150% → 100% で 2/3、逆方向で 1.5 倍になるのを実測）。
  不可視フレームの太さも DPI で変わる。そこで上の再確認で実フレームが目標と
  違っていたら、不可視フレームを測り直して配置し直す。「要求より広い」場合も
  まず 1 度配置し直し、それでも広いときだけ最小サイズ制約として扱う
  （DPI による拡大を最小幅と誤認しないため）。

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


### フローティング窓の装飾と後始末（FR-4.9）

- `State::highlighted()` は「フォーカスされた見た目にすべき窓」を返す。前面がフロート窓
  (`set_float_foreground` で Foreground イベントごとに通知) ならその窓、そうでなければ
  タイルの `focused`。DWM 枠色・半透明 (FR-3.8)・太枠 (FR-3.7) はすべてこれを基準にする。
- フロート窓の太枠は、自由に動くため毎ループ `frame_rect` で実フレームを引いて重ねる。
  最前面 (TOPMOST) の窓の直後へ挿入したオーバーレイは同じ最前面帯に入るので、
  Z オーダーで「対象の直下」の方針がそのまま使える。ドラッグ中は追従が遅れるため、
  `EVENT_SYSTEM_MOVESIZESTART` (`WmEvent::MoveSizeStart`) でそのフロート窓の枠を隠し、
  `MOVESIZEEND` で戻す。タイルの MoveSizeStart は何もしない。
- フロート窓は `RESTORE_RECTS` から外れるため、終了時の後始末用に `FLOATS: (hwnd, WM が
  TOPMOST を付けたか)` を持つ。`restore_all()` は枠色を戻し、WM が付けた TOPMOST だけ外す。
  `--restore` 用の state ファイルへも `{"hwnd", "floating": true, "topmost"}` で永続化する
  (旧形式の `{"hwnd", "topmost": true}` も読める)。
- フロート窓が前面のとき Alt+Shift+H/J/K/L (MoveColumn / MoveToWorkspace) は `nudge_rect` で作業領域の 1/20 ずつ窓を動かし、タイルのレイアウトは触らない。Alt+H / Alt+L (Focus 左右) は `pick_in_direction` (中心点比較で 主軸距離 + 2 * 直交軸距離 が最小の窓) でフロート窓間を移動し、該当がなければ何もしない。 Alt+J / Alt+K (Focus 上下) はワークスペースを切り替え、前面化要求は捨ててフォーカスをフロート窓に残す。
  移動後にどのモニターの作業領域とも交差しなくなる場合は適用しない。
- 受動フロート窓: ダイアログ・非リサイズ窓・`action = "float"` のルールの窓など、WM が浮かせた
  わけではない窓は `State::passive_floats` に記録し、枠色・太枠・半透明・`highlighted()`・
  キー移動を `toggle-floating` の窓と同様に適用する (`state.floats()` / `is_float()`)。
  対象は「判定が Float・可視・自プロセス外・WS_CAPTION あり・TOOLWINDOW なし・非最小化」
  (`passive_float_candidate`)。登録は起動時スキャン・Appeared/MinimizeEnd・非管理窓の
  Foreground で行い、位置も Z オーダーも触らない。Gone/最小化/cloak で登録を外し装飾を戻す
  (`toggle-floating` の窓は実際に破棄されたときだけ)。Reload で再評価する。
  タイルの下へ沈まないよう登録時に位置を変えず TOPMOST を付け (元から TOPMOST なら付けない)、
  `FLOATS` の WM-set フラグで記録する。登録解除・終了・`--restore` では WM が付けたときだけ外す
  (破棄済み HWND には触らない)。開いた直後の窓は FOREGROUND が可視化前に届くため、
  登録時に `GetForegroundWindow()` と照合してフォーカス扱い (`note_float_focus` / `set_float_foreground`) にする。
  `focus-floating` の候補は 最後に前面だった窓 → toggle-floating 窓 → 受動窓 (いずれも新しい順)。
- 最前面の維持: アプリが自分で TOPMOST を落とすことがあるため、Foreground のたびに
  `FLOATS` のうち WM が最前面にした窓 (受動フロート窓を含む) の `WS_EX_TOPMOST` を確認し、外れていれば付け直す
  (info ログ「最前面指定が外れていたため付け直す」)。

### Overview (FR-4.11)

- **なぜ DWM サムネイルか**: 他プロセスのウィンドウは外からは縮小描画できない。実ウィンドウを
  小さくリサイズする案は、アプリごとの最小サイズ制約で縮まらない・レイアウトが崩れる (FR-3.6) ため採れない。
  そこで `DwmRegisterThumbnail` で「ライブな縮小画像」を自前の最前面ウィンドウ (`emakiwm_overview`) へ貼る。
  実ウィンドウは通常のレイアウトのまま背後に置いておく (Overview 中も `State` の更新と配置は止めない)。
  サムネイルは DWM がウィンドウ自身の描画の上に合成するので、背景・ワークスペース行・フォーカスの輪は
  WM_PAINT で描き、輪はサムネイルの外側へはみ出させる。ソース矩形は `GetWindowRect` と
  `DWMWA_EXTENDED_FRAME_BOUNDS` の差 (§8-2 の不可視枠) を除いて切り出す。
- **スレッド構成**: ウィンドウはマウス・キー入力を受けるため、メッセージポンプを持つフックスレッドが所有する
  (起動時に作成)。入力は `OverviewClick` / `OverviewClose` を送るだけで状態には触れない。表示・サムネイル更新・
  非表示は Core スレッドが毎周回に行う (同一プロセス内の他スレッド所有ウィンドウへの SetWindowPos / DWM 呼び出しは可能)。
  SetWindowPos は所有スレッドへ同期メッセージを送るので、WM_PAINT と共有する描画データの Mutex は
  ウィンドウ呼び出しの前に手放す (デッドロック回避)。
- **シーン計算は純粋ロジック** (`emakiwm_core::overview`): 各ワークスペースの `Strip::project` の論理 Rect
  (Visible / Offscreen を区別せず、退避座標にもしない) を縦に `(i - active) * (作業領域高 + 行間)` ずらし、
  作業領域の中心で `zoom` 倍する。`zoom == 1.0` のときの active ワークスペースは通常配置の論理位置と厳密に一致するため、
  開閉アニメーション (実効 zoom を 1.0 ⇔ `overview_zoom` で ease-out 補間) の端点が実ウィンドウと継ぎ目なくつながる。
  他モニターへ絵がはみ出さないよう、タイル・行は属するモニターの領域でクリップする (サムネイルは dest と source を
  同じ割合で切る)。
- **サムネイルの更新**: 前回送ったシーンと同じなら DWM を呼ばない。登録は「未登録の窓が増えた / 手前奥の順が変わった」
  ときだけ全部登録し直す (後に登録したものが手前に描かれるので、フロート窓をタイルの後ろに登録し直す)。
  画面外へ出ただけの窓は登録を残して `fVisible = FALSE` にする。登録失敗は debug ログのみで、失敗も記録して再試行しない。
- **Overview 中はフォーカスを動かさない**: `focus_window` を呼ぶと隠れたアプリへキーボードフォーカスが移り、
  Overview が Esc / Enter を受けられなくなる。そのため State は通常どおり更新するが、前面化要求は捨て、
  閉じ終わりに `highlighted()` へ一度だけ適用する (クリック選択も `State` を更新して閉じるだけ)。
  この間は WM が付けたフロート窓の TOPMOST の付け直しと、太枠オーバーレイも止める (Overview より手前に出るため)。
- **ループ**: 開閉アニメーション中だけ `FRAME` 間隔で回り、開き切ったら再びイベント待ちでブロックする。


### 無変換を修飾キーにするバインド (FR-6.1 / FR-6.2)

- **なぜフックか**: `RegisterHotKey` の修飾は Alt/Ctrl/Shift/Win のみ。無変換 (VK_NONCONVERT) を含むバインドは
  独自ビット `MOD_MUHENKAN` を立て、`WH_KEYBOARD_LL` で処理する。それ以外のバインドは従来どおり `RegisterHotKey`。
  `apply_binds` は `hotkeys` の全体の添字 + 1 を id にして登録するので、無変換バインドを飛ばしても
  `WM_HOTKEY` の id → `hotkeys[id-1]` の対応は崩れない (解除も同じ id)。
- **食って再送する理由**: 無変換の down/up はフックで食う (でないと、コンボ中に IME へ無変換が届いて
  変換モードが切り替わる)。その代わり、他キーを使わずに離した単打だけ、離した時点で `SendInput` により
  無変換の down+up を再送する。フック内ではブロックせず、フックスレッド自身へ thread message
  (`WM_APP + 2`) を post して、メッセージループ側で `SendInput` する (LowLevelHooksTimeout 対策)。
- **再送の目印**: 再送イベントは `dwExtraInfo = MUHENKAN_REPLAY_TAG` を付け、フックはそれだけを素通しする。
  `LLKHF_INJECTED` 全般を除外しないのは、他のキー割当ツールが注入したキー (例: 変換キーの置き換え) にも
  バインドを効かせたいため。
- **状態機械は core の `modkey::ModKeyTracker`**: 保持中 (held)・使用済み (used)・発火済みで押下中のキー (down) を持つ。
  発火済みキーのオートリピートは食う (`MOD_NOREPEAT` 相当)。key-up は無変換を先に離した後でも食い、
  アプリへ孤児の key-up を渡さない。未割当の無変換+キーは素通しで、離したときの IME 再送も抑止する。
  修飾キー (Shift/Ctrl/Alt/Win) は「使用」に数えない。同時押しの修飾は `GetAsyncKeyState` で取る
  (これらはフックに食われないのでシステム側の状態が正しい)。
- **5 秒のアイドル失効**: 保持中に `HOLD_IDLE_MS` (5000ms) イベントがなければ、無変換が離されたものとみなして解除する。無変換単独の押しっぱなしはオートリピートで延長されるが、コンボを押すとリピートはそのキーへ移るため、コンボキーを離して 5 秒止まると解除される。
  管理者権限ウィンドウが前面・ロック画面・UAC などで key-up を取り逃すと、保持が残って素の h/j/k/l を
  食い続けるため。`GetAsyncKeyState(VK_NONCONVERT)` で実状態を確かめる案は使えない: フックが無変換を食うので、
  システムのキー状態には無変換が一度も押されたことにならない。リピートや保持中のキー操作は時刻を更新する。
- **フックの登録**: キーボードフックは「無変換バインドがある」または `mouse_scroll_focus` のときだけ、
  マウスフックは `mouse_scroll_focus` のときだけ登録する (2 フラグで独立管理)。一度登録したフックは残し、
  機能が無効になったら素通しする (無変換バインドが空なら即 `CallNextHookEx`)。reload のたびにトラッカーは初期化する。
- **キーのマージは展開後の組み合わせで**: `keybind::resolve_binds` は `$name` を展開してからパースし、
  `(mods, vk)` をキーにデフォルトとユーザー設定をマージする。文字列のままだと `"shift+alt+h"` が
  `"$mod+shift+h"` を上書きできない。ユーザー設定が勝ち、ユーザー設定内の重複はキー昇順で後勝ち (警告つき)。
  コマンドが空 / `"none"` のものはマージ後に除く (デフォルトの無効化)。
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

---

## 6. アプリアイコン

- **置き場所**: `assets/icon/emakiwm.ico` (16/20/24/32/40/48/64/256 のマルチサイズ)。
  元データは `assets/icon/emakiwm.svg` (後で追加予定) から ImageMagick 等で生成する。
  差し替えは .ico を置き換えるだけでよく、コード変更は不要。
- **埋め込み**: `emakiwm` / `emakiwm-setup` / `emakiwmc` の `build.rs` が `embed-resource` で
  `1 ICON "<.ico の絶対パス>"` だけの .rc (OUT_DIR に生成) をコンパイルする。
  マニフェストは従来どおり `embed-manifest` が埋め込むので、exe には RT_MANIFEST (id 1) が 1 つ、
  アイコングループ (id 1) が入る (GNU: windres / MSVC: rc.exe)。`emakiwmc` はマニフェストなし。
  .rc は相対パス解決に依存しないよう絶対パス (`/` 区切り) で参照する。
- **トレイ**: `tray.rs` の `app_icon()` が `LoadImageW(GetModuleHandleW, リソース ID 1, IMAGE_ICON,
  SM_CXSMICON x SM_CYSMICON)` で読み込む。TaskbarCreated での再登録時にリークしないよう
  一度だけ読み込んで保持する。失敗時は `IDI_APPLICATION` にフォールバック。
- **インストーラ**: アンインストールレジストリの `DisplayIcon` に `"<dir>\emakiwm.exe",0` を書く。
  スタートメニューのショートカットは emakiwm.exe を指すので exe のアイコンを継承する。
- **Nix**: flake は git 管理下のファイルだけを見るため、新規ファイル (.ico, build.rs) は
  `git add` (または `-N`) してから `nix build` すること。
