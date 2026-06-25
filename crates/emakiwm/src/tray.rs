//! タスクトレイアイコン (P3)。
//!
//! - setup(hwnd): hook thread のウィンドウ生成後に呼ぶ。アイコンを登録する
//! - remove():    restore_all() など終了経路から呼ぶ。NIM_DELETE でゴーストを防ぐ
//! - handle_message(): settings_wnd_proc から WM_TRAY / TaskbarCreated を転送する

use std::sync::OnceLock;
use std::sync::atomic::{AtomicIsize, Ordering};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, POINT, WPARAM};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DestroyMenu, GetCursorPos, IDI_APPLICATION, LoadIconW,
    MF_SEPARATOR, MF_STRING, PostMessageW, RegisterWindowMessageW, SetForegroundWindow,
    TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu, WM_APP, WM_NULL, WM_RBUTTONUP,
};

use crate::events::WmEvent;

/// Shell_NotifyIconW のコールバックメッセージ番号。
/// border.rs が WM_APP+1 を使っているため +2 を使用。
pub const WM_TRAY: u32 = WM_APP + 2;

static TRAY_HWND: AtomicIsize = AtomicIsize::new(0);
static TASKBAR_CREATED_MSG: OnceLock<u32> = OnceLock::new();

const ICON_ID: u32 = 1;
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
    let mut data = base_nid(HWND(h as _));
    unsafe { let _ = Shell_NotifyIconW(NIM_DELETE, &mut data); };
}

/// settings_wnd_proc からトレイ関連メッセージを受け取るか判定する。
pub fn is_tray_message(msg: u32) -> bool {
    msg == WM_TRAY || TASKBAR_CREATED_MSG.get().copied().map_or(false, |id| id == msg)
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
    let hicon = unsafe { LoadIconW(None, IDI_APPLICATION).unwrap_or_default() };
    let mut data = base_nid(hwnd);
    data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    data.uCallbackMessage = WM_TRAY;
    data.hIcon = hicon;
    let tip: Vec<u16> = "emakiwm".encode_utf16().chain(std::iter::once(0u16)).collect();
    for (i, &c) in tip.iter().enumerate().take(128) {
        data.szTip[i] = c;
    }
    unsafe { let _ = Shell_NotifyIconW(NIM_ADD, &mut data); };
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
    let mut data = NOTIFYICONDATAW::default();
    data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    data.hWnd = hwnd;
    data.uID = ICON_ID;
    data
}
