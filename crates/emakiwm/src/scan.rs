//! Phase 0: トップレベルウィンドウのスキャンと管理対象判定の dry-run 出力。
//!
//! Win32 から属性を収集して `emakiwm_core::filter::WindowInfo` に詰め、
//! 判定結果と座標 (GetWindowRect / DWMWA_EXTENDED_FRAME_BOUNDS) を表出力する。
//! 不可視フレーム差分 (REQUIREMENTS.md §8-2) の実測値を取るのが目的。

use emakiwm_core::filter::{decide, Decision, Rule, WindowInfo};
use windows::core::BOOL;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, RECT};
use windows::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS,
};
use windows::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
use windows::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, OpenProcessToken, QueryFullProcessImageNameW,
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClassNameW, GetWindow, GetWindowLongPtrW, GetWindowRect, GetWindowTextW,
    GetWindowThreadProcessId, IsWindowVisible, GWL_EXSTYLE, GWL_STYLE, GW_OWNER,
};

/// 1 ウィンドウぶんのスキャン結果。
pub struct ScanEntry {
    pub hwnd: isize,
    pub info: WindowInfo,
    pub decision: Decision,
    /// None = トークンを開けず不明 (≒ 昇格プロセスの可能性大、FR-2.3 の untracked 候補)
    pub elevated: Option<bool>,
    pub rect: RECT,
    /// DWMWA_EXTENDED_FRAME_BOUNDS。取得失敗時 None
    pub frame: Option<RECT>,
}

pub fn dry_run(verbose: bool, rules: &[Rule]) {
    let entries = scan(rules);

    let mut managed = 0;
    for e in &entries {
        let (tag, reason) = match e.decision {
            Decision::Manage => {
                managed += 1;
                ("MANAGE", "")
            }
            Decision::Float(r) => ("FLOAT ", r),
            Decision::Ignore(r) => {
                if !verbose {
                    continue;
                }
                ("IGNORE", r)
            }
        };

        let elevated = match e.elevated {
            Some(true) => " elevated=yes(untracked)",
            Some(false) => "",
            None => " elevated=unknown(untracked)",
        };
        println!(
            "{} hwnd={:#08x} exe={} class=\"{}\"{}",
            tag,
            e.hwnd,
            e.info.exe_name.as_deref().unwrap_or("?"),
            e.info.class_name,
            elevated,
        );
        println!("       title=\"{}\"", e.info.title);
        if !reason.is_empty() {
            println!("       reason: {reason}");
        }
        let r = &e.rect;
        match &e.frame {
            Some(f) => {
                // 不可視フレーム補正量 (§8-2): GetWindowRect と実フレームの差
                println!(
                    "       rect=({},{},{},{}) frame=({},{},{},{}) invisible_border=(l{} t{} r{} b{})",
                    r.left, r.top, r.right, r.bottom,
                    f.left, f.top, f.right, f.bottom,
                    f.left - r.left, f.top - r.top, r.right - f.right, r.bottom - f.bottom,
                );
            }
            None => {
                println!(
                    "       rect=({},{},{},{}) frame=<unavailable>",
                    r.left, r.top, r.right, r.bottom
                );
            }
        }
    }
    println!("--- {} windows scanned, {} managed", entries.len(), managed);
}

pub fn scan(rules: &[Rule]) -> Vec<ScanEntry> {
    let mut hwnds: Vec<isize> = Vec::new();

    unsafe extern "system" fn enum_cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let hwnds = unsafe { &mut *(lparam.0 as *mut Vec<isize>) };
        hwnds.push(hwnd.0 as isize);
        true.into()
    }

    unsafe {
        EnumWindows(
            Some(enum_cb),
            LPARAM(&mut hwnds as *mut Vec<isize> as isize),
        )
        .expect("EnumWindows failed");
    }
    hwnds
        .into_iter()
        .map(|h| inspect(HWND(h as _), rules))
        .collect()
}

/// ウィンドウタイトルだけを取得する (IPC の state 出力用)。
pub fn window_title(hwnd: HWND) -> String {
    let mut buf = [0u16; 512];
    let len = unsafe { GetWindowTextW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..len.max(0) as usize])
}

/// 1 つの HWND から WindowInfo と付帯情報を収集する。
/// 個々の API の失敗はデフォルト値で吸収する (FR-2.3: クラッシュ禁止)。
pub fn inspect(hwnd: HWND, rules: &[Rule]) -> ScanEntry {
    unsafe {
        let mut title_buf = [0u16; 512];
        let len = GetWindowTextW(hwnd, &mut title_buf);
        let title = String::from_utf16_lossy(&title_buf[..len.max(0) as usize]);

        let mut class_buf = [0u16; 256];
        let len = GetClassNameW(hwnd, &mut class_buf);
        let class_name = String::from_utf16_lossy(&class_buf[..len.max(0) as usize]);

        let style = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32;
        let ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;

        let mut cloaked = 0u32;
        let _ = DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut u32 as *mut _,
            size_of::<u32>() as u32,
        );

        let has_owner = GetWindow(hwnd, GW_OWNER).is_ok_and(|h| !h.is_invalid());

        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        let is_own_process = pid == GetCurrentProcessId();

        let (exe_name, elevated) = inspect_process(pid);

        let mut rect = RECT::default();
        let _ = GetWindowRect(hwnd, &mut rect);

        let mut frame = RECT::default();
        let frame = DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut frame as *mut RECT as *mut _,
            size_of::<RECT>() as u32,
        )
        .map(|_| frame)
        .ok();

        let info = WindowInfo {
            title,
            class_name,
            exe_name,
            style,
            ex_style,
            is_visible: IsWindowVisible(hwnd).as_bool(),
            is_cloaked: cloaked != 0,
            has_owner,
            is_own_process,
        };
        let decision = decide(&info, rules);

        ScanEntry {
            hwnd: hwnd.0 as isize,
            info,
            decision,
            elevated,
            rect,
            frame,
        }
    }
}

/// exe ファイル名と昇格状態を取得する。
/// OpenProcess / OpenProcessToken の失敗 (UIPI 等) は None で返す。
fn inspect_process(pid: u32) -> (Option<String>, Option<bool>) {
    unsafe {
        let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return (None, None);
        };

        let mut path_buf = [0u16; 1024];
        let mut path_len = path_buf.len() as u32;
        let exe_name = QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(path_buf.as_mut_ptr()),
            &mut path_len,
        )
        .ok()
        .map(|_| String::from_utf16_lossy(&path_buf[..path_len as usize]))
        .and_then(|p| p.rsplit('\\').next().map(str::to_owned));

        let mut token = HANDLE::default();
        let elevated = OpenProcessToken(process, TOKEN_QUERY, &mut token)
            .ok()
            .and_then(|_| {
                let mut elevation = TOKEN_ELEVATION::default();
                let mut ret_len = 0u32;
                let r = GetTokenInformation(
                    token,
                    TokenElevation,
                    Some(&mut elevation as *mut TOKEN_ELEVATION as *mut _),
                    size_of::<TOKEN_ELEVATION>() as u32,
                    &mut ret_len,
                );
                let _ = CloseHandle(token);
                r.ok().map(|_| elevation.TokenIsElevated != 0)
            });

        let _ = CloseHandle(process);
        (exe_name, elevated)
    }
}
