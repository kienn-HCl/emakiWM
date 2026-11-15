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
//! 役割分担: 論理状態とイベントに対する状態遷移は `emakiwm_core::state::State`
//! (純粋ロジック) が持つ。このモジュールは Win32 への問い合わせと副作用
//! (配置・前面化・cloak・枠・永続化) だけを担う。
//!
//! 状態の持ち方: `State` は常に最終ターゲット (論理状態) を保持し、
//! 画面上の現在位置は `visual` マップが持つ。アニメーションは visual → 論理状態の
//! 射影 (targets) への補間。イベントで論理状態が変われば、進行中のアニメは
//! 現在の visual から新ターゲットへ向け直される (§8-8)。

use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use emakiwm_core::anim::{ease_out_cubic, lerp_rect};
use emakiwm_core::filter::Decision;
use emakiwm_core::layout::{nudge_rect, pick_in_direction, FocusDir, Rect, WindowId};
use emakiwm_core::overview::{
    clip_crop, hit_test, intersect, overview_float, overview_scene, Hit, OverviewScene,
    OverviewTile,
};
use emakiwm_core::state::{Monitor, MonitorGeom, State};
use windows::core::BOOL;
use windows::Win32::Foundation::{
    GetLastError, COLORREF, ERROR_ALREADY_EXISTS, HWND, LPARAM, POINT, RECT,
    TYPE_E_ELEMENTNOTFOUND, WPARAM,
};
use windows::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_CLOAKED,
    DWMWA_EXTENDED_FRAME_BOUNDS,
};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, MonitorFromPoint, MonitorFromWindow, HDC, HMONITOR,
    MONITORINFO, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY,
};
use windows::Win32::System::Console::SetConsoleCtrlHandler;
use windows::Win32::System::Threading::{AttachThreadInput, CreateMutexW, GetCurrentThreadId};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{
    BeginDeferWindowPos, DeferWindowPos, EndDeferWindowPos, GetForegroundWindow,
    GetLayeredWindowAttributes, GetWindowLongPtrW, GetWindowRect, GetWindowThreadProcessId,
    IsIconic, IsWindow, IsWindowVisible, IsZoomed, PostMessageW, SetForegroundWindow,
    SetLayeredWindowAttributes, SetWindowLongPtrW, SetWindowPos, GWL_EXSTYLE, HWND_NOTOPMOST,
    HWND_TOPMOST, LWA_ALPHA, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SW_SHOWDEFAULT,
    WM_CLOSE, WS_EX_LAYERED, WS_EX_TOPMOST,
};

use crate::events::{self, WmEvent};
use crate::{border, com, config, ipc, overview, scan, tray, ws};

/// アニメーション中のフレーム間隔。
const FRAME: Duration = Duration::from_millis(8);

/// 実フレーム幅と要求幅の差がこれ以下なら同じとみなす (px)。丸め誤差の吸収用。
const SIZE_TOLERANCE: i32 = 2;

/// 1 回のレイアウト変化につき実サイズを読み戻す回数の上限と、再確認の間隔 (FR-3.6)。
/// 配置完了後に実サイズを読み戻すタイミング (FR-3.6)。完了直後に 1 回確認し、
/// その後は前回からこの間隔 (ms) で再確認する。要求サイズを一旦受け入れてから
/// 遅れて広がり直すアプリ (Firefox 等) を拾うため。
const SIZE_RECHECK_MS: [u64; 4] = [40, 80, 180, 400];

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

/// toggle-floating でフロート化した全ウィンドウと、WM が最前面 (TOPMOST) を付けたか。
/// タイルへ戻すとき・終了時に最前面指定を外し (WM が付けたものだけ)、枠色を戻す。
/// フロート窓は RESTORE_RECTS から外れるため、終了時の後始末はこの記録で行う。
static FLOATS: Mutex<Vec<(u64, bool)>> = Mutex::new(Vec::new());

/// 1 ウィンドウぶんの補間区間。
struct AnimEntry {
    hwnd: u64,
    from: Rect,
    to: Rect,
}

/// 不可視フレーム (§8-2): GetWindowRect と DWMWA_EXTENDED_FRAME_BOUNDS の差
/// (left, top, right, bottom)。論理 Rect (見た目のフレーム) ⇔ ウィンドウ Rect の変換に使う。
type Border = (i32, i32, i32, i32);

/// Overview (FR-4.11) の開閉アニメーション。`Some` の間は Overview ウィンドウが
/// 出ている (開き切った静止状態を含む)。`t` は 0 (通常) .. 1 (縮小表示)。
struct OverviewAnim {
    from: f32,
    to: f32,
    started: Instant,
    /// 補間中か。false なら `to` で静止していて、メインループはポーリングしない
    running: bool,
    /// Overview ウィンドウを表示済みか。最初のサムネイル登録を終えてから出すため
    shown: bool,
}

impl OverviewAnim {
    fn opening() -> Self {
        OverviewAnim {
            from: 0.0,
            to: 1.0,
            started: Instant::now(),
            running: true,
            shown: false,
        }
    }

    /// 現在の補間位置。duration が 0 なら即座に終点。
    fn t(&self, dur: Duration) -> f32 {
        if !self.running {
            return self.to;
        }
        let p = if dur.is_zero() {
            1.0
        } else {
            (self.started.elapsed().as_secs_f32() / dur.as_secs_f32()).min(1.0)
        };
        self.from + (self.to - self.from) * p
    }

    /// 開く/閉じる向きへ切り替える。途中からでも現在位置から折り返す。
    fn retarget(&mut self, open: bool, dur: Duration) {
        let to = if open { 1.0 } else { 0.0 };
        if self.to == to {
            return;
        }
        self.from = self.t(dur);
        self.to = to;
        self.started = Instant::now();
        self.running = true;
    }

    fn is_closing(&self) -> bool {
        self.to == 0.0
    }
}

/// Overview 1 フレームぶんの描画内容。前回送ったものと比べて差分があるときだけ反映する。
#[derive(PartialEq)]
struct OvFrame {
    items: Vec<overview::Item>,
    rows: Vec<(Rect, bool)>,
    highlight: Option<Rect>,
    ring: u32,
}

/// 全モニターの `monitor` 領域の外接矩形 (Overview ウィンドウの位置)。
fn monitors_bounds(monitors: &[Monitor]) -> Rect {
    let mut it = monitors.iter().map(|m| m.monitor);
    let first = it.next().unwrap_or_default();
    it.fold(first, |a, r| {
        let (x, y) = (a.x.min(r.x), a.y.min(r.y));
        let (right, bottom) = ((a.x + a.w).max(r.x + r.w), (a.y + a.h).max(r.y + r.h));
        Rect {
            x,
            y,
            w: right - x,
            h: bottom - y,
        }
    })
}

/// Overview の 1 フレームを組み立てる。タイル → フロートの順 (後ろほど手前)。
/// フロート窓は動き得るので、毎回実際のフレームを問い合わせて縮小する。
/// 返す `OverviewScene` / フロート一覧はクリックの当たり判定に使う。
fn build_overview(
    state: &State,
    cfg: &config::Config,
    zoom: f32,
) -> (OvFrame, OverviewScene, Vec<OverviewTile>) {
    let scene = overview_scene(state, zoom);
    let mut floats = Vec::new();
    for fl in state.floats() {
        let hwnd = HWND(fl.0 as _);
        if unsafe { !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() } {
            continue;
        }
        if let Some(frame) = frame_rect(fl.0 as isize) {
            floats.push(overview_float(state, fl, frame, zoom));
        }
    }
    let all = || scene.tiles.iter().chain(floats.iter());
    let items = all()
        .filter_map(|t| {
            clip_crop(t.rect, t.clip).map(|(rect, crop)| overview::Item {
                hwnd: t.id.0,
                rect,
                crop,
            })
        })
        .collect();
    let highlight = state
        .highlighted()
        .and_then(|id| all().find(|t| t.id == id))
        .and_then(|t| intersect(t.rect, t.clip));
    let ring = border_colors(cfg)
        .map(|c| c.0)
        .filter(|c| *c < 0xFFFF_FFFE)
        .unwrap_or(overview::DEFAULT_RING);
    let frame = OvFrame {
        items,
        rows: scene.rows.iter().map(|r| (r.rect, r.active)).collect(),
        highlight,
        ring,
    };
    (frame, scene, floats)
}

pub fn run() {
    // 多重起動の防止: 2 つの WM が同じウィンドウを取り合うと配置が壊れる。
    // ハンドルは閉じず、プロセス終了時に OS が解放する
    unsafe {
        let mutex = CreateMutexW(None, false, windows::core::w!("Local\\emakiwm-instance"));
        if mutex.is_err() || GetLastError() == ERROR_ALREADY_EXISTS {
            tracing::error!("emakiwm は既に起動しています");
            return;
        }
    }

    install_restore_hooks();

    let mut cfg = config::load();
    events::set_mouse_scroll_focus(cfg.mouse_scroll_focus);

    let (tx, rx) = mpsc::channel::<WmEvent>();
    events::spawn_hook_thread(tx.clone(), input_binds(&cfg));
    if let Some(port) = cfg.ws_port {
        ws::spawn_ws_thread(port, tx.clone());
    }
    ipc::spawn_ipc_thread(tx);

    let mut geoms = monitor_geoms(&cfg);
    if geoms.is_empty() {
        // 列挙失敗時のフォールバック: プライマリモニターのみ
        let (work, monitor) = primary_monitor_fallback();
        geoms.push(MonitorGeom {
            hmonitor: 0,
            work,
            monitor,
        });
    }
    let mut state = State::new(&geoms, cfg.gap, cfg.default_ratio);
    tracing::info!(
        "monitors: {}",
        state
            .monitors
            .iter()
            .enumerate()
            .map(|(i, m)| format!(
                "[{}] ({},{}) {}x{}",
                i, m.work.x, m.work.y, m.work.w, m.work.h
            ))
            .collect::<Vec<_>>()
            .join(", ")
    );

    // Core (emakiwm_core::state) へ渡す単調時刻 (ms) の起点
    let epoch = Instant::now();
    let now_ms = || epoch.elapsed().as_millis() as u64;

    let mut visual: HashMap<u64, Rect> = HashMap::new();
    let mut borders: HashMap<u64, Border> = HashMap::new();
    let mut subscribers: Vec<mpsc::Sender<String>> = Vec::new();
    let mut last_state = String::new();
    let mut bordered: Option<u64> = None;
    // マウスで移動・リサイズ中のフロート窓 (太枠オーバーレイが遅れるので隠す)
    let mut dragging: Option<u64> = None;
    let mut pinned: Vec<u64> = Vec::new();
    // FR-3.6 の実サイズ読み戻し: 現在のレイアウト変化について残り何回確認するかと、
    // 次に確認する時刻 (None = 予定なし)
    let mut size_checks_done = 0usize;
    let mut size_check_at: Option<Instant> = None;
    // 読み戻しで判明したウィンドウごとの最小幅 (FR-3.6)
    let mut min_widths: HashMap<u64, i32> = HashMap::new();
    // 今回のレイアウト変化で、要求より広かったために配置し直したウィンドウ
    let mut reapplied: HashSet<u64> = HashSet::new();
    // Overview (FR-4.11): 開閉状態、前回 Overview へ送った内容、クリック判定用の直近シーン
    let mut ov: Option<OverviewAnim> = None;
    let mut ov_sent: Option<OvFrame> = None;
    let mut ov_hit: Option<(OverviewScene, Vec<OverviewTile>)> = None;

    // FR-1.1: 起動時スキャン。モニターごとに左端 x 順に並べてタイル化する
    let (all_initial, initial_others): (Vec<scan::ScanEntry>, Vec<scan::ScanEntry>) =
        scan::scan(&cfg.rules).into_iter().partition(|e| {
            e.decision == Decision::Manage
                && e.elevated != Some(true)
                && unsafe { !IsIconic(HWND(e.hwnd as _)).as_bool() }
        });
    let all_initial = {
        let mut v = all_initial;
        // (monitor_index, rect.left) でソートして各モニター内の左→右順を保つ
        v.sort_by_key(|e| (monitor_for_window(e.hwnd, &state.monitors), e.rect.left));
        v
    };
    for e in &all_initial {
        let mi = monitor_for_window(e.hwnd, &state.monitors);
        register_window(&mut visual, &mut borders, e, state.monitors[mi].work);
        state.adopt_initial(mi, WindowId(e.hwnd as u64));
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

    // 起動時点で既にあるダイアログ・ルールの float 窓も装飾対象にする
    sync_passive_floats(&mut state, &cfg, &initial_others);
    persist_restore_rects();

    for cmd in &cfg.startup {
        shell_spawn(cmd);
    }

    state.set_initial_focus(WindowId(unsafe { GetForegroundWindow() }.0 as u64));

    tracing::info!(
        "managing {} windows on {} monitor(s). Ctrl+C / Alt+Shift+E で終了",
        state.window_count(),
        state.monitors.len()
    );

    let mut own_map = own_monitor_map(&state.monitors);
    let mut last_targets = targets(&state);
    let mut anim = start_transition(
        &mut visual,
        &last_targets,
        &own_map,
        &state.monitors,
        &borders,
        cfg.cloak,
    );

    'main: loop {
        // deferred になった uncloak の再試行。残っている間はフレーム間隔で回す
        let pending_uncloak = retry_pending_uncloaks();
        // 起きる必要がある時刻まで待つ。何もなければイベントが来るまでブロックする
        let ov_running = ov.as_ref().is_some_and(|o| o.running);
        let timeout = if anim.is_some() || pending_uncloak || ov_running {
            Some(FRAME)
        } else {
            size_check_at.map(|t| t.saturating_duration_since(Instant::now()))
        };
        let ev = match timeout {
            Some(d) => match rx.recv_timeout(d) {
                Ok(ev) => Some(ev),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => break,
            },
            None => match rx.recv() {
                Ok(ev) => Some(ev),
                Err(_) => break,
            },
        };

        if let Some(ev) = ev {
            match &ev {
                WmEvent::Query(_) | WmEvent::Subscribe(_) => {}
                e => tracing::debug!("event: {e:?}"),
            }
            // 論理状態が変わらなくても定位置へ配置し直す (手動で動かされた場合)
            let mut resnap = false;
            // 状態遷移は Core (State) が行い、前面化すべきウィンドウを返す
            let focus_request: Option<WindowId> = match ev {
                WmEvent::Appeared(h) | WmEvent::MinimizeEnd(h) => {
                    let id = WindowId(h as u64);
                    // どこかの Stack が既に管理中なら何もしない。物理位置ベースで
                    // adopt すると、退避・アニメ中に他モニター上で UNCLOAKED/SHOW が
                    // 発火したとき別モニターの Stack へ二重登録されてしまう
                    if state.monitor_index_of(id).is_some() || state.is_floating(id) {
                        None
                    } else {
                        let (e, manageable) = inspect_manageable(h, &cfg.rules);
                        match manageable {
                            true => {
                                let work = state.monitors[state.active_monitor].work;
                                register_window(&mut visual, &mut borders, &e, work);
                                let focus = state.adopt(id);
                                persist_restore_rects();
                                if let Some(c) = border_colors(&cfg) {
                                    set_border_color(h as u64, c.1);
                                }
                                focus
                            }
                            false => {
                                // タイルにしない窓がダイアログ等のフロートなら装飾対象にする
                                if register_passive_float(&mut state, &cfg, &e) {
                                    persist_restore_rects();
                                }
                                None
                            }
                        }
                    }
                }
                WmEvent::Cloaked(h) if cloaked_by_us(h as u64) || !window_cloaked(h) => None,
                WmEvent::Gone(h) | WmEvent::MinimizeStart(h) | WmEvent::Cloaked(h) => {
                    match state.remove(WindowId(h as u64), now_ms()) {
                        Some(removal) => {
                            if removal.rolled_back {
                                tracing::debug!("remove {h:#x}: 直前のモニター跨ぎ追従を巻き戻す");
                            }
                            tracing::info!("remove {h:#x}");
                            visual.remove(&(h as u64));
                            borders.remove(&(h as u64));
                            cloak_set(h as u64, Hide::Show);
                            undim_window(h as u64);
                            pinned.retain(|&x| x != h as u64);
                            min_widths.remove(&(h as u64));
                            if !unsafe { IsWindow(Some(HWND(h as _))).as_bool() } {
                                if let Some(m) = RESTORE_RECTS.lock().unwrap().as_mut() {
                                    m.remove(&h);
                                }
                            }
                            persist_restore_rects();
                            removal.focus
                        }
                        None => {
                            // フロート化した窓が破棄されたら記録を消す (最小化では消さない)
                            let id = WindowId(h as u64);
                            if state.is_floating(id)
                                && !unsafe { IsWindow(Some(HWND(h as _))).as_bool() }
                            {
                                state.forget_floating(id);
                                forget_float_record(h as u64);
                                undim_window(h as u64);
                                if dragging == Some(h as u64) {
                                    dragging = None;
                                }
                                persist_restore_rects();
                            } else if state.passive_floats.contains(&id) {
                                // 受動フロート窓は破棄・非表示・最小化・別デスクトップ行きの
                                // いずれでも登録を外す (再表示されれば Appeared で再登録)
                                unregister_passive_float(&mut state, h as u64);
                                if dragging == Some(h as u64) {
                                    dragging = None;
                                }
                                persist_restore_rects();
                            }
                            None
                        }
                    }
                }
                WmEvent::Foreground(h) => {
                    let id = WindowId(h as u64);
                    // WM が付けた最前面指定をアプリが落としていたら付け直す。
                    // Overview 中は付け直さない (フロート窓が Overview より手前に出てしまう)
                    if ov.is_none() {
                        reassert_float_topmost();
                    }
                    // 管理外のフロート窓 (ダイアログ・ルールや手動でフロートにした窓) が
                    // 前面になったら、focus-floating の戻り先として覚える
                    if state.monitor_index_of(id).is_none() {
                        if state.is_float(id) {
                            state.note_float_focus(id);
                        } else if !HWND(h as _).is_invalid() {
                            // 同じ窓を二重に inspect しないよう 1 回の結果を共用する
                            let e = scan::inspect(HWND(h as _), &cfg.rules);
                            if tileable_ok(&e) {
                                state.note_float_focus(id);
                            }
                            if register_passive_float(&mut state, &cfg, &e) {
                                persist_restore_rects();
                            }
                        }
                    }
                    // フロート窓が前面なら、そちらを「フォーカスされた見た目」にする
                    state.set_float_foreground(state.is_float(id).then_some(id));
                    let focused_dead = state.focused.is_some_and(|f| unsafe {
                        let hw = HWND(f.0 as _);
                        !IsWindow(Some(hw)).as_bool() || IsIconic(hw).as_bool()
                    });
                    let reassert = state.foreground(WindowId(h as u64), focused_dead, now_ms());
                    if let Some(succ) = reassert {
                        tracing::debug!(
                            "foreground {h:#x}: 閉じ直後の stale フォールバックを無視、後継 {:#x} を再主張",
                            succ.0
                        );
                    }
                    reassert
                }
                WmEvent::Focus(dir) => {
                    // フロート窓が前面のとき: 左右は方向にいちばん近いフロート窓へ
                    // フォーカスする (なければ何もしない)。上下はワークスペースを切り替え、
                    // フォーカスはフロート窓に残す (フロート窓を手前に置いたまま下の
                    // タイルを入れ替えられる)
                    let fg = unsafe { GetForegroundWindow() };
                    if !state.is_float(WindowId(fg.0 as u64)) {
                        state.focus(dir)
                    } else if matches!(dir, FocusDir::Left | FocusDir::Right) {
                        focus_float_in_direction(fg, dir, &state)
                    } else {
                        state.switch_workspace(dir == FocusDir::Down);
                        None
                    }
                }
                WmEvent::MoveColumn(dir) => {
                    // フロート窓が前面なら、レイアウトではなくその窓を左右へ動かす
                    let fg = unsafe { GetForegroundWindow() };
                    if state.is_float(WindowId(fg.0 as u64)) {
                        nudge_float(fg, dir, &state);
                    } else {
                        state.move_column(dir);
                    }
                    None
                }
                WmEvent::Expel => {
                    state.expel();
                    None
                }
                WmEvent::Consume => {
                    state.consume();
                    None
                }
                WmEvent::MoveToWorkspace(down) => {
                    // フロート窓が前面なら、ワークスペース移動ではなくその窓を上下へ動かす
                    let fg = unsafe { GetForegroundWindow() };
                    if state.is_float(WindowId(fg.0 as u64)) {
                        let dir = if down { FocusDir::Down } else { FocusDir::Up };
                        nudge_float(fg, dir, &state);
                    } else {
                        state.move_to_workspace(down);
                    }
                    None
                }
                WmEvent::SwitchWorkspace(down) => state.switch_workspace(down),
                WmEvent::CycleWidth => {
                    state.cycle_width();
                    None
                }
                WmEvent::MaximizeColumn => {
                    state.maximize_column();
                    None
                }
                WmEvent::Fullscreen => {
                    state.toggle_fullscreen();
                    None
                }
                WmEvent::Scroll(forward) => {
                    state.scroll(forward);
                    None
                }
                // ユーザーによる手動移動・リサイズの完了 (FR-1.2)。リサイズ後の幅は
                // Column 幅として採用し、位置はタイルの定位置へ戻す
                WmEvent::MoveSizeStart(h) => {
                    if state.is_float(WindowId(h as u64)) {
                        dragging = Some(h as u64);
                    }
                    None
                }
                WmEvent::MoveSizeEnd(h) => {
                    let id = WindowId(h as u64);
                    if dragging == Some(h as u64) {
                        dragging = None;
                    }
                    if state.monitor_index_of(id).is_some() {
                        if let Some(actual) = frame_rect(h) {
                            let target_w = last_targets
                                .iter()
                                .find(|(t, _)| *t == h as u64)
                                .map(|(_, r)| r.w);
                            if state.fullscreen != Some(id)
                                && target_w.is_some_and(|w| (actual.w - w).abs() > SIZE_TOLERANCE)
                            {
                                state.resize_column(id, actual.w);
                            }
                            // 手動リサイズの結果は最小幅の記録より信頼できる
                            min_widths.remove(&(h as u64));
                            visual.insert(h as u64, actual);
                            resnap = true;
                        }
                    }
                    None
                }
                WmEvent::CloseFocused => {
                    if let Some(h) = close_target(&state) {
                        unsafe {
                            let _ =
                                PostMessageW(Some(HWND(h as _)), WM_CLOSE, WPARAM(0), LPARAM(0));
                        }
                    }
                    None
                }
                WmEvent::Spawn(cmd) => {
                    shell_spawn(&cmd);
                    None
                }
                // フローティングの切替: 前面がタイルならレイアウトから外し、
                // 管理外のウィンドウならタイルとして取り込む
                WmEvent::ToggleFloating => {
                    let fg = unsafe { GetForegroundWindow() };
                    let h = fg.0 as isize;
                    let id = WindowId(h as u64);
                    if let Some(mi) = state.monitor_index_of(id) {
                        let work = state.monitors[mi].work;
                        state.float(id, now_ms());
                        visual.remove(&(h as u64));
                        borders.remove(&(h as u64));
                        cloak_set(h as u64, Hide::Show);
                        undim_window(h as u64);
                        pinned.retain(|&x| x != h as u64);
                        min_widths.remove(&(h as u64));
                        float_window(h, work);
                        persist_restore_rects();
                        tracing::info!("float {h:#x}");
                        None
                    } else if let Some(e) = inspect_tileable(h) {
                        tracing::info!("tile {h:#x} \"{}\"", e.info.title);
                        unfloat_window(h);
                        let work = state.monitors[state.active_monitor].work;
                        register_window(&mut visual, &mut borders, &e, work);
                        let focus = state.unfloat(id);
                        persist_restore_rects();
                        if let Some(c) = border_colors(&cfg) {
                            set_border_color(h as u64, c.1);
                        }
                        // 枠色の追従に再評価させる (フォーカス中なら下で focused 色になる)
                        if bordered == Some(h as u64) {
                            bordered = None;
                        }
                        if dragging == Some(h as u64) {
                            dragging = None;
                        }
                        focus
                    } else {
                        None
                    }
                }
                // フローティングとタイルの間でフォーカスを行き来する
                WmEvent::FocusFloating => {
                    let fg = WindowId(unsafe { GetForegroundWindow() }.0 as u64);
                    let on_tile = state.monitor_index_of(fg).is_some();
                    state
                        .focus_toggle_candidates(on_tile)
                        .into_iter()
                        .find(|c| unsafe {
                            let hw = HWND(c.0 as _);
                            IsWindowVisible(hw).as_bool() && !IsIconic(hw).as_bool()
                        })
                }
                WmEvent::ToggleOpacity => {
                    if let Some(f) = state.focused {
                        if let Some(i) = pinned.iter().position(|&x| x == f.0) {
                            pinned.swap_remove(i);
                        } else {
                            pinned.push(f.0);
                        }
                    }
                    None
                }
                WmEvent::FocusMonitor(forward) => state.focus_monitor(forward),
                WmEvent::MoveToMonitor(forward) => state.move_to_monitor(forward),
                WmEvent::ToggleOverview => {
                    match ov.as_mut() {
                        // 開いている/開く途中なら閉じ始める。閉じる途中なら開き直す
                        Some(o) => o.retarget(o.is_closing(), cfg.anim),
                        None => {
                            overview::place(monitors_bounds(&state.monitors));
                            ov = Some(OverviewAnim::opening());
                        }
                    }
                    None
                }
                WmEvent::OverviewClose => {
                    if let Some(o) = ov.as_mut() {
                        o.retarget(false, cfg.anim);
                    }
                    None
                }
                WmEvent::OverviewClick(x, y) => {
                    if let Some(o) = ov.as_mut().filter(|o| !o.is_closing()) {
                        let hit = ov_hit
                            .as_ref()
                            .map_or(Hit::None, |(scene, floats)| hit_test(scene, floats, x, y));
                        // 選択結果は State へ反映するだけ。実際のフォーカスは閉じ終わりに適用する
                        match hit {
                            Hit::Float(id) => {
                                state.note_float_focus(id);
                                state.set_float_foreground(Some(id));
                            }
                            Hit::Tile(id) => {
                                state.focus_window_by_id(id);
                            }
                            Hit::Row { monitor, workspace } => {
                                state.activate_workspace(monitor, workspace);
                            }
                            Hit::None => {}
                        }
                        o.retarget(false, cfg.anim);
                    }
                    None
                }
                WmEvent::Reload => {
                    cfg = config::load();
                    events::set_mouse_scroll_focus(cfg.mouse_scroll_focus);
                    events::rebind(input_binds(&cfg));
                    state.reconfigure(&monitor_geoms(&cfg), cfg.gap, cfg.default_ratio);
                    if !cfg.cloak {
                        uncloak_all();
                    }
                    undim_all();
                    let unfocused = border_colors(&cfg).map_or(BORDER_DEFAULT, |c| c.1);
                    for h in own_monitor_map(&state.monitors).into_keys() {
                        set_border_color(h, unfocused);
                    }
                    for f in state.floats() {
                        set_border_color(f.0, unfocused);
                    }
                    // ルールが変わり得るので受動フロート窓を再評価する
                    // (外れた窓は装飾を戻し、新たに該当する窓を登録)
                    if sync_passive_floats(&mut state, &cfg, &scan::scan(&cfg.rules)) {
                        persist_restore_rects();
                    }
                    bordered = None;
                    tracing::info!(
                        "config reloaded: gap={} anim={:?} rules={} cloak={}",
                        cfg.gap,
                        cfg.anim,
                        cfg.rules.len(),
                        cfg.cloak
                    );
                    None
                }
                WmEvent::WorkAreaChanged => {
                    // WM_SETTINGCHANGE はノイズが多いので、構成が変わったときだけログする
                    let before = state.monitors.len();
                    state.reconfigure(&monitor_geoms(&cfg), cfg.gap, cfg.default_ratio);
                    if state.monitors.len() != before {
                        tracing::info!("work area changed: {} monitor(s)", state.monitors.len());
                    }
                    None
                }
                WmEvent::Query(reply) => {
                    let _ = reply.send(state_json(&state, ov.is_some()));
                    None
                }
                WmEvent::Subscribe(s) => {
                    last_state = state_json(&state, ov.is_some());
                    if s.send(last_state.clone()).is_ok() {
                        subscribers.push(s);
                    }
                    None
                }
                WmEvent::Shutdown => break 'main,
            };
            // Overview 中は前面化しない (隠れたアプリへキーボードフォーカスが飛び、
            // Esc / Enter が Overview に届かなくなる)。閉じ終わりに highlighted() へ適用する
            if let Some(f) = focus_request.filter(|_| ov.is_none()) {
                focus_window(f.0 as isize);
            }

            own_map = own_monitor_map(&state.monitors);
            // 既知の最小幅を下回る Column は配置前に広げておく (重なりを一瞬も見せない)
            for (&h, &w) in &min_widths {
                state.grow_column(WindowId(h), w);
            }
            let new_targets = targets(&state);
            if new_targets != last_targets || resnap {
                size_checks_done = 0;
                reapplied.clear();
                size_check_at = None;
                last_targets = new_targets;
                anim = start_transition(
                    &mut visual,
                    &last_targets,
                    &own_map,
                    &state.monitors,
                    &borders,
                    cfg.cloak,
                );
            }

            if let Some((focused_color, unfocused_color)) = border_colors(&cfg) {
                let cur = state.highlighted().map(|f| f.0);
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
                let s = state_json(&state, ov.is_some());
                if s != last_state {
                    last_state = s;
                    subscribers.retain(|sub| sub.send(last_state.clone()).is_ok());
                }
            }
        }

        let fullscreen = state.fullscreen.map(|f| f.0);

        // フレーム描画
        let mut settled = false;
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
                    let level = match hide_level(h, r, &own_map, &state.monitors) {
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
                settled = true;
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

        // FR-3.6: 最小サイズ制約で要求より広くなったウィンドウは、実幅を Column 幅へ
        // 読み戻して隣との重なりを解消する。配置完了時に加え、要求サイズを一旦
        // 受け入れてから遅れて広がり直すアプリのために少し後にも確認する。
        // 確認は 1 回のレイアウト変化につき SIZE_RECHECK_MS.len() + 1 回まで (ループ防止)
        let due = anim.is_none() && size_check_at.is_some_and(|t| Instant::now() >= t);
        if (settled || due) && size_checks_done <= SIZE_RECHECK_MS.len() {
            size_check_at = SIZE_RECHECK_MS
                .get(size_checks_done)
                .map(|ms| Instant::now() + Duration::from_millis(*ms));
            size_checks_done += 1;
            if read_back_sizes(
                &mut state,
                &last_targets,
                &own_map,
                &mut min_widths,
                &mut borders,
                &mut reapplied,
            ) {
                last_targets = targets(&state);
                anim = start_transition(
                    &mut visual,
                    &last_targets,
                    &own_map,
                    &state.monitors,
                    &borders,
                    cfg.cloak,
                );
            }
        }

        // cloak モード: アニメーション完了後、自モニターに写らないウィンドウを隠す。
        // active ワークスペースの viewport 外は Alt+Tab に残す (Hide::Render)
        if cfg.cloak && anim.is_none() {
            for &(h, r) in &last_targets {
                let level = hide_level(h, r, &own_map, &state.monitors);
                cloak_set(h, level);
                if level == Hide::Show && window_cloaked(h as isize) {
                    shell_cloak(h, false);
                }
            }
        }

        // FR-3.8: 非フォーカスウィンドウの半透明化
        if cfg.unfocused_alpha.is_some() || !pinned.is_empty() {
            let f = state.highlighted().map(|f| f.0);
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
            // フロート窓もタイルと同じく、フォーカス外なら半透明にする
            for fl in state.floats() {
                if f == Some(fl.0) {
                    undim_window(fl.0);
                } else {
                    match cfg.unfocused_alpha {
                        Some(a) if a != 255 => dim_window(fl.0, a),
                        _ => undim_window(fl.0),
                    }
                }
            }
        }

        // Overview (FR-4.11): 開閉アニメーションを進め、シーンが変わったときだけ
        // サムネイルを更新する。枠オーバーレイより先に処理する
        // (閉じ終わった同じ周回で枠を描き戻すため)
        if let Some(o) = ov.as_mut() {
            let t = o.t(cfg.anim);
            if o.running && o.started.elapsed() >= cfg.anim {
                o.running = false;
                o.from = o.to;
            }
            if !o.running && o.is_closing() {
                overview::hide();
                ov = None;
                ov_sent = None;
                ov_hit = None;
                tracing::debug!("overview closed");
                // 保留していたフォーカスを適用してキーボードフォーカスを返す
                if let Some(f) = state.highlighted() {
                    focus_window(f.0 as isize);
                }
                if !subscribers.is_empty() {
                    let s = state_json(&state, false);
                    if s != last_state {
                        last_state = s;
                        subscribers.retain(|sub| sub.send(last_state.clone()).is_ok());
                    }
                }
            } else {
                let zoom = 1.0 + (cfg.overview_zoom - 1.0) * ease_out_cubic(t);
                let (frame, scene, floats) = build_overview(&state, &cfg, zoom);
                ov_hit = Some((scene, floats));
                if ov_sent.as_ref() != Some(&frame) {
                    overview::update(&frame.items, &frame.rows, frame.highlight, frame.ring);
                    ov_sent = Some(frame);
                }
                if !o.shown {
                    o.shown = true;
                    overview::show();
                    tracing::debug!("overview opened");
                }
            }
        }

        // FR-3.7: 太枠オーバーレイ
        if ov.is_none() && cfg.border_thickness > 0 && border_colors(&cfg).is_some() {
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
                    Some(&mi) => intersects(r, state.monitors[mi].work),
                    None => intersects_any(r, &state.monitors),
                };
                if !visible {
                    continue;
                }
                let color = if state.highlighted().map(|f| f.0) == Some(h) {
                    fc
                } else {
                    uc
                };
                if color >= 0xFFFF_FFFE {
                    continue;
                }
                items.push(border::Item {
                    owner: h,
                    rect: r,
                    color,
                });
            }
            // フロート窓: 自由に動くので毎回実際のフレームを問い合わせる。
            // ドラッグ中は枠が遅れるため出さない (MOVESIZEEND で復帰)
            for fl in state.floats() {
                let h = fl.0;
                if dragging == Some(h) {
                    continue;
                }
                let hwnd = HWND(h as _);
                if unsafe { !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() } {
                    continue;
                }
                let Some(r) = frame_rect(h as isize) else {
                    continue;
                };
                let color = if state.highlighted() == Some(fl) {
                    fc
                } else {
                    uc
                };
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
    overview::hide();
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
    monitors: &[Monitor],
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
        let a = if vis_from {
            from
        } else {
            clamp_near(from, own)
        };
        let b = if vis_to { to } else { clamp_near(to, own) };
        let crosses_other = |r: Rect| {
            monitors
                .iter()
                .enumerate()
                .any(|(j, ms)| j != mi && intersects(r, ms.monitor))
        };
        if !cloak && ((!vis_from && crosses_other(a)) || (!vis_to && crosses_other(b))) {
            snaps.push((hwnd, to));
            continue;
        }
        entries.push(AnimEntry {
            hwnd,
            from: a,
            to: b,
        });
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
fn own_monitor_map(monitors: &[Monitor]) -> HashMap<u64, usize> {
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
fn intersects_any(r: Rect, monitors: &[Monitor]) -> bool {
    monitors.iter().any(|ms| intersects(r, ms.work))
}

/// cloak モードでの隠蔽レベルを決める。
/// 自モニターに写っていれば表示。写っていない場合、active ワークスペースの
/// ウィンドウは描画停止のみ (Alt+Tab に残す)、非 active は完全に隠す。
fn hide_level(h: u64, r: Rect, own_map: &HashMap<u64, usize>, monitors: &[Monitor]) -> Hide {
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

/// Core のターゲット Rect を HWND 値キーで返す。
fn targets(state: &State) -> Vec<(u64, Rect)> {
    state
        .targets()
        .into_iter()
        .map(|(id, r)| (id.0, r))
        .collect()
}

/// 新規ウィンドウが管理対象かを判定する (FR-1.3)。対象なら true。
/// 受動フロート判定に再利用できるよう、スキャン結果も一緒に返す。
fn inspect_manageable(h: isize, rules: &[emakiwm_core::filter::Rule]) -> (scan::ScanEntry, bool) {
    let e = scan::inspect(HWND(h as _), rules);
    if e.decision != Decision::Manage || e.elevated == Some(true) {
        return (e, false);
    }
    if unsafe { IsIconic(HWND(h as _)).as_bool() } {
        return (e, false);
    }
    tracing::info!(
        "adopt {:#x} {} \"{}\"",
        h,
        e.info.exe_name.as_deref().unwrap_or("?"),
        e.info.title
    );
    (e, true)
}

/// 受動的フロート窓 (ダイアログ・ルールの float 等、WM が浮かせたわけではない窓) として
/// 装飾・キー移動の対象にするか。ルール判定が Float で、可視・自プロセス外・
/// キャプション付き・ツールウィンドウでなく、最小化されていないもの。
fn passive_float_candidate(e: &scan::ScanEntry) -> bool {
    let caption = emakiwm_core::filter::WS_CAPTION;
    matches!(e.decision, Decision::Float(_))
        && e.info.is_visible
        && !e.info.is_own_process
        && e.info.style & caption == caption
        && e.info.ex_style & emakiwm_core::filter::WS_EX_TOOLWINDOW == 0
        && e.elevated != Some(true)
        && unsafe { !IsIconic(HWND(e.hwnd as _)).as_bool() }
}

/// 受動的フロート窓として未登録なら登録する (枠色・終了時の後始末の記録を含む)。
/// 位置・Z オーダーには触らない。登録したら true。
fn register_passive_float(state: &mut State, cfg: &config::Config, e: &scan::ScanEntry) -> bool {
    let id = WindowId(e.hwnd as u64);
    if state.is_float(id) || state.monitor_index_of(id).is_some() || !passive_float_candidate(e) {
        return false;
    }
    state.add_passive_float(id);
    // タイルの下へ沈まないよう最前面にする。位置・大きさは変えない。
    // 元から最前面だった窓は、終了時に外さないよう「WM が付けた」記録にしない
    let hwnd = HWND(e.hwnd as _);
    let we_set_topmost =
        unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } & WS_EX_TOPMOST.0 as isize == 0;
    if we_set_topmost {
        set_topmost(e.hwnd as u64);
    }
    record_float(e.hwnd as u64, we_set_topmost);
    if let Some(c) = border_colors(cfg) {
        set_border_color(e.hwnd as u64, c.1);
    }
    // 開いた直後の窓は FOREGROUND イベントが可視化前に届き、フロートとして
    // 認識できないまま取り逃がす。登録時点で前面ならここでフォーカス扱いにする
    if unsafe { GetForegroundWindow() } == hwnd {
        state.note_float_focus(id);
        state.set_float_foreground(Some(id));
    }
    tracing::info!(
        "passive float {:#x} {} \"{}\"",
        e.hwnd,
        e.info.exe_name.as_deref().unwrap_or("?"),
        e.info.title
    );
    true
}

/// 受動的フロート窓の登録を外し、装飾 (半透明・枠色) と後始末の記録を戻す。
fn unregister_passive_float(state: &mut State, h: u64) {
    state.forget_floating(WindowId(h));
    undim_window(h);
    set_border_color(h, BORDER_DEFAULT);
    // 記録を消し、WM が付けた最前面指定を外す
    unfloat_window(h as isize);
}

/// 受動的フロート窓の集合をスキャン結果へ合わせる (起動時・設定の再読み込み時)。
/// もう該当しない窓は登録を外し、新たに該当する窓を登録する。変化があれば true。
fn sync_passive_floats(
    state: &mut State,
    cfg: &config::Config,
    scanned: &[scan::ScanEntry],
) -> bool {
    let mut changed = false;
    for id in state.passive_floats.clone() {
        let still = scanned
            .iter()
            .any(|e| e.hwnd as u64 == id.0 && passive_float_candidate(e));
        if !still {
            unregister_passive_float(state, id.0);
            changed = true;
        }
    }
    for e in scanned {
        changed |= register_passive_float(state, cfg, e);
    }
    changed
}

/// フロート窓の記録 (終了時の後始末用)。記録済みなら何もしない。
fn record_float(h: u64, topmost: bool) {
    let mut floats = FLOATS.lock().unwrap_or_else(|p| p.into_inner());
    if !floats.iter().any(|&(x, _)| x == h) {
        floats.push((h, topmost));
    }
}

/// WM が最前面にしたフロート窓から TOPMOST が外れていたら付け直す。
/// アプリが自分で最前面指定を落とすことがあるため、前面窓が変わるたびに確認する。
fn reassert_float_topmost() {
    let hs: Vec<u64> = FLOATS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .iter()
        .filter(|&&(_, topmost)| topmost)
        .map(|&(h, _)| h)
        .collect();
    for h in hs {
        unsafe {
            let hwnd = HWND(h as _);
            if !IsWindow(Some(hwnd)).as_bool() {
                continue;
            }
            if GetWindowLongPtrW(hwnd, GWL_EXSTYLE) & WS_EX_TOPMOST.0 as isize != 0 {
                continue;
            }
            tracing::info!("{h:#x}: 最前面指定が外れていたため付け直す");
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }
    }
}

/// 取り込むウィンドウの Win32 側の付帯情報を記録する
/// (復元 Rect・不可視フレーム・現在位置)。論理状態への挿入は Core が行う。
fn register_window(
    visual: &mut HashMap<u64, Rect>,
    borders: &mut HashMap<u64, Border>,
    e: &scan::ScanEntry,
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

/// `close` コマンドの対象。前面ウィンドウが管理外のフロート (ダイアログ等) なら
/// それを閉じる — `focused` は管理ウィンドウしか追わないため、そのまま使うと
/// 見えているダイアログではなく背後のタイルを閉じてしまう。
/// キャプションを持たない前面窓 (デスクトップ・タスクバー等のシェル) は対象外。
fn close_target(state: &State) -> Option<isize> {
    let focused = state.focused;
    let fg = unsafe { GetForegroundWindow() };
    let fg_id = WindowId(fg.0 as u64);
    if !fg.is_invalid() && focused != Some(fg_id) {
        if state.monitor_index_of(fg_id).is_some() {
            return Some(fg.0 as isize);
        }
        let e = scan::inspect(fg, &[]);
        let caption = emakiwm_core::filter::WS_CAPTION;
        if !e.info.is_own_process && e.info.is_visible && e.info.style & caption == caption {
            return Some(fg.0 as isize);
        }
    }
    focused.map(|f| f.0 as isize)
}

/// IPC `state` 応答の JSON (FR-7.3)。マルチモニター情報を含む。
fn state_json(state: &State, overview: bool) -> String {
    let monitors = &state.monitors;
    let active_monitor = state.active_monitor;
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
        "focused": state.focused.map(|f| f.0),
        "fullscreen": state.fullscreen.map(|f| f.0),
        "overview": overview,
        // 動作中の WM のバージョン (exe を入れ替えたのに古いプロセスが残っていないかの確認用)
        "version": env!("CARGO_PKG_VERSION"),
        "floating": state.floating.iter().map(|f| f.0).collect::<Vec<_>>(),
        "passive_floats": state.passive_floats.iter().map(|f| f.0).collect::<Vec<_>>(),
        "workspaces": active_workspaces,
        "monitors": monitors_json,
    })
    .to_string()
}

pub(crate) fn focus_window(h: isize) {
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

/// 全物理モニターの領域を列挙する。作業領域には設定の margin を適用する。
/// 列挙に失敗した場合は空を返す。
fn monitor_geoms(cfg: &config::Config) -> Vec<MonitorGeom> {
    let (top, right, bottom, left) = cfg.margin;
    collect_monitors()
        .into_iter()
        .map(|(hmonitor, mi)| {
            let mut work = to_rect(mi.rcWork);
            work.x += left;
            work.y += top;
            work.w = (work.w - left - right).max(200);
            work.h = (work.h - top - bottom).max(200);
            MonitorGeom {
                hmonitor,
                work,
                monitor: to_rect(mi.rcMonitor),
            }
        })
        .collect()
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
fn monitor_for_window(h: isize, monitors: &[Monitor]) -> usize {
    let hmon = unsafe { MonitorFromWindow(HWND(h as _), MONITOR_DEFAULTTONEAREST) };
    monitors
        .iter()
        .position(|m| m.hmonitor == hmon.0 as isize)
        .unwrap_or(0)
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
    let fallback = Rect {
        x: 0,
        y: 0,
        w: 1920,
        h: 1080,
    };
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

/// 配置後の実フレームを目標と突き合わせる (FR-3.6, §8-6)。
///
/// - ずれていたら不可視フレームを測り直して配置し直す。DPI の異なるモニターへ
///   移した直後は、アプリが WM_DPICHANGED に応じて自分でリサイズしてこちらの
///   配置を上書きするため (不可視フレームの太さも DPI で変わる)
/// - 配置し直しても要求より広いままなら最小サイズ制約とみなし、Column 幅を
///   実幅へ広げて `min_widths` に記録する
///
/// `reapplied` は今回のレイアウト変化で「広すぎる」ために配置し直したウィンドウ。
/// 戻り値: 広げた Column があるか。
fn read_back_sizes(
    state: &mut State,
    targets: &[(u64, Rect)],
    own_map: &HashMap<u64, usize>,
    min_widths: &mut HashMap<u64, i32>,
    borders: &mut HashMap<u64, Border>,
    reapplied: &mut HashSet<u64>,
) -> bool {
    let off = |a: i32, b: i32| (a - b).abs() > SIZE_TOLERANCE;
    let mut grown = false;
    let mut retry: Vec<(u64, Rect)> = Vec::new();
    for &(h, r) in targets {
        if state.fullscreen == Some(WindowId(h))
            || hide_level(h, r, own_map, &state.monitors) != Hide::Show
            || unsafe { IsZoomed(HWND(h as _)).as_bool() }
        {
            continue;
        }
        let Some(actual) = frame_rect(h as isize) else {
            continue;
        };
        let wider = actual.w > r.w + SIZE_TOLERANCE;
        // 一度配置し直しても広いまま → 最小サイズ制約
        if wider && !reapplied.insert(h) {
            if state.grow_column(WindowId(h), actual.w) {
                tracing::info!(
                    "{h:#x}: 最小サイズ制約により列幅を {} → {} へ",
                    r.w,
                    actual.w
                );
                grown = true;
                min_widths.insert(h, actual.w);
            }
            continue;
        }
        if off(actual.x, r.x) || off(actual.y, r.y) || off(actual.w, r.w) || off(actual.h, r.h) {
            tracing::debug!("{h:#x}: 実フレーム {actual:?} が目標 {r:?} と違うため配置し直す");
            if let Some(b) = measure_border(h as isize) {
                borders.insert(h, b);
            }
            retry.push((h, r));
        }
    }
    apply_rects(&retry, borders);
    grown
}

/// 不可視フレーム (§8-2) を現在の状態から測る。DPI が変わると太さも変わる。
pub(crate) fn measure_border(h: isize) -> Option<Border> {
    let frame = frame_rect(h)?;
    let mut rect = RECT::default();
    unsafe { GetWindowRect(HWND(h as _), &mut rect).ok()? };
    Some((
        frame.x - rect.left,
        frame.y - rect.top,
        rect.right - (frame.x + frame.w),
        rect.bottom - (frame.y + frame.h),
    ))
}

/// 見た目のフレーム (DWMWA_EXTENDED_FRAME_BOUNDS) の現在位置。
fn frame_rect(h: isize) -> Option<Rect> {
    let mut frame = RECT::default();
    unsafe {
        DwmGetWindowAttribute(
            HWND(h as _),
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut frame as *mut RECT as *mut c_void,
            size_of::<RECT>() as u32,
        )
        .ok()?;
    }
    Some(to_rect(frame))
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
    let mut entries: Vec<serde_json::Value> = {
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
    // フロート窓。kill 後の --restore で枠色・半透明・WM が付けた最前面指定を戻すために残す
    for &(h, topmost) in FLOATS.lock().unwrap_or_else(|p| p.into_inner()).iter() {
        entries.push(serde_json::json!({ "hwnd": h, "floating": true, "topmost": topmost }));
    }
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
        // フロート窓: 位置は動かさず、枠色・半透明・WM が付けた最前面指定だけ戻す
        if e["floating"].as_bool() == Some(true) {
            if let Some(h) = e["hwnd"].as_u64() {
                if unsafe { IsWindow(Some(HWND(h as _))).as_bool() } {
                    clear_layered(h);
                    set_border_color(h, BORDER_DEFAULT);
                    if e["topmost"].as_bool() == Some(true) {
                        clear_topmost(h);
                    }
                }
            }
            continue;
        }
        // 旧形式: 最前面指定だけの記録
        if e["topmost"].as_bool() == Some(true) {
            if let Some(h) = e["hwnd"].as_u64() {
                clear_topmost(h);
            }
            continue;
        }
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
        // 復元後は状態を失っているので動き続けない。IPC 等の補助スレッドの
        // panic でも同じ (ウィンドウだけ戻して WM が居座るのを防ぐ)
        std::process::exit(101);
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
    reset_all_floats();
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
    let Some((prog, args)) = emakiwm_core::cmdline::split_program(cmd) else {
        tracing::warn!("spawn \"{cmd}\" を解釈できません (引用符が閉じていない等)");
        return;
    };

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

/// 設定から入力割当 (キーバインド・ジェスチャ・Alt+ホイール) を取り出す。
fn input_binds(cfg: &config::Config) -> events::InputBinds {
    events::InputBinds {
        hotkeys: cfg.hotkeys.clone(),
        gestures: cfg.gestures.clone(),
        mouse_scroll_focus: cfg.mouse_scroll_focus,
    }
}

/// 管理外の前面ウィンドウがフロート窓として扱えるか (focus-floating の戻り先、
/// toggle-floating の取り込み対象)。可視・トップレベル・キャプション付きのもの。
/// 設定ルールは見ない (ルールでフロートにした窓も手動でタイルへ入れられる)。
fn inspect_tileable(h: isize) -> Option<scan::ScanEntry> {
    let hwnd = HWND(h as _);
    if hwnd.is_invalid() {
        return None;
    }
    let e = scan::inspect(hwnd, &[]);
    tileable_ok(&e).then_some(e)
}

/// [`inspect_tileable`] の判定部分 (スキャン結果の使い回し用)。
fn tileable_ok(e: &scan::ScanEntry) -> bool {
    let caption = emakiwm_core::filter::WS_CAPTION;
    !matches!(e.decision, Decision::Ignore(_))
        && e.info.style & caption == caption
        && e.elevated != Some(true)
        && unsafe { !IsIconic(HWND(e.hwnd as _)).as_bool() }
}

/// タイルから外したウィンドウをフロートとして置き直す: 元の大きさ (作業領域の
/// 8 割まで) で作業領域の中央へ、タイルの上に常に見えるよう最前面にする。
/// 以後は WM が位置を管理しないので、復元 Rect の記録も手放す。
fn float_window(h: isize, work: Rect) {
    let hwnd = HWND(h as _);
    let original = RESTORE_RECTS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .as_mut()
        .and_then(|m| m.remove(&h));
    let (w, ht) = match original {
        Some(r) => (r.right - r.left, r.bottom - r.top),
        None => (work.w / 2, work.h / 2),
    };
    let w = w.min(work.w * 8 / 10);
    let ht = ht.min(work.h * 8 / 10);
    unsafe {
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let we_set_topmost = ex & WS_EX_TOPMOST.0 as isize == 0;
        let mut floats = FLOATS.lock().unwrap_or_else(|p| p.into_inner());
        floats.retain(|&(x, _)| x != h as u64);
        floats.push((h as u64, we_set_topmost));
        drop(floats);
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            work.x + (work.w - w) / 2,
            work.y + (work.h - ht) / 2,
            w,
            ht,
            SWP_NOACTIVATE,
        );
    }
}

/// フロートの記録を消し、フロート化のときに付けた最前面指定を外す
/// (元から最前面だった窓には触らない)。
fn unfloat_window(h: isize) {
    let mut floats = FLOATS.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(i) = floats.iter().position(|&(x, _)| x == h as u64) {
        let (_, topmost) = floats.swap_remove(i);
        // 破棄済みの窓には触らない
        if topmost && unsafe { IsWindow(Some(HWND(h as _))).as_bool() } {
            clear_topmost(h as u64);
        }
    }
}

/// 破棄されたフロート窓の記録だけを消す (窓がないので何も戻さない)。
fn forget_float_record(h: u64) {
    FLOATS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .retain(|&(x, _)| x != h);
}

/// 前面のフロート窓から見て `dir` 側にいちばん近い (可視・非最小化の) フロート窓を返す。
fn focus_float_in_direction(fg: HWND, dir: FocusDir, state: &State) -> Option<WindowId> {
    let from = frame_rect(fg.0 as isize)?;
    let cands: Vec<(WindowId, Rect)> = state
        .floats()
        .filter(|f| f.0 != fg.0 as u64)
        .filter_map(|f| {
            let hwnd = HWND(f.0 as _);
            if unsafe { !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() } {
                return None;
            }
            frame_rect(f.0 as isize).map(|r| (f, r))
        })
        .collect();
    pick_in_direction(from, dir, &cands)
}

/// フロート窓を方向キーで 1 ステップ動かす。作業領域の 1/20 ずつ、
/// 移動後も全モニターの作業領域のいずれかと交差する場合だけ適用する
/// (別モニターへは渡れるが、完全に画面外へは出さない)。
fn nudge_float(hwnd: HWND, dir: FocusDir, state: &State) {
    let mut rc = RECT::default();
    if unsafe { GetWindowRect(hwnd, &mut rc) }.is_err() {
        return;
    }
    let r = to_rect(rc);
    let (cx, cy) = (r.x + r.w / 2, r.y + r.h / 2);
    let work = state
        .monitors
        .iter()
        .map(|m| m.work)
        .find(|w| cx >= w.x && cx < w.x + w.w && cy >= w.y && cy < w.y + w.h)
        .unwrap_or(state.monitors[state.active_monitor].work);
    let moved = nudge_rect(r, dir, work);
    if !intersects_any(moved, &state.monitors) {
        return;
    }
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            None,
            moved.x,
            moved.y,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

/// 位置・大きさ・アクティブ状態を変えずに最前面にする。
fn set_topmost(h: u64) {
    unsafe {
        let _ = SetWindowPos(
            HWND(h as _),
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
}

fn clear_topmost(h: u64) {
    unsafe {
        let _ = SetWindowPos(
            HWND(h as _),
            Some(HWND_NOTOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
}

/// 終了・レスキュー時: 記録した全フロート窓の枠色を戻し、WM が付けた最前面指定を外す。
fn reset_all_floats() {
    let drained = std::mem::take(&mut *FLOATS.lock().unwrap_or_else(|p| p.into_inner()));
    for (h, topmost) in drained {
        set_border_color(h, BORDER_DEFAULT);
        if topmost {
            clear_topmost(h);
        }
    }
}
