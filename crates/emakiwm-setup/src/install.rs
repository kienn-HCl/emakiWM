//! インストール・アンインストール処理。

use std::path::{Path, PathBuf};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{ERROR_SUCCESS, LPARAM, WPARAM};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, IPersistFile, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyW, RegDeleteKeyW, RegDeleteValueW, RegOpenKeyExW,
    RegQueryValueExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
    KEY_QUERY_VALUE, KEY_SET_VALUE, REG_DWORD, REG_SZ,
};
use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
use windows::Win32::UI::WindowsAndMessaging::{
    SendMessageTimeoutW, HWND_BROADCAST, SMTO_ABORTIFHUNG, WM_SETTINGCHANGE,
};

const UNINSTALL_SUBKEY: PCWSTR =
    w!("Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\emakiwm");
const ENV_SUBKEY: PCWSTR = w!("Environment");
const PATH_VALUE: PCWSTR = w!("Path");

/// 必須バイナリ（存在しない場合はインストールエラー）。
const REQUIRED: &[&str] = &["emakiwm.exe", "emakiwm-setup.exe"];
/// オプショナルバイナリ（存在する場合のみコピー）。
/// emakiwmc は CLI IPC ツールで上級者向けのため別途ダウンロード形式。
const OPTIONAL: &[&str] = &["emakiwmc.exe"];

pub struct InstallOptions {
    pub install_dir: PathBuf,
    pub autostart: bool,
    pub launch_now: bool,
}

pub struct UninstallOptions {
    pub delete_config: bool,
}

/// インストールを実行する。
pub fn install(opts: &InstallOptions) -> std::io::Result<()> {
    let dir = &opts.install_dir;
    let src_dir = std::env::current_exe()?
        .parent()
        .ok_or_else(|| std::io::Error::other("実行ファイルの親ディレクトリが取得できません"))?
        .to_path_buf();

    // 必須バイナリの存在確認
    for name in REQUIRED {
        let src = src_dir.join(name);
        if !src.exists() {
            return Err(std::io::Error::other(format!(
                "{name} が見つかりません: {}",
                src.display()
            )));
        }
    }

    // インストール先を作成
    std::fs::create_dir_all(dir)?;

    // 必須バイナリをコピー
    for name in REQUIRED {
        std::fs::copy(src_dir.join(name), dir.join(name))?;
    }
    // オプショナルバイナリは存在する場合のみコピー
    for name in OPTIONAL {
        let src = src_dir.join(name);
        if src.exists() {
            let _ = std::fs::copy(src, dir.join(name));
        }
    }

    // PATH に追加
    add_to_path(dir);

    // Uninstall レジストリ登録
    register_uninstall(dir);

    // スタートメニューショートカット作成（Windows Search で引っかかるようになる）
    create_start_menu_shortcut(dir);

    // スタートアップ登録
    if opts.autostart {
        let exe = dir.join("emakiwm.exe");
        set_autostart_for_exe(&exe, true);
    }

    Ok(())
}

/// アンインストールを実行する。
pub fn uninstall(opts: &UninstallOptions) -> std::io::Result<()> {
    // Uninstall キーが消えていても default_install_dir にフォールバックしてクリーンアップする
    let install_dir = read_install_dir().unwrap_or_else(default_install_dir);

    // スタートアップ解除
    set_autostart_for_exe(&install_dir.join("emakiwm.exe"), false);

    // PATH から削除
    remove_from_path(&install_dir);

    // Uninstall レジストリ削除
    unregister_uninstall();

    // スタートメニューショートカット削除
    remove_start_menu_shortcut();

    // ユーザー設定削除（任意）
    if opts.delete_config {
        if let Ok(profile) = std::env::var("USERPROFILE") {
            let config_dir = PathBuf::from(profile).join(".config").join("emakiwm");
            if config_dir.exists() {
                let _ = std::fs::remove_dir_all(&config_dir);
            }
        }
    }

    // バイナリ削除（setup.exe 自身以外）
    for name in REQUIRED.iter().chain(OPTIONAL.iter()) {
        if *name == "emakiwm-setup.exe" {
            continue; // 実行中の自分は削除できないのでスキップ
        }
        let _ = std::fs::remove_file(install_dir.join(name));
    }

    // setup.exe 自身と空になったディレクトリを次回再起動時に削除
    schedule_delete_on_reboot(&install_dir.join("emakiwm-setup.exe"));
    schedule_delete_on_reboot(&install_dir);

    Ok(())
}

/// レジストリに記録されているインストール先を取得する。
pub fn read_install_dir() -> Option<PathBuf> {
    let mut hkey = HKEY::default();
    let err = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            UNINSTALL_SUBKEY,
            None,
            KEY_QUERY_VALUE,
            &mut hkey,
        )
    };
    if err != ERROR_SUCCESS {
        return None;
    }
    let value = read_reg_sz(hkey, w!("InstallLocation"));
    unsafe { let _ = RegCloseKey(hkey); };
    value.map(PathBuf::from)
}

pub fn default_install_dir() -> PathBuf {
    let local_appdata = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".into());
    PathBuf::from(local_appdata).join("Programs").join("emakiwm")
}

// ─── PATH 操作 ────────────────────────────────────────────────────────────────

fn add_to_path(dir: &Path) {
    let dir_str = dir.to_string_lossy().to_string();
    let mut hkey = HKEY::default();
    if unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, ENV_SUBKEY, None, KEY_SET_VALUE | KEY_QUERY_VALUE, &mut hkey) } != ERROR_SUCCESS {
        return;
    }
    let current = read_reg_sz(hkey, PATH_VALUE).unwrap_or_default();
    let already = current.split(';').any(|p| p.trim_end_matches('\\') == dir_str.trim_end_matches('\\'));
    if !already {
        let new_path = if current.is_empty() { dir_str.clone() } else { format!("{current};{dir_str}") };
        write_reg_sz(hkey, PATH_VALUE, &new_path);
    }
    unsafe { let _ = RegCloseKey(hkey); };
    broadcast_env_change();
}

fn remove_from_path(dir: &Path) {
    let dir_str = dir.to_string_lossy().to_string();
    let mut hkey = HKEY::default();
    if unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, ENV_SUBKEY, None, KEY_SET_VALUE | KEY_QUERY_VALUE, &mut hkey) } != ERROR_SUCCESS {
        return;
    }
    if let Some(current) = read_reg_sz(hkey, PATH_VALUE) {
        let new_path: Vec<&str> = current
            .split(';')
            .filter(|p| p.trim_end_matches('\\') != dir_str.trim_end_matches('\\'))
            .collect();
        write_reg_sz(hkey, PATH_VALUE, &new_path.join(";"));
    }
    unsafe { let _ = RegCloseKey(hkey); };
    broadcast_env_change();
}

/// PATH 変更を既存プロセスに通知する（これを省くと再起動するまで反映されない）。
fn broadcast_env_change() {
    unsafe {
        let _ = SendMessageTimeoutW(
            HWND_BROADCAST,
            WM_SETTINGCHANGE,
            WPARAM(0),
            LPARAM(w!("Environment").0 as _),
            SMTO_ABORTIFHUNG,
            5000,
            None,
        );
    }
}

// ─── Uninstall レジストリ ─────────────────────────────────────────────────────

fn register_uninstall(install_dir: &Path) {
    let mut hkey = HKEY::default();
    let err = unsafe { RegCreateKeyW(HKEY_CURRENT_USER, UNINSTALL_SUBKEY, &mut hkey) };
    if err != ERROR_SUCCESS {
        return;
    }

    let setup_exe = install_dir.join("emakiwm-setup.exe");
    let uninstall_str = format!("\"{}\" --uninstall", setup_exe.display());
    let quiet_str = format!("\"{}\" --uninstall --quiet", setup_exe.display());
    let version = env!("CARGO_PKG_VERSION");
    let install_dir_str = install_dir.to_string_lossy().to_string();

    write_reg_sz(hkey, w!("DisplayName"), "emakiwm");
    write_reg_sz(hkey, w!("DisplayVersion"), version);
    write_reg_sz(hkey, w!("Publisher"), "frort");
    write_reg_sz(hkey, w!("InstallLocation"), &install_dir_str);
    write_reg_sz(hkey, w!("UninstallString"), &uninstall_str);
    write_reg_sz(hkey, w!("QuietUninstallString"), &quiet_str);
    write_reg_sz(hkey, w!("URLInfoAbout"), "https://github.com/frort/emakiwm");
    write_reg_dword(hkey, w!("NoModify"), 1);
    write_reg_dword(hkey, w!("NoRepair"), 1);

    // インストールサイズ (KB)
    let size_kb: u32 = REQUIRED.iter().chain(OPTIONAL.iter()).map(|name| {
        std::fs::metadata(install_dir.join(name))
            .map(|m| (m.len() / 1024) as u32)
            .unwrap_or(0)
    }).sum();
    write_reg_dword(hkey, w!("EstimatedSize"), size_kb);

    unsafe { let _ = RegCloseKey(hkey); };
}

fn unregister_uninstall() {
    unsafe { let _ = RegDeleteKeyW(HKEY_CURRENT_USER, UNINSTALL_SUBKEY); };
}

// ─── Autostart ───────────────────────────────────────────────────────────────

// emakiwm::autostart::set_autostart_for_exe と同等の処理。
// emakiwm-setup は emakiwm クレートに依存できないため複製している。
fn set_autostart_for_exe(exe: &Path, enable: bool) {
    use windows::Win32::Foundation::ERROR_FILE_NOT_FOUND;
    const RUN_SUBKEY: PCWSTR =
        w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
    const VALUE_NAME: PCWSTR = w!("emakiwm");

    let mut hkey = HKEY::default();
    if unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, RUN_SUBKEY, None, KEY_SET_VALUE, &mut hkey) } != ERROR_SUCCESS {
        return;
    }
    if enable {
        let value = format!("\"{}\"", exe.display());
        write_reg_sz(hkey, VALUE_NAME, &value);
    } else {
        let err = unsafe { RegDeleteValueW(hkey, VALUE_NAME) };
        // 値が存在しない場合は正常 (ERROR_FILE_NOT_FOUND を無視)
        let _ = err != ERROR_SUCCESS && err != ERROR_FILE_NOT_FOUND;
    }
    unsafe { let _ = RegCloseKey(hkey); };
}

// ─── 再起動時削除スケジュール ────────────────────────────────────────────────

/// MoveFileExW(MOVEFILE_DELAY_UNTIL_REBOOT) でパスを次回再起動時に削除登録する。
/// バッチファイル方式より安全で、セキュリティソフトに誤検出されにくい。
fn schedule_delete_on_reboot(path: &Path) {
    use windows::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_DELAY_UNTIL_REBOOT};
    let path_w: Vec<u16> = path.to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        let _ = MoveFileExW(
            PCWSTR(path_w.as_ptr()),
            PCWSTR(std::ptr::null()),
            MOVEFILE_DELAY_UNTIL_REBOOT,
        );
    }
}

// ─── レジストリ汎用ヘルパー ───────────────────────────────────────────────────

fn write_reg_sz(hkey: HKEY, name: PCWSTR, value: &str) {
    let utf16: Vec<u16> = value.encode_utf16().chain(std::iter::once(0u16)).collect();
    let bytes: Vec<u8> = utf16.iter().flat_map(|c| c.to_le_bytes()).collect();
    unsafe { let _ = RegSetValueExW(hkey, name, None, REG_SZ, Some(&bytes)); };
}

fn write_reg_dword(hkey: HKEY, name: PCWSTR, value: u32) {
    let bytes = value.to_le_bytes();
    unsafe { let _ = RegSetValueExW(hkey, name, None, REG_DWORD, Some(&bytes)); };
}

fn read_reg_sz(hkey: HKEY, name: PCWSTR) -> Option<String> {
    let mut size = 0u32;
    let err = unsafe {
        RegQueryValueExW(hkey, name, None, None, None, Some(&mut size))
    };
    if err != ERROR_SUCCESS || size == 0 {
        return None;
    }
    let mut buf = vec![0u8; size as usize];
    let err = unsafe {
        RegQueryValueExW(hkey, name, None, None, Some(buf.as_mut_ptr()), Some(&mut size))
    };
    if err != ERROR_SUCCESS {
        return None;
    }
    // buf は UTF-16LE バイト列（NUL 終端含む）
    let words: Vec<u16> = buf.chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    Some(String::from_utf16_lossy(&words).trim_end_matches('\0').to_string())
}

// ─── スタートメニューショートカット ──────────────────────────────────────────

fn shortcut_path() -> PathBuf {
    let appdata = std::env::var("APPDATA").unwrap_or_default();
    PathBuf::from(appdata)
        .join("Microsoft")
        .join("Windows")
        .join("Start Menu")
        .join("Programs")
        .join("emakiwm.lnk")
}

fn create_start_menu_shortcut(install_dir: &Path) {
    let com_init = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    unsafe { try_create_shortcut(install_dir) };
    if com_init.is_ok() { unsafe { CoUninitialize() }; }
}

unsafe fn try_create_shortcut(install_dir: &Path) {
    let link: IShellLinkW = match CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER) {
        Ok(l) => l,
        Err(_) => return,
    };

    let target = install_dir.join("emakiwm.exe");
    let target_w: Vec<u16> = target.to_string_lossy().encode_utf16().chain(std::iter::once(0)).collect();
    let _ = link.SetPath(PCWSTR(target_w.as_ptr()));
    let _ = link.SetDescription(w!("emakiwm - niri風スクロールタイリングWM"));
    let workdir_w: Vec<u16> = install_dir.to_string_lossy().encode_utf16().chain(std::iter::once(0)).collect();
    let _ = link.SetWorkingDirectory(PCWSTR(workdir_w.as_ptr()));

    let persist: IPersistFile = match windows_core::Interface::cast(&link) {
        Ok(p) => p,
        Err(_) => return,
    };

    let lnk = shortcut_path();
    let lnk_w: Vec<u16> = lnk.to_string_lossy().encode_utf16().chain(std::iter::once(0)).collect();
    let _ = persist.Save(PCWSTR(lnk_w.as_ptr()), true);
}

fn remove_start_menu_shortcut() {
    let _ = std::fs::remove_file(shortcut_path());
}
