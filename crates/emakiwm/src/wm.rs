//! タイリングランタイム。
//!
//! - 起動時スキャンで既存ウィンドウを Strip へ取り込む (FR-1.1)
//! - WinEventHook の開閉イベントに追従 (FR-1.2)、Alt+H/L/J/K でフォーカス移動 (FR-4.1)
//! - Viewport 内のみ DeferWindowPos で配置、外は offscreen 退避または cloak (FR-3.2, FR-3.3)
//! - レイアウト変化は各ウィンドウ Rect の補間で滑らかに反映 (FR-4.8)
//! - 正常終了 (Ctrl+C / Alt+Shift+E)・panic とも全ウィンドウを元の位置へ復元 (FR-1.5, NFR-4)
//! - 復元 Rect は state.json へも永続化し、kill 後は `--restore` で戻せる (FR-1.6)
//! - 各物理モニターが独立した Stack (ワークスペース列) を持つ
//!
//! 状態の持ち方: `strip` は常に最終ターゲット (論理状態) を保持し、
//! 画面上の現在位置は `visual` マップが持つ。アニメーションは visual → 論理状態の
//! 射影 (targets) への補間。イベントで論理状態が変われば、進行中のアニメは
//! 現在の visual から新ターゲットへ向け直される (§8-8)。

use std::collections::HashMap;
use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use emakiwm_core::anim::{ease_out_cubic, lerp_rect};
use emakiwm_core::filter::Decision;
use emakiwm_core::layout::{FocusDir, Placement, Rect, Stack, WindowId};
use windows::core::BOOL;
use windows::Win32::Foundation::{
    COLORREF, HWND, LPARAM, POINT, RECT, TYPE_E_ELEMENTNOTFOUND, WPARAM,
};
use windows::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_CLOAKED,
};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, MonitorFromPoint, MonitorFromWindow, HDC, HMONITOR,
    MONITORINFO, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY,
};
use windows::Win32::System::Console::SetConsoleCtrlHandler;
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{
    BeginDeferWindowPos, DeferWindowPos, EndDeferWindowPos, GetForegroundWindow,
    GetLayeredWindowAttributes, GetWindowLongPtrW, GetWindowThreadProcessId, IsIconic, IsWindow,
    PostMessageW, SetForegroundWindow, SetLayeredWindowAttributes, SetWindowLongPtrW, SetWindowPos,
    GWL_EXSTYLE, LWA_ALPHA, SWP_NOACTIVATE, SWP_NOZORDER, SW_SHOWDEFAULT, WM_CLOSE, WS_EX_LAYERED,
};

use crate::events::{self, WmEvent};
use crate::{border, com, config, ipc, scan, tray, ws};

/// アニメーション中のフレーム間隔。
const FRAME: Duration = Duration::from_millis(8);

/// 終了時復元用の元 Rect (FR-1.5)。
/// panic フックと Ctrl+C ハンドラから参照するためグローバルに持つ。
static RESTORE_RECTS: Mutex<Option<HashMap<isize, RECT>>> = Mutex::new(None);

/// FR-3.3 cloak モードで自分が cloak したウィンドウの実状態。
/// CLOAKED イベントの自己無視と、終了時の一括 uncloak に使う。
///
/// 不変条件: エントリがある間は「自分が cloak した (まだ解けていない)」。
/// uncloak は SetCloak(false) が実際に成功するまでエントリを消さない —
/// 先に消すと、失敗 (view 再作成中の deferred) の間に届く CLOAKED イベントを
/// 外部由来と誤認して管理外へ追い出すループになる。
struct CloakEntry {
    hwnd: u64,
    /// WS_EX_TOOLWINDOW 付与済み (Alt+Tab から消えている)
    tool: bool,
    /// uncloak 要求済みだが SetCloak(false) が失敗していて再試行待ち
    pending: bool,
    attempts: u32,
}
static CLOAKED: Mutex<Vec<CloakEntry>> = Mutex::new(Vec::new());

/// FR-3.3 cloak モードの隠蔽レベル。
/// SetCloak は描画停止のみで Alt+Tab には残る。一覧から消すのは
/// WS_EX_TOOLWINDOW の併用 — この 2 段を分けて使う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hide {
    /// 表示 (uncloak)
    Show,
    /// 描画のみ停止。Alt+Tab・タスクバーに残る (active ワークスペースの viewport 外)
    Render,
    /// 完全に隠す。Alt+Tab からも消える (非 active ワークスペース)
    Full,
}

/// FR-3.8 半透明化のために WS_EX_LAYERED を付与したウィンドウと現在のアルファ。
/// 終了時に必ず不透明へ戻す。
static DIMMED: Mutex<Vec<(u64, u8)>> = Mutex::new(Vec::new());

/// 1 ウィンドウぶんの補間区間。
struct AnimEntry {
    hwnd: u64,
    from: Rect,
    to: Rect,
}

/// 不可視フレーム (§8-2): GetWindowRect と DWMWA_EXTENDED_FRAME_BOUNDS の差
/// (left, top, right, bottom)。論理 Rect (見た目のフレーム) ⇔ ウィンドウ Rect の変換に使う。
type Border = (i32, i32, i32, i32);

/// 1 物理モニターぶんの状態。
struct MonitorState {
    /// HMONITOR 値 (モニター識別用)
    hmonitor: isize,
    /// 実効作業領域 (gap・margin 適用後)
    work: Rect,
    /// モニター全面 (fullscreen 用)
    monitor: Rect,
    /// work.w - 2 * gap
    viewport_w: i32,
    /// 新規 Column のデフォルト幅
    default_width: i32,
    /// このモニターのワークスペース列
    stack: Stack,
}

pub fn run() {
    install_restore_hooks();

    let mut cfg = config::load();
    events::set_mouse_scroll_focus(cfg.mouse_scroll_focus);

    let (tx, rx) = mpsc::channel::<WmEvent>();
    events::spawn_hook_thread(tx.clone(), cfg.hotkeys.clone(), cfg.mouse_scroll_focus);
    if let Some(port) = cfg.ws_port {
        ws::spawn_ws_thread(port, tx.clone());
    }
    ipc::spawn_ipc_thread(tx);

    let mut monitors = enumerate_monitors(&cfg);
    let mut active_monitor = 0usize;
    tracing::info!(
        "monitors: {}",
        monitors
            .iter()
            .enumerate()
            .map(|(i, m)| format!(
                "[{}] ({},{}) {}x{}",
                i,
                m.work.x,
                m.work.y,
                m.work.w,
                m.work.h
            ))
            .collect::<Vec<_>>()
            .join(", ")
    );

    let mut focused: Option<WindowId> = None;
    // 直前のモニター跨ぎフォーカス追従の記録 (前フォーカス窓, 時刻)。
    // 追従直後にその前フォーカス窓が消えた場合、それは「閉じたことによる
    // OS の Z 順フォールバックへの追従」だったと分かるので巻き戻す
    // (Firefox のように前面を手放してから遅れて窓を破棄するアプリ対策)
    let mut cross_follow: Option<(WindowId, Instant)> = None;
    // 閉じイベントで後継フォーカスを選んだ直後の防衛 (後継窓, 時刻)。
    // 閉じたアプリが複数の削除系イベントを出す場合、後継決定の後から OS の
    // Z 順フォールバック前面化が届くことがある。防衛時間内の別モニターへの
    // 前面化は stale とみなして無視し、後継を再主張する
    let mut focus_guard: Option<(WindowId, Instant)> = None;
    let mut visual: HashMap<u64, Rect> = HashMap::new();
    let mut borders: HashMap<u64, Border> = HashMap::new();
    let mut fullscreen: Option<u64> = None;
    let mut subscribers: Vec<mpsc::Sender<String>> = Vec::new();
    let mut last_state = String::new();
    let mut bordered: Option<u64> = None;
    let mut pinned: Vec<u64> = Vec::new();

    // FR-1.1: 起動時スキャン。モニターごとに左端 x 順に並べてタイル化する
    let all_initial: Vec<scan::ScanEntry> = {
        let mut v: Vec<scan::ScanEntry> = scan::scan(&cfg.rules)
            .into_iter()
            .filter(|e| e.decision == Decision::Manage && e.elevated != Some(true))
            .filter(|e| unsafe { !IsIconic(HWND(e.hwnd as _)).as_bool() })
            .collect();
        // (monitor_index, rect.left) でソートして各モニター内の左→右順を保つ
        let mon_ref = &monitors;
        v.sort_by_key(|e| {
            let mi = monitor_for_window(e.hwnd, mon_ref);
            (mi, e.rect.left)
        });
        v
    };
    for e in &all_initial {
        let mi = monitor_for_window(e.hwnd, &monitors);
        let (work, dw) = {
            let ms = &monitors[mi];
            (ms.work, ms.default_width)
        };
        adopt(
            &mut monitors[mi].stack,
            &mut visual,
            &mut borders,
            e,
            dw,
            cfg.gap,
            None,
            work,
        );
        tracing::info!(
            "adopt {:#x} {} \"{}\" → monitor {}",
            e.hwnd,
            e.info.exe_name.as_deref().unwrap_or("?"),
            e.info.title,
            mi,
        );
        if let Some(c) = border_colors(&cfg) {
            set_border_color(e.hwnd as u64, c.1);
        }
    }

    persist_restore_rects();

    for cmd in &cfg.startup {
        shell_spawn(cmd);
    }

    let fg = WindowId(unsafe { GetForegroundWindow() }.0 as u64);
    if let Some(mi) = monitor_index_of(fg, &monitors) {
        active_monitor = mi;
        focused = Some(fg);
        let ms = &mut monitors[active_monitor];
        let strip = ms.stack.active_mut();
        strip.set_active(fg);
        strip.last_focused = Some(fg);
    }

    tracing::info!(
        "managing {} windows on {} monitor(s). Ctrl+C / Alt+Shift+E で終了",
        monitors
            .iter()
            .flat_map(|ms| ms.stack.strips.iter())
            .map(|s| s.columns.len())
            .sum::<usize>(),
        monitors.len()
    );

    let mut own_map = own_monitor_map(&monitors);
    let mut last_targets = targets_all(&monitors, fullscreen, cfg.gap);
    let mut anim = start_transition(
        &mut visual,
        &last_targets,
        &own_map,
        &monitors,
        &borders,
        cfg.cloak,
    );

    'main: loop {
        // deferred になった uncloak の再試行。残っている間はフレーム間隔で回す
        let pending_uncloak = retry_pending_uncloaks();
        let ev = if anim.is_some() || pending_uncloak {
            match rx.recv_timeout(FRAME) {
                Ok(ev) => Some(ev),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        } else {
            match rx.recv() {
                Ok(ev) => Some(ev),
                Err(_) => break,
            }
        };

        if let Some(ev) = ev {
            match &ev {
                WmEvent::Query(_) | WmEvent::Subscribe(_) => {}
                e => tracing::debug!("event: {e:?}"),
            }
            match ev {
                // どこかの Stack が既に管理中なら何もしない。物理位置ベースで
                // adopt すると、退避・アニメ中に他モニター上で UNCLOAKED/SHOW が
                // 発火したとき別モニターの Stack へ二重登録されてしまう
                WmEvent::Appeared(h) | WmEvent::MinimizeEnd(h)
                    if monitor_index_of(WindowId(h as u64), &monitors).is_some() => {}
                WmEvent::Appeared(h) | WmEvent::MinimizeEnd(h) => {
                    // 新規ウィンドウは物理的な出現位置 (アプリ任せ ≒ プライマリ) では
                    // なく、フォーカスのあるモニターへ取り込む
                    let mi = active_monitor;
                    let work = monitors[mi].work;
                    let dw = monitors[mi].default_width;
                    let local_focused = focused.filter(|f| monitors[mi].stack.contains(*f));
                    if try_adopt(
                        &mut monitors[mi].stack,
                        &mut visual,
                        &mut borders,
                        h,
                        dw,
                        cfg.gap,
                        &cfg.rules,
                        local_focused,
                        work,
                    ) {
                        active_monitor = mi;
                        let id = WindowId(h as u64);
                        focused = Some(id);
                        let ms = &mut monitors[active_monitor];
                        let vw = ms.viewport_w;
                        let strip = ms.stack.active_mut();
                        strip.set_active(id);
                        strip.last_focused = Some(id);
                        strip.ensure_visible(id, vw);
                        focus_window(h);
                        persist_restore_rects();
                        if let Some(c) = border_colors(&cfg) {
                            set_border_color(h as u64, c.1);
                        }
                    }
                }
                WmEvent::Cloaked(h) if cloaked_by_us(h as u64) || !window_cloaked(h) => {}
                WmEvent::Gone(h) | WmEvent::MinimizeStart(h) | WmEvent::Cloaked(h) => {
                    let id = WindowId(h as u64);
                    if let Some(mi) = monitor_index_of(id, &monitors) {
                        let ms = &mut monitors[mi];
                        if let Some(si) = ms.stack.strip_index_of(id) {
                            let vw = ms.viewport_w;
                            // 消えた窓が「直前のモニター跨ぎ追従の前フォーカス窓」なら、
                            // その追従は OS フォールバックだった → 巻き戻し対象
                            let fallback_rollback = cross_follow.is_some_and(|(p, t)| {
                                p == id && t.elapsed() < Duration::from_millis(800)
                            });
                            if fallback_rollback {
                                tracing::debug!(
                                    "remove {h:#x}: 直前のモニター跨ぎ追従を巻き戻す"
                                );
                            }
                            let was_focused = focused == Some(id) || fallback_rollback;
                            if cross_follow.is_some_and(|(p, _)| p == id) {
                                cross_follow = None;
                            }
                            let strip = &mut ms.stack.strips[si];
                            // 後継候補: 同じ位置の列 (末尾なら左隣) の active Tile。
                            // 縦スタック内で消えた場合は同列の残りが繰り上がる
                            let col_hint = strip.column_index_of(id);
                            strip.remove_window(id, cfg.gap);
                            strip.clamp_offset(vw);
                            if strip.last_focused == Some(id) {
                                strip.last_focused = None;
                            }
                            let succ = was_focused
                                .then(|| {
                                    let cols = &strip.columns;
                                    col_hint
                                        .and_then(|i| cols.get(i.min(cols.len().checked_sub(1)?)))
                                        .and_then(|c| c.active())
                                })
                                .flatten();
                            ms.stack.normalize();
                            // フォーカス窓の消滅はモニター内で完結させる:
                            // 同モニターの後継へフォーカスを移す (OS の Z 順
                            // フォールバックで別モニターへ飛ばさない)
                            if was_focused {
                                let strip = ms.stack.active_mut();
                                let succ = succ
                                    .filter(|s| strip.contains(*s))
                                    .or_else(|| {
                                        strip.last_focused.filter(|f| strip.contains(*f))
                                    })
                                    .or_else(|| {
                                        strip.columns.first().and_then(|c| c.active())
                                    });
                                if let Some(s) = succ {
                                    active_monitor = mi;
                                    focused = Some(s);
                                    strip.set_active(s);
                                    strip.last_focused = Some(s);
                                    strip.ensure_visible(s, vw);
                                    focus_window(s.0 as isize);
                                    focus_guard = Some((s, Instant::now()));
                                }
                            }
                        }
                        tracing::info!("remove {h:#x}");
                        visual.remove(&(h as u64));
                        borders.remove(&(h as u64));
                        cloak_set(h as u64, Hide::Show);
                        undim_window(h as u64);
                        pinned.retain(|&x| x != h as u64);
                        if fullscreen == Some(h as u64) {
                            fullscreen = None;
                        }
                        if focused == Some(id) {
                            focused = None;
                        }
                        if !unsafe { IsWindow(Some(HWND(h as _))).as_bool() } {
                            if let Some(m) = RESTORE_RECTS.lock().unwrap().as_mut() {
                                m.remove(&h);
                            }
                        }
                        persist_restore_rects();
                    }
                }
                WmEvent::Foreground(h) => {
                    let id = WindowId(h as u64);
                    // 後継フォーカスの防衛: 閉じ処理で後継を選んだ直後に届く
                    // 別モニターへの stale なフォールバック前面化は無視して再主張
                    let mut guarded = false;
                    if let Some((succ, t)) = focus_guard {
                        if t.elapsed() >= Duration::from_millis(500) || id == succ {
                            focus_guard = None;
                        } else if monitor_index_of(id, &monitors)
                            .is_some_and(|mi| mi != active_monitor)
                        {
                            tracing::debug!(
                                "foreground {h:#x}: 閉じ直後の stale フォールバックを無視、後継 {:#x} を再主張",
                                succ.0
                            );
                            focus_window(succ.0 as isize);
                            guarded = true;
                        }
                    }
                    // フォーカス窓が消滅/最小化した直後に OS が Z 順で別モニターの
                    // 窓を前面化するフォールバックには追従しない
                    // (Gone/MinimizeStart 側がモニター内の後継へフォーカスを移す)
                    let os_fallback = monitor_index_of(id, &monitors)
                        .is_some_and(|mi| mi != active_monitor)
                        && focused.is_some_and(|f| unsafe {
                            let hw = HWND(f.0 as _);
                            !IsWindow(Some(hw)).as_bool() || IsIconic(hw).as_bool()
                        });
                    if guarded || os_fallback {
                        if os_fallback {
                            tracing::debug!("foreground {h:#x}: OS フォールバックとして無視");
                        }
                    } else if let Some(mi) = monitor_index_of(id, &monitors) {
                        if mi != active_monitor {
                            cross_follow = focused.map(|f| (f, Instant::now()));
                        }
                        active_monitor = mi;
                        let ms = &mut monitors[active_monitor];
                        // 別ワークスペースのウィンドウなら切替えて追従
                        if let Some(si) = ms.stack.strip_index_of(id) {
                            if si != ms.stack.active {
                                tracing::debug!(
                                    "foreground follow: monitor {mi} ws {} → {si}",
                                    ms.stack.active
                                );
                            }
                            ms.stack.active = si;
                        }
                        focused = Some(id);
                        let vw = ms.viewport_w;
                        let strip = ms.stack.active_mut();
                        strip.set_active(id);
                        strip.last_focused = Some(id);
                        strip.ensure_visible(id, vw);
                    }
                }
                WmEvent::Focus(dir) => {
                    let ms = &mut monitors[active_monitor];
                    let vw = ms.viewport_w;
                    let strip = ms.stack.active_mut();
                    let target = match focused.filter(|f| strip.contains(*f)) {
                        Some(cur) => strip.neighbor(cur, dir),
                        None => strip.columns.first().and_then(|c| c.active()),
                    };
                    match target {
                        Some(t) => {
                            focused = Some(t);
                            strip.set_active(t);
                            strip.last_focused = Some(t);
                            strip.ensure_visible(t, vw);
                            focus_window(t.0 as isize);
                        }
                        None if dir == FocusDir::Down => {
                            switch_workspace(&mut ms.stack, &mut focused, true)
                        }
                        None if dir == FocusDir::Up => {
                            switch_workspace(&mut ms.stack, &mut focused, false)
                        }
                        None => {}
                    }
                }
                WmEvent::MoveColumn(dir) => {
                    if let Some(f) = focused {
                        let ms = &mut monitors[active_monitor];
                        let vw = ms.viewport_w;
                        let strip = ms.stack.active_mut();
                        if strip.move_column(f, dir, cfg.gap) {
                            strip.ensure_visible(f, vw);
                        }
                    }
                }
                WmEvent::Expel => {
                    if let Some(f) = focused {
                        let ms = &mut monitors[active_monitor];
                        let vw = ms.viewport_w;
                        let strip = ms.stack.active_mut();
                        if strip.expel(f, cfg.gap) {
                            strip.ensure_visible(f, vw);
                        }
                    }
                }
                WmEvent::Consume => {
                    if let Some(f) = focused {
                        monitors[active_monitor].stack.active_mut().consume_right(f, cfg.gap);
                    }
                }
                WmEvent::MoveToWorkspace(down) => {
                    if let Some(f) = focused {
                        let ms = &mut monitors[active_monitor];
                        let vw = ms.viewport_w;
                        let dw = ms.default_width;
                        if ms.stack.move_window(f, down, dw, vw, cfg.gap) {
                            let strip = ms.stack.active_mut();
                            strip.set_active(f);
                            strip.ensure_visible(f, vw);
                        }
                    }
                }
                WmEvent::SwitchWorkspace(down) => {
                    let ms = &mut monitors[active_monitor];
                    switch_workspace(&mut ms.stack, &mut focused, down);
                }
                WmEvent::CycleWidth => {
                    if let Some(f) = focused {
                        let ms = &mut monitors[active_monitor];
                        let vw = ms.viewport_w;
                        let strip = ms.stack.active_mut();
                        if strip.cycle_width(f, vw, cfg.gap) {
                            strip.ensure_visible(f, vw);
                        }
                    }
                }
                WmEvent::MaximizeColumn => {
                    if let Some(f) = focused {
                        let ms = &mut monitors[active_monitor];
                        let vw = ms.viewport_w;
                        let strip = ms.stack.active_mut();
                        if strip.toggle_maximize(f, vw, cfg.gap) {
                            strip.ensure_visible(f, vw);
                        }
                    }
                }
                WmEvent::Fullscreen => {
                    if let Some(f) = focused {
                        fullscreen = (fullscreen != Some(f.0)).then_some(f.0);
                    }
                }
                WmEvent::Scroll(forward) => {
                    let ms = &mut monitors[active_monitor];
                    let vw = ms.viewport_w;
                    ms.stack.active_mut().scroll_columnwise(forward, vw);
                }
                WmEvent::CloseFocused => {
                    if let Some(f) = focused {
                        unsafe {
                            let _ = PostMessageW(
                                Some(HWND(f.0 as _)),
                                WM_CLOSE,
                                WPARAM(0),
                                LPARAM(0),
                            );
                        }
                    }
                }
                WmEvent::Spawn(cmd) => {
                    shell_spawn(&cmd);
                }
                WmEvent::ToggleOpacity => {
                    if let Some(f) = focused {
                        if let Some(i) = pinned.iter().position(|&x| x == f.0) {
                            pinned.swap_remove(i);
                        } else {
                            pinned.push(f.0);
                        }
                    }
                }
                WmEvent::FocusMonitor(forward) => {
                    let n = monitors.len();
                    if n > 1 {
                        let new_m = if forward {
                            (active_monitor + 1) % n
                        } else {
                            if active_monitor == 0 { n - 1 } else { active_monitor - 1 }
                        };
                        if new_m != active_monitor {
                            active_monitor = new_m;
                            let ms = &mut monitors[active_monitor];
                            let vw = ms.viewport_w;
                            let strip = ms.stack.active_mut();
                            let f = strip
                                .last_focused
                                .filter(|f| strip.contains(*f))
                                .or_else(|| strip.columns.first().and_then(|c| c.active()));
                            focused = f;
                            if let Some(f) = f {
                                strip.set_active(f);
                                strip.ensure_visible(f, vw);
                                focus_window(f.0 as isize);
                            }
                        }
                    }
                }
                WmEvent::MoveToMonitor(forward) => {
                    if let Some(f) = focused {
                        if let Some(src_mi) = monitor_index_of(f, &monitors) {
                            let n = monitors.len();
                            if n > 1 {
                                let dst_mi = if forward {
                                    (src_mi + 1) % n
                                } else {
                                    if src_mi == 0 { n - 1 } else { src_mi - 1 }
                                };
                                // ソース側から除去
                                {
                                    let ms = &mut monitors[src_mi];
                                    let vw = ms.viewport_w;
                                    if let Some(si) = ms.stack.strip_index_of(f) {
                                        ms.stack.strips[si].remove_window(f, cfg.gap);
                                        ms.stack.strips[si].clamp_offset(vw);
                                        if ms.stack.strips[si].last_focused == Some(f) {
                                            ms.stack.strips[si].last_focused = None;
                                        }
                                    }
                                    ms.stack.normalize();
                                }
                                // 宛先側へ追加
                                {
                                    let ms = &mut monitors[dst_mi];
                                    let dw = ms.default_width;
                                    let anchor = ms.stack.active_mut().last_focused;
                                    ms.stack.active_mut().insert_column_after(
                                        anchor, f, dw, cfg.gap,
                                    );
                                    ms.stack.active_mut().last_focused = Some(f);
                                    ms.stack.normalize();
                                }
                                active_monitor = dst_mi;
                                focused = Some(f);
                                let ms = &mut monitors[active_monitor];
                                let vw = ms.viewport_w;
                                ms.stack.active_mut().set_active(f);
                                ms.stack.active_mut().ensure_visible(f, vw);
                                focus_window(f.0 as isize);
                            }
                        }
                    }
                }
                WmEvent::Reload => {
                    cfg = config::load();
                    events::set_mouse_scroll_focus(cfg.mouse_scroll_focus);
                    // モニター作業領域を更新
                    let raw = collect_monitors();
                    let (top, right, bottom, left) = cfg.margin;
                    let gap = cfg.gap;
                    for ms in &mut monitors {
                        if let Some((_, mi)) = raw.iter().find(|(h, _)| *h == ms.hmonitor) {
                            let mut work = to_rect(mi.rcWork);
                            work.x += left;
                            work.y += top;
                            work.w = (work.w - left - right).max(200);
                            work.h = (work.h - top - bottom).max(200);
                            ms.work = work;
                            ms.monitor = to_rect(mi.rcMonitor);
                            ms.viewport_w = work.w - 2 * gap;
                            ms.default_width =
                                ((ms.viewport_w - gap) as f32 * cfg.default_ratio) as i32;
                        }
                        for s in &mut ms.stack.strips {
                            s.relayout(cfg.gap);
                            s.clamp_offset(ms.viewport_w);
                        }
                    }
                    if !cfg.cloak {
                        uncloak_all();
                    }
                    undim_all();
                    let unfocused = border_colors(&cfg).map_or(BORDER_DEFAULT, |c| c.1);
                    for ms in &monitors {
                        for s in &ms.stack.strips {
                            for col in &s.columns {
                                for t in &col.tiles {
                                    set_border_color(t.0, unfocused);
                                }
                            }
                        }
                    }
                    bordered = None;
                    tracing::info!(
                        "config reloaded: gap={} anim={:?} rules={} cloak={} (キーバインド変更は再起動が必要)",
                        cfg.gap,
                        cfg.anim,
                        cfg.rules.len(),
                        cfg.cloak
                    );
                }
                WmEvent::WorkAreaChanged => {
                    // モニター構成の再取得
                    let raw = collect_monitors();
                    let (top, right, bottom, left) = cfg.margin;
                    let gap = cfg.gap;

                    // 既存モニターを更新し、消えたモニターを検出
                    let mut orphaned: Vec<WindowId> = Vec::new();
                    monitors.retain(|ms| {
                        if raw.iter().any(|(h, _)| *h == ms.hmonitor) {
                            true
                        } else {
                            // 消えたモニターのウィンドウを回収
                            for strip in &ms.stack.strips {
                                for col in &strip.columns {
                                    orphaned.extend_from_slice(&col.tiles);
                                }
                            }
                            false
                        }
                    });

                    let existing_hmons: Vec<isize> =
                        monitors.iter().map(|ms| ms.hmonitor).collect();
                    for (h, mi) in &raw {
                        if existing_hmons.contains(h) {
                            // 作業領域更新
                            if let Some(ms) = monitors.iter_mut().find(|m| m.hmonitor == *h) {
                                let mut work = to_rect(mi.rcWork);
                                work.x += left;
                                work.y += top;
                                work.w = (work.w - left - right).max(200);
                                work.h = (work.h - top - bottom).max(200);
                                if work != ms.work || to_rect(mi.rcMonitor) != ms.monitor {
                                    ms.work = work;
                                    ms.monitor = to_rect(mi.rcMonitor);
                                    ms.viewport_w = work.w - 2 * gap;
                                    ms.default_width =
                                        ((ms.viewport_w - gap) as f32 * cfg.default_ratio) as i32;
                                }
                            }
                        } else {
                            // 新規モニター
                            let mut work = to_rect(mi.rcWork);
                            work.x += left;
                            work.y += top;
                            work.w = (work.w - left - right).max(200);
                            work.h = (work.h - top - bottom).max(200);
                            let vw = work.w - 2 * gap;
                            let dw = ((vw - gap) as f32 * cfg.default_ratio) as i32;
                            monitors.push(MonitorState {
                                hmonitor: *h,
                                work,
                                monitor: to_rect(mi.rcMonitor),
                                viewport_w: vw,
                                default_width: dw,
                                stack: Stack::default(),
                            });
                        }
                    }

                    monitors.sort_by_key(|m| (m.work.x, m.work.y));
                    active_monitor = active_monitor.min(monitors.len().saturating_sub(1));

                    // 孤立ウィンドウを active_monitor へ移す
                    for id in orphaned {
                        let ms = &mut monitors[active_monitor];
                        let dw = ms.default_width;
                        ms.stack.active_mut().insert_column_after(None, id, dw, gap);
                    }
                    if !monitors.is_empty() {
                        monitors[active_monitor].stack.normalize();
                    }

                    for ms in &mut monitors {
                        for s in &mut ms.stack.strips {
                            s.relayout(cfg.gap);
                            s.clamp_offset(ms.viewport_w);
                        }
                    }

                    tracing::info!(
                        "work area changed: {} monitor(s)",
                        monitors.len()
                    );
                }
                WmEvent::Query(reply) => {
                    let _ = reply
                        .send(state_json(&monitors, active_monitor, focused, fullscreen));
                }
                WmEvent::Subscribe(s) => {
                    last_state = state_json(&monitors, active_monitor, focused, fullscreen);
                    if s.send(last_state.clone()).is_ok() {
                        subscribers.push(s);
                    }
                }
                WmEvent::Shutdown => break 'main,
            }

            own_map = own_monitor_map(&monitors);
            let new_targets = targets_all(&monitors, fullscreen, cfg.gap);
            if new_targets != last_targets {
                last_targets = new_targets;
                anim = start_transition(
                    &mut visual,
                    &last_targets,
                    &own_map,
                    &monitors,
                    &borders,
                    cfg.cloak,
                );
            }

            if let Some((focused_color, unfocused_color)) = border_colors(&cfg) {
                let cur = focused.map(|f| f.0);
                if cur != bordered {
                    if let Some(prev) = bordered {
                        set_border_color(prev, unfocused_color);
                    }
                    if let Some(now) = cur {
                        set_border_color(now, focused_color);
                    }
                    bordered = cur;
                }
            }

            if !subscribers.is_empty() {
                let s = state_json(&monitors, active_monitor, focused, fullscreen);
                if s != last_state {
                    last_state = s;
                    subscribers.retain(|sub| sub.send(last_state.clone()).is_ok());
                }
            }
        }

        // フレーム描画
        if let Some((entries, started)) = &anim {
            let t_raw = if cfg.anim.is_zero() {
                1.0
            } else {
                started.elapsed().as_secs_f32() / cfg.anim.as_secs_f32()
            };
            let t = ease_out_cubic(t_raw);
            let frame: Vec<(u64, Rect)> = entries
                .iter()
                .map(|e| (e.hwnd, lerp_rect(e.from, e.to, t)))
                .collect();
            apply_rects(&frame, &borders);
            // cloak モード: 自モニターの縁でクリップ — 縁を越えた瞬間に隠し、
            // 入った瞬間に出す。他モニターへの写り込みを防ぐ。
            // アニメ中は TOOLWINDOW を触らない (view 再作成で SetCloak が
            // 失敗しやすくなるため)。Full は静止後の一括判定でのみ適用する
            if cfg.cloak {
                for &(h, r) in &frame {
                    let level = match hide_level(h, r, &own_map, &monitors) {
                        Hide::Full => Hide::Render,
                        l => l,
                    };
                    cloak_set(h, level);
                }
            }
            for (h, r) in frame {
                visual.insert(h, r);
            }
            if t_raw >= 1.0 {
                anim = None;
                // アニメーションの終点はモニター縁のすぐ外にクランプされている。
                // 完了後、本来の退避先 (全モニター外) へ不可視のままスナップする
                let settles: Vec<(u64, Rect)> = last_targets
                    .iter()
                    .filter(|(h, r)| visual.get(h) != Some(r))
                    .copied()
                    .collect();
                if !settles.is_empty() {
                    apply_rects(&settles, &borders);
                    for (h, r) in settles {
                        visual.insert(h, r);
                    }
                }
            }
        }

        // cloak モード: アニメーション完了後、自モニターに写らないウィンドウを隠す。
        // active ワークスペースの viewport 外は Alt+Tab に残す (Hide::Render)
        if cfg.cloak && anim.is_none() {
            for &(h, r) in &last_targets {
                let level = hide_level(h, r, &own_map, &monitors);
                cloak_set(h, level);
                if level == Hide::Show && window_cloaked(h as isize) {
                    shell_cloak(h, false);
                }
            }
        }

        // FR-3.8: 非フォーカスウィンドウの半透明化
        if cfg.unfocused_alpha.is_some() || !pinned.is_empty() {
            let f = focused.map(|f| f.0);
            for &(h, _) in &last_targets {
                let alpha = if pinned.contains(&h) {
                    cfg.pinned_alpha
                } else if f == Some(h) || Some(h) == fullscreen {
                    255
                } else {
                    cfg.unfocused_alpha.unwrap_or(255)
                };
                if alpha == 255 {
                    undim_window(h);
                } else {
                    dim_window(h, alpha);
                }
            }
        }

        // FR-3.7: 太枠オーバーレイ
        if cfg.border_thickness > 0 && border_colors(&cfg).is_some() {
            let (fc, uc) = border_colors(&cfg).unwrap();
            let mut items = Vec::new();
            for &(h, _) in &last_targets {
                if Some(h) == fullscreen {
                    continue;
                }
                let Some(r) = visual.get(&h).copied() else {
                    continue;
                };
                // 自モニターの外にいる間は枠も描かない (他モニターへのゴースト防止)
                let visible = match own_map.get(&h) {
                    Some(&mi) => intersects(r, monitors[mi].work),
                    None => intersects_any(r, &monitors),
                };
                if !visible {
                    continue;
                }
                let color = if focused.map(|f| f.0) == Some(h) { fc } else { uc };
                if color >= 0xFFFF_FFFE {
                    continue;
                }
                items.push(border::Item {
                    owner: h,
                    rect: r,
                    color,
                });
            }
            border::update_all(&items, cfg.border_thickness);
        } else {
            border::update_all(&[], 1);
        }
    }

    tracing::info!("shutting down, restoring windows");
    restore_all();
}

/// ターゲットと visual の差分から遷移を開始する。
/// 自モニターに写らない移動は即時反映 (snap) する。
///
/// 退避先は全モニター外の遠い座標だが、そのまま補間すると移動距離が伸びて
/// 速度が上がり、経路が他モニターを横切る。そこでアニメーションの端点は
/// [`clamp_near`] で自モニターの縁のすぐ外に寄せ、旧来の移動距離を保つ
/// (本来の退避先へはアニメ完了後にスナップ)。
///
/// offscreen モード (非 cloak) では縁の外が他モニターに重なる配置
/// (上下配置のワークスペース切替など) を隠す手段がないため、
/// その移動はアニメせず snap してチラつきを避ける。
fn start_transition(
    visual: &mut HashMap<u64, Rect>,
    targets: &[(u64, Rect)],
    own_map: &HashMap<u64, usize>,
    monitors: &[MonitorState],
    borders: &HashMap<u64, Border>,
    cloak: bool,
) -> Option<(Vec<AnimEntry>, Instant)> {
    let mut snaps: Vec<(u64, Rect)> = Vec::new();
    let mut entries: Vec<AnimEntry> = Vec::new();
    for &(hwnd, to) in targets {
        let from = visual.get(&hwnd).copied().unwrap_or(to);
        if from == to {
            continue;
        }
        let Some(&mi) = own_map.get(&hwnd) else {
            entries.push(AnimEntry { hwnd, from, to });
            continue;
        };
        let own = monitors[mi].work;
        let vis_from = intersects(from, own);
        let vis_to = intersects(to, own);
        if !vis_from && !vis_to {
            snaps.push((hwnd, to));
            continue;
        }
        let a = if vis_from { from } else { clamp_near(from, own) };
        let b = if vis_to { to } else { clamp_near(to, own) };
        let crosses_other = |r: Rect| {
            monitors
                .iter()
                .enumerate()
                .any(|(j, ms)| j != mi && intersects(r, ms.monitor))
        };
        if !cloak
            && ((!vis_from && crosses_other(a)) || (!vis_to && crosses_other(b)))
        {
            snaps.push((hwnd, to));
            continue;
        }
        entries.push(AnimEntry { hwnd, from: a, to: b });
    }
    if !snaps.is_empty() {
        apply_rects(&snaps, borders);
        for (h, r) in snaps {
            visual.insert(h, r);
        }
    }
    (!entries.is_empty()).then(|| (entries, Instant::now()))
}

/// 画面外 Rect を own の縁のすぐ外 (+100px) へ寄せる。
/// アニメーションの始点/終点として使い、移動距離をモニター 1 枚ぶんに保つ。
fn clamp_near(r: Rect, own: Rect) -> Rect {
    let mut out = r;
    if r.y >= own.y + own.h {
        out.y = own.y + own.h + 100;
    } else if r.y + r.h <= own.y {
        out.y = own.y - r.h - 100;
    }
    if r.x >= own.x + own.w {
        out.x = own.x + own.w + 100;
    } else if r.x + r.w <= own.x {
        out.x = own.x - r.w - 100;
    }
    out
}

/// hwnd → 所属モニターのインデックス。イベント処理のたびに再構築する。
fn own_monitor_map(monitors: &[MonitorState]) -> HashMap<u64, usize> {
    let mut map = HashMap::new();
    for (i, ms) in monitors.iter().enumerate() {
        for s in &ms.stack.strips {
            for col in &s.columns {
                for t in &col.tiles {
                    map.insert(t.0, i);
                }
            }
        }
    }
    map
}

/// r がいずれかのモニター作業領域と交差するか (= 画面上に見え得るか)。
fn intersects_any(r: Rect, monitors: &[MonitorState]) -> bool {
    monitors.iter().any(|ms| intersects(r, ms.work))
}

/// cloak モードでの隠蔽レベルを決める。
/// 自モニターに写っていれば表示。写っていない場合、active ワークスペースの
/// ウィンドウは描画停止のみ (Alt+Tab に残す)、非 active は完全に隠す。
fn hide_level(
    h: u64,
    r: Rect,
    own_map: &HashMap<u64, usize>,
    monitors: &[MonitorState],
) -> Hide {
    let Some(&mi) = own_map.get(&h) else {
        return Hide::Show;
    };
    let ms = &monitors[mi];
    if intersects(r, ms.work) {
        return Hide::Show;
    }
    if ms.stack.strip_index_of(WindowId(h)) == Some(ms.stack.active) {
        Hide::Render
    } else {
        Hide::Full
    }
}

/// 2 矩形が交差するか。
fn intersects(r: Rect, work: Rect) -> bool {
    r.x < work.x + work.w && r.x + r.w > work.x && r.y < work.y + work.h && r.y + r.h > work.y
}

/// 全モニターのターゲット Rect を結合して返す。
fn targets_all(
    monitors: &[MonitorState],
    fullscreen: Option<u64>,
    gap: i32,
) -> Vec<(u64, Rect)> {
    let bounds = virtual_bounds(monitors);
    monitors
        .iter()
        .flat_map(|ms| {
            targets_for_monitor(&ms.stack, ms.work, ms.monitor, bounds, fullscreen, gap)
        })
        .collect()
}

/// 全モニターの物理領域を結合したバウンディングボックス。
/// 退避座標はこの外に取る — モニターが上下・左右どう配置されても
/// 非 active ワークスペースや offscreen Column が他モニターに写り込まない。
fn virtual_bounds(monitors: &[MonitorState]) -> Rect {
    let mut it = monitors.iter().map(|ms| ms.monitor);
    let Some(first) = it.next() else {
        return Rect { x: 0, y: 0, w: 1920, h: 1080 };
    };
    it.fold(first, |acc, r| {
        let x = acc.x.min(r.x);
        let y = acc.y.min(r.y);
        let right = (acc.x + acc.w).max(r.x + r.w);
        let bottom = (acc.y + acc.h).max(r.y + r.h);
        Rect { x, y, w: right - x, h: bottom - y }
    })
}

/// 1 モニター分のターゲット Rect を計算する。
/// active ワークスペースは通常配置、非 active は上下にずらす。
/// ずらし幅は仮想デスクトップ全体の高さより大きく取り、他モニターと重ねない。
fn targets_for_monitor(
    stack: &Stack,
    work: Rect,
    monitor: Rect,
    bounds: Rect,
    fullscreen: Option<u64>,
    gap: i32,
) -> Vec<(u64, Rect)> {
    let step = bounds.h + work.h + 200;
    let mut out = Vec::new();
    for (i, strip) in stack.strips.iter().enumerate() {
        let dy = (i as i32 - stack.active as i32) * step;
        for (id, p) in strip.project(work, gap) {
            let r = if Some(id.0) == fullscreen && i == stack.active {
                monitor
            } else {
                let base = park(p, work, bounds);
                Rect {
                    y: base.y + dy,
                    ..base
                }
            };
            out.push((id.0, r));
        }
    }
    out
}

/// 新規ウィンドウの管理対象判定と取り込み (FR-1.3)。
#[allow(clippy::too_many_arguments)]
fn try_adopt(
    stack: &mut Stack,
    visual: &mut HashMap<u64, Rect>,
    borders: &mut HashMap<u64, Border>,
    h: isize,
    width: i32,
    gap: i32,
    rules: &[emakiwm_core::filter::Rule],
    focused: Option<WindowId>,
    work: Rect,
) -> bool {
    if stack.contains(WindowId(h as u64)) {
        return false;
    }
    let e = scan::inspect(HWND(h as _), rules);
    if e.decision != Decision::Manage || e.elevated == Some(true) {
        return false;
    }
    if unsafe { IsIconic(HWND(h as _)).as_bool() } {
        return false;
    }
    tracing::info!(
        "adopt {:#x} {} \"{}\"",
        h,
        e.info.exe_name.as_deref().unwrap_or("?"),
        e.info.title
    );
    adopt(stack, visual, borders, &e, width, gap, focused, work);
    true
}

#[allow(clippy::too_many_arguments)]
fn adopt(
    stack: &mut Stack,
    visual: &mut HashMap<u64, Rect>,
    borders: &mut HashMap<u64, Border>,
    e: &scan::ScanEntry,
    width: i32,
    gap: i32,
    focused: Option<WindowId>,
    work: Rect,
) {
    let h = e.hwnd;
    if let Some(m) = RESTORE_RECTS.lock().unwrap().as_mut() {
        m.entry(h).or_insert(clamp_into_work(e.rect, work));
    }
    let frame = e.frame.unwrap_or(e.rect);
    borders.insert(
        h as u64,
        (
            frame.left - e.rect.left,
            frame.top - e.rect.top,
            e.rect.right - frame.right,
            e.rect.bottom - frame.bottom,
        ),
    );
    heal_leftover_alpha(h as u64);
    visual.insert(h as u64, to_rect(frame));
    stack
        .active_mut()
        .insert_column_after(focused, WindowId(h as u64), width, gap);
    stack.normalize();
}

fn to_rect(r: RECT) -> Rect {
    Rect {
        x: r.left,
        y: r.top,
        w: r.right - r.left,
        h: r.bottom - r.top,
    }
}

fn expand(r: Rect, b: Border) -> Rect {
    let (l, t, rt, bm) = b;
    Rect {
        x: r.x - l,
        y: r.y - t,
        w: r.w + l + rt,
        h: r.h + t + bm,
    }
}

fn apply_rects(rects: &[(u64, Rect)], borders: &HashMap<u64, Border>) {
    if rects.is_empty() {
        return;
    }
    unsafe {
        let Ok(mut hdwp) = BeginDeferWindowPos(rects.len() as i32) else {
            tracing::warn!("BeginDeferWindowPos failed");
            return;
        };
        for (h, r) in rects {
            let r = expand(*r, borders.get(h).copied().unwrap_or_default());
            match DeferWindowPos(
                hdwp,
                HWND(*h as _),
                None,
                r.x,
                r.y,
                r.w,
                r.h,
                SWP_NOZORDER | SWP_NOACTIVATE,
            ) {
                Ok(next) => hdwp = next,
                Err(e) => {
                    tracing::warn!("DeferWindowPos failed for {h:#x}: {e}");
                    apply_individually(rects, borders);
                    return;
                }
            }
        }
        if let Err(e) = EndDeferWindowPos(hdwp) {
            tracing::warn!("EndDeferWindowPos failed: {e}, falling back");
            apply_individually(rects, borders);
        }
    }
}

fn apply_individually(rects: &[(u64, Rect)], borders: &HashMap<u64, Border>) {
    for (h, r) in rects {
        let r = expand(*r, borders.get(h).copied().unwrap_or_default());
        unsafe {
            let _ = SetWindowPos(
                HWND(*h as _),
                None,
                r.x,
                r.y,
                r.w,
                r.h,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }
}

fn switch_workspace(stack: &mut Stack, focused: &mut Option<WindowId>, down: bool) {
    if !stack.switch(down) {
        return;
    }
    let strip = stack.active_mut();
    *focused = strip
        .last_focused
        .filter(|f| strip.contains(*f))
        .or_else(|| strip.columns.first().and_then(|c| c.active()));
    if let Some(f) = *focused {
        strip.set_active(f);
        focus_window(f.0 as isize);
    }
}

/// IPC `state` 応答の JSON (FR-7.3)。マルチモニター情報を含む。
fn state_json(
    monitors: &[MonitorState],
    active_monitor: usize,
    focused: Option<WindowId>,
    fullscreen: Option<u64>,
) -> String {
    let monitors_json: Vec<_> = monitors
        .iter()
        .map(|ms| {
            let workspaces: Vec<_> = ms
                .stack
                .strips
                .iter()
                .map(|s| {
                    serde_json::json!({
                        "offset_x": s.offset_x,
                        "columns": s.columns.iter().map(|c| serde_json::json!({
                            "x": c.x,
                            "width": c.width,
                            "tiles": c.tiles.iter().map(|t| serde_json::json!({
                                "hwnd": t.0,
                                "title": scan::window_title(HWND(t.0 as _)),
                            })).collect::<Vec<_>>(),
                        })).collect::<Vec<_>>(),
                    })
                })
                .collect();
            serde_json::json!({
                "active_workspace": ms.stack.active,
                "workspaces": workspaces,
                "work": {
                    "x": ms.work.x, "y": ms.work.y,
                    "w": ms.work.w, "h": ms.work.h,
                },
            })
        })
        .collect();

    // 後方互換: active_monitor の workspaces をトップレベルにも露出
    let act = active_monitor.min(monitors.len().saturating_sub(1));
    let active_workspaces = monitors_json
        .get(act)
        .and_then(|m| m.get("workspaces"))
        .cloned()
        .unwrap_or(serde_json::json!([]));
    let active_ws_idx = monitors.get(act).map(|ms| ms.stack.active).unwrap_or(0);

    serde_json::json!({
        "active_monitor": active_monitor,
        "active_workspace": active_ws_idx,
        "focused": focused.map(|f| f.0),
        "fullscreen": fullscreen,
        "workspaces": active_workspaces,
        "monitors": monitors_json,
    })
    .to_string()
}

fn focus_window(h: isize) {
    // cloak 中のウィンドウを前面化する前に解除する (この時点の位置は
    // 全モニター外の退避先なので、解除しても画面には写らない)
    if cloaked_by_us(h as u64) {
        cloak_set(h as u64, Hide::Show);
    }
    let hwnd = HWND(h as _);
    unsafe {
        if SetForegroundWindow(hwnd).as_bool() {
            return;
        }
        let fg = GetForegroundWindow();
        let fg_tid = GetWindowThreadProcessId(fg, None);
        let our_tid = GetCurrentThreadId();
        if fg_tid != 0 && fg_tid != our_tid {
            let _ = AttachThreadInput(our_tid, fg_tid, true);
            let ok = SetForegroundWindow(hwnd);
            let _ = AttachThreadInput(our_tid, fg_tid, false);
            if ok.as_bool() {
                return;
            }
        }
        tracing::warn!("SetForegroundWindow({h:#x}) failed (foreground lock)");
    }
}

/// Offscreen Column の退避先。仮想デスクトップ全体 (bounds) の外へ出し、
/// 左右に別モニターがある配置でも隣のモニターに写り込まないようにする。
fn park(p: Placement, work: Rect, bounds: Rect) -> Rect {
    match p {
        Placement::Visible(r) => r,
        Placement::Offscreen(r) => {
            let x = if r.x < work.x {
                bounds.x - r.w - 100
            } else {
                bounds.x + bounds.w + 100
            };
            Rect { x, ..r }
        }
    }
}

/// 全物理モニターを列挙し MonitorState を生成する (左→右順)。
fn enumerate_monitors(cfg: &config::Config) -> Vec<MonitorState> {
    let raw = collect_monitors();
    let gap = cfg.gap;
    let (top, right, bottom, left) = cfg.margin;

    let mut states: Vec<MonitorState> = raw
        .into_iter()
        .map(|(hmon, mi)| {
            let mut work = to_rect(mi.rcWork);
            work.x += left;
            work.y += top;
            work.w = (work.w - left - right).max(200);
            work.h = (work.h - top - bottom).max(200);
            let viewport_w = work.w - 2 * gap;
            let default_width = ((viewport_w - gap) as f32 * cfg.default_ratio) as i32;
            MonitorState {
                hmonitor: hmon,
                work,
                monitor: to_rect(mi.rcMonitor),
                viewport_w,
                default_width,
                stack: Stack::default(),
            }
        })
        .collect();

    // 左→右、同じ x なら上→下 (上下配置でも順序が安定するように)
    states.sort_by_key(|m| (m.work.x, m.work.y));

    if states.is_empty() {
        // フォールバック: プライマリモニターのみ
        let (work, monitor) = primary_monitor_fallback();
        let viewport_w = work.w - 2 * gap;
        let default_width = ((viewport_w - gap) as f32 * cfg.default_ratio) as i32;
        states.push(MonitorState {
            hmonitor: 0,
            work,
            monitor,
            viewport_w,
            default_width,
            stack: Stack::default(),
        });
    }

    states
}

/// EnumDisplayMonitors で全モニターの (HMONITOR, MONITORINFO) を収集する。
fn collect_monitors() -> Vec<(isize, MONITORINFO)> {
    let mut result: Vec<(isize, MONITORINFO)> = Vec::new();

    unsafe extern "system" fn enum_cb(
        hmon: HMONITOR,
        _hdc: HDC,
        _rect: *mut RECT,
        data: LPARAM,
    ) -> BOOL {
        let result = &mut *(data.0 as *mut Vec<(isize, MONITORINFO)>);
        let mut mi = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(hmon, &mut mi).as_bool() {
            result.push((hmon.0 as isize, mi));
        }
        true.into()
    }

    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(enum_cb),
            LPARAM(&mut result as *mut _ as isize),
        );
    }
    result
}

/// HWND が属するモニターのインデックスを返す (見つからなければ 0)。
fn monitor_for_window(h: isize, monitors: &[MonitorState]) -> usize {
    let hmon = unsafe { MonitorFromWindow(HWND(h as _), MONITOR_DEFAULTTONEAREST) };
    monitors
        .iter()
        .position(|m| m.hmonitor == hmon.0 as isize)
        .unwrap_or(0)
}

/// WindowId がどのモニターの Stack に含まれるかを返す。
fn monitor_index_of(id: WindowId, monitors: &[MonitorState]) -> Option<usize> {
    monitors.iter().position(|m| m.stack.contains(id))
}

/// プライマリモニターのフォールバック (EnumDisplayMonitors 失敗時)。
fn primary_monitor_fallback() -> (Rect, Rect) {
    unsafe {
        let monitor = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
        let mut mi = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(monitor, &mut mi).as_bool() {
            return (to_rect(mi.rcWork), to_rect(mi.rcMonitor));
        }
    }
    let fallback = Rect { x: 0, y: 0, w: 1920, h: 1080 };
    (fallback, fallback)
}

fn dim_window(h: u64, alpha: u8) {
    {
        let mut set = DIMMED.lock().unwrap_or_else(|p| p.into_inner());
        match set.iter_mut().find(|(x, _)| *x == h) {
            Some((_, a)) if *a == alpha => return,
            Some((_, a)) => *a = alpha,
            None => {
                unsafe {
                    let hwnd = HWND(h as _);
                    let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
                    if ex & WS_EX_LAYERED.0 as isize != 0 {
                        return;
                    }
                    SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex | WS_EX_LAYERED.0 as isize);
                }
                set.push((h, alpha));
            }
        }
    }
    unsafe {
        let _ = SetLayeredWindowAttributes(HWND(h as _), COLORREF(0), alpha, LWA_ALPHA);
    }
}

fn undim_window(h: u64) {
    {
        let mut set = DIMMED.lock().unwrap_or_else(|p| p.into_inner());
        let Some(i) = set.iter().position(|(x, _)| *x == h) else {
            return;
        };
        set.swap_remove(i);
    }
    clear_layered(h);
}

fn undim_all() {
    let drained = std::mem::take(&mut *DIMMED.lock().unwrap_or_else(|p| p.into_inner()));
    for (h, _) in drained {
        clear_layered(h);
    }
}

fn clear_layered(h: u64) {
    unsafe {
        let hwnd = HWND(h as _);
        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA);
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex & !(WS_EX_LAYERED.0 as isize));
    }
}

fn heal_leftover_alpha(h: u64) {
    unsafe {
        let mut alpha = 0u8;
        let mut flags = LWA_ALPHA;
        if GetLayeredWindowAttributes(HWND(h as _), None, Some(&mut alpha), Some(&mut flags))
            .is_ok()
            && flags.contains(LWA_ALPHA)
            && alpha < 255
        {
            tracing::info!("半透明の取り残しを修復: {h:#x} (alpha={alpha})");
            clear_layered(h);
        }
    }
}

const BORDER_DEFAULT: u32 = 0xFFFF_FFFF;

fn border_colors(cfg: &config::Config) -> Option<(u32, u32)> {
    if cfg.border_focused.is_none() && cfg.border_unfocused.is_none() {
        return None;
    }
    Some((
        cfg.border_focused.unwrap_or(BORDER_DEFAULT),
        cfg.border_unfocused.unwrap_or(BORDER_DEFAULT),
    ))
}

fn set_border_color(h: u64, color: u32) {
    unsafe {
        let _ = DwmSetWindowAttribute(
            HWND(h as _),
            DWMWA_BORDER_COLOR,
            &color as *const u32 as *const c_void,
            size_of::<u32>() as u32,
        );
    }
}

fn cloaked_by_us(h: u64) -> bool {
    CLOAKED
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .iter()
        .any(|e| e.hwnd == h)
}

/// 隠蔽レベルを実状態に近づける。
/// 順序重要: TOOLWINDOW 付与中はシェルが view を破棄するので、
/// 隠す = cloak → TOOLWINDOW、戻す = TOOLWINDOW 解除 → uncloak。
/// uncloak の失敗 (view 再作成中の deferred) は pending として登録を維持し、
/// [`retry_pending_uncloaks`] で成功するまで再試行する。
fn cloak_set(h: u64, level: Hide) {
    let mut set = CLOAKED.lock().unwrap_or_else(|p| p.into_inner());
    let idx = set.iter().position(|e| e.hwnd == h);
    {
        let prev = match idx {
            None => "show",
            Some(i) if set[i].tool => "full",
            Some(i) if set[i].pending => "pending",
            Some(_) => "render",
        };
        let want = match level {
            Hide::Show => "show",
            Hide::Render => "render",
            Hide::Full => "full",
        };
        if prev != want {
            tracing::debug!("cloak {h:#x}: {prev} → {want}");
        }
    }
    match level {
        Hide::Show => {
            let Some(i) = idx else { return };
            if set[i].tool {
                set_toolwindow(h, false);
                set[i].tool = false;
            }
            if cloak_render(h, false) {
                set.remove(i);
            } else {
                set[i].pending = true;
            }
        }
        Hide::Render => match idx {
            Some(i) => {
                if set[i].tool {
                    set_toolwindow(h, false);
                    set[i].tool = false;
                }
                set[i].pending = false;
            }
            None => {
                if cloak_render(h, true) {
                    set.push(CloakEntry {
                        hwnd: h,
                        tool: false,
                        pending: false,
                        attempts: 0,
                    });
                }
            }
        },
        Hide::Full => match idx {
            Some(i) => {
                if !set[i].tool {
                    set_toolwindow(h, true);
                    set[i].tool = true;
                }
                set[i].pending = false;
            }
            None => {
                if cloak_render(h, true) {
                    set_toolwindow(h, true);
                    set.push(CloakEntry {
                        hwnd: h,
                        tool: true,
                        pending: false,
                        attempts: 0,
                    });
                }
            }
        },
    }
}

/// deferred になった uncloak を再試行する。戻り値: まだ残っているか。
fn retry_pending_uncloaks() -> bool {
    let mut set = CLOAKED.lock().unwrap_or_else(|p| p.into_inner());
    set.retain_mut(|e| {
        if !e.pending {
            return true;
        }
        if !unsafe { IsWindow(Some(HWND(e.hwnd as _))).as_bool() } {
            return false;
        }
        if cloak_render(e.hwnd, false) {
            return false;
        }
        e.attempts += 1;
        if e.attempts > 250 {
            tracing::warn!("uncloak {:#x} を諦めます ({} 回失敗)", e.hwnd, e.attempts);
            return false;
        }
        true
    });
    set.iter().any(|e| e.pending)
}

fn uncloak_all() {
    let drained = std::mem::take(&mut *CLOAKED.lock().unwrap_or_else(|p| p.into_inner()));
    for e in drained {
        shell_cloak(e.hwnd, false);
    }
}

/// SetCloak (描画停止) のみを切り替える。TOOLWINDOW は触らない。
/// 戻り値: 適用に成功したか。
fn cloak_render(h: u64, on: bool) -> bool {
    match com::set_cloak(h as isize, on) {
        Ok(()) => true,
        Err(e) => {
            if !on && e.code() == TYPE_E_ELEMENTNOTFOUND {
                tracing::debug!("SetCloak(false) deferred for {h:#x} (view 再作成中)");
            } else {
                tracing::warn!("SetCloak({on}) failed for {h:#x}: {e}");
            }
            false
        }
    }
}

pub(crate) fn shell_cloak(h: u64, on: bool) {
    if on {
        cloak_render(h, true);
        set_toolwindow(h, true);
    } else {
        set_toolwindow(h, false);
        cloak_render(h, false);
    }
}

fn set_toolwindow(h: u64, on: bool) {
    unsafe {
        let hwnd = HWND(h as _);
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let tool = emakiwm_core::filter::WS_EX_TOOLWINDOW as isize;
        let new = if on { ex | tool } else { ex & !tool };
        if new != ex {
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, new);
        }
    }
}

fn window_cloaked(h: isize) -> bool {
    let mut cloaked = 0u32;
    unsafe {
        let _ = DwmGetWindowAttribute(
            HWND(h as _),
            DWMWA_CLOAKED,
            &mut cloaked as *mut u32 as *mut c_void,
            size_of::<u32>() as u32,
        );
    }
    cloaked != 0
}

fn clamp_into_work(r: RECT, work: Rect) -> RECT {
    let (w, h) = (r.right - r.left, r.bottom - r.top);
    let as_rect = Rect {
        x: r.left,
        y: r.top,
        w,
        h,
    };
    if intersects(as_rect, work) {
        return r;
    }
    let x = r.left.clamp(work.x, (work.x + work.w - w).max(work.x));
    let y = r.top.clamp(work.y, (work.y + work.h - h).max(work.y));
    RECT {
        left: x,
        top: y,
        right: x + w,
        bottom: y + h,
    }
}

pub fn uncloak_all_leftovers() {
    let mut count = 0;
    for e in scan::scan(&[]) {
        if !e.info.is_visible {
            continue;
        }
        let exe = e.info.exe_name.as_deref().unwrap_or("");
        if e.info.is_cloaked {
            if exe.eq_ignore_ascii_case("ApplicationFrameHost.exe")
                || e.info.class_name == "Windows.UI.Core.CoreWindow"
                || e.info.class_name == "ApplicationFrameWindow"
            {
                continue;
            }
            shell_cloak(e.hwnd as u64, false);
            heal_leftover_alpha(e.hwnd as u64);
            println!("uncloak {:#x} {} \"{}\"", e.hwnd, exe, e.info.title);
            count += 1;
        } else if e.decision == Decision::Manage {
            heal_leftover_alpha(e.hwnd as u64);
        }
    }
    println!("--- {count} windows uncloaked");
}

fn state_path() -> PathBuf {
    let base = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".into());
    PathBuf::from(base).join("emakiwm").join("state.json")
}

fn persist_restore_rects() {
    let entries: Vec<serde_json::Value> = {
        let guard = RESTORE_RECTS.lock().unwrap_or_else(|p| p.into_inner());
        let Some(map) = guard.as_ref() else { return };
        map.iter()
            .map(|(h, r)| {
                serde_json::json!({
                    "hwnd": h,
                    "left": r.left, "top": r.top, "right": r.right, "bottom": r.bottom,
                })
            })
            .collect()
    };
    let path = state_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = std::fs::write(&path, serde_json::Value::Array(entries).to_string()) {
        tracing::warn!("{} の書き込みに失敗: {e}", path.display());
    }
}

pub fn restore_from_disk() {
    let path = state_path();
    let data = match std::fs::read_to_string(&path) {
        Ok(d) => d,
        Err(_) => {
            tracing::info!("{} がありません (復元の必要なし)", path.display());
            return;
        }
    };
    let Ok(entries) = serde_json::from_str::<Vec<serde_json::Value>>(&data) else {
        tracing::error!("{} を解析できません", path.display());
        return;
    };
    let (work, _) = primary_monitor_fallback();
    let mut restored = 0;
    for e in &entries {
        let (Some(h), Some(l), Some(t), Some(r), Some(b)) = (
            e["hwnd"].as_i64(),
            e["left"].as_i64(),
            e["top"].as_i64(),
            e["right"].as_i64(),
            e["bottom"].as_i64(),
        ) else {
            continue;
        };
        let hwnd = HWND(h as isize as _);
        if !unsafe { IsWindow(Some(hwnd)).as_bool() } {
            continue;
        }
        shell_cloak(h as u64, false);
        set_border_color(h as u64, BORDER_DEFAULT);
        clear_layered(h as u64);
        let rect = clamp_into_work(
            RECT {
                left: l as i32,
                top: t as i32,
                right: r as i32,
                bottom: b as i32,
            },
            work,
        );
        unsafe {
            let _ = SetWindowPos(
                hwnd,
                None,
                rect.left,
                rect.top,
                rect.right - rect.left,
                rect.bottom - rect.top,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
        restored += 1;
    }
    let _ = std::fs::remove_file(&path);
    tracing::info!("{restored} / {} ウィンドウを復元しました", entries.len());
}

fn install_restore_hooks() {
    *RESTORE_RECTS.lock().unwrap() = Some(HashMap::new());

    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_all();
        default_hook(info);
    }));

    unsafe extern "system" fn ctrl_handler(_ctrl_type: u32) -> BOOL {
        if let Some(tx) = events::sender() {
            let _ = tx.send(WmEvent::Shutdown);
        }
        restore_all();
        true.into()
    }
    unsafe {
        SetConsoleCtrlHandler(Some(ctrl_handler), true).expect("SetConsoleCtrlHandler failed");
    }
}

fn restore_all() {
    tray::remove();
    uncloak_all();
    undim_all();
    let mut guard = RESTORE_RECTS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(map) = guard.take() else {
        return;
    };
    let (work, _) = primary_monitor_fallback();
    for (h, r) in &map {
        unsafe {
            if !IsWindow(Some(HWND(*h as _))).as_bool() {
                continue;
            }
            set_border_color(*h as u64, BORDER_DEFAULT);
            let r = clamp_into_work(*r, work);
            let _ = SetWindowPos(
                HWND(*h as _),
                None,
                r.left,
                r.top,
                r.right - r.left,
                r.bottom - r.top,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }
    let _ = std::fs::remove_file(state_path());
    tracing::info!("restored {} windows", map.len());
}

fn shell_spawn(cmd: &str) {
    let mut parts = cmd.split_whitespace();
    let Some(prog) = parts.next() else { return };
    let args: String = parts.collect::<Vec<_>>().join(" ");

    let prog_w: Vec<u16> = prog.encode_utf16().chain(std::iter::once(0)).collect();
    let args_w: Vec<u16> = args.encode_utf16().chain(std::iter::once(0)).collect();

    let result = unsafe {
        ShellExecuteW(
            None,
            windows::core::PCWSTR(std::ptr::null()),
            windows::core::PCWSTR(prog_w.as_ptr()),
            if args.is_empty() {
                windows::core::PCWSTR(std::ptr::null())
            } else {
                windows::core::PCWSTR(args_w.as_ptr())
            },
            windows::core::PCWSTR(std::ptr::null()),
            SW_SHOWDEFAULT,
        )
    };

    if result.0 as isize > 32 {
        tracing::info!("spawn: {cmd}");
    } else {
        tracing::warn!(
            "spawn \"{cmd}\" failed (ShellExecuteW={})",
            result.0 as isize
        );
    }
}
