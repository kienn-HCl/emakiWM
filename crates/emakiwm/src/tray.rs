//! タスクトレイアイコン (P3)。
//!
//! - setup(hwnd): hook thread のウィンドウ生成後に呼ぶ。アイコンを登録する
//! - remove():    restore_all() など終了経路から呼ぶ。NIM_DELETE でゴーストを防ぐ
//! - handle_message(): settings_wnd_proc から WM_TRAY / TaskbarCreated を転送する

use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::OnceLock;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DestroyMenu, GetCursorPos, GetSystemMetrics, LoadIconW,
    LoadImageW, PostMessageW, RegisterWindowMessageW, SetForegroundWindow, TrackPopupMenu, HICON,
    IDI_APPLICATION, IMAGE_ICON, LR_DEFAULTCOLOR, MF_SEPARATOR, MF_STRING, SM_CXSMICON,
    SM_CYSMICON, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_APP, WM_NULL, WM_RBUTTONUP,
};

use crate::events::WmEvent;

/// Shell_NotifyIconW のコールバックメッセージ番号。
/// border.rs が WM_APP+1 を使っているため +2 を使用。
pub const WM_TRAY: u32 = WM_APP + 2;

static TRAY_HWND: AtomicIsize = AtomicIsize::new(0);
static TASKBAR_CREATED_MSG: OnceLock<u32> = OnceLock::new();

const ICON_ID: u32 = 1;

/// build.rs が埋め込むアイコン (RT_GROUP_ICON) のリソース ID。
const APP_ICON_RESOURCE_ID: usize = 1;

/// 読み込み済みアイコン。TaskbarCreated での再登録時に使い回し、ハンドルをリークさせない。
static APP_ICON: AtomicIsize = AtomicIsize::new(0);
const MENU_RELOAD: usize = 1;
const MENU_QUIT: usize = 2;

/// hook thread がウィンドウを生成した直後に呼ぶ。
pub fn setup(hwnd: HWND) {
    TRAY_HWND.store(hwnd.0 as isize, Ordering::Relaxed);
    let msg_id = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };
    if msg_id != 0 {
        let _ = TASKBAR_CREATED_MSG.set(msg_id);
    }
    register_icon(hwnd);
}

/// トレイアイコンを削除する。冪等 (2回呼んでも無害)。
pub fn remove() {
    let h = TRAY_HWND.swap(0, Ordering::Relaxed);
    if h == 0 {
        return;
    }
    let data = base_nid(HWND(h as _));
    unsafe {
        let _ = Shell_NotifyIconW(NIM_DELETE, &data);
    };
}

/// settings_wnd_proc からトレイ関連メッセージを受け取るか判定する。
pub fn is_tray_message(msg: u32) -> bool {
    msg == WM_TRAY
        || TASKBAR_CREATED_MSG
            .get()
            .copied()
            .is_some_and(|id| id == msg)
}

/// settings_wnd_proc からトレイ関連メッセージを処理する。
pub fn handle_message(hwnd: HWND, msg: u32, lp: LPARAM) {
    if msg == WM_TRAY {
        // lParam 下位 WORD が実際のイベント種別
        let notif = (lp.0 & 0xffff) as u32;
        if notif == WM_RBUTTONUP {
            unsafe { show_menu(hwnd) };
        }
    } else {
        // TaskbarCreated: Explorer 再起動後にアイコンを再登録
        register_icon(hwnd);
    }
}

fn register_icon(hwnd: HWND) {
    let hicon = app_icon();
    let mut data = base_nid(hwnd);
    data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    data.uCallbackMessage = WM_TRAY;
    data.hIcon = hicon;
    let tip: Vec<u16> = "emakiwm"
        .encode_utf16()
        .chain(std::iter::once(0u16))
        .collect();
    for (i, &c) in tip.iter().enumerate().take(128) {
        data.szTip[i] = c;
    }
    unsafe {
        let _ = Shell_NotifyIconW(NIM_ADD, &data);
    };
}

/// 通知領域用 (小アイコンサイズ) のアイコンを返す。初回のみ読み込む。
/// 埋め込みリソースの読み込みに失敗したら標準アイコンへフォールバックする。
fn app_icon() -> HICON {
    let cached = APP_ICON.load(Ordering::Relaxed);
    if cached != 0 {
        return HICON(cached as _);
    }
    let hicon = unsafe {
        let hinst = GetModuleHandleW(None).unwrap_or_default();
        // MAKEINTRESOURCE 相当: ID を PCWSTR にそのまま詰める
        LoadImageW(
            Some(hinst.into()),
            PCWSTR(APP_ICON_RESOURCE_ID as *const u16),
            IMAGE_ICON,
            GetSystemMetrics(SM_CXSMICON),
            GetSystemMetrics(SM_CYSMICON),
            LR_DEFAULTCOLOR,
        )
        .map(|h| HICON(h.0))
        .or_else(|_| LoadIconW(None, IDI_APPLICATION))
        .unwrap_or_default()
    };
    APP_ICON.store(hicon.0 as isize, Ordering::Relaxed);
    hicon
}

unsafe fn show_menu(hwnd: HWND) {
    let hmenu = match CreatePopupMenu() {
        Ok(m) => m,
        Err(_) => return,
    };
    let _ = AppendMenuW(hmenu, MF_STRING, MENU_RELOAD, w!("Reload config"));
    // セパレータ: テキストは無視される
    let _ = AppendMenuW(hmenu, MF_SEPARATOR, 0, PCWSTR(std::ptr::null()));
    let _ = AppendMenuW(hmenu, MF_STRING, MENU_QUIT, w!("Quit"));

    // TrackPopupMenu の前面化バグ回避: SetForegroundWindow が必須
    let _ = SetForegroundWindow(hwnd);

    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);

    let cmd = TrackPopupMenu(
        hmenu,
        TPM_RIGHTBUTTON | TPM_RETURNCMD,
        pt.x,
        pt.y,
        Some(0),
        hwnd,
        None,
    );
    // TrackPopupMenu 後の古典的ワークアラウンド
    let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
    let _ = DestroyMenu(hmenu);

    if let Some(tx) = crate::events::sender() {
        match cmd.0 as usize {
            MENU_RELOAD => {
                let _ = tx.send(WmEvent::Reload);
            }
            MENU_QUIT => {
                let _ = tx.send(WmEvent::Shutdown);
            }
            _ => {}
        }
    }
}

fn base_nid(hwnd: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: ICON_ID,
        ..Default::default()
    }
}
