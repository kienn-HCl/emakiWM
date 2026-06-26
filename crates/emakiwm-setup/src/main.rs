//! emakiwm インストーラ。
//!
//! 引数なし: GUI ウィザードを表示
//! --install [--dir <path>] [--no-autostart] [--no-launch]: サイレントインストール
//! --uninstall [--keep-config] [--quiet]: サイレントアンインストール
#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
mod dialog;
#[cfg(windows)]
mod install;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    #[cfg(windows)]
    run(&args);

    #[cfg(not(windows))]
    {
        let _ = args;
        eprintln!("emakiwm-setup は Windows 専用です");
        std::process::exit(1);
    }
}

#[cfg(windows)]
fn attach_console() {
    use windows::Win32::System::Console::AttachConsole;
    unsafe {
        let _ = AttachConsole(u32::MAX);
    };
}

#[cfg(windows)]
fn run(args: &[String]) {
    use install::{default_install_dir, install, uninstall, InstallOptions, UninstallOptions};
    use std::path::PathBuf;

    // CLI モードは親コンソールにアタッチして出力を流す
    let is_cli = args.iter().any(|a| a == "--install" || a == "--uninstall");
    if is_cli {
        attach_console();
    }

    if args.iter().any(|a| a == "--install") {
        // CLI インストール
        let dir = args
            .iter()
            .position(|a| a == "--dir")
            .and_then(|i| args.get(i + 1))
            .map(PathBuf::from)
            .unwrap_or_else(default_install_dir);
        let autostart = !args.iter().any(|a| a == "--no-autostart");
        let launch_now = !args.iter().any(|a| a == "--no-launch");

        match install(&InstallOptions {
            install_dir: dir.clone(),
            autostart,
            launch_now,
        }) {
            Ok(()) => {
                println!("インストール完了: {}", dir.display());
                if launch_now {
                    launch_emakiwm(&dir);
                }
            }
            Err(e) => {
                eprintln!("インストール失敗: {e}");
                std::process::exit(1);
            }
        }
        return;
    }

    if args.iter().any(|a| a == "--uninstall") {
        // CLI アンインストール
        let keep_config = args.iter().any(|a| a == "--keep-config");
        let quiet = args.iter().any(|a| a == "--quiet");
        let opts = UninstallOptions {
            delete_config: !keep_config,
        };

        match uninstall(&opts) {
            Ok(()) => {
                if !quiet {
                    println!("アンインストール完了");
                }
            }
            Err(e) => {
                eprintln!("アンインストール失敗: {e}");
                std::process::exit(1);
            }
        }
        return;
    }

    // GUI モード
    use dialog::*;

    match show_main_menu() {
        MainMenuResult::Cancel => {}
        MainMenuResult::Install => {
            if let Some(opts) = show_install_dialog() {
                let launch = opts.launch_now;
                let dir = opts.install_dir.clone();
                match install(&opts) {
                    Ok(()) => {
                        show_install_complete(&dir);
                        if launch {
                            launch_emakiwm(&dir);
                        }
                    }
                    Err(e) => show_error(&format!("インストールに失敗しました:\n{e}")),
                }
            }
        }
        MainMenuResult::Uninstall => {
            if let Some(opts) = show_uninstall_dialog() {
                match uninstall(&opts) {
                    Ok(()) => show_uninstall_complete(),
                    Err(e) => show_error(&format!("アンインストールに失敗しました:\n{e}")),
                }
            }
        }
    }
}

#[cfg(windows)]
fn launch_emakiwm(install_dir: &std::path::Path) {
    let exe = install_dir.join("emakiwm.exe");
    let _ = std::process::Command::new(exe).spawn();
}
