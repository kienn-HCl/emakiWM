//! cloak (FR-3.3): 自モニターに写らないウィンドウの隠蔽。
//!
//! `CLOAKED` は自分が cloak したウィンドウの実状態で、`restore_all` (panic フック・
//! Ctrl ハンドラ = 別スレッド) からも参照される。ここに新しいロックを足さないこと。

use std::sync::Mutex;

use emakiwm_core::layout::{Rect, WindowId};
use emakiwm_core::state::Monitor;
use std::collections::HashMap;
use std::ffi::c_void;
use windows::Win32::Foundation::{HWND, TYPE_E_ELEMENTNOTFOUND};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowLongPtrW, IsWindow, SetWindowLongPtrW, GWL_EXSTYLE,
};

use crate::com;
use crate::wm::intersects;

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
pub(crate) enum Hide {
    /// 表示 (uncloak)
    Show,
    /// 描画のみ停止。Alt+Tab・タスクバーに残る (active ワークスペースの viewport 外)
    Render,
    /// 完全に隠す。Alt+Tab からも消える (非 active ワークスペース)
    Full,
}

/// cloak モードでの隠蔽レベルを決める。
/// 自モニターに写っていれば表示。写っていない場合、active ワークスペースの
/// ウィンドウは描画停止のみ (Alt+Tab に残す)、非 active は完全に隠す。
pub(crate) fn hide_level(
    h: u64,
    r: Rect,
    own_map: &HashMap<u64, usize>,
    monitors: &[Monitor],
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

pub(crate) fn cloaked_by_us(h: u64) -> bool {
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
pub(crate) fn cloak_set(h: u64, level: Hide) {
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
pub(crate) fn retry_pending_uncloaks() -> bool {
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

pub(crate) fn uncloak_all() {
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

pub(crate) fn window_cloaked(h: isize) -> bool {
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
