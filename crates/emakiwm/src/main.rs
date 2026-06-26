//! emakiwm — niri 風スクロール型タイリング WM for Windows。
//! Phase 1: 静的タイリング (開閉追従・終了時復元)。スクロールは Phase 2。
// GUI サブシステム: 通常起動でコンソールウィンドウを出さない。
// CLI フラグ (--verbose / --dry-run 等) が渡された場合は attach_console() で
// 親ターミナルに接続してログを流す。
#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
mod autostart;
#[cfg(windows)]
mod border;
#[cfg(windows)]
mod com;
#[cfg(windows)]
mod config;
#[cfg(windows)]
mod events;
#[cfg(windows)]
mod ipc;
#[cfg(windows)]
mod scan;
#[cfg(windows)]
mod tray;
#[cfg(windows)]
mod wm;
#[cfg(windows)]
mod ws;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let verbose = args.iter().any(|a| a == "--verbose");
    let dry_run = args.iter().any(|a| a == "--dry-run");
    let restore = args.iter().any(|a| a == "--restore");

    // CLI フラグが渡された場合は親プロセスのコンソールにアタッチしてログを流す
    #[cfg(windows)]
    if !args.is_empty() {
        attach_console();
    }

    tracing_subscriber::fmt()
        .with_max_level(if verbose {
            tracing::Level::TRACE
        } else {
            tracing::Level::INFO
        })
        .init();

    #[cfg(windows)]
    {
        if dry_run {
            scan::dry_run(verbose, &config::load().rules);
            return;
        }
        if restore {
            // FR-1.6: 強制終了後のレスキュー。state.json の位置へ戻す
            wm::restore_from_disk();
            return;
        }
        // レスキュー: cloak されたまま取り残されたウィンドウの一括解除
        if args.iter().any(|a| a == "--uncloak-all") {
            wm::uncloak_all_leftovers();
            return;
        }
        // スタートアップ自動起動の登録・解除 (--autostart on|off)
        if let Some(i) = args.iter().position(|a| a == "--autostart") {
            match args.get(i + 1).map(String::as_str) {
                Some("on") => autostart::set_autostart(true),
                Some("off") => autostart::set_autostart(false),
                _ => {
                    eprintln!("usage: emakiwm --autostart on|off");
                    std::process::exit(2);
                }
            }
            return;
        }
        // デバッグ用: 単発で cloak/uncloak を試す (--cloak <hex-hwnd> on|off)
        if let Some(i) = args.iter().position(|a| a == "--cloak") {
            let hwnd = args
                .get(i + 1)
                .and_then(|s| isize::from_str_radix(s.trim_start_matches("0x"), 16).ok());
            let on = args.get(i + 2).map(String::as_str) != Some("off");
            match hwnd {
                Some(h) => {
                    wm::shell_cloak(h as u64, on);
                    println!("cloak({on}) + toolwindow({on}) applied to {h:#x}");
                }
                None => eprintln!("usage: emakiwm --cloak <hex-hwnd> [on|off]"),
            }
            return;
        }
        wm::run();
    }

    #[cfg(not(windows))]
    {
        let _ = (dry_run, verbose, restore);
        eprintln!(
            "emakiwm は Windows 専用です。\
             cargo build --target x86_64-pc-windows-gnu でクロスビルドしてください"
        );
        std::process::exit(1);
    }
}

/// 親プロセスのコンソールにアタッチする。
/// GUI サブシステムでも CLI フラグ使用時にログを流せるようにするため。
/// 親コンソールがない場合（エクスプローラから起動等）は何もしない。
#[cfg(windows)]
fn attach_console() {
    use windows::Win32::System::Console::AttachConsole;
    // ATTACH_PARENT_PROCESS = 0xFFFFFFFF
    unsafe {
        let _ = AttachConsole(u32::MAX);
    };
}
