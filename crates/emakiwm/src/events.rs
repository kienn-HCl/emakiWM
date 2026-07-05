//! SetWinEventHook によるイベント購読 (FR-1.2)。
//!
//! WINEVENT_OUTOFCONTEXT のコールバックはメッセージポンプを持つスレッドで
//! 受ける必要があるため、専用スレッドでフック登録 + GetMessageW ループを回し、
//! 生イベントを mpsc チャネルで Core スレッドへ送る (§5 アーキテクチャ)。

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::Sender;
use std::sync::OnceLock;

use emakiwm_core::layout::FocusDir;
use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Accessibility::{SetWinEventHook, HWINEVENTHOOK};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, HOT_KEY_MODIFIERS, VK_LMENU, VK_MENU, VK_RMENU,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, RegisterClassW,
    SetWindowsHookExW, EVENT_OBJECT_CLOAKED, EVENT_OBJECT_DESTROY, EVENT_OBJECT_HIDE,
    EVENT_OBJECT_SHOW, EVENT_OBJECT_UNCLOAKED, EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_MINIMIZEEND,
    EVENT_SYSTEM_MINIMIZESTART, KBDLLHOOKSTRUCT, MSG, MSLLHOOKSTRUCT, OBJID_WINDOW, WH_KEYBOARD_LL,
    WH_MOUSE_LL, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS, WM_DISPLAYCHANGE, WM_HOTKEY,
    WM_KEYDOWN, WM_MOUSEWHEEL, WM_SETTINGCHANGE, WM_SYSKEYDOWN, WNDCLASSW, WS_POPUP,
};

/// Core スレッドへ送るイベント。hwnd は isize (HWND は Send でないため)。
#[derive(Debug, Clone)]
pub enum WmEvent {
    /// ウィンドウが出現した可能性 (SHOW / UNCLOAKED)
    Appeared(isize),
    /// ウィンドウが消えた (DESTROY / HIDE)
    Gone(isize),
    /// ウィンドウが cloak された (CLOAKED)。FR-3.3 cloak モードで
    /// 自分が隠したものは無視し、それ以外は Gone と同様に扱う
    Cloaked(isize),
    Foreground(isize),
    MinimizeStart(isize),
    MinimizeEnd(isize),
    /// Alt+H/L/J/K (FR-4.1)
    Focus(FocusDir),
    /// Alt+Shift+H/L: Column の並べ替え (FR-4.4)
    MoveColumn(FocusDir),
    /// Alt+Shift+Period: Tile を右隣へ押し出し (FR-4.5)
    Expel,
    /// Alt+Shift+Comma: 右隣の Tile を取り込み縦スタック (FR-4.5)
    Consume,
    /// Alt+Shift+J/K: ウィンドウを下/上のワークスペースへ移動 (FR-5.4)。true = 下
    MoveToWorkspace(bool),
    /// Alt+U/I: ワークスペース切替 (FR-5.4)。true = 下
    SwitchWorkspace(bool),
    /// Alt+R: Column 幅プリセットのサイクル (FR-3.4)
    CycleWidth,
    /// Alt+F: maximize-column トグル (FR-4.6)
    MaximizeColumn,
    /// Alt+Shift+F: fullscreen トグル (FR-4.7)
    Fullscreen,
    /// Alt+Comma / Period: 明示的スクロール (FR-4.3)。true = 右へ
    Scroll(bool),
    /// Alt+Shift+Q: フォーカスウィンドウを閉じる
    CloseFocused,
    /// アプリ起動 ("spawn <コマンドライン>")。新規ウィンドウは FR-1.3 で取り込まれる
    Spawn(String),
    /// フォーカスウィンドウの opacity ピンのトグル (FR-3.8)。
    /// ピン中は非フォーカスでも pinned_opacity を維持する
    ToggleOpacity,
    /// 隣のモニタへフォーカスを移動する。true = 右/次
    FocusMonitor(bool),
    /// フォーカスウィンドウを隣のモニタへ移動する。true = 右/次
    MoveToMonitor(bool),
    /// 設定ファイルの再読込 (FR-7.2)
    Reload,
    /// 解像度・タスクバー等の変更 (WM_DISPLAYCHANGE / WM_SETTINGCHANGE)。
    /// Core 側で作業領域を再取得して relayout する (FR-5.5)
    WorkAreaChanged,
    /// IPC: 状態の JSON を返信チャネルへ送る (FR-7.3)
    Query(Sender<String>),
    /// IPC: 状態変化の購読を登録する (FR-7.4)。登録時と変化時に JSON を送る
    Subscribe(Sender<String>),
    /// Ctrl+C / Alt+Shift+E による終了要求
    Shutdown,
}

/// 設定から構築されるホットキー 1 件 (FR-6.3)。
#[derive(Debug, Clone)]
pub struct Hotkey {
    /// MOD_ALT 等のビット和
    pub mods: u32,
    pub vk: u32,
    pub event: WmEvent,
}

/// CLI / IPC / キーバインド値の共通コマンド構文 (FR-7.3)。
pub fn parse_command(s: &str) -> Option<WmEvent> {
    // spawn のみ残り全体を 1 つのコマンドラインとして受け取る
    if let Some(cmdline) = s.strip_prefix("spawn ") {
        let cmdline = cmdline.trim();
        if cmdline.is_empty() {
            return None;
        }
        return Some(WmEvent::Spawn(cmdline.to_string()));
    }
    let mut it = s.split_whitespace();
    let head = it.next()?;
    let arg = it.next().unwrap_or("");
    Some(match (head, arg) {
        ("focus", "left") => WmEvent::Focus(FocusDir::Left),
        ("focus", "right") => WmEvent::Focus(FocusDir::Right),
        ("focus", "down") => WmEvent::Focus(FocusDir::Down),
        ("focus", "up") => WmEvent::Focus(FocusDir::Up),
        ("move-column", "left") => WmEvent::MoveColumn(FocusDir::Left),
        ("move-column", "right") => WmEvent::MoveColumn(FocusDir::Right),
        ("move-window", "down") => WmEvent::MoveToWorkspace(true),
        ("move-window", "up") => WmEvent::MoveToWorkspace(false),
        ("workspace", "down") => WmEvent::SwitchWorkspace(true),
        ("workspace", "up") => WmEvent::SwitchWorkspace(false),
        ("scroll", "left") => WmEvent::Scroll(false),
        ("scroll", "right") => WmEvent::Scroll(true),
        ("expel", "") => WmEvent::Expel,
        ("consume", "") => WmEvent::Consume,
        ("cycle-width", "") => WmEvent::CycleWidth,
        ("maximize", "") => WmEvent::MaximizeColumn,
        ("fullscreen", "") => WmEvent::Fullscreen,
        ("close", "") => WmEvent::CloseFocused,
        ("toggle-opacity", "") => WmEvent::ToggleOpacity,
        ("focus-monitor", "right") => WmEvent::FocusMonitor(true),
        ("focus-monitor", "left") => WmEvent::FocusMonitor(false),
        ("move-to-monitor", "right") => WmEvent::MoveToMonitor(true),
        ("move-to-monitor", "left") => WmEvent::MoveToMonitor(false),
        ("reload", "") => WmEvent::Reload,
        ("quit", "") => WmEvent::Shutdown,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_keeps_full_command_line() {
        match parse_command("spawn wt -p NixOS") {
            Some(WmEvent::Spawn(cmd)) => assert_eq!(cmd, "wt -p NixOS"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn spawn_requires_command() {
        assert!(parse_command("spawn").is_none());
        assert!(parse_command("spawn   ").is_none());
    }
}

/// コールバックは引数を受け取れないため static で渡す。
static SENDER: OnceLock<Sender<WmEvent>> = OnceLock::new();

pub fn sender() -> Option<&'static Sender<WmEvent>> {
    SENDER.get()
}

/// Alt+ホイールフォーカス移動の有効/無効。reload で切り替え可能
static MOUSE_SCROLL_FOCUS: AtomicBool = AtomicBool::new(false);

pub fn set_mouse_scroll_focus(enabled: bool) {
    MOUSE_SCROLL_FOCUS.store(enabled, Ordering::Relaxed);
}

/// Alt 押下の最終 tick (KBDLLHOOKSTRUCT.time, ms 単位)。
/// keydown で更新、keyup で ALT_UP_TICK が更新される。
/// 初期値 0 / 1 にすることで「up が down より新しい」→ 非押下状態とする。
static ALT_DOWN_TICK: AtomicU32 = AtomicU32::new(0);
static ALT_UP_TICK: AtomicU32 = AtomicU32::new(1);

/// フックスレッドを起動する。プロセス終了までフックは張りっぱなしでよい
/// (フックはプロセス終了時に OS が解除する)。
/// キーバインドの変更はプロセス再起動が必要 (フックスレッドで登録するため)。
/// mouse_scroll_focus が false のときは WH_KEYBOARD_LL / WH_MOUSE_LL を登録しない
/// (不要なグローバルフックはセキュリティソフトの誤検出を招くため)。
pub fn spawn_hook_thread(tx: Sender<WmEvent>, hotkeys: Vec<Hotkey>, mouse_scroll_focus: bool) {
    SENDER.set(tx).expect("hook thread spawned twice");

    std::thread::spawn(move || unsafe {
        // 必要レンジのみフックする (全イベント購読は高コスト):
        //   0x0003..0x0017: FOREGROUND / MINIMIZESTART / MINIMIZEEND
        //   0x8001..0x8003: DESTROY / SHOW / HIDE
        //   0x8017..0x8018: CLOAKED / UNCLOAKED
        let flags = WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS;
        let ranges = [
            (EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_MINIMIZEEND),
            (EVENT_OBJECT_DESTROY, EVENT_OBJECT_HIDE),
            (EVENT_OBJECT_CLOAKED, EVENT_OBJECT_UNCLOAKED),
        ];
        for (min, max) in ranges {
            let hook = SetWinEventHook(min, max, None, Some(win_event_proc), 0, 0, flags);
            if hook.is_invalid() {
                tracing::error!("SetWinEventHook({min:#06x}..{max:#06x}) failed");
            }
        }
        tracing::debug!("WinEventHook registered");

        // FR-5.5: WM_DISPLAYCHANGE / WM_SETTINGCHANGE のブロードキャストを受ける
        // 不可視トップレベルウィンドウ (message-only はブロードキャストが来ない)
        let instance = GetModuleHandleW(None).unwrap_or_default();
        let wc = WNDCLASSW {
            lpfnWndProc: Some(settings_wnd_proc),
            hInstance: instance.into(),
            lpszClassName: w!("emakiwm_events"),
            ..Default::default()
        };
        if RegisterClassW(&wc) != 0 {
            if let Ok(hwnd) = CreateWindowExW(
                Default::default(),
                w!("emakiwm_events"),
                w!(""),
                WS_POPUP,
                0,
                0,
                0,
                0,
                None,
                None,
                Some(instance.into()),
                None,
            ) {
                crate::tray::setup(hwnd);
            }
        }

        // FR-6.1: ホットキーはメッセージループを持つこのスレッドで登録する。
        // 失敗 (他アプリと競合等) は警告に留めて続行する
        const MOD_NOREPEAT: u32 = 0x4000;
        for (i, hk) in hotkeys.iter().enumerate() {
            if let Err(e) = RegisterHotKey(
                None,
                (i + 1) as i32,
                HOT_KEY_MODIFIERS(hk.mods | MOD_NOREPEAT),
                hk.vk,
            ) {
                tracing::warn!(
                    "RegisterHotKey(mods={:#x}, vk={:#04x}) failed: {e}",
                    hk.mods,
                    hk.vk
                );
            }
        }

        // mouse_scroll_focus が有効なときのみグローバル入力フックを登録する。
        // WH_KEYBOARD_LL / WH_MOUSE_LL は正当な用途でもセキュリティソフトに
        // キーロガーとして誤検出されやすいため、必要なときだけ登録する。
        if mouse_scroll_focus {
            match SetWindowsHookExW(
                WH_KEYBOARD_LL,
                Some(keyboard_ll_proc),
                Some(instance.into()),
                0,
            ) {
                Ok(h) if !h.is_invalid() => tracing::debug!("WH_KEYBOARD_LL registered"),
                _ => tracing::warn!("WH_KEYBOARD_LL の登録に失敗しました"),
            }
            match SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_ll_proc), Some(instance.into()), 0) {
                Ok(h) if !h.is_invalid() => tracing::debug!("WH_MOUSE_LL registered"),
                _ => tracing::warn!("WH_MOUSE_LL の登録に失敗しました"),
            }
        }

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if msg.message == WM_HOTKEY {
                let pressed = msg.wParam.0;
                if let Some(hk) = pressed.checked_sub(1).and_then(|i| hotkeys.get(i)) {
                    if let Some(tx) = SENDER.get() {
                        let _ = tx.send(hk.event.clone());
                    }
                }
                continue;
            }
            let _ = DispatchMessageW(&msg);
        }
    });
}

/// 解像度・作業領域系のブロードキャストを Core へ転送する (FR-5.5)。
/// WM_SETTINGCHANGE はノイズが多いが、Core 側で実際の作業領域を
/// 比較して変化時のみ反応するのでそのまま流してよい。
unsafe extern "system" fn settings_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
) -> LRESULT {
    match msg {
        WM_DISPLAYCHANGE | WM_SETTINGCHANGE => {
            if let Some(tx) = SENDER.get() {
                let _ = tx.send(WmEvent::WorkAreaChanged);
            }
            LRESULT(0)
        }
        _ if crate::tray::is_tray_message(msg) => {
            crate::tray::handle_message(hwnd, msg, lp);
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

unsafe extern "system" fn win_event_proc(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    id_child: i32,
    _id_thread: u32,
    _time: u32,
) {
    // FR-1.2.1: トップレベルウィンドウ自身のイベントのみ通す
    if id_object != OBJID_WINDOW.0 || id_child != 0 || hwnd.is_invalid() {
        return;
    }
    let h = hwnd.0 as isize;
    let ev = match event {
        EVENT_OBJECT_SHOW | EVENT_OBJECT_UNCLOAKED => WmEvent::Appeared(h),
        EVENT_OBJECT_DESTROY | EVENT_OBJECT_HIDE => WmEvent::Gone(h),
        EVENT_OBJECT_CLOAKED => WmEvent::Cloaked(h),
        EVENT_SYSTEM_FOREGROUND => WmEvent::Foreground(h),
        EVENT_SYSTEM_MINIMIZESTART => WmEvent::MinimizeStart(h),
        EVENT_SYSTEM_MINIMIZEEND => WmEvent::MinimizeEnd(h),
        _ => return,
    };
    // コールバック内は最小限の仕事に留める (受信側で inspect する)
    if let Some(tx) = SENDER.get() {
        let _ = tx.send(ev);
    }
}

/// WH_MOUSE_LL コールバック。Alt+ホイールをフォーカス移動へ変換する。
/// LowLevelHooksTimeout (既定 ~300ms) 以内に返す必要があるため、
/// tx.send() のみ行い重い処理は絶対にしない。
/// WH_KEYBOARD_LL: Alt キーの押下/解放タイムスタンプを記録するだけで
/// イベントは必ず通過させる (consume しない)。
/// キーリピートで SYSKEYDOWN が繰り返し届くため、長押し中は ALT_DOWN_TICK が更新され続ける。
unsafe extern "system" fn keyboard_ll_proc(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code >= 0 {
        let ks = &*(lp.0 as *const KBDLLHOOKSTRUCT);
        let vk = ks.vkCode;
        if vk == VK_MENU.0 as u32 || vk == VK_LMENU.0 as u32 || vk == VK_RMENU.0 as u32 {
            if matches!(wp.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN) {
                ALT_DOWN_TICK.store(ks.time, Ordering::Relaxed);
            } else {
                ALT_UP_TICK.store(ks.time, Ordering::Relaxed);
            }
        }
    }
    CallNextHookEx(None, code, wp, lp)
}

/// WH_MOUSE_LL: タイムスタンプ比較で Alt 押下を判定しフォーカス移動へ変換する。
///
/// 判定ロジック:
/// - last_down > last_up (循環比較) かつ スクロール時刻 - last_down < 1500ms
///
/// これにより:
/// - GetAsyncKeyState に依存しない → タッチパッドドライバの干渉を受けない
/// - keyup をドライバに消費された場合でも 1.5s 後に自動解消 (刺さり防止)
/// - キーリピートで last_down が更新されるため長押し中は制限なく動作する
unsafe extern "system" fn mouse_ll_proc(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code >= 0 && wp.0 as u32 == WM_MOUSEWHEEL && MOUSE_SCROLL_FOCUS.load(Ordering::Relaxed) {
        let ms = &*(lp.0 as *const MSLLHOOKSTRUCT);
        let now = ms.time;
        let down = ALT_DOWN_TICK.load(Ordering::Relaxed);
        let up = ALT_UP_TICK.load(Ordering::Relaxed);
        // 循環タイムスタンプで「down が up より新しい」かつ「1.5s 以内」を確認
        let down_is_newer = down.wrapping_sub(up) < u32::MAX / 2;
        let within_window = now.wrapping_sub(down) < 1500;
        let alt_down = down_is_newer && within_window;
        tracing::trace!("mouse_ll: WM_MOUSEWHEEL alt={alt_down}");
        if alt_down {
            let delta = (ms.mouseData >> 16) as i16;
            let dir = if delta > 0 {
                FocusDir::Right
            } else {
                FocusDir::Left
            };
            if let Some(tx) = SENDER.get() {
                let _ = tx.send(WmEvent::Focus(dir));
            }
            return LRESULT(1);
        }
    }
    CallNextHookEx(None, code, wp, lp)
}
