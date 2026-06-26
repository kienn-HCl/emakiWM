# emakiWM

Windows 向けのスクロール型タイリングウィンドウマネージャー。

![demo](assets/demo.gif)

## 特徴

- **スクロール型タイリング** — ウィンドウは列単位で右に並ぶ。新しいウィンドウを開いても既存のウィンドウはリサイズされない
- **ワークスペース** — モニタごとに複数のワークスペースを縦に持てる
- **気軽に導入・撤退** — 終了すればウィンドウはすべて元の位置に戻る。管理者権限不要
- **設定ファイルで自由にカスタマイズ** — ギャップ・アニメーション・ウィンドウルール・キーバインドを TOML で設定
- **タスクトレイ常駐** — 右クリックから設定再読み込み・終了が可能

## 動作環境

- Windows 10 21H2 以降 / Windows 11

## インストール

[Releases](../../releases) から `emakiwm-vX.X.X-windows.zip` をダウンロードして展開し、`emakiwm-setup.exe` を実行。

インストーラが以下を自動で行います：

- `%LOCALAPPDATA%\Programs\emakiwm` へバイナリを配置
- PATH への追加
- スタートメニューへのショートカット追加
- ログイン時の自動起動登録（オプション）

**Windows Defender の警告について：** 署名なしのソフトウェアのため SmartScreen の警告が出ることがあります。「詳細情報」→「実行」で続行できます。

### アンインストール

`emakiwm-setup.exe` を再度実行して「アンインストール」を選ぶか、設定 → アプリ からも削除できます。

## 使い方

インストール後、スタートメニューから `emakiwm` を起動する。既存のウィンドウが自動的にタイル配置される。

タスクトレイのアイコンを右クリックすると設定の再読み込みや終了が行える。

### デフォルトキーバインド

| キー | 動作 |
|------|------|
| `Alt+H` / `Alt+L` | 左 / 右の列へフォーカス |
| `Alt+J` / `Alt+K` | 列内で上 / 下のウィンドウへフォーカス |
| `Alt+Shift+H` / `Alt+Shift+L` | 列を左 / 右へ移動 |
| `Alt+Shift+J` / `Alt+Shift+K` | ウィンドウを上 / 下のワークスペースへ移動 |
| `Alt+U` / `Alt+I` | ワークスペースを切替 |
| `Alt+R` | 列幅をサイクル (1/3 → 1/2 → 2/3) |
| `Alt+F` | 列を全幅に最大化（トグル） |
| `Alt+Shift+F` | フルスクリーン（トグル） |
| `Alt+Comma` / `Alt+Period` | Viewport を左 / 右へスクロール |
| `Alt+Shift+Comma` / `Alt+Shift+Period` | ウィンドウを隣の列へ取り込み / 押し出し |
| `Alt+Shift+Q` | フォーカスウィンドウを閉じる |
| `Alt+Shift+E` | WM 終了 |

キーバインドは設定ファイルで変更できます。

### 設定

`%USERPROFILE%\.config\emakiwm\config.toml` に配置。ファイルがなければデフォルトで起動します。

設定を変えたら `emakiwmc reload` で即時反映（キーバインドの変更のみ再起動が必要）。

設定例: [`examples/config.example.toml`](examples/config.example.toml)

```toml
gap = 8             # ウィンドウ間の隙間 (px)
anim_ms = 180       # アニメーション時間 (ms、0 で無効)

# バー (Zebar 等) を使う場合: バーのぶんだけ余白を確保
margin = [40, 0, 0, 0]

# WM 起動時に一緒に起動するアプリ
startup = ["zebar"]
```

## CLI (emakiwmc)

IPC 操作用の CLI ツールです。[Releases](../../releases) から単体でダウンロードできます。

```bash
emakiwmc state      # 状態を JSON で取得
emakiwmc reload     # 設定を再読み込み
emakiwmc quit       # WM 終了
emakiwmc spawn wt   # アプリを起動
```

## 既知の制約

- 管理者権限で動作するウィンドウはタイル管理できない (UIPI)
- `Alt+Shift` 系のキーバインドは Windows のキーボードレイアウト切替と衝突することがある  
  → 設定 > 時刻と言語 > 入力 > キーボードの詳細設定 > 入力言語のホットキー から無効化できる
- `mouse_scroll_focus` によるタッチパッドスクロールは WM_POINTER 対応アプリ（Firefox 等）では動作しない

## ライセンス

MIT — [LICENSE](LICENSE)

---

## 開発者向け

### ビルド

```powershell
cargo build --release
```

WSL2 からクロスビルドする場合:

```bash
nix develop
cargo build --release --target x86_64-pc-windows-gnu
```

### スタートアップ登録

```powershell
emakiwm --autostart on   # ログイン時の自動起動を登録
emakiwm --autostart off  # 解除
```

### レスキューコマンド

強制終了後にウィンドウが画面外に残った場合:

```powershell
emakiwm --restore       # state.json の位置へウィンドウを戻す
emakiwm --uncloak-all   # cloak が残ったウィンドウを一括解除
emakiwm --dry-run       # 管理対象の判定を確認 (デバッグ用)
```
