//! TaskDialogIndirect + IFileOpenDialog による GUI。

use std::path::PathBuf;

use windows::core::{w, BOOL, PCWSTR};
use windows::Win32::System::Com::IBindCtx;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::Controls::{
    TaskDialogIndirect, TASKDIALOGCONFIG, TASKDIALOG_BUTTON,
    TDF_ALLOW_DIALOG_CANCELLATION, TDF_USE_COMMAND_LINKS,
    TDCBF_CANCEL_BUTTON,
};
use windows::Win32::UI::Shell::{
    FileOpenDialog, IFileOpenDialog, IShellItem, SHCreateItemFromParsingName,
    FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS,
};

use crate::install::{default_install_dir, read_install_dir, InstallOptions, UninstallOptions};

/// メインメニューの選択結果。
pub enum MainMenuResult {
    Install,
    Uninstall,
    Cancel,
}

const BTN_INSTALL: i32 = 101;
const BTN_UNINSTALL: i32 = 102;
const BTN_CHANGE_DIR: i32 = 103;
const BTN_LAUNCH: i32 = 104;
const BTN_NO_LAUNCH: i32 = 105;

/// メインメニューダイアログを表示する。
pub fn show_main_menu() -> MainMenuResult {
    let buttons = [
        TASKDIALOG_BUTTON {
            nButtonID: BTN_INSTALL,
            pszButtonText: w!("インストール"),
        },
        TASKDIALOG_BUTTON {
            nButtonID: BTN_UNINSTALL,
            pszButtonText: w!("アンインストール"),
        },
    ];

    let mut result_btn = 0i32;
    let cfg = TASKDIALOGCONFIG {
        cbSize: std::mem::size_of::<TASKDIALOGCONFIG>() as u32,
        pszWindowTitle: w!("emakiwm セットアップ"),
        pszMainInstruction: w!("操作を選択してください"),
        cButtons: buttons.len() as u32,
        pButtons: buttons.as_ptr(),
        nDefaultButton: BTN_INSTALL,
        dwCommonButtons: TDCBF_CANCEL_BUTTON,
        dwFlags: TDF_ALLOW_DIALOG_CANCELLATION,
        ..Default::default()
    };

    unsafe { TaskDialogIndirect(&cfg, Some(&mut result_btn), None, None) }.ok();

    match result_btn {
        BTN_INSTALL => MainMenuResult::Install,
        BTN_UNINSTALL => MainMenuResult::Uninstall,
        _ => MainMenuResult::Cancel,
    }
}

/// インストールオプションダイアログを表示する。
/// フォルダ変更ボタンを押すたびに IFileOpenDialog を開いてループする。
pub fn show_install_dialog() -> Option<InstallOptions> {
    // S_OK/S_FALSE = refcount 増加 → CoUninitialize 必須。
    // RPC_E_CHANGED_MODE 等のエラーは refcount 増加なし → CoUninitialize 不要。
    let com_init = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };

    let mut install_dir = default_install_dir();

    // フォルダ確認 + 変更ループ
    loop {
        let dir_str = install_dir.to_string_lossy().to_string();
        let content = format!("インストール先:\n{dir_str}");
        let content_w: Vec<u16> = content.encode_utf16().chain(std::iter::once(0)).collect();

        let buttons = [
            TASKDIALOG_BUTTON {
                nButtonID: BTN_CHANGE_DIR,
                pszButtonText: w!("フォルダを変更..."),
            },
        ];

        let mut result_btn = 0i32;
        let cfg = TASKDIALOGCONFIG {
            cbSize: std::mem::size_of::<TASKDIALOGCONFIG>() as u32,
            pszWindowTitle: w!("emakiwm セットアップ — インストール先"),
            pszMainInstruction: w!("インストール先を確認してください"),
            pszContent: PCWSTR(content_w.as_ptr()),
            cButtons: buttons.len() as u32,
            pButtons: buttons.as_ptr(),
            nDefaultButton: 1, // IDOK
            dwCommonButtons: windows::Win32::UI::Controls::TDCBF_OK_BUTTON
                | TDCBF_CANCEL_BUTTON,
            dwFlags: TDF_ALLOW_DIALOG_CANCELLATION,
            ..Default::default()
        };

        unsafe { TaskDialogIndirect(&cfg, Some(&mut result_btn), None, None) }.ok();

        match result_btn {
            BTN_CHANGE_DIR => {
                if let Some(new_dir) = pick_folder(&install_dir) {
                    install_dir = new_dir;
                }
                // ループで再表示
            }
            1 => break, // IDOK
            _ => {
                if com_init.is_ok() { unsafe { CoUninitialize() }; }
                return None;
            }
        }
    }

    // オプション選択（autostart チェックボックス + launch コマンドリンク）
    let mut autostart_checked = BOOL::from(true);
    let buttons = [
        TASKDIALOG_BUTTON {
            nButtonID: BTN_LAUNCH,
            pszButtonText: w!("インストールしてすぐに起動する"),
        },
        TASKDIALOG_BUTTON {
            nButtonID: BTN_NO_LAUNCH,
            pszButtonText: w!("インストールのみ（後で手動起動）"),
        },
    ];

    let mut result_btn = 0i32;
    let cfg = TASKDIALOGCONFIG {
        cbSize: std::mem::size_of::<TASKDIALOGCONFIG>() as u32,
        pszWindowTitle: w!("emakiwm セットアップ — オプション"),
        pszMainInstruction: w!("インストールオプションを選択してください"),
        pszVerificationText: w!("ログイン時に自動起動する"),
        cButtons: buttons.len() as u32,
        pButtons: buttons.as_ptr(),
        nDefaultButton: BTN_LAUNCH,
        dwCommonButtons: TDCBF_CANCEL_BUTTON,
        dwFlags: TDF_ALLOW_DIALOG_CANCELLATION | TDF_USE_COMMAND_LINKS,
        ..Default::default()
    };

    unsafe {
        TaskDialogIndirect(
            &cfg,
            Some(&mut result_btn),
            None,
            Some(&mut autostart_checked),
        )
    }
    .ok();

    if com_init.is_ok() { unsafe { CoUninitialize() }; }

    match result_btn {
        BTN_LAUNCH | BTN_NO_LAUNCH => Some(InstallOptions {
            install_dir,
            autostart: autostart_checked.as_bool(),
            launch_now: result_btn == BTN_LAUNCH,
        }),
        _ => None,
    }
}

/// アンインストール確認ダイアログを表示する。
pub fn show_uninstall_dialog() -> Option<UninstallOptions> {
    let install_dir = read_install_dir().unwrap_or_else(default_install_dir);
    let dir_str = install_dir.to_string_lossy().to_string();
    let content = format!("以下の場所の emakiwm をアンインストールします:\n{dir_str}");
    let content_w: Vec<u16> = content.encode_utf16().chain(std::iter::once(0)).collect();

    let mut del_config = BOOL::from(false);
    let mut result_btn = 0i32;

    let cfg = TASKDIALOGCONFIG {
        cbSize: std::mem::size_of::<TASKDIALOGCONFIG>() as u32,
        pszWindowTitle: w!("emakiwm アンインストール"),
        pszMainInstruction: w!("emakiwm をアンインストールしますか？"),
        pszContent: PCWSTR(content_w.as_ptr()),
        pszVerificationText: w!("設定ファイルも削除する（%USERPROFILE%\\.config\\emakiwm）"),
        nDefaultButton: 1, // IDOK
        dwCommonButtons: windows::Win32::UI::Controls::TDCBF_OK_BUTTON
            | TDCBF_CANCEL_BUTTON,
        dwFlags: TDF_ALLOW_DIALOG_CANCELLATION,
        ..Default::default()
    };

    unsafe {
        TaskDialogIndirect(&cfg, Some(&mut result_btn), None, Some(&mut del_config))
    }
    .ok();

    if result_btn == 1 {
        // IDOK
        Some(UninstallOptions {
            delete_config: del_config.as_bool(),
        })
    } else {
        None
    }
}

/// IFileOpenDialog でフォルダを選択する。
fn pick_folder(default: &std::path::Path) -> Option<PathBuf> {
    unsafe {
        let dialog: IFileOpenDialog =
            CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;

        // フォルダ選択モード
        let opts = dialog.GetOptions().ok()?;
        dialog
            .SetOptions(opts | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM)
            .ok()?;

        // デフォルトフォルダを設定
        let default_w: Vec<u16> = default
            .to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        if let Ok(item) =
            SHCreateItemFromParsingName::<_, _, IShellItem>(PCWSTR(default_w.as_ptr()), None::<&IBindCtx>)
        {
            let _ = dialog.SetFolder(&item);
        }

        if dialog.Show(None).is_err() {
            return None; // キャンセル
        }

        let result: IShellItem = dialog.GetResult().ok()?;
        let display =
            result.GetDisplayName(windows::Win32::UI::Shell::SIGDN_FILESYSPATH).ok()?;
        let path_str = display.to_string().ok()?;
        Some(PathBuf::from(path_str))
    }
}

/// インストール完了ダイアログ。
pub fn show_install_complete(install_dir: &std::path::Path) {
    let dir_str = install_dir.to_string_lossy().to_string();
    let content = format!(
        "emakiwm を以下にインストールしました:\n{dir_str}\n\n新しいターミナルで emakiwm コマンドが使えます。"
    );
    let content_w: Vec<u16> = content.encode_utf16().chain(std::iter::once(0)).collect();

    let cfg = TASKDIALOGCONFIG {
        cbSize: std::mem::size_of::<TASKDIALOGCONFIG>() as u32,
        pszWindowTitle: w!("emakiwm セットアップ"),
        pszMainInstruction: w!("インストール完了"),
        pszContent: PCWSTR(content_w.as_ptr()),
        nDefaultButton: 1,
        dwCommonButtons: windows::Win32::UI::Controls::TDCBF_OK_BUTTON,
        dwFlags: TDF_ALLOW_DIALOG_CANCELLATION,
        ..Default::default()
    };

    unsafe { TaskDialogIndirect(&cfg, None, None, None) }.ok();
}

/// アンインストール完了ダイアログ。
pub fn show_uninstall_complete() {
    let cfg = TASKDIALOGCONFIG {
        cbSize: std::mem::size_of::<TASKDIALOGCONFIG>() as u32,
        pszWindowTitle: w!("emakiwm アンインストール"),
        pszMainInstruction: w!("アンインストール完了"),
        pszContent: w!("emakiwm を削除しました。"),
        nDefaultButton: 1,
        dwCommonButtons: windows::Win32::UI::Controls::TDCBF_OK_BUTTON,
        dwFlags: TDF_ALLOW_DIALOG_CANCELLATION,
        ..Default::default()
    };
    unsafe { TaskDialogIndirect(&cfg, None, None, None) }.ok();
}

/// エラーダイアログ。
pub fn show_error(msg: &str) {
    let content: Vec<u16> = msg.encode_utf16().chain(std::iter::once(0)).collect();
    let cfg = TASKDIALOGCONFIG {
        cbSize: std::mem::size_of::<TASKDIALOGCONFIG>() as u32,
        pszWindowTitle: w!("emakiwm セットアップ — エラー"),
        pszMainInstruction: w!("エラーが発生しました"),
        pszContent: PCWSTR(content.as_ptr()),
        nDefaultButton: 1,
        dwCommonButtons: windows::Win32::UI::Controls::TDCBF_OK_BUTTON,
        dwFlags: TDF_ALLOW_DIALOG_CANCELLATION,
        ..Default::default()
    };
    unsafe { TaskDialogIndirect(&cfg, None, None, None) }.ok();
}
