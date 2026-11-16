//! フロート窓の Win32 側 (最前面指定・登録・方向キー操作)。
//!
//! `FLOATS` は `restore_all` (別スレッド) からも `reset_all_floats` 経由で参照される。
//! ここに新しいロックを足さないこと。

use std::sync::Mutex;

use emakiwm_core::filter::Decision;
use emakiwm_core::layout::{nudge_rect, pick_in_direction, FocusDir, Rect, WindowId};
use emakiwm_core::state::{FloatFacts, ForegroundProbe, State};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetWindowLongPtrW, GetWindowRect, IsIconic, IsWindow, IsWindowVisible,
    SetWindowPos, GWL_EXSTYLE, HWND_NOTOPMOST, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOSIZE, SWP_NOZORDER, WS_EX_TOPMOST,
};

use crate::decor::{border_colors, set_border_color, undim_window, BORDER_DEFAULT};
use crate::restore::RESTORE_RECTS;
use crate::wm::{frame_rect, intersects_any, to_rect};
use crate::{config, scan};

/// toggle-floating でフロート化した全ウィンドウと、WM が最前面 (TOPMOST) を付けたか。
/// タイルへ戻すとき・終了時に最前面指定を外し (WM が付けたものだけ)、枠色を戻す。
/// フロート窓は RESTORE_RECTS から外れるため、終了時の後始末はこの記録で行う。
static FLOATS: Mutex<Vec<(u64, bool)>> = Mutex::new(Vec::new());

/// 受動的フロート窓 (ダイアログ・ルールの float 等、WM が浮かせたわけではない窓) として
/// 装飾・キー移動の対象にするか。ルール判定が Float で、可視・自プロセス外・
/// キャプション付き・ツールウィンドウでなく、最小化されていないもの。
pub(crate) fn passive_float_candidate(e: &scan::ScanEntry) -> bool {
    let caption = emakiwm_core::filter::WS_CAPTION;
    matches!(e.decision, Decision::Float(_))
        && e.info.is_visible
        && !e.info.is_own_process
        && e.info.style & caption == caption
        && e.info.ex_style & emakiwm_core::filter::WS_EX_TOOLWINDOW == 0
        && e.elevated != Some(true)
        && unsafe { !IsIconic(HWND(e.hwnd as _)).as_bool() }
}

/// 受動的フロート窓の登録判断に要る事実を Win32 から集める (判断は `State` が行う)。
fn float_facts(e: &scan::ScanEntry) -> FloatFacts {
    FloatFacts {
        candidate: passive_float_candidate(e),
        foreground: unsafe { GetForegroundWindow() } == HWND(e.hwnd as _),
    }
}

/// `State` が受動フロート窓として登録した窓の装飾 (最前面・枠色・終了時の後始末の記録)。
/// 位置・Z オーダーには触らない。
fn decorate_passive_float(cfg: &config::Config, e: &scan::ScanEntry) {
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
    tracing::info!(
        "passive float {:#x} {} \"{}\"",
        e.hwnd,
        e.info.exe_name.as_deref().unwrap_or("?"),
        e.info.title
    );
}

/// 受動的フロート窓として未登録なら登録する (登録の判断は `State::register_passive_float`)。
/// 登録したら true。
pub(crate) fn register_passive_float(
    state: &mut State,
    cfg: &config::Config,
    e: &scan::ScanEntry,
) -> bool {
    if !state.register_passive_float(WindowId(e.hwnd as u64), float_facts(e)) {
        return false;
    }
    decorate_passive_float(cfg, e);
    true
}

/// 前面になった窓がフロート窓として扱えるかを調べて `State` へ渡す。
/// 管理外で未登録の窓のときだけ inspect する。新たに登録したら true。
pub(crate) fn foreground_float(state: &mut State, cfg: &config::Config, h: isize) -> bool {
    // 登録できた場合に装飾するため、1 回の inspect の結果を持ち出す
    let mut inspected: Option<scan::ScanEntry> = None;
    let registered = state.float_foreground(WindowId(h as u64), || {
        if HWND(h as _).is_invalid() {
            return None;
        }
        let e = scan::inspect(HWND(h as _), &cfg.rules);
        let probe = ForegroundProbe {
            tileable: tileable_ok(&e),
            facts: float_facts(&e),
        };
        inspected = Some(e);
        Some(probe)
    });
    if registered {
        if let Some(e) = &inspected {
            decorate_passive_float(cfg, e);
        }
    }
    registered
}

/// 受動的フロート窓の登録が外れた (`State` 側は済み) ときの後始末:
/// 装飾 (半透明・枠色) と記録を戻す。
pub(crate) fn release_passive_float(h: u64) {
    undim_window(h);
    set_border_color(h, BORDER_DEFAULT);
    // 記録を消し、WM が付けた最前面指定を外す
    unfloat_window(h as isize);
}

/// 受動的フロート窓の集合をスキャン結果へ合わせる (起動時・設定の再読み込み時)。
/// 変化があれば true。
pub(crate) fn sync_passive_floats(
    state: &mut State,
    cfg: &config::Config,
    scanned: &[scan::ScanEntry],
) -> bool {
    let facts: Vec<(WindowId, FloatFacts)> = scanned
        .iter()
        .map(|e| (WindowId(e.hwnd as u64), float_facts(e)))
        .collect();
    let sync = state.sync_passive_floats(&facts);
    for id in &sync.dropped {
        release_passive_float(id.0);
    }
    for id in &sync.added {
        if let Some(e) = scanned.iter().find(|e| e.hwnd as u64 == id.0) {
            decorate_passive_float(cfg, e);
        }
    }
    !sync.dropped.is_empty() || !sync.added.is_empty()
}

/// フロート窓の記録 (終了時の後始末用)。記録済みなら何もしない。
pub(crate) fn record_float(h: u64, topmost: bool) {
    let mut floats = FLOATS.lock().unwrap_or_else(|p| p.into_inner());
    if !floats.iter().any(|&(x, _)| x == h) {
        floats.push((h, topmost));
    }
}

/// WM が最前面にしたフロート窓から TOPMOST が外れていたら付け直す。
/// アプリが自分で最前面指定を落とすことがあるため、前面窓が変わるたびに確認する。
pub(crate) fn reassert_float_topmost() {
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

/// 管理外の前面ウィンドウがフロート窓として扱えるか (focus-floating の戻り先、
/// toggle-floating の取り込み対象)。可視・トップレベル・キャプション付きのもの。
/// 設定ルールは見ない (ルールでフロートにした窓も手動でタイルへ入れられる)。
pub(crate) fn inspect_tileable(h: isize) -> Option<scan::ScanEntry> {
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
pub(crate) fn float_window(h: isize, work: Rect) {
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
pub(crate) fn unfloat_window(h: isize) {
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
pub(crate) fn forget_float_record(h: u64) {
    FLOATS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .retain(|&(x, _)| x != h);
}

/// 前面のフロート窓から見て `dir` 側にいちばん近い (可視・非最小化の) フロート窓を返す。
pub(crate) fn focus_float_in_direction(fg: HWND, dir: FocusDir, state: &State) -> Option<WindowId> {
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
pub(crate) fn nudge_float(hwnd: HWND, dir: FocusDir, state: &State) {
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
pub(crate) fn set_topmost(h: u64) {
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

pub(crate) fn clear_topmost(h: u64) {
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
pub(crate) fn reset_all_floats() {
    let drained = std::mem::take(&mut *FLOATS.lock().unwrap_or_else(|p| p.into_inner()));
    for (h, topmost) in drained {
        set_border_color(h, BORDER_DEFAULT);
        if topmost {
            clear_topmost(h);
        }
    }
}

/// 記録済みのフロート窓 (hwnd, WM が最前面を付けたか) のスナップショット。
/// state.json への永続化用。
pub(crate) fn float_records() -> Vec<(u64, bool)> {
    FLOATS.lock().unwrap_or_else(|p| p.into_inner()).clone()
}
