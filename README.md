# emakiWM

niri 風のスクロール型タイリング WM for Windows。

<video src="assets/demo.mp4" controls width="100%"></video>

Windows 上で [niri](https://github.com/YaLTeR/niri) の「スクロール型タイリング」レイアウトを再現する。DWM を置換するのではなく、既存ウィンドウを外部から操作するオーバーレイ型 WM として動作する。

## 特徴

- **スクロール型タイリング** — ウィンドウは「列 (Column)」単位で右方向に並び、新しいウィンドウを開いても既存ウィンドウはリサイズされない
- **縦ワークスペース** — モニタごとに縦方向へ複数のワークスペースを持つ (niri の動的モデル)
- **スムーズなアニメーション** — レイアウト変化を Rect 補間で滑らかに反映 (180ms / ease-out)
- **設定ファイル** — TOML で gap・アニメーション・ウィンドウルール・キーバインドを設定
- **タスクトレイ** — 右クリックメニューから設定再読込・終了が可能
- **起動時アプリ自動起動** — 設定ファイルで WM 起動時に任意のアプリを起動できる
- **IPC** — 名前付きパイプ経由で `emakiwmc` CLI から全操作が可能 (別途ダウンロード)
- **Zebar 連携** — WebSocket でステータスバーへ状態をリアルタイム配信

## 動作環境

- Windows 10 21H2 以降 / Windows 11
- 管理者権限不要 (管理者権限のウィンドウは管理対象外)

## インストール

### インストーラを使う（推奨）

1. [Releases](../../releases) から `emakiwm-vX.X.X-windows.zip` をダウンロード
2. zip を展開して `emakiwm-setup.exe` を実行
3. インストール先・自動起動の設定を選んで「インストール」

インストーラが以下を自動で行います:
- `%LOCALAPPDATA%\Programs\emakiwm` へバイナリを配置
- PATH への追加（新しいターミナルで `emakiwm` コマンドが使えるようになる）
- スタートメニューへのショートカット追加（Windows 検索対応）
- ログイン時の自動起動登録（オプション）

**Windows Defender の警告について:**  
署名なしの新しいソフトウェアのため、初回起動時に SmartScreen の警告が出る場合があります。「詳細情報」→「実行」で続行できます。

### アンインストール

`emakiwm-setup.exe` を再度実行し「アンインストール」を選ぶか:

```powershell
emakiwm-setup --uninstall
```

設定 → アプリ → インストール済みアプリからも削除できます。

### 手動インストール

インストーラを使わず `emakiwm.exe` を直接任意のフォルダに配置して実行することもできます。

### スタートアップ登録のみ変更したい場合

```powershell
emakiwm --autostart on   # ログイン時の自動起動を登録
emakiwm --autostart off  # 解除
```

## ビルド

```powershell
cargo build --release
```

WSL2 環境からクロスビルドする場合:

```bash
# flake.nix の devShell を使用
nix develop
cargo build --release --target x86_64-pc-windows-gnu
```

## 設定

`%USERPROFILE%\.config\emakiwm\config.toml` に設定ファイルを配置する。
ファイルがない場合はデフォルト設定で起動する。設定変更は `emakiwmc reload` で即時反映（キーバインド変更のみ再起動が必要）。

サンプル設定: [`examples/config.example.toml`](examples/config.example.toml)

```toml
gap = 8             # ウィンドウ間・画面端の隙間 (px)
anim_ms = 180       # アニメーション時間 (ms、0 で無効)
hide = "offscreen"  # Viewport 外の隠し方: "offscreen" | "cloak"

# WM 起動時に自動で起動するアプリ
startup = ["zebar"]

# Alt+ホイールでカラム間フォーカス移動 (デフォルト無効)
# mouse_scroll_focus = true
```

### バー (Zebar 等) との連携

AppBar 登録しないバーを使う場合は `margin` でバーの高さぶんを空ける:

```toml
margin = [40, 0, 0, 0]  # [上, 右, 下, 左] px
ws_port = 6573           # WebSocket ポート (省略で無効)
```

Zebar 用サンプルウィジェット: [`examples/zebar/emakiwm-widget.html`](examples/zebar/emakiwm-widget.html)

## デフォルトキーバインド

| キー | 動作 |
|------|------|
| `Alt+H` / `Alt+L` | 左 / 右の列へフォーカス |
| `Alt+J` / `Alt+K` | 列内で下 / 上のウィンドウへフォーカス (端でワークスペース切替) |
| `Alt+Shift+H` / `Alt+Shift+L` | 列を左 / 右へ移動 |
| `Alt+Shift+J` / `Alt+Shift+K` | ウィンドウを下 / 上のワークスペースへ移動 |
| `Alt+U` / `Alt+I` | ワークスペースを下 / 上へ切替 |
| `Alt+R` | 列幅プリセットをサイクル (1/3 → 1/2 → 2/3) |
| `Alt+F` | 列を Viewport 全幅に最大化 (トグル) |
| `Alt+Shift+F` | フルスクリーン (トグル) |
| `Alt+Comma` / `Alt+Period` | Viewport を左 / 右へスクロール |
| `Alt+Shift+Comma` / `Alt+Shift+Period` | ウィンドウを隣の列へ取り込み / 押し出し |
| `Alt+Shift+Q` | フォーカスウィンドウを閉じる |
| `Alt+Shift+E` | WM 終了 (全ウィンドウ復元) |

キーバインドは設定ファイルで変更できる。

## CLI (emakiwmc)

`emakiwmc.exe` は IPC 操作用の CLI ツールです。[Releases](../../releases) から単体でダウンロードできます。

```bash
emakiwmc state              # 現在の状態を JSON で取得
emakiwmc subscribe          # 状態変化を購読 (Ctrl+C で終了)
emakiwmc focus left         # フォーカス移動
emakiwmc spawn wt           # アプリ起動
emakiwmc reload             # 設定ファイルを再読込
emakiwmc quit               # WM 終了
```

## レスキューコマンド

強制終了後にウィンドウが画面外に残った場合:

```powershell
emakiwm --restore       # state.json の位置へウィンドウを戻す
emakiwm --uncloak-all   # cloak が残ったウィンドウを一括解除
emakiwm --dry-run       # 管理対象の判定結果を表示 (デバッグ用)
```

## 既知の制約

- 管理者権限のウィンドウがフォアグラウンドの間、ホットキーは発火しない (UIPI)
- `Alt+Shift` 系キーバインドは Windows のキーボードレイアウト切替と衝突することがある  
  → 設定 > 時刻と言語 > 入力 > キーボードの詳細設定 > 入力言語のホットキー から無効化できる
- マルチモニタ / DPI 混在環境は未検証
- `mouse_scroll_focus` によるタッチパッドスクロールは WM_POINTER 対応アプリ (Firefox 等) では動作しない

## ライセンス

MIT — 詳細は [LICENSE](LICENSE) を参照。
