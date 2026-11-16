//! 装飾 (FR-3.7, FR-3.8): 枠色・半透明化・太枠オーバーレイ。
//!
//! `DIMMED` は `restore_all` (別スレッド) からも `undim_all` 経由で参照される。
//! ここに新しいロックを足さないこと。

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::Mutex;

use emakiwm_core::layout::Rect;
use emakiwm_core::state::State;
use windows::Win32::Foundation::{COLORREF, HWND};
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_BORDER_COLOR};
use windows::Win32::UI::WindowsAndMessaging::{
    GetLayeredWindowAttributes, GetWindowLongPtrW, IsIconic, IsWindowVisible,
    SetLayeredWindowAttributes, SetWindowLongPtrW, GWL_EXSTYLE, LWA_ALPHA, WS_EX_LAYERED,
};

use crate::wm::{frame_rect, intersects, intersects_any};
use crate::{border, config};

/// FR-3.8 半透明化のために WS_EX_LAYERED を付与したウィンドウと現在のアルファ。
/// 終了時に必ず不透明へ戻す。
static DIMMED: Mutex<Vec<(u64, u8)>> = Mutex::new(Vec::new());

pub(crate) fn dim_window(h: u64, alpha: u8) {
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

pub(crate) fn undim_window(h: u64) {
    {
        let mut set = DIMMED.lock().unwrap_or_else(|p| p.into_inner());
        let Some(i) = set.iter().position(|(x, _)| *x == h) else {
            return;
        };
        set.swap_remove(i);
    }
    clear_layered(h);
}

pub(crate) fn undim_all() {
    let drained = std::mem::take(&mut *DIMMED.lock().unwrap_or_else(|p| p.into_inner()));
    for (h, _) in drained {
        clear_layered(h);
    }
}

pub(crate) fn clear_layered(h: u64) {
    unsafe {
        let hwnd = HWND(h as _);
        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA);
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex & !(WS_EX_LAYERED.0 as isize));
    }
}

pub(crate) fn heal_leftover_alpha(h: u64) {
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

pub(crate) const BORDER_DEFAULT: u32 = 0xFFFF_FFFF;

pub(crate) fn border_colors(cfg: &config::Config) -> Option<(u32, u32)> {
    if cfg.border_focused.is_none() && cfg.border_unfocused.is_none() {
        return None;
    }
    Some((
        cfg.border_focused.unwrap_or(BORDER_DEFAULT),
        cfg.border_unfocused.unwrap_or(BORDER_DEFAULT),
    ))
}

pub(crate) fn set_border_color(h: u64, color: u32) {
    unsafe {
        let _ = DwmSetWindowAttribute(
            HWND(h as _),
            DWMWA_BORDER_COLOR,
            &color as *const u32 as *const c_void,
            size_of::<u32>() as u32,
        );
    }
}

/// 枠色の追従: 強調すべき窓 (`highlighted`) が変わったら、前の窓を非フォーカス色へ、
/// 新しい窓をフォーカス色へ切り替える。`bordered` は現在フォーカス色を付けている窓。
pub(crate) fn sync_border_colors(cfg: &config::Config, state: &State, bordered: &mut Option<u64>) {
    if let Some((focused_color, unfocused_color)) = border_colors(cfg) {
        let cur = state.highlighted().map(|f| f.0);
        if cur != *bordered {
            if let Some(prev) = *bordered {
                set_border_color(prev, unfocused_color);
            }
            if let Some(now) = cur {
                set_border_color(now, focused_color);
            }
            *bordered = cur;
        }
    }
}

/// FR-3.8: 非フォーカスウィンドウの半透明化 (タイルとフロート窓)。
pub(crate) fn update_dimming(
    cfg: &config::Config,
    state: &State,
    targets: &[(u64, Rect)],
    pinned: &[u64],
) {
    if cfg.unfocused_alpha.is_some() || !pinned.is_empty() {
        let fullscreen = state.fullscreen.map(|f| f.0);
        let f = state.highlighted().map(|f| f.0);
        for &(h, _) in targets {
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
}

/// FR-3.7: 太枠オーバーレイ。Overview 表示中・枠なし設定のときは全て消す。
/// `dragging`: マウスで移動・リサイズ中のフロート窓 (枠が遅れるので出さない)。
pub(crate) fn update_overlay(
    cfg: &config::Config,
    state: &State,
    targets: &[(u64, Rect)],
    visual: &HashMap<u64, Rect>,
    own_map: &HashMap<u64, usize>,
    dragging: Option<u64>,
    overview_open: bool,
) {
    if !overview_open && cfg.border_thickness > 0 && border_colors(cfg).is_some() {
        let fullscreen = state.fullscreen.map(|f| f.0);
        let (fc, uc) = border_colors(cfg).unwrap();
        let mut items = Vec::new();
        for &(h, _) in targets {
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
