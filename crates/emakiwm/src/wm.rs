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
//! (配置・前面化・枠・永続化) だけを担う。
//!
//! モジュール構成: メインループの状態は [`Runtime`] へまとめた。副作用の種類ごとに
//! `floats` (フロート窓)・`decor` (枠色・半透明・太枠)・`cloak` (隠蔽)・
//! `restore` (終了時復元)・`ovctl` (Overview 駆動) へ分けてある。
//!
//! 状態の持ち方: `State` は常に最終ターゲット (論理状態) を保持し、
//! 画面上の現在位置は `visual` マップが持つ。アニメーションは visual → 論理状態の
//! 射影 (targets) への補間。イベントで論理状態が変われば、進行中のアニメは
//! 現在の visual から新ターゲットへ向け直される (§8-8)。
//!
//! メインループ 1 周の副作用の順序 (この順序は挙動の一部なので変えないこと):
//!
//! 1. `wait_timeout`: deferred な uncloak の再試行 → 待ち時間の決定 → イベント待ち
//! 2. イベントがあれば `handle_event` (状態遷移と Win32 副作用) の後に `after_event`:
//!    前面化 (Overview 中は保留) → own_map 再構築 → 既知の最小幅で Column を広げる →
//!    ターゲット再計算・遷移開始 → 枠色の追従 → 購読者への状態配信
//! 3. `tick` (イベントの有無によらず毎周):
//!    フレーム描画 (補間・cloak Render・完了時のスナップ) → 実サイズの読み戻し →
//!    静止後の cloak 判定 → 半透明化 → Overview の進行 (閉じ終わりなら前面化と配信) →
//!    太枠オーバーレイ
//!
//! `restore_all` は panic フック・Ctrl ハンドラ (別スレッド) から呼ばれる。Runtime には
//! 触らず、グローバルな static だけを参照する (`restore` モジュール参照)。

use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::ops::ControlFlow;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use emakiwm_core::anim::{ease_out_cubic, lerp_rect};
use emakiwm_core::filter::Decision;
use emakiwm_core::layout::{FocusDir, Rect, WindowId};
use emakiwm_core::state::{FloatGone, Monitor, MonitorGeom, State};
use windows::core::BOOL;
use windows::Win32::Foundation::{
    GetLastError, ERROR_ALREADY_EXISTS, HWND, LPARAM, POINT, RECT, WPARAM,
};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, MonitorFromPoint, MonitorFromWindow, HDC, HMONITOR,
    MONITORINFO, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY,
};
use windows::Win32::System::Threading::{AttachThreadInput, CreateMutexW, GetCurrentThreadId};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{
    BeginDeferWindowPos, DeferWindowPos, EndDeferWindowPos, GetForegroundWindow, GetWindowRect,
    GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible, IsZoomed, PostMessageW,
    SetForegroundWindow, SetWindowPos, SWP_NOACTIVATE, SWP_NOZORDER, SW_SHOWDEFAULT, WM_CLOSE,
};

use crate::cloak::{
    cloak_set, cloaked_by_us, hide_level, retry_pending_uncloaks, shell_cloak, uncloak_all,
    window_cloaked, Hide,
};
use crate::decor::{
    self, border_colors, heal_leftover_alpha, set_border_color, undim_all, undim_window,
    BORDER_DEFAULT,
};
use crate::events::{self, WmEvent};
use crate::floats::{
    float_window, focus_float_in_direction, foreground_float, forget_float_record,
    inspect_tileable, nudge_float, reassert_float_topmost, register_passive_float,
    release_passive_float, sync_passive_floats, unfloat_window,
};
use crate::ovctl::OvCtl;
use crate::restore::{
    clamp_into_work, install_restore_hooks, persist_restore_rects, restore_all, RESTORE_RECTS,
};
use crate::{config, ipc, overview, scan, ws};

/// アニメーション中のフレーム間隔。
const FRAME: Duration = Duration::from_millis(8);

/// 実フレーム幅と要求幅の差がこれ以下なら同じとみなす (px)。丸め誤差の吸収用。
const SIZE_TOLERANCE: i32 = 2;

/// 1 回のレイアウト変化につき実サイズを読み戻す回数の上限と、再確認の間隔 (FR-3.6)。
/// 配置完了後に実サイズを読み戻すタイミング (FR-3.6)。完了直後に 1 回確認し、
/// その後は前回からこの間隔 (ms) で再確認する。要求サイズを一旦受け入れてから
/// 遅れて広がり直すアプリ (Firefox 等) を拾うため。
const SIZE_RECHECK_MS: [u64; 4] = [40, 80, 180, 400];

/// ディスプレイ構成・作業領域の変更後に、実ウィンドウの位置を目標と突き合わせ直す間隔 (ms、
/// 前回からの間隔)。モニターを接続すると Windows 自身が「モニターの接続に基づいて
/// ウィンドウの位置を記憶する」機能でウィンドウを遅れて動かすことがあり、論理状態とは
/// 別のモニターに実ウィンドウが置かれたままになるため。
const DISPLAY_RESYNC_MS: [u64; 5] = [300, 700, 1500, 3000, 5000];

/// 1 ウィンドウぶんの補間区間。
struct AnimEntry {
    hwnd: u64,
    from: Rect,
    to: Rect,
}

/// 不可視フレーム (§8-2): GetWindowRect と DWMWA_EXTENDED_FRAME_BOUNDS の差
/// (left, top, right, bottom)。論理 Rect (見た目のフレーム) ⇔ ウィンドウ Rect の変換に使う。
type Border = (i32, i32, i32, i32);

/// `handle_event` の結果。`after_event` へ渡す。
struct Outcome {
    /// 前面化すべきウィンドウ (Core が決める)
    focus: Option<WindowId>,
    /// 論理状態が変わらなくても定位置へ配置し直す (手動で動かされた場合)
    resnap: bool,
}

/// メインループの可変状態。
struct Runtime {
    cfg: config::Config,
    state: State,
    /// Core (`emakiwm_core::state`) へ渡す単調時刻 (ms) の起点
    epoch: Instant,
    visual: HashMap<u64, Rect>,
    borders: HashMap<u64, Border>,
    subscribers: Vec<mpsc::Sender<String>>,
    last_state: String,
    /// フォーカス色を付けている窓
    bordered: Option<u64>,
    /// マウスで移動・リサイズ中のフロート窓 (太枠オーバーレイが遅れるので隠す)
    dragging: Option<u64>,
    pinned: Vec<u64>,
    /// FR-3.6 の実サイズ読み戻し: 現在のレイアウト変化について残り何回確認するかと、
    /// 次に確認する時刻 (None = 予定なし)
    size_checks_done: usize,
    size_check_at: Option<Instant>,
    /// ディスプレイ変更後の位置の突き合わせ: 済んだ回数と次の予定 (None = 予定なし)
    display_resyncs_done: usize,
    display_resync_at: Option<Instant>,
    /// 読み戻しで判明したウィンドウごとの最小幅 (FR-3.6)。キーは測ったときのモニター
    /// (HMONITOR) と DPI。最小幅は DPI に比例するので、別のモニターへ移った・スケール設定が
    /// 変わった記録は使わない (移動先で必要なら読み戻しが測り直す)。モニターは論理状態から
    /// 取るので、move-to-monitor の直後 (実ウィンドウがまだ動いておらず DPI が旧値) でも捨てられる
    min_widths: HashMap<u64, MinWidth>,
    /// 今回のレイアウト変化で、要求より広かったために配置し直したウィンドウ
    reapplied: HashSet<u64>,
    /// Overview (FR-4.11): 開閉状態、前回 Overview へ送った内容、クリック判定用の直近シーン
    ov: OvCtl,
    own_map: HashMap<u64, usize>,
    last_targets: Vec<(u64, Rect)>,
    anim: Option<(Vec<AnimEntry>, Instant)>,
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

    let cfg = config::load();
    events::set_mouse_scroll_focus(cfg.mouse_scroll_focus);

    let (tx, rx) = mpsc::channel::<WmEvent>();
    events::spawn_hook_thread(tx.clone(), input_binds(&cfg));
    if let Some(port) = cfg.ws_port {
        ws::spawn_ws_thread(port, tx.clone());
    }
    ipc::spawn_ipc_thread(tx);

    let mut rt = Runtime::start(cfg);

    loop {
        // 起きる必要がある時刻まで待つ。何もなければイベントが来るまでブロックする
        let ev = match rt.wait_timeout() {
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
            match rt.handle_event(ev) {
                ControlFlow::Continue(outcome) => rt.after_event(outcome),
                ControlFlow::Break(()) => break,
            }
        }

        rt.tick();
    }

    tracing::info!("shutting down, restoring windows");
    overview::hide();
    restore_all();
}

impl Runtime {
    /// 起動時スキャン (FR-1.1) で既存ウィンドウを取り込み、初期ターゲットへの遷移を始める。
    fn start(cfg: config::Config) -> Runtime {
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

        let epoch = Instant::now();
        let mut visual: HashMap<u64, Rect> = HashMap::new();
        let mut borders: HashMap<u64, Border> = HashMap::new();

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

        let own_map = own_monitor_map(&state.monitors);
        let last_targets = targets(&state);
        let anim = start_transition(
            &mut visual,
            &last_targets,
            &own_map,
            &state.monitors,
            &borders,
            cfg.cloak,
        );

        Runtime {
            cfg,
            state,
            epoch,
            visual,
            borders,
            subscribers: Vec::new(),
            last_state: String::new(),
            bordered: None,
            dragging: None,
            pinned: Vec::new(),
            size_checks_done: 0,
            size_check_at: None,
            display_resyncs_done: 0,
            display_resync_at: None,
            min_widths: HashMap::new(),
            reapplied: HashSet::new(),
            ov: OvCtl::default(),
            own_map,
            last_targets,
            anim,
        }
    }

    /// Core へ渡す単調時刻 (ms)。
    fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }

    /// 現在の `last_targets` / `visual` から遷移を開始し直す。
    fn restart_transition(&mut self) {
        self.anim = start_transition(
            &mut self.visual,
            &self.last_targets,
            &self.own_map,
            &self.state.monitors,
            &self.borders,
            self.cfg.cloak,
        );
    }

    /// IPC 購読者へ状態を配信する (前回と変わったときだけ)。
    fn broadcast_state(&mut self, overview: bool) {
        if !self.subscribers.is_empty() {
            let s = state_json(&self.state, overview);
            if s != self.last_state {
                self.last_state = s;
                let last = &self.last_state;
                self.subscribers
                    .retain(|sub| sub.send(last.clone()).is_ok());
            }
        }
    }

    /// 次にイベント待ちから起きる必要がある時間。`None` ならイベントが来るまでブロックする。
    /// deferred になった uncloak の再試行もここで行う。残っている間はフレーム間隔で回る。
    fn wait_timeout(&mut self) -> Option<Duration> {
        let pending_uncloak = retry_pending_uncloaks();
        if self.anim.is_some() || pending_uncloak || self.ov.is_running() {
            Some(FRAME)
        } else {
            [self.size_check_at, self.display_resync_at]
                .into_iter()
                .flatten()
                .min()
                .map(|t| t.saturating_duration_since(Instant::now()))
        }
    }

    /// イベント 1 件ぶんの状態遷移と Win32 副作用。前面化・再配置は `after_event` が行う。
    fn handle_event(&mut self, ev: WmEvent) -> ControlFlow<(), Outcome> {
        match &ev {
            WmEvent::Query(_) | WmEvent::Subscribe(_) => {}
            e => tracing::debug!("event: {e:?}"),
        }
        let mut resnap = false;
        // 状態遷移は Core (State) が行い、前面化すべきウィンドウを返す
        let focus: Option<WindowId> = match ev {
            WmEvent::Appeared(h) | WmEvent::MinimizeEnd(h) => self.on_appeared(h),
            WmEvent::Cloaked(h) if cloaked_by_us(h as u64) || !window_cloaked(h) => None,
            WmEvent::Gone(h) | WmEvent::MinimizeStart(h) | WmEvent::Cloaked(h) => {
                self.on_removed(h)
            }
            WmEvent::Foreground(h) => self.on_foreground(h),
            WmEvent::Focus(dir) => {
                // フロート窓が前面のとき: 左右は方向にいちばん近いフロート窓へ
                // フォーカスする (なければ何もしない)。上下はワークスペースを切り替え、
                // フォーカスはフロート窓に残す (フロート窓を手前に置いたまま下の
                // タイルを入れ替えられる)
                let fg = unsafe { GetForegroundWindow() };
                if !self.state.is_float(WindowId(fg.0 as u64)) {
                    self.state.focus(dir)
                } else if matches!(dir, FocusDir::Left | FocusDir::Right) {
                    focus_float_in_direction(fg, dir, &self.state)
                } else {
                    self.state.switch_workspace(dir == FocusDir::Down);
                    None
                }
            }
            WmEvent::MoveColumn(dir) => {
                // フロート窓が前面なら、レイアウトではなくその窓を左右へ動かす
                let fg = unsafe { GetForegroundWindow() };
                if self.state.is_float(WindowId(fg.0 as u64)) {
                    nudge_float(fg, dir, &self.state);
                } else {
                    self.state.move_column(dir);
                }
                None
            }
            WmEvent::Expel => {
                self.state.expel();
                None
            }
            WmEvent::Consume => {
                self.state.consume();
                None
            }
            WmEvent::MoveToWorkspace(down) => {
                // フロート窓が前面なら、ワークスペース移動ではなくその窓を上下へ動かす
                let fg = unsafe { GetForegroundWindow() };
                if self.state.is_float(WindowId(fg.0 as u64)) {
                    let dir = if down { FocusDir::Down } else { FocusDir::Up };
                    nudge_float(fg, dir, &self.state);
                } else {
                    self.state.move_to_workspace(down);
                }
                None
            }
            WmEvent::SwitchWorkspace(down) => self.state.switch_workspace(down),
            WmEvent::CycleWidth => {
                self.state.cycle_width();
                None
            }
            WmEvent::MaximizeColumn => {
                self.state.maximize_column();
                None
            }
            WmEvent::Fullscreen => {
                self.state.toggle_fullscreen();
                None
            }
            WmEvent::Scroll(forward) => {
                self.state.scroll(forward);
                None
            }
            // ユーザーによる手動移動・リサイズの完了 (FR-1.2)。リサイズ後の幅は
            // Column 幅として採用し、位置はタイルの定位置へ戻す
            WmEvent::MoveSizeStart(h) => {
                if self.state.is_float(WindowId(h as u64)) {
                    self.dragging = Some(h as u64);
                }
                None
            }
            WmEvent::MoveSizeEnd(h) => {
                resnap = self.on_move_size_end(h);
                None
            }
            WmEvent::CloseFocused => {
                if let Some(h) = close_target(&self.state) {
                    unsafe {
                        let _ = PostMessageW(Some(HWND(h as _)), WM_CLOSE, WPARAM(0), LPARAM(0));
                    }
                }
                None
            }
            WmEvent::Spawn(cmd) => {
                shell_spawn(&cmd);
                None
            }
            WmEvent::ToggleFloating => self.on_toggle_floating(),
            // フローティングとタイルの間でフォーカスを行き来する
            WmEvent::FocusFloating => {
                let fg = WindowId(unsafe { GetForegroundWindow() }.0 as u64);
                let on_tile = self.state.monitor_index_of(fg).is_some();
                self.state
                    .focus_toggle_candidates(on_tile)
                    .into_iter()
                    .find(|c| unsafe {
                        let hw = HWND(c.0 as _);
                        IsWindowVisible(hw).as_bool() && !IsIconic(hw).as_bool()
                    })
            }
            WmEvent::ToggleOpacity => {
                if let Some(f) = self.state.focused {
                    if let Some(i) = self.pinned.iter().position(|&x| x == f.0) {
                        self.pinned.swap_remove(i);
                    } else {
                        self.pinned.push(f.0);
                    }
                }
                None
            }
            WmEvent::FocusMonitor(forward) => self.state.focus_monitor(forward),
            WmEvent::MoveToMonitor(forward) => {
                // DPI の違うモニターへ移すと、アプリが WM_DPICHANGED に応じて遅れて
                // (配置完了後の読み戻しが終わってから) 自分でリサイズすることがある (Firefox で実測)。
                // ディスプレイ変更時と同じく、しばらく実位置・実サイズを突き合わせ直す
                self.schedule_resync();
                self.state.move_to_monitor(forward)
            }
            WmEvent::ToggleOverview => {
                self.ov.toggle(&self.state.monitors, self.cfg.anim);
                None
            }
            WmEvent::OverviewClose => {
                self.ov.close(self.cfg.anim);
                None
            }
            WmEvent::OverviewClick(x, y) => {
                self.ov.click(&mut self.state, x, y, self.cfg.anim);
                None
            }
            WmEvent::Reload => {
                self.on_reload();
                None
            }
            WmEvent::WorkAreaChanged => {
                // WM_SETTINGCHANGE はノイズが多いので、構成が変わったときだけログする
                let before = self.state.monitors.len();
                self.state.reconfigure(
                    &monitor_geoms(&self.cfg),
                    self.cfg.gap,
                    self.cfg.default_ratio,
                );
                if self.state.monitors.len() != before {
                    tracing::info!(
                        "work area changed: {} monitor(s)",
                        self.state.monitors.len()
                    );
                }
                // Windows が遅れてウィンドウを動かすことがあるので、しばらく実位置を
                // 目標と突き合わせ直す (論理状態が変わらなくても行う)
                self.schedule_resync();
                None
            }
            WmEvent::Query(reply) => {
                let _ = reply.send(state_json(&self.state, self.ov.is_open()));
                None
            }
            WmEvent::Subscribe(s) => {
                self.last_state = state_json(&self.state, self.ov.is_open());
                if s.send(self.last_state.clone()).is_ok() {
                    self.subscribers.push(s);
                }
                None
            }
            WmEvent::Shutdown => return ControlFlow::Break(()),
        };
        ControlFlow::Continue(Outcome { focus, resnap })
    }

    /// 新しいウィンドウの出現・最小化解除。
    fn on_appeared(&mut self, h: isize) -> Option<WindowId> {
        let id = WindowId(h as u64);
        // どこかの Stack が既に管理中なら何もしない。物理位置ベースで
        // adopt すると、退避・アニメ中に他モニター上で UNCLOAKED/SHOW が
        // 発火したとき別モニターの Stack へ二重登録されてしまう
        if self.state.monitor_index_of(id).is_some() || self.state.is_floating(id) {
            return None;
        }
        let (e, manageable) = inspect_manageable(h, &self.cfg.rules);
        match manageable {
            true => {
                let work = self.state.monitors[self.state.active_monitor].work;
                register_window(&mut self.visual, &mut self.borders, &e, work);
                let focus = self.state.adopt(id);
                persist_restore_rects();
                if let Some(c) = border_colors(&self.cfg) {
                    set_border_color(h as u64, c.1);
                }
                focus
            }
            false => {
                // タイルにしない窓がダイアログ等のフロートなら装飾対象にする
                if register_passive_float(&mut self.state, &self.cfg, &e) {
                    persist_restore_rects();
                }
                None
            }
        }
    }

    /// ウィンドウの消滅・最小化・cloak (別デスクトップ行きを含む)。
    fn on_removed(&mut self, h: isize) -> Option<WindowId> {
        let now = self.now_ms();
        match self.state.remove(WindowId(h as u64), now) {
            Some(removal) => {
                if removal.rolled_back {
                    tracing::debug!("remove {h:#x}: 直前のモニター跨ぎ追従を巻き戻す");
                }
                tracing::info!("remove {h:#x}");
                self.visual.remove(&(h as u64));
                self.borders.remove(&(h as u64));
                cloak_set(h as u64, Hide::Show);
                undim_window(h as u64);
                self.pinned.retain(|&x| x != h as u64);
                self.min_widths.remove(&(h as u64));
                if !unsafe { IsWindow(Some(HWND(h as _))).as_bool() } {
                    if let Some(m) = RESTORE_RECTS.lock().unwrap().as_mut() {
                        m.remove(&h);
                    }
                }
                persist_restore_rects();
                removal.focus
            }
            None => {
                // フロート窓の記録の扱いは State::float_gone が決める:
                // フロート化した窓は破棄されたら消す (最小化では消さない)。
                // 受動フロート窓は破棄・非表示・最小化・別デスクトップ行きの
                // いずれでも外す (再表示されれば Appeared で再登録)
                let id = WindowId(h as u64);
                let destroyed = !unsafe { IsWindow(Some(HWND(h as _))).as_bool() };
                match self.state.float_gone(id, destroyed) {
                    FloatGone::UserFloat => {
                        forget_float_record(h as u64);
                        undim_window(h as u64);
                    }
                    FloatGone::Passive => release_passive_float(h as u64),
                    FloatGone::Kept => return None,
                }
                if self.dragging == Some(h as u64) {
                    self.dragging = None;
                }
                persist_restore_rects();
                None
            }
        }
    }

    /// 前面ウィンドウの変化。
    fn on_foreground(&mut self, h: isize) -> Option<WindowId> {
        // WM が付けた最前面指定をアプリが落としていたら付け直す。
        // Overview 中は付け直さない (フロート窓が Overview より手前に出てしまう)
        if !self.ov.is_open() {
            reassert_float_topmost();
        }
        // 管理外のフロート窓 (ダイアログ・ルールや手動でフロートにした窓) が前面になったら
        // focus-floating の戻り先として覚え、フロート窓が前面ならそちらを
        // 「フォーカスされた見た目」にする (判断は State::float_foreground)
        if foreground_float(&mut self.state, &self.cfg, h) {
            persist_restore_rects();
        }
        let focused_dead = self.state.focused.is_some_and(|f| unsafe {
            let hw = HWND(f.0 as _);
            !IsWindow(Some(hw)).as_bool() || IsIconic(hw).as_bool()
        });
        let now = self.now_ms();
        let reassert = self.state.foreground(WindowId(h as u64), focused_dead, now);
        if let Some(succ) = reassert {
            tracing::debug!(
                "foreground {h:#x}: 閉じ直後の stale フォールバックを無視、後継 {:#x} を再主張",
                succ.0
            );
        }
        reassert
    }

    /// 手動移動・リサイズの完了。再配置が要るなら true。
    fn on_move_size_end(&mut self, h: isize) -> bool {
        let id = WindowId(h as u64);
        if self.dragging == Some(h as u64) {
            self.dragging = None;
        }
        if self.state.monitor_index_of(id).is_some() {
            if let Some(actual) = frame_rect(h) {
                let target_w = self
                    .last_targets
                    .iter()
                    .find(|(t, _)| *t == h as u64)
                    .map(|(_, r)| r.w);
                if self.state.fullscreen != Some(id)
                    && target_w.is_some_and(|w| (actual.w - w).abs() > SIZE_TOLERANCE)
                {
                    self.state.resize_column(id, actual.w);
                }
                // 手動リサイズの結果は最小幅の記録より信頼できる
                self.min_widths.remove(&(h as u64));
                self.visual.insert(h as u64, actual);
                return true;
            }
        }
        false
    }

    /// フローティングの切替: 前面がタイルならレイアウトから外し、
    /// 管理外のウィンドウならタイルとして取り込む。
    fn on_toggle_floating(&mut self) -> Option<WindowId> {
        let fg = unsafe { GetForegroundWindow() };
        let h = fg.0 as isize;
        let id = WindowId(h as u64);
        if let Some(mi) = self.state.monitor_index_of(id) {
            let work = self.state.monitors[mi].work;
            let now = self.now_ms();
            self.state.float(id, now);
            self.visual.remove(&(h as u64));
            self.borders.remove(&(h as u64));
            cloak_set(h as u64, Hide::Show);
            undim_window(h as u64);
            self.pinned.retain(|&x| x != h as u64);
            self.min_widths.remove(&(h as u64));
            float_window(h, work);
            persist_restore_rects();
            tracing::info!("float {h:#x}");
            None
        } else if let Some(e) = inspect_tileable(h) {
            tracing::info!("tile {h:#x} \"{}\"", e.info.title);
            unfloat_window(h);
            let work = self.state.monitors[self.state.active_monitor].work;
            register_window(&mut self.visual, &mut self.borders, &e, work);
            let focus = self.state.unfloat(id);
            persist_restore_rects();
            if let Some(c) = border_colors(&self.cfg) {
                set_border_color(h as u64, c.1);
            }
            // 枠色の追従に再評価させる (フォーカス中なら下で focused 色になる)
            if self.bordered == Some(h as u64) {
                self.bordered = None;
            }
            if self.dragging == Some(h as u64) {
                self.dragging = None;
            }
            focus
        } else {
            None
        }
    }

    /// 設定の再読み込み。
    fn on_reload(&mut self) {
        self.cfg = config::load();
        events::set_mouse_scroll_focus(self.cfg.mouse_scroll_focus);
        events::rebind(input_binds(&self.cfg));
        self.state.reconfigure(
            &monitor_geoms(&self.cfg),
            self.cfg.gap,
            self.cfg.default_ratio,
        );
        if !self.cfg.cloak {
            uncloak_all();
        }
        undim_all();
        let unfocused = border_colors(&self.cfg).map_or(BORDER_DEFAULT, |c| c.1);
        for h in own_monitor_map(&self.state.monitors).into_keys() {
            set_border_color(h, unfocused);
        }
        for f in self.state.floats() {
            set_border_color(f.0, unfocused);
        }
        // ルールが変わり得るので受動フロート窓を再評価する
        // (外れた窓は装飾を戻し、新たに該当する窓を登録)
        if sync_passive_floats(&mut self.state, &self.cfg, &scan::scan(&self.cfg.rules)) {
            persist_restore_rects();
        }
        self.bordered = None;
        tracing::info!(
            "config reloaded: gap={} anim={:?} rules={} cloak={}",
            self.cfg.gap,
            self.cfg.anim,
            self.cfg.rules.len(),
            self.cfg.cloak
        );
    }

    /// イベント処理後の共通後始末: 前面化・ターゲット再計算・枠色・購読者への配信。
    fn after_event(&mut self, outcome: Outcome) {
        // Overview 中は前面化しない (隠れたアプリへキーボードフォーカスが飛び、
        // Esc / Enter が Overview に届かなくなる)。閉じ終わりに highlighted() へ適用する
        if let Some(f) = outcome.focus.filter(|_| !self.ov.is_open()) {
            focus_window(f.0 as isize);
        }

        self.own_map = own_monitor_map(&self.state.monitors);
        // 既知の最小幅を下回る Column は配置前に広げておく (重なりを一瞬も見せない)
        let state = &self.state;
        self.min_widths
            .retain(|&h, m| monitor_of(state, h) == Some(m.hmonitor) && window_dpi(h) == m.dpi);
        for (&h, m) in &self.min_widths {
            self.state.grow_column(WindowId(h), m.width);
        }
        let new_targets = targets(&self.state);
        if new_targets != self.last_targets || outcome.resnap {
            self.size_checks_done = 0;
            self.reapplied.clear();
            self.size_check_at = None;
            self.last_targets = new_targets;
            self.restart_transition();
        }

        decor::sync_border_colors(&self.cfg, &self.state, &mut self.bordered);

        self.broadcast_state(self.ov.is_open());
    }

    /// 毎周の描画・確認・装飾の更新 (イベントの有無によらない)。
    fn tick(&mut self) {
        let settled = self.draw_frame();
        self.check_sizes(settled);
        self.resync_after_display_change();
        self.apply_cloak();
        decor::update_dimming(&self.cfg, &self.state, &self.last_targets, &self.pinned);

        // Overview (FR-4.11): 開閉アニメーションを進め、シーンが変わったときだけ
        // サムネイルを更新する。枠オーバーレイより先に処理する
        // (閉じ終わった同じ周回で枠を描き戻すため)
        if self.ov.tick(&self.state, &self.cfg) {
            // 保留していたフォーカスを適用してキーボードフォーカスを返す
            if let Some(f) = self.state.highlighted() {
                focus_window(f.0 as isize);
            }
            self.broadcast_state(false);
        }

        decor::update_overlay(
            &self.cfg,
            &self.state,
            &self.last_targets,
            &self.visual,
            &self.own_map,
            self.dragging,
            self.ov.is_open(),
        );
    }

    /// 実位置・実サイズの突き合わせを最初からやり直すよう予定する
    /// (ディスプレイ変更・モニター間移動のあと)。
    fn schedule_resync(&mut self) {
        self.display_resyncs_done = 0;
        self.display_resync_at = Some(Instant::now() + Duration::from_millis(DISPLAY_RESYNC_MS[0]));
    }

    /// ディスプレイ変更・モニター間移動後の突き合わせ (DISPLAY_RESYNC_MS の予定に従う)。
    /// アニメーション中は行わない (補間中の位置は目標と違って当然なので)。
    fn resync_after_display_change(&mut self) {
        let due =
            self.anim.is_none() && self.display_resync_at.is_some_and(|t| Instant::now() >= t);
        if !due {
            return;
        }
        self.display_resyncs_done += 1;
        self.display_resync_at = DISPLAY_RESYNC_MS
            .get(self.display_resyncs_done)
            .map(|ms| Instant::now() + Duration::from_millis(*ms));
        let fixed = resync_positions(&self.last_targets, &mut self.borders);
        if fixed > 0 {
            tracing::info!("位置・大きさがずれていた {fixed} 個のウィンドウを戻す");
        }
    }

    /// フレーム描画: 補間した位置へ配置し、cloak モードでは縁でクリップする。
    /// 戻り値: このコマでアニメーションが完了したか。
    fn draw_frame(&mut self) -> bool {
        let mut settled = false;
        if let Some((entries, started)) = &self.anim {
            let t_raw = if self.cfg.anim.is_zero() {
                1.0
            } else {
                started.elapsed().as_secs_f32() / self.cfg.anim.as_secs_f32()
            };
            let t = ease_out_cubic(t_raw);
            let frame: Vec<(u64, Rect)> = entries
                .iter()
                .map(|e| (e.hwnd, lerp_rect(e.from, e.to, t)))
                .collect();
            apply_rects(&frame, &self.borders);
            // cloak モード: 自モニターの縁でクリップ — 縁を越えた瞬間に隠し、
            // 入った瞬間に出す。他モニターへの写り込みを防ぐ。
            // アニメ中は TOOLWINDOW を触らない (view 再作成で SetCloak が
            // 失敗しやすくなるため)。Full は静止後の一括判定でのみ適用する
            if self.cfg.cloak {
                for &(h, r) in &frame {
                    let level = match hide_level(h, r, &self.own_map, &self.state.monitors) {
                        Hide::Full => Hide::Render,
                        l => l,
                    };
                    cloak_set(h, level);
                }
            }
            for (h, r) in frame {
                self.visual.insert(h, r);
            }
            if t_raw >= 1.0 {
                self.anim = None;
                settled = true;
                // アニメーションの終点はモニター縁のすぐ外にクランプされている。
                // 完了後、本来の退避先 (全モニター外) へ不可視のままスナップする
                let settles: Vec<(u64, Rect)> = self
                    .last_targets
                    .iter()
                    .filter(|(h, r)| self.visual.get(h) != Some(r))
                    .copied()
                    .collect();
                if !settles.is_empty() {
                    apply_rects(&settles, &self.borders);
                    for (h, r) in settles {
                        self.visual.insert(h, r);
                    }
                }
            }
        }
        settled
    }

    /// FR-3.6: 最小サイズ制約で要求より広くなったウィンドウは、実幅を Column 幅へ
    /// 読み戻して隣との重なりを解消する。配置完了時に加え、要求サイズを一旦
    /// 受け入れてから遅れて広がり直すアプリのために少し後にも確認する。
    /// 確認は 1 回のレイアウト変化につき SIZE_RECHECK_MS.len() + 1 回まで (ループ防止)
    fn check_sizes(&mut self, settled: bool) {
        let due = self.anim.is_none() && self.size_check_at.is_some_and(|t| Instant::now() >= t);
        if (settled || due) && self.size_checks_done <= SIZE_RECHECK_MS.len() {
            self.size_check_at = SIZE_RECHECK_MS
                .get(self.size_checks_done)
                .map(|ms| Instant::now() + Duration::from_millis(*ms));
            self.size_checks_done += 1;
            if read_back_sizes(
                &mut self.state,
                &self.last_targets,
                &self.own_map,
                &mut self.min_widths,
                &mut self.borders,
                &mut self.reapplied,
            ) {
                self.last_targets = targets(&self.state);
                self.restart_transition();
            }
        }
    }

    /// cloak モード: アニメーション完了後、自モニターに写らないウィンドウを隠す。
    /// active ワークスペースの viewport 外は Alt+Tab に残す (Hide::Render)
    fn apply_cloak(&self) {
        if self.cfg.cloak && self.anim.is_none() {
            for &(h, r) in &self.last_targets {
                let level = hide_level(h, r, &self.own_map, &self.state.monitors);
                cloak_set(h, level);
                if level == Hide::Show && window_cloaked(h as isize) {
                    shell_cloak(h, false);
                }
            }
        }
    }
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
pub(crate) fn intersects_any(r: Rect, monitors: &[Monitor]) -> bool {
    monitors.iter().any(|ms| intersects(r, ms.work))
}

/// 2 矩形が交差するか。
pub(crate) fn intersects(r: Rect, work: Rect) -> bool {
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

pub(crate) fn to_rect(r: RECT) -> Rect {
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
pub(crate) fn primary_monitor_fallback() -> (Rect, Rect) {
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
    min_widths: &mut HashMap<u64, MinWidth>,
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
                if let Some(hmonitor) = monitor_of(state, h) {
                    min_widths.insert(
                        h,
                        MinWidth {
                            hmonitor,
                            dpi: window_dpi(h),
                            width: actual.w,
                        },
                    );
                }
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
pub(crate) fn frame_rect(h: isize) -> Option<Rect> {
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

/// ウィンドウの現在の DPI (Per-Monitor 対応アプリなら表示中のモニターの DPI)。
/// 取得できない (ウィンドウが消えた等) ときは 0。
fn window_dpi(h: u64) -> u32 {
    unsafe { GetDpiForWindow(HWND(h as _)) }
}

/// 全管理ウィンドウの実フレームを目標と突き合わせ、ずれているものを目標へ置き直す。
/// 画面外へ退避中のウィンドウも対象にする (別モニターへ動かされると写り込むため)。
/// DPI が変わっていることがあるので、置き直す前に不可視フレームを測り直す。
/// 戻り値: 置き直したウィンドウ数。
fn resync_positions(targets: &[(u64, Rect)], borders: &mut HashMap<u64, Border>) -> usize {
    let off = |a: i32, b: i32| (a - b).abs() > SIZE_TOLERANCE;
    let mut retry: Vec<(u64, Rect)> = Vec::new();
    for &(h, r) in targets {
        let Some(actual) = frame_rect(h as isize) else {
            continue;
        };
        // 位置と高さが合っていて幅だけ広いのは最小サイズ制約 (読み戻しが列幅を広げて扱う)。
        // ここで置き直すとアプリが広がり直してちらつくだけなので触らない
        let min_size_only =
            !off(actual.x, r.x) && !off(actual.y, r.y) && !off(actual.h, r.h) && actual.w > r.w;
        if min_size_only {
            continue;
        }
        if off(actual.x, r.x) || off(actual.y, r.y) || off(actual.w, r.w) || off(actual.h, r.h) {
            tracing::debug!("{h:#x}: 実フレーム {actual:?} が目標 {r:?} とずれている");
            if let Some(b) = measure_border(h as isize) {
                borders.insert(h, b);
            }
            retry.push((h, r));
        }
    }
    apply_rects(&retry, borders);
    retry.len()
}

/// 記録した最小幅 (FR-3.6) と、それを測ったときのモニター・DPI。
#[derive(Debug, Clone, Copy)]
struct MinWidth {
    hmonitor: isize,
    dpi: u32,
    width: i32,
}

/// 論理状態の上でウィンドウが属するモニターの HMONITOR (未管理なら None)。
fn monitor_of(state: &State, h: u64) -> Option<isize> {
    state
        .monitor_index_of(WindowId(h))
        .map(|mi| state.monitors[mi].hmonitor)
}
