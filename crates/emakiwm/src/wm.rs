//! タイリングランタイム。
//!
//! - 起動時スキャンで既存ウィンドウを Strip へ取り込む (FR-1.1)
//! - WinEventHook の開閉イベントに追従 (FR-1.2)、Alt+H/L/J/K でフォーカス移動 (FR-4.1)
//! - Viewport 内のみ DeferWindowPos で配置、外は offscreen 退避または cloak (FR-3.2, FR-3.3)
//! - レイアウト変化は各ウィンドウ Rect の補間で滑らかに反映 (FR-4.8)
//! - 正常終了 (Ctrl+C / Alt+Shift+E)・panic とも全ウィンドウを元の位置へ復元 (FR-1.5, NFR-4)
//! - 復元 Rect は state.json へも永続化し、kill 後は `--restore` で戻せる (FR-1.6)
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
    GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTOPRIMARY,
};
use windows::Win32::System::Console::SetConsoleCtrlHandler;
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::WindowsAndMessaging::{
    BeginDeferWindowPos, DeferWindowPos, EndDeferWindowPos, GetForegroundWindow,
    GetLayeredWindowAttributes, GetWindowLongPtrW, GetWindowThreadProcessId, IsIconic, IsWindow,
    PostMessageW, SetForegroundWindow, SetLayeredWindowAttributes, SetWindowLongPtrW, SetWindowPos,
    GWL_EXSTYLE, LWA_ALPHA, SWP_NOACTIVATE, SWP_NOZORDER, WM_CLOSE, WS_EX_LAYERED,
};

use crate::events::{self, WmEvent};
use crate::{border, com, config, ipc, scan, ws};

/// アニメーション中のフレーム間隔。
const FRAME: Duration = Duration::from_millis(8);

/// 終了時復元用の元 Rect (FR-1.5)。
/// panic フックと Ctrl+C ハンドラから参照するためグローバルに持つ。
static RESTORE_RECTS: Mutex<Option<HashMap<isize, RECT>>> = Mutex::new(None);

/// FR-3.3 cloak モードで自分が隠したウィンドウ。
/// CLOAKED イベントの自己無視と、終了時の一括 uncloak に使う。
static CLOAKED: Mutex<Vec<u64>> = Mutex::new(Vec::new());

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

pub fn run() {
    install_restore_hooks();

    let mut cfg = config::load();

    let (tx, rx) = mpsc::channel::<WmEvent>();
    events::spawn_hook_thread(tx.clone(), cfg.hotkeys.clone());
    // FR-7.5: WebSocket 状態配信 (Zebar 等向け)。ポート変更は再起動が必要
    if let Some(port) = cfg.ws_port {
        ws::spawn_ws_thread(port, tx.clone());
    }
    ipc::spawn_ipc_thread(tx);

    let (mut work, mut monitor) = effective_work(&cfg);
    tracing::info!("work area: ({}, {}) {}x{}", work.x, work.y, work.w, work.h);
    let mut viewport_w = work.w - 2 * cfg.gap;
    // 新規 Column の幅 = Viewport (gap 1 個ぶん引いた実効幅) × 比率。
    // 0.5 でちょうど 2 枚、0.48 等にすると隣の列の端が見える (FR-3.4)
    let mut default_width = ((viewport_w - cfg.gap) as f32 * cfg.default_ratio) as i32;

    let mut stack = Stack::default();
    let mut focused: Option<WindowId> = None;
    // 各ウィンドウが画面上に実際にいる Rect (アニメーションの from)。論理 (フレーム) 座標
    let mut visual: HashMap<u64, Rect> = HashMap::new();
    // 不可視フレーム補正量 (§8-2)
    let mut borders: HashMap<u64, Border> = HashMap::new();
    // fullscreen 中のウィンドウ (FR-4.7)
    let mut fullscreen: Option<u64> = None;
    // 状態購読者 (FR-7.4)。変化時に state JSON を配信する
    let mut subscribers: Vec<mpsc::Sender<String>> = Vec::new();
    let mut last_state = String::new();
    // 現在フォーカス色を塗っているウィンドウ (FR-3.7 枠色)
    let mut bordered: Option<u64> = None;
    // opacity ピン中のウィンドウ (FR-3.8 toggle-opacity)
    let mut pinned: Vec<u64> = Vec::new();

    // FR-1.1: 起動時スキャン。Strip 上の並びは既存ウィンドウの画面上の
    // 左端 x 順にして、タイル化後の並びが直感と一致するようにする
    let mut initial: Vec<scan::ScanEntry> = scan::scan(&cfg.rules)
        .into_iter()
        .filter(|e| e.decision == Decision::Manage && e.elevated != Some(true))
        .filter(|e| unsafe { !IsIconic(HWND(e.hwnd as _)).as_bool() })
        .collect();
    initial.sort_by_key(|e| e.rect.left);
    for e in &initial {
        adopt(
            &mut stack,
            &mut visual,
            &mut borders,
            e,
            default_width,
            cfg.gap,
            None,
            work,
        );
        tracing::info!(
            "adopt {:#x} {} \"{}\"",
            e.hwnd,
            e.info.exe_name.as_deref().unwrap_or("?"),
            e.info.title
        );
        if let Some(c) = border_colors(&cfg) {
            set_border_color(e.hwnd as u64, c.1);
        }
    }

    persist_restore_rects();

    let fg = WindowId(unsafe { GetForegroundWindow() }.0 as u64);
    if stack.contains(fg) {
        focused = Some(fg);
        let strip = stack.active_mut();
        strip.set_active(fg);
        strip.last_focused = Some(fg);
    }

    tracing::info!(
        "managing {} windows. Ctrl+C / Alt+Shift+E で終了 (全ウィンドウ復元)",
        stack.strips.iter().map(|s| s.columns.len()).sum::<usize>()
    );

    // 初期配置も元の位置からタイルへ流し込むアニメーションで行う
    let mut last_targets = targets(&stack, work, monitor, fullscreen, cfg.gap);
    let mut anim = start_transition(&mut visual, &last_targets, work, &borders);

    // Core ループ: イベント → 論理状態更新 → リターゲット → フレーム描画。
    // アニメーション中のみ FRAME 間隔で起き、アイドル時は recv() でブロックして
    // CPU を使わない (NFR-2)
    'main: loop {
        let ev = if anim.is_some() {
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
            match ev {
                WmEvent::Appeared(h) | WmEvent::MinimizeEnd(h) => {
                    if try_adopt(
                        &mut stack,
                        &mut visual,
                        &mut borders,
                        h,
                        default_width,
                        cfg.gap,
                        &cfg.rules,
                        focused,
                        work,
                    ) {
                        // FR-1.3.1: 新規ウィンドウへフォーカスを移し Viewport を追従させる
                        let id = WindowId(h as u64);
                        focused = Some(id);
                        let strip = stack.active_mut();
                        strip.set_active(id);
                        strip.last_focused = Some(id);
                        strip.ensure_visible(id, viewport_w);
                        focus_window(h);
                        persist_restore_rects();
                        if let Some(c) = border_colors(&cfg) {
                            set_border_color(h as u64, c.1);
                        }
                    }
                }
                // 自分が cloak したウィンドウの CLOAKED イベントは無視する (FR-3.3)。
                // uncloak 直後に届く古いイベントも、実際の cloak 状態で弾く
                WmEvent::Cloaked(h) if cloaked_by_us(h as u64) || !window_cloaked(h) => {}
                WmEvent::Gone(h) | WmEvent::MinimizeStart(h) | WmEvent::Cloaked(h) => {
                    let id = WindowId(h as u64);
                    if let Some(si) = stack.strip_index_of(id) {
                        let strip = &mut stack.strips[si];
                        strip.remove_window(id, cfg.gap);
                        strip.clamp_offset(viewport_w);
                        if strip.last_focused == Some(id) {
                            strip.last_focused = None;
                        }
                        stack.normalize();
                        tracing::info!("remove {h:#x}");
                        visual.remove(&(h as u64));
                        borders.remove(&(h as u64));
                        // cloak / 半透明のまま消えた (HIDE 等) 場合に備え戻しておく
                        cloak_set(h as u64, false);
                        undim_window(h as u64);
                        pinned.retain(|&x| x != h as u64);
                        if fullscreen == Some(h as u64) {
                            fullscreen = None;
                        }
                        if focused == Some(id) {
                            focused = None;
                        }
                        // DESTROY されたウィンドウは復元対象からも外す
                        // (MINIMIZESTART は IsWindow が生きているので保持される)
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
                    if let Some(si) = stack.strip_index_of(id) {
                        // 別ワークスペースのウィンドウなら、そのワークスペースへ切替えて追従
                        stack.active = si;
                        focused = Some(id);
                        let strip = stack.active_mut();
                        strip.set_active(id);
                        strip.last_focused = Some(id);
                        // Alt+Tab やクリックで画面外の Column へ移った場合も追従する (FR-4.2)
                        strip.ensure_visible(id, viewport_w);
                    }
                }
                WmEvent::Focus(dir) => {
                    let strip = stack.active_mut();
                    // フォーカスが未確定なら先頭 Column から始める
                    let target = match focused.filter(|f| strip.contains(*f)) {
                        Some(cur) => strip.neighbor(cur, dir),
                        None => strip.columns.first().and_then(|c| c.active()),
                    };
                    match target {
                        Some(t) => {
                            focused = Some(t);
                            strip.set_active(t);
                            strip.last_focused = Some(t);
                            strip.ensure_visible(t, viewport_w);
                            focus_window(t.0 as isize);
                        }
                        // Column の上下端からさらに J/K → Workspace 切替へ続く (FR-4.1)
                        None if dir == FocusDir::Down => {
                            switch_workspace(&mut stack, &mut focused, true)
                        }
                        None if dir == FocusDir::Up => {
                            switch_workspace(&mut stack, &mut focused, false)
                        }
                        None => {} // 左右端は停止 (ラップしない)
                    }
                }
                WmEvent::MoveColumn(dir) => {
                    if let Some(f) = focused {
                        let strip = stack.active_mut();
                        if strip.move_column(f, dir, cfg.gap) {
                            strip.ensure_visible(f, viewport_w);
                        }
                    }
                }
                WmEvent::Expel => {
                    if let Some(f) = focused {
                        let strip = stack.active_mut();
                        if strip.expel(f, cfg.gap) {
                            strip.ensure_visible(f, viewport_w);
                        }
                    }
                }
                WmEvent::Consume => {
                    if let Some(f) = focused {
                        stack.active_mut().consume_right(f, cfg.gap);
                    }
                }
                WmEvent::MoveToWorkspace(down) => {
                    // FR-5.4: 下端は動的に新規作成。フォーカスはウィンドウに追従
                    if let Some(f) = focused {
                        if stack.move_window(f, down, default_width, viewport_w, cfg.gap) {
                            let strip = stack.active_mut();
                            strip.set_active(f);
                            strip.ensure_visible(f, viewport_w);
                        }
                    }
                }
                WmEvent::SwitchWorkspace(down) => {
                    switch_workspace(&mut stack, &mut focused, down);
                }
                WmEvent::CycleWidth => {
                    if let Some(f) = focused {
                        let strip = stack.active_mut();
                        if strip.cycle_width(f, viewport_w, cfg.gap) {
                            strip.ensure_visible(f, viewport_w);
                        }
                    }
                }
                WmEvent::MaximizeColumn => {
                    if let Some(f) = focused {
                        let strip = stack.active_mut();
                        if strip.toggle_maximize(f, viewport_w, cfg.gap) {
                            strip.ensure_visible(f, viewport_w);
                        }
                    }
                }
                WmEvent::Fullscreen => {
                    if let Some(f) = focused {
                        fullscreen = (fullscreen != Some(f.0)).then_some(f.0);
                    }
                }
                WmEvent::Scroll(forward) => {
                    stack.active_mut().scroll_columnwise(forward, viewport_w);
                }
                WmEvent::CloseFocused => {
                    if let Some(f) = focused {
                        // WM_CLOSE で行儀よく閉じる。消滅は Gone イベントで追従する
                        unsafe {
                            let _ =
                                PostMessageW(Some(HWND(f.0 as _)), WM_CLOSE, WPARAM(0), LPARAM(0));
                        }
                    }
                }
                WmEvent::Spawn(cmd) => {
                    // 空白区切りの素朴な分割 (空白を含むパスの引用符には未対応)。
                    // 起動したアプリのウィンドウは Appeared イベントで取り込まれる
                    let mut parts = cmd.split_whitespace();
                    if let Some(prog) = parts.next() {
                        match std::process::Command::new(prog).args(parts).spawn() {
                            Ok(_) => tracing::info!("spawn: {cmd}"),
                            Err(e) => tracing::warn!("spawn \"{cmd}\" failed: {e}"),
                        }
                    }
                }
                WmEvent::ToggleOpacity => {
                    // FR-3.8: フォーカスウィンドウの opacity ピンをトグル。
                    // 反映はループ末尾の半透明スイープで行う
                    if let Some(f) = focused {
                        if let Some(i) = pinned.iter().position(|&x| x == f.0) {
                            pinned.swap_remove(i);
                        } else {
                            pinned.push(f.0);
                        }
                    }
                }
                WmEvent::Reload => {
                    // FR-7.2: gap / anim / rules / hide / reserve を反映。
                    // キーバインドは再起動が必要
                    cfg = config::load();
                    (work, monitor) = effective_work(&cfg);
                    viewport_w = work.w - 2 * cfg.gap;
                    default_width = ((viewport_w - cfg.gap) as f32 * cfg.default_ratio) as i32;
                    for s in &mut stack.strips {
                        s.relayout(cfg.gap);
                        s.clamp_offset(viewport_w);
                    }
                    if !cfg.cloak {
                        uncloak_all();
                    }
                    // 半透明の値変更・無効化に追従 (有効ならループ末尾で再適用される)
                    undim_all();
                    // 枠色の変更・無効化を全ウィンドウへ再適用 (FR-3.7)。
                    // フォーカス色は直後の共通処理が塗り直す
                    let unfocused = border_colors(&cfg).map_or(BORDER_DEFAULT, |c| c.1);
                    for s in &stack.strips {
                        for col in &s.columns {
                            for t in &col.tiles {
                                set_border_color(t.0, unfocused);
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
                    // FR-5.5: 解像度・タスクバー・バー領域の変更に追従する。
                    // WM_SETTINGCHANGE はノイズが多いので実際に変わったときだけ動く
                    let (w2, m2) = effective_work(&cfg);
                    if w2 != work || m2 != monitor {
                        work = w2;
                        monitor = m2;
                        viewport_w = work.w - 2 * cfg.gap;
                        default_width = ((viewport_w - cfg.gap) as f32 * cfg.default_ratio) as i32;
                        for s in &mut stack.strips {
                            s.relayout(cfg.gap);
                            s.clamp_offset(viewport_w);
                        }
                        tracing::info!(
                            "work area changed: ({}, {}) {}x{}",
                            work.x,
                            work.y,
                            work.w,
                            work.h
                        );
                    }
                }
                WmEvent::Query(reply) => {
                    let _ = reply.send(state_json(&stack, focused, fullscreen));
                }
                WmEvent::Subscribe(s) => {
                    // FR-7.4: 登録時に現在の状態を 1 回送り、以後は変化時に配信する
                    last_state = state_json(&stack, focused, fullscreen);
                    if s.send(last_state.clone()).is_ok() {
                        subscribers.push(s);
                    }
                }
                WmEvent::Shutdown => break 'main,
            }
            // ターゲット集合が変わったときだけ、現在の visual から向け直す (§8-8)。
            // 変わっていなければ進行中のアニメを乱さない (イベントノイズで
            // 補間が再スタートして減速し続けるのを防ぐ)
            let new_targets = targets(&stack, work, monitor, fullscreen, cfg.gap);
            if new_targets != last_targets {
                last_targets = new_targets;
                // cloak モード: 画面内へ向かうウィンドウはスライドインが
                // 見えるよう先に uncloak する (FR-3.3)
                if cfg.cloak {
                    for &(h, r) in &last_targets {
                        if intersects(r, work) {
                            cloak_set(h, false);
                        }
                    }
                }
                anim = start_transition(&mut visual, &last_targets, work, &borders);
            }
            // フォーカス枠の塗り替え (FR-3.7)。前のフォーカスを非フォーカス色へ戻す
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
            // 状態変化を購読者へ配信する (FR-7.4)。切断された購読者はここで除く
            if !subscribers.is_empty() {
                let s = state_json(&stack, focused, fullscreen);
                if s != last_state {
                    last_state = s;
                    subscribers.retain(|sub| sub.send(last_state.clone()).is_ok());
                }
            }
        }

        // フレーム描画
        if let Some((entries, started)) = &anim {
            let t_raw = if cfg.anim.is_zero() {
                1.0 // アニメーション無効 (FR-4.8): 即時配置
            } else {
                started.elapsed().as_secs_f32() / cfg.anim.as_secs_f32()
            };
            let t = ease_out_cubic(t_raw);
            let frame: Vec<(u64, Rect)> = entries
                .iter()
                .map(|e| (e.hwnd, lerp_rect(e.from, e.to, t)))
                .collect();
            apply_rects(&frame, &borders);
            for (h, r) in frame {
                visual.insert(h, r);
            }
            if t_raw >= 1.0 {
                anim = None;
            }
        }

        // cloak モード: アニメーション完了後、画面外に確定したウィンドウを隠す (FR-3.3)。
        // スライドアウト中は隠さない (動きが見えるように)
        if cfg.cloak && anim.is_none() {
            for &(h, r) in &last_targets {
                let off = !intersects(r, work);
                cloak_set(h, off);
                // 取りこぼし救済: uncloak が view 再作成のタイミングで効かず
                // 画面内なのに cloak が残っていたら強制解除する
                if !off && window_cloaked(h as isize) {
                    shell_cloak(h, false);
                }
            }
        }

        // FR-3.8: 非フォーカスウィンドウの半透明化。フォーカス・fullscreen は不透明。
        // opacity ピン中 (toggle-opacity) はフォーカスに関わらず pinned_alpha を維持
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

        // FR-3.7: 太枠オーバーレイを画面内の全管理ウィンドウの現在位置 (visual) へ
        // 追従させる。アニメーション中も毎フレーム呼ばれるので一緒に滑る。
        // 色は focused / unfocused を使い分け、"default"/"none" のセンチネル
        // (0xFFFF_FFFE 以上) はそのウィンドウのオーバーレイなしを意味する
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
                if !intersects(r, work) {
                    continue;
                }
                let color = if focused.map(|f| f.0) == Some(h) {
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
    restore_all();
}

/// ターゲットと visual の差分から遷移を開始する。
/// 画面外→画面外の移動 (退避位置の左右入れ替え等) は補間すると Viewport を
/// 横切って飛ぶため、即時反映して visual を更新する。補間対象がなければ None。
fn start_transition(
    visual: &mut HashMap<u64, Rect>,
    targets: &[(u64, Rect)],
    work: Rect,
    borders: &HashMap<u64, Border>,
) -> Option<(Vec<AnimEntry>, Instant)> {
    let mut snaps: Vec<(u64, Rect)> = Vec::new();
    let mut entries: Vec<AnimEntry> = Vec::new();
    for &(hwnd, to) in targets {
        // visual 未登録 (取り込み直後に実 Rect が取れなかった等) は補間せず直行
        let from = visual.get(&hwnd).copied().unwrap_or(to);
        if from == to {
            continue;
        }
        if !intersects(from, work) && !intersects(to, work) {
            snaps.push((hwnd, to));
        } else {
            entries.push(AnimEntry { hwnd, from, to });
        }
    }
    if !snaps.is_empty() {
        apply_rects(&snaps, borders);
        for (h, r) in snaps {
            visual.insert(h, r);
        }
    }
    (!entries.is_empty()).then(|| (entries, Instant::now()))
}

/// 作業領域と交差するか (= 画面内に見え得るか)。
/// 退避は左右 (Viewport 外) と上下 (非 active ワークスペース) の両方にある。
fn intersects(r: Rect, work: Rect) -> bool {
    r.x < work.x + work.w && r.x + r.w > work.x && r.y < work.y + work.h && r.y + r.h > work.y
}

/// 論理状態 → 各ウィンドウの最終画面 Rect。
/// active ワークスペースは通常の射影 (Offscreen は左右の退避位置)。
/// 非 active ワークスペースは相対位置に応じて 1 画面ぶん上下へずらす —
/// 切替・移動が縦スライドのアニメーションになる (FR-5.4)。
/// fullscreen 中のウィンドウはモニタ全面 (タスクバー含む) で上書き (FR-4.7)。
fn targets(
    stack: &Stack,
    work: Rect,
    monitor: Rect,
    fullscreen: Option<u64>,
    gap: i32,
) -> Vec<(u64, Rect)> {
    let mut out = Vec::new();
    for (i, strip) in stack.strips.iter().enumerate() {
        let dy = (i as i32 - stack.active as i32) * (work.h + 100);
        for (id, p) in strip.project(work, gap) {
            let r = if Some(id.0) == fullscreen && i == stack.active {
                monitor
            } else {
                let base = park(p, work);
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
        // FR-1.5: 取り込み時点で画面外 (前回 kill の退避位置のまま等) なら、
        // 復元先として汚染されないよう画面内へクランプして記録する
        m.entry(h).or_insert(clamp_into_work(e.rect, work));
    }
    // 不可視フレーム補正量を記録 (§8-2)。frame が取れなければ補正 0
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
    // 強制終了の取り残し (半透明) があれば取り込み時に直す (FR-3.8)
    heal_leftover_alpha(h as u64);
    // 現在の実フレーム位置を visual に登録 → 元の位置からタイルへ滑り込む
    visual.insert(h as u64, to_rect(frame));
    stack
        .active_mut()
        .insert_column_after(focused, WindowId(h as u64), width, gap);
    // 末尾の空ワークスペースへの取り込みで不変条件が崩れた場合に備える
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

/// 論理 (フレーム) Rect → 実ウィンドウ Rect へ不可視フレームぶん広げる (§8-2)。
fn expand(r: Rect, b: Border) -> Rect {
    let (l, t, rt, bm) = b;
    Rect {
        x: r.x - l,
        y: r.y - t,
        w: r.w + l + rt,
        h: r.h + t + bm,
    }
}

/// Rect 群を DeferWindowPos バッチで適用する (FR-3.2)。
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
                    // UIPI 等の失敗は握りつぶす (FR-2.3)。バッチ全体は継続不能なので
                    // フォールバックとして個別 SetWindowPos に切り替える
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

/// ワークスペース切替 + last_focused へのフォーカス復帰 (FR-5.4)。端ではクランプ。
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

/// IPC `state` 応答の JSON (FR-7.3)。
fn state_json(stack: &Stack, focused: Option<WindowId>, fullscreen: Option<u64>) -> String {
    let workspaces: Vec<_> = stack
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
        "active_workspace": stack.active,
        "focused": focused.map(|f| f.0),
        "fullscreen": fullscreen,
        "workspaces": workspaces,
    })
    .to_string()
}

/// フォーカスを実ウィンドウへ渡す (§8-5)。
/// SetForegroundWindow はフォアグラウンドロックで失敗することがあるため、
/// 失敗時はフォアグラウンドスレッドへ AttachThreadInput してリトライする。
fn focus_window(h: isize) {
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

/// Offscreen はサイズを維持したまま作業領域の外へ退避する (FR-3.3 offscreen)。
/// Viewport のどちら側にいるかに合わせて左右に分ける — 左の列が
/// 右の退避位置から滑り込んで見える違和感を防ぐ。Alt+Tab・タスクバーには残る。
fn park(p: Placement, work: Rect) -> Rect {
    match p {
        Placement::Visible(r) => r,
        Placement::Offscreen(r) => {
            let x = if r.x < work.x {
                work.x - r.w - 100
            } else {
                work.x + work.w + 100
            };
            Rect { x, ..r }
        }
    }
}

/// 実効作業領域 = OS の作業領域 − 画面余白 (FR-5.5)。
/// Zebar 等が AppBar 登録しない場合は margin 設定で空けてもらう。
fn effective_work(cfg: &config::Config) -> (Rect, Rect) {
    let (mut work, monitor) = primary_monitor();
    let (top, right, bottom, left) = cfg.margin;
    work.x += left;
    work.y += top;
    work.w = (work.w - left - right).max(200);
    work.h = (work.h - top - bottom).max(200);
    (work, monitor)
}

/// プライマリモニタの (作業領域, モニタ全面) を物理 px で返す。
/// 作業領域はタスクバー除く。全面は fullscreen (FR-4.7) 用。
fn primary_monitor() -> (Rect, Rect) {
    unsafe {
        let monitor = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
        let mut mi = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(monitor, &mut mi).as_bool() {
            tracing::error!("GetMonitorInfoW failed, falling back to 1920x1080");
            let fallback = Rect {
                x: 0,
                y: 0,
                w: 1920,
                h: 1080,
            };
            return (fallback, fallback);
        }
        (to_rect(mi.rcWork), to_rect(mi.rcMonitor))
    }
}

/// FR-3.8: ウィンドウを半透明にする。すでに同じアルファなら何もしない。
/// アプリ自身が WS_EX_LAYERED を使っている窓は壊さないよう触らない。
fn dim_window(h: u64, alpha: u8) {
    {
        let mut set = DIMMED.lock().unwrap_or_else(|p| p.into_inner());
        match set.iter_mut().find(|(x, _)| *x == h) {
            Some((_, a)) if *a == alpha => return,
            Some((_, a)) => *a = alpha, // アルファ値の変更 (ピン⇔通常の切替等)
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

/// FR-3.8: 半透明を解除して WS_EX_LAYERED も外す。
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

/// 半透明化した全ウィンドウを戻す (終了・設定変更時)。冪等。
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

/// FR-3.8: 強制終了で半透明が残ったウィンドウの修復。
/// 「LWA_ALPHA で alpha < 255」は自分の dim の取り残しとみなして不透明へ戻す
/// (アプリ自身の半透明と区別はできないが、管理対象のトップレベルが
/// SetLayeredWindowAttributes でフレームを薄くしている例は稀)。
fn heal_leftover_alpha(h: u64) {
    unsafe {
        let mut alpha = 0u8;
        let mut flags = LWA_ALPHA; // ダミー初期値 (上書きされる)
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

/// DWMWA_BORDER_COLOR の OS 既定値 (DWMWA_COLOR_DEFAULT)。
const BORDER_DEFAULT: u32 = 0xFFFF_FFFF;

/// 枠色機能が有効なら (フォーカス色, 非フォーカス色) を返す (FR-3.7)。
/// 片方だけ指定された場合、もう片方は OS 既定色。
fn border_colors(cfg: &config::Config) -> Option<(u32, u32)> {
    if cfg.border_focused.is_none() && cfg.border_unfocused.is_none() {
        return None;
    }
    Some((
        cfg.border_focused.unwrap_or(BORDER_DEFAULT),
        cfg.border_unfocused.unwrap_or(BORDER_DEFAULT),
    ))
}

/// ウィンドウの枠色を設定する (Win11 build 22000+)。
/// 失敗 (Win10・消滅済み・UIPI) は握りつぶす (FR-2.3)。
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

/// 自分が cloak したウィンドウか (CLOAKED イベントの自己無視用)。
fn cloaked_by_us(h: u64) -> bool {
    CLOAKED
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .contains(&h)
}

/// cloak 状態を変更する (FR-3.3)。すでにその状態なら何もしない。
fn cloak_set(h: u64, on: bool) {
    {
        let mut set = CLOAKED.lock().unwrap_or_else(|p| p.into_inner());
        if on {
            if set.contains(&h) {
                return;
            }
            set.push(h);
        } else {
            let Some(i) = set.iter().position(|&x| x == h) else {
                return;
            };
            set.swap_remove(i);
        }
    }
    shell_cloak(h, on);
}

/// 自分が cloak した全ウィンドウを戻す (終了・モード切替時)。冪等。
fn uncloak_all() {
    let drained = std::mem::take(&mut *CLOAKED.lock().unwrap_or_else(|p| p.into_inner()));
    for h in drained {
        shell_cloak(h, false);
    }
}

/// ウィンドウを完全に隠す / 戻す (FR-3.3 cloak モード)。
/// - SetCloak (シェル COM): 描画の停止。DwmSetWindowAttribute(DWMWA_CLOAK) は
///   他プロセスには E_ACCESSDENIED となり使えない
/// - WS_EX_TOOLWINDOW: Alt+Tab・タスクバーの一覧から消す (文書化された挙動)。
///   cloak だけでは一覧に残る。SetShowInSwitchers は Win11 25H2 で E_NOTIMPL
///
/// 管理対象は adopt 時点で WS_EX_TOOLWINDOW を持たない (decide() が float に
/// 落とす) ため、解除時に無条件でビットを消しても元のスタイルを壊さない。
/// 失敗 (消滅済み・view 不在) は握りつぶす (FR-2.3)。
pub(crate) fn shell_cloak(h: u64, on: bool) {
    // 順序が重要: TOOLWINDOW 付与中はシェルが application view を破棄して
    // GetViewForHwnd が失敗するため、隠すときは view があるうちに cloak →
    // TOOLWINDOW、戻すときは TOOLWINDOW を外して view を復活させてから uncloak。
    if on {
        if let Err(e) = com::set_cloak(h as isize, true) {
            tracing::warn!("SetCloak(true) failed for {h:#x}: {e}");
        }
        set_toolwindow(h, true);
    } else {
        set_toolwindow(h, false);
        if let Err(e) = com::set_cloak(h as isize, false) {
            // TYPE_E_ELEMENTNOTFOUND は view の非同期再作成中で、再作成時に
            // cloak もリセットされるため実質成功 (取りこぼしは Core ループの
            // 同期処理が修復する)
            if e.code() == TYPE_E_ELEMENTNOTFOUND {
                tracing::debug!("SetCloak(false) deferred for {h:#x} (view 再作成中)");
            } else {
                tracing::warn!("SetCloak(false) failed for {h:#x}: {e}");
            }
        }
    }
}

/// WS_EX_TOOLWINDOW ビットの付与 / 解除。既にその状態なら何もしない。
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

/// いま実際に cloak されているか (DWMWA_CLOAKED)。
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

/// FR-1.5: 復元先が作業領域の外なら、サイズを保ったまま画面内へ寄せる。
/// 記録時 (adopt) と復元時の両方で使い、画面外への「復元」を防ぐ。
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

/// レスキュー: 強制終了の取り残し (cloak・半透明) を一括で戻す
/// (`emakiwm --uncloak-all`)。UWP の正規の cloak (ApplicationFrameHost /
/// CoreWindow のサスペンド) は触らない。
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
            // 管理対象なのに半透明が残っている窓を不透明へ戻す (FR-3.8)
            heal_leftover_alpha(e.hwnd as u64);
        }
    }
    println!("--- {count} windows uncloaked");
}

/// FR-1.6: 復元用 Rect の永続化先。
fn state_path() -> PathBuf {
    let base = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".into());
    PathBuf::from(base).join("emakiwm").join("state.json")
}

/// FR-1.6: RESTORE_RECTS を state.json へ書き出す。
/// 管理ウィンドウの増減時に呼ぶ (プロセス kill・電源断に備える)。
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

/// FR-1.6: 強制終了後のレスキュー (`emakiwm --restore`)。
/// state.json に残った Rect へ、まだ生きているウィンドウを戻す。
/// cloak されたまま死んだ場合に備え uncloak もする。
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
    let (work, _) = primary_monitor();
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
        // 半透明のまま kill された場合に備え不透明へ戻す。アプリ自身が
        // layered を使う管理ウィンドウは稀という前提で無条件にクリアする
        clear_layered(h as u64);
        // 記録が画面外で汚染されていても必ず画面内へ戻す (FR-1.5)
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

/// FR-1.5 / NFR-4: panic・Ctrl+C・コンソールクローズで必ず復元する。
fn install_restore_hooks() {
    *RESTORE_RECTS.lock().unwrap() = Some(HashMap::new());

    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_all();
        default_hook(info);
    }));

    unsafe extern "system" fn ctrl_handler(_ctrl_type: u32) -> BOOL {
        // Core ループに終了を依頼。CTRL_CLOSE 等で猶予が短い場合に備え
        // ここでも直接復元しておく (冪等)
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

/// 管理中の全ウィンドウを取り込み時の Rect へ戻す。冪等。
/// panic 後の poisoned lock でも復元を諦めない (NFR-4)。
/// 正常に復元できたら state.json は不要なので消す (FR-1.6)。
fn restore_all() {
    uncloak_all();
    undim_all();
    let mut guard = RESTORE_RECTS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(map) = guard.take() else {
        return; // 復元済み
    };
    let (work, _) = primary_monitor();
    for (h, r) in &map {
        unsafe {
            if !IsWindow(Some(HWND(*h as _))).as_bool() {
                continue;
            }
            set_border_color(*h as u64, BORDER_DEFAULT);
            // 記録が画面外で汚染されていても必ず画面内へ戻す (FR-1.5)
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
