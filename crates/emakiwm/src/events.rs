//! SetWinEventHook によるイベント購読 (FR-1.2)。
//!
//! WINEVENT_OUTOFCONTEXT のコールバックはメッセージポンプを持つスレッドで
//! 受ける必要があるため、専用スレッドでフック登録 + GetMessageW ループを回し、
//! 生イベントを mpsc チャネルで Core スレッドへ送る (§5 アーキテクチャ)。

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Mutex, OnceLock};

use emakiwm_core::keybind::MOD_MUHENKAN;
use emakiwm_core::layout::FocusDir;
use emakiwm_core::modkey::{KeyAction, ModKeyTracker, VK_NONCONVERT};
use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Accessibility::{SetWinEventHook, HWINEVENTHOOK};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, MapVirtualKeyW, RegisterHotKey, SendInput, UnregisterHotKey,
    HOT_KEY_MODIFIERS, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP,
    MAPVK_VK_TO_VSC, VK_CONTROL, VK_LMENU, VK_LWIN, VK_MENU, VK_RMENU, VK_RWIN, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DispatchMessageW, GetAncestor, GetMessageTime,
    GetMessageW, PostThreadMessageW, RegisterClassW, SetWindowsHookExW, EVENT_OBJECT_CLOAKED,
    EVENT_OBJECT_DESTROY, EVENT_OBJECT_HIDE, EVENT_OBJECT_SHOW, EVENT_OBJECT_UNCLOAKED,
    EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_MINIMIZEEND, EVENT_SYSTEM_MINIMIZESTART,
    EVENT_SYSTEM_MOVESIZEEND, EVENT_SYSTEM_MOVESIZESTART, GA_ROOT, KBDLLHOOKSTRUCT, MSG,
    MSLLHOOKSTRUCT, OBJID_WINDOW, WH_KEYBOARD_LL, WH_MOUSE_LL, WINEVENT_OUTOFCONTEXT,
    WINEVENT_SKIPOWNPROCESS, WM_APP, WM_DISPLAYCHANGE, WM_HOTKEY, WM_INPUT, WM_KEYDOWN, WM_KEYUP,
    WM_MOUSEWHEEL, WM_SETTINGCHANGE, WM_SYSKEYDOWN, WM_SYSKEYUP, WNDCLASSW, WS_POPUP,
};

use crate::touchpad::GestureBinds;

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
    /// ユーザーによる手動移動・リサイズの開始 (MOVESIZESTART)。
    /// フロート窓のドラッグ中は太枠オーバーレイを隠すために使う
    MoveSizeStart(isize),
    /// ユーザーによる手動移動・リサイズの完了 (MOVESIZEEND)
    MoveSizeEnd(isize),
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
    /// 前面ウィンドウのフローティング ⇔ タイルの切替
    ToggleFloating,
    /// フローティングウィンドウとタイルの間でフォーカスを切替
    FocusFloating,
    /// 隣のモニタへフォーカスを移動する。true = 右/次
    FocusMonitor(bool),
    /// フォーカスウィンドウを隣のモニタへ移動する。true = 右/次
    MoveToMonitor(bool),
    /// Overview (全ワークスペースの縮小一覧) の開閉トグル (FR-4.11)
    ToggleOverview,
    /// Overview を閉じる要求 (Overview ウィンドウでの Esc / Enter)
    OverviewClose,
    /// Overview ウィンドウ上のクリック (スクリーン座標)
    OverviewClick(i32, i32),
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
        ("toggle-floating", "") => WmEvent::ToggleFloating,
        ("focus-floating", "") => WmEvent::FocusFloating,
        ("focus-monitor", "right") => WmEvent::FocusMonitor(true),
        ("focus-monitor", "left") => WmEvent::FocusMonitor(false),
        ("move-to-monitor", "right") => WmEvent::MoveToMonitor(true),
        ("move-to-monitor", "left") => WmEvent::MoveToMonitor(false),
        ("toggle-overview", "") => WmEvent::ToggleOverview,
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

/// 無変換を修飾にするバインド (mods は MOD_MUHENKAN を含む, vk, イベント)。
/// apply_binds が差し替え、keyboard_ll_proc が読む。
static MK_BINDS: Mutex<Vec<(u32, u32, WmEvent)>> = Mutex::new(Vec::new());
/// 無変換の保持状態機械 (フックスレッドからのみ操作。reload 時に初期化)
static MK_TRACKER: Mutex<ModKeyTracker> = Mutex::new(ModKeyTracker::new());
/// 単打再送時に SendInput へ付ける目印 (dwExtraInfo)。
/// 自分が再送したイベントだけを見分けて素通しする。
/// 他のキー割当ツールが注入したイベントは通常どおり処理したいので、
/// LLKHF_INJECTED 全般は除外しない
const MUHENKAN_REPLAY_TAG: usize = 0x454D_4B4D; // "EMKM"
/// 無変換単打の再送要求 (フック → フックスレッド自身へ)
const WM_APP_REPLAY_MUHENKAN: u32 = WM_APP + 2;

/// 登録済みの低レベルフック。一度張ったらプロセス終了まで残す
#[derive(Default)]
struct LlHooked {
    keyboard: bool,
    mouse: bool,
}

/// 設定由来の入力割当。reload で差し替えられる。
pub struct InputBinds {
    pub hotkeys: Vec<Hotkey>,
    pub gestures: GestureBinds,
    pub mouse_scroll_focus: bool,
}

/// フックスレッドの ID と、差し替え待ちの入力割当。
/// ホットキーは登録したスレッドのメッセージキューに届くため、
/// 差し替えもフックスレッド上で行う必要がある。
static HOOK_THREAD_ID: AtomicU32 = AtomicU32::new(0);
static PENDING_BINDS: Mutex<Option<InputBinds>> = Mutex::new(None);
const WM_APP_REBIND: u32 = WM_APP + 1;

/// 入力割当の差し替えをフックスレッドへ依頼する (reload 用、FR-7.2)。
pub fn rebind(binds: InputBinds) {
    *PENDING_BINDS.lock().unwrap_or_else(|p| p.into_inner()) = Some(binds);
    let tid = HOOK_THREAD_ID.load(Ordering::Relaxed);
    if tid != 0 {
        unsafe {
            let _ = PostThreadMessageW(tid, WM_APP_REBIND, WPARAM(0), LPARAM(0));
        }
    }
}

/// 入力割当をフックスレッド上で適用する。`hotkeys` は現在登録中のもので、
/// 解除してから `binds` のものへ入れ替える。
/// WH_KEYBOARD_LL は mouse_scroll_focus か無変換バインドがあるときだけ、WH_MOUSE_LL は
/// mouse_scroll_focus のときだけ登録する
/// (不要なグローバルフックはセキュリティソフトの誤検出を招くため)。
/// gestures も同様に、割当が 1 つもなければ Raw Input を登録しない。
unsafe fn apply_binds(
    binds: InputBinds,
    hotkeys: &mut Vec<Hotkey>,
    hooked: &mut LlHooked,
    events_hwnd: Option<HWND>,
) {
    for (i, hk) in hotkeys.iter().enumerate() {
        if hk.mods & MOD_MUHENKAN == 0 {
            let _ = UnregisterHotKey(None, (i + 1) as i32);
        }
    }
    // FR-6.1: ホットキーはメッセージループを持つこのスレッドで登録する。
    // 失敗 (他アプリと競合等) は警告に留めて続行する。
    // 無変換を含むバインドは RegisterHotKey では扱えないので WH_KEYBOARD_LL 側へ回す。
    // WM_HOTKEY の id から `hotkeys` を引くため、id は無変換バインドを含む全体の添字 + 1
    const MOD_NOREPEAT: u32 = 0x4000;
    for (i, hk) in binds.hotkeys.iter().enumerate() {
        if hk.mods & MOD_MUHENKAN != 0 {
            continue;
        }
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
    // フックが読む無変換バインドの一覧を差し替える。保持状態も初期化する
    // (旧割当の添字を引きずらないため)
    let mk_binds: Vec<(u32, u32, WmEvent)> = binds
        .hotkeys
        .iter()
        .filter(|hk| hk.mods & MOD_MUHENKAN != 0)
        .map(|hk| (hk.mods, hk.vk, hk.event.clone()))
        .collect();
    let any_mk = !mk_binds.is_empty();
    // 押下状態 (MK_TRACKER) はリセットしない。reload を無変換のコンボに割り当てて
    // いると、無変換を押したままここに来るため、リセットすると同じ押下中の
    // 次のコンボやリピートが素のキーとして漏れる。トラッカーはバインドの添字を
    // 持たない (vk のみ) ので、割当が変わってもそのまま使える
    *MK_BINDS.lock().unwrap_or_else(|p| p.into_inner()) = mk_binds;
    *hotkeys = binds.hotkeys;

    // 3 本指スワイプ (FR-6.5): タッチパッドの Raw Input を events ウィンドウ宛に
    // 登録する。割当がなければ購読自体しない
    if let Some(hwnd) = events_hwnd {
        crate::touchpad::set_binds(hwnd, binds.gestures);
    }

    // グローバル入力フックは必要なときだけ登録する (FR-6.1)。
    // WH_KEYBOARD_LL / WH_MOUSE_LL は正当な用途でもセキュリティソフトに
    // キーロガーとして誤検出されやすいため。
    //   WH_KEYBOARD_LL: mouse_scroll_focus (Alt 追跡) または無変換バインドがあるとき
    //   WH_MOUSE_LL:    mouse_scroll_focus のときだけ
    // 一度登録したフックは無効化後も残すが、機能が無効なら素通しする
    let instance = GetModuleHandleW(None).unwrap_or_default();
    if (binds.mouse_scroll_focus || any_mk) && !hooked.keyboard {
        hooked.keyboard = true;
        match SetWindowsHookExW(
            WH_KEYBOARD_LL,
            Some(keyboard_ll_proc),
            Some(instance.into()),
            0,
        ) {
            Ok(h) if !h.is_invalid() => tracing::debug!("WH_KEYBOARD_LL registered"),
            _ => tracing::warn!("WH_KEYBOARD_LL の登録に失敗しました"),
        }
    }
    if binds.mouse_scroll_focus && !hooked.mouse {
        hooked.mouse = true;
        match SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_ll_proc), Some(instance.into()), 0) {
            Ok(h) if !h.is_invalid() => tracing::debug!("WH_MOUSE_LL registered"),
            _ => tracing::warn!("WH_MOUSE_LL の登録に失敗しました"),
        }
    }
}

/// フックスレッドを起動する。プロセス終了までフックは張りっぱなしでよい
/// (フックはプロセス終了時に OS が解除する)。
/// 入力割当は [`rebind`] で起動後も差し替えられる。
pub fn spawn_hook_thread(tx: Sender<WmEvent>, binds: InputBinds) {
    SENDER.set(tx).expect("hook thread spawned twice");

    std::thread::spawn(move || unsafe {
        HOOK_THREAD_ID.store(GetCurrentThreadId(), Ordering::Relaxed);

        // 必要レンジのみフックする (全イベント購読は高コスト):
        //   0x0003..0x0017: FOREGROUND / MOVESIZESTART / MOVESIZEEND / MINIMIZESTART / MINIMIZEEND
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
        let mut events_hwnd = None;
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
                events_hwnd = Some(hwnd);
            }
        }

        // Overview 用ウィンドウ (FR-4.11)。マウス・キー入力を受けるため、
        // メッセージポンプを持つこのスレッドが所有する
        crate::overview::create_window();

        let mut hotkeys: Vec<Hotkey> = Vec::new();
        let mut hooked = LlHooked::default();
        apply_binds(binds, &mut hotkeys, &mut hooked, events_hwnd);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            // reload による入力割当の差し替え (スレッド宛メッセージなので hwnd は NULL)
            if msg.message == WM_APP_REBIND && msg.hwnd.is_invalid() {
                let pending = PENDING_BINDS
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .take();
                if let Some(binds) = pending {
                    apply_binds(binds, &mut hotkeys, &mut hooked, events_hwnd);
                    tracing::info!("入力割当を更新しました ({} キーバインド)", hotkeys.len());
                }
                continue;
            }
            // 無変換の単打: フックが食った down/up の代わりに IME へ再送する
            if msg.message == WM_APP_REPLAY_MUHENKAN && msg.hwnd.is_invalid() {
                replay_muhenkan_tap();
                continue;
            }
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

/// 無変換の down + up を目印つきで注入する。目印によりフックは素通しする
unsafe fn replay_muhenkan_tap() {
    let scan = MapVirtualKeyW(VK_NONCONVERT, MAPVK_VK_TO_VSC) as u16;
    let key = |flags| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY(VK_NONCONVERT as u16),
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: MUHENKAN_REPLAY_TAG,
            },
        },
    };
    let inputs = [key(Default::default()), key(KEYEVENTF_KEYUP)];
    if SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) != inputs.len() as u32 {
        tracing::warn!("無変換の再送 (SendInput) に失敗しました");
    }
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
        // タッチパッド HID レポート (FR-6.5)。処理後もシステム側の後始末が
        // 必要なため DefWindowProc へ流す
        WM_INPUT => {
            crate::touchpad::handle_input(lp, unsafe { GetMessageTime() } as u32);
            unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
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
        // 子ウィンドウ (コントロール類) の SHOW は大量に発火するのでここで落とす。
        // 消滅系は破棄済み HWND に GetAncestor が効かないため絞らない
        // (Core 側は未管理の HWND を即座に無視する)
        EVENT_OBJECT_SHOW | EVENT_OBJECT_UNCLOAKED if GetAncestor(hwnd, GA_ROOT) != hwnd => return,
        EVENT_OBJECT_SHOW | EVENT_OBJECT_UNCLOAKED => WmEvent::Appeared(h),
        EVENT_OBJECT_DESTROY | EVENT_OBJECT_HIDE => WmEvent::Gone(h),
        EVENT_OBJECT_CLOAKED => WmEvent::Cloaked(h),
        EVENT_SYSTEM_FOREGROUND => WmEvent::Foreground(h),
        EVENT_SYSTEM_MINIMIZESTART => WmEvent::MinimizeStart(h),
        EVENT_SYSTEM_MINIMIZEEND => WmEvent::MinimizeEnd(h),
        EVENT_SYSTEM_MOVESIZESTART => WmEvent::MoveSizeStart(h),
        EVENT_SYSTEM_MOVESIZEEND => WmEvent::MoveSizeEnd(h),
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
/// WH_KEYBOARD_LL: Alt キーの押下/解放タイムスタンプを記録する (mouse_scroll_focus 用。
/// Alt のイベント自体は食わない)。
/// キーリピートで SYSKEYDOWN が繰り返し届くため、長押し中は ALT_DOWN_TICK が更新され続ける。
/// 続けて、無変換バインドがあれば ModKeyTracker で無変換の保持・コンボ発火を処理する。
/// 重い処理はせず、イベント送信と thread message の post だけを行う。
unsafe extern "system" fn keyboard_ll_proc(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code >= 0 {
        let ks = &*(lp.0 as *const KBDLLHOOKSTRUCT);
        let vk = ks.vkCode;
        let down = matches!(wp.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
        if vk == VK_MENU.0 as u32 || vk == VK_LMENU.0 as u32 || vk == VK_RMENU.0 as u32 {
            if down {
                ALT_DOWN_TICK.store(ks.time, Ordering::Relaxed);
            } else {
                ALT_UP_TICK.store(ks.time, Ordering::Relaxed);
            }
        }
        let is_key_event = matches!(
            wp.0 as u32,
            WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP
        );
        // 自分が再送した無変換は触らない (他ツールの注入イベントは通常どおり扱う)
        if is_key_event && ks.dwExtraInfo != MUHENKAN_REPLAY_TAG {
            if let Some(ret) = handle_muhenkan(vk, down, ks.time) {
                return ret;
            }
        }
    }
    CallNextHookEx(None, code, wp, lp)
}

/// 無変換バインドの処理。None = 通常どおり次のフックへ渡す。
unsafe fn handle_muhenkan(vk: u32, down: bool, time: u32) -> Option<LRESULT> {
    let event;
    {
        let binds = MK_BINDS.lock().unwrap_or_else(|p| p.into_inner());
        if binds.is_empty() {
            return None; // 無変換バインドなし (reload で無効化済み含む): 素通し
        }
        let pressed = |k: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY| {
            GetAsyncKeyState(k.0 as i32) < 0
        };
        let mut other_mods = 0;
        if pressed(VK_SHIFT) {
            other_mods |= emakiwm_core::keybind::MOD_SHIFT;
        }
        if pressed(VK_CONTROL) {
            other_mods |= emakiwm_core::keybind::MOD_CONTROL;
        }
        if pressed(VK_MENU) {
            other_mods |= emakiwm_core::keybind::MOD_ALT;
        }
        if pressed(VK_LWIN) || pressed(VK_RWIN) {
            other_mods |= emakiwm_core::keybind::MOD_WIN;
        }
        let mut tracker = MK_TRACKER.lock().unwrap_or_else(|p| p.into_inner());
        let action = tracker.on_key(vk, down, time, other_mods, |m, k| {
            binds.iter().position(|(bm, bk, _)| *bm == m && *bk == k)
        });
        match action {
            KeyAction::Pass => return None,
            KeyAction::Swallow => return Some(LRESULT(1)),
            KeyAction::ReplayTap => {
                let tid = HOOK_THREAD_ID.load(Ordering::Relaxed);
                if tid != 0 {
                    let _ = PostThreadMessageW(tid, WM_APP_REPLAY_MUHENKAN, WPARAM(0), LPARAM(0));
                }
                return Some(LRESULT(1));
            }
            KeyAction::Fire(i) => event = binds[i].2.clone(),
        }
    }
    if let Some(tx) = SENDER.get() {
        let _ = tx.send(event);
    }
    Some(LRESULT(1))
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
