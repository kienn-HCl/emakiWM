//! emakiwm 操作用 CLI (FR-7.3)。
//! 引数をそのまま名前付きパイプ `\\.\pipe\emakiwm` へ送り、JSON 応答を表示する。

const USAGE: &str = "\
usage: emakiwmc <command>

commands:
  state                       状態を JSON で取得
  subscribe                   状態変化を 1 行 1 JSON で購読し続ける (Ctrl+C で終了)
  focus left|right|down|up    フォーカス移動
  move-column left|right      Column の並べ替え
  move-window down|up         ウィンドウをワークスペースへ移動
  workspace down|up           ワークスペース切替
  scroll left|right           Viewport のスクロール
  expel | consume             Tile の押し出し / 取り込み
  cycle-width | maximize | fullscreen
  close                       フォーカスウィンドウを閉じる
  toggle-opacity              フォーカスウィンドウの opacity ピンをトグル
  spawn <command...>          アプリ起動
  reload                      設定ファイル再読込
  quit                        WM 終了 (全ウィンドウ復元)";

#[cfg(windows)]
fn main() {
    use std::io::{Read, Write};

    let cmd = std::env::args().skip(1).collect::<Vec<_>>().join(" ");
    if cmd.is_empty() || cmd == "--help" || cmd == "-h" {
        eprintln!("{USAGE}");
        std::process::exit(2);
    }

    // サーバが次のパイプインスタンスを用意するまでの隙間 (pipe busy) は
    // 短いリトライで吸収する
    let mut attempts = 0;
    let mut pipe = loop {
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(r"\\.\pipe\emakiwm")
        {
            Ok(p) => break p,
            Err(e) => {
                attempts += 1;
                if attempts >= 10 {
                    eprintln!("emakiwm に接続できません (起動していますか?): {e}");
                    std::process::exit(1);
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
    };

    if let Err(e) = pipe.write_all(cmd.as_bytes()) {
        eprintln!("送信失敗: {e}");
        std::process::exit(1);
    }

    // FR-7.4: subscribe は切断されるまで受信した JSON 行を流し続ける
    if cmd == "subscribe" {
        let mut buf = [0u8; 8192];
        loop {
            match pipe.read(&mut buf) {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    print!("{}", String::from_utf8_lossy(&buf[..n]));
                    let _ = std::io::stdout().flush();
                }
            }
        }
    }

    let mut resp = String::new();
    let _ = pipe.read_to_string(&mut resp);
    println!("{resp}");
    if resp.contains("\"error\"") {
        std::process::exit(1);
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("emakiwmc は Windows 専用です");
    std::process::exit(1);
}
