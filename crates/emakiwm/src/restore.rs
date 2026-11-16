//! 終了時復元 (FR-1.5, FR-1.6, NFR-4)。
//!
//! `restore_all` は panic フック・コンソール Ctrl ハンドラ (別スレッド) から呼ばれる。
//! 触る static (`RESTORE_RECTS` / `CLOAKED` / `FLOATS` / `DIMMED`) はいずれも
//! グローバルの Mutex で、Runtime (メインスレッドの状態) には依存しない。
//! この経路に新しいロックを足さないこと。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use emakiwm_core::filter::Decision;
use emakiwm_core::layout::Rect;
use windows::core::BOOL;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::System::Console::SetConsoleCtrlHandler;
use windows::Win32::UI::WindowsAndMessaging::{
    IsWindow, SetWindowPos, SWP_NOACTIVATE, SWP_NOZORDER,
};

use crate::cloak::{shell_cloak, uncloak_all};
use crate::decor::{
    clear_layered, heal_leftover_alpha, set_border_color, undim_all, BORDER_DEFAULT,
};
use crate::events::{self, WmEvent};
use crate::floats::{clear_topmost, float_records, reset_all_floats};
use crate::wm::{intersects, primary_monitor_fallback};
use crate::{scan, tray};

/// 終了時復元用の元 Rect (FR-1.5)。
/// panic フックと Ctrl+C ハンドラから参照するためグローバルに持つ。
pub(crate) static RESTORE_RECTS: Mutex<Option<HashMap<isize, RECT>>> = Mutex::new(None);

pub(crate) fn clamp_into_work(r: RECT, work: Rect) -> RECT {
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

pub(crate) fn persist_restore_rects() {
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
    for (h, topmost) in float_records() {
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

pub(crate) fn install_restore_hooks() {
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

pub(crate) fn restore_all() {
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
