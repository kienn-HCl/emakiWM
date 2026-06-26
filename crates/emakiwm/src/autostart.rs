//! HKCU Run キーによるスタートアップ自動起動の登録・解除 (P2)。
//! `emakiwm --autostart on|off` から呼ばれる。WM 本体の起動は不要。

use windows::core::w;
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows::Win32::System::Registry::{
    RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
    KEY_SET_VALUE, REG_SZ,
};

const RUN_SUBKEY: windows::core::PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const VALUE_NAME: windows::core::PCWSTR = w!("emakiwm");

pub fn set_autostart(enable: bool) {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("autostart: 実行ファイルパスの取得に失敗: {e}");
            return;
        }
    };
    set_autostart_for_exe(&exe, enable);
}

/// インストーラから呼ぶ用: インストール先のパスを明示的に指定する。
pub fn set_autostart_for_exe(exe: &std::path::Path, enable: bool) {
    let mut hkey = HKEY::default();
    let err = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            RUN_SUBKEY,
            None,
            KEY_SET_VALUE,
            &mut hkey,
        )
    };
    if err != ERROR_SUCCESS {
        eprintln!("autostart: レジストリキーを開けませんでした (err={err:?})");
        return;
    }

    if enable {
        let value = format!("\"{}\"", exe.display());
        let utf16: Vec<u16> = value.encode_utf16().chain(std::iter::once(0u16)).collect();
        let bytes: Vec<u8> = utf16.iter().flat_map(|c| c.to_le_bytes()).collect();
        let err = unsafe { RegSetValueExW(hkey, VALUE_NAME, None, REG_SZ, Some(&bytes)) };
        if err != ERROR_SUCCESS {
            eprintln!("autostart: 書き込みに失敗しました (err={err:?})");
        } else {
            println!("autostart: 登録しました\n  {value}");
        }
    } else {
        let err = unsafe { RegDeleteValueW(hkey, VALUE_NAME) };
        if err != ERROR_SUCCESS && err != ERROR_FILE_NOT_FOUND {
            eprintln!("autostart: 削除に失敗しました (err={err:?})");
        } else {
            println!("autostart: 登録を解除しました");
        }
    }

    unsafe {
        let _ = RegCloseKey(hkey);
    };
}
