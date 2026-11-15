//! Overview (全ワークスペースの縮小一覧、FR-4.11) の表示。
//!
//! 他アプリのウィンドウは実際には縮小できないため、DWM サムネイル
//! (`DwmRegisterThumbnail`) を使う。自前の全画面ポップアップに各ウィンドウの
//! ライブな縮小画像を貼り、実ウィンドウは元の位置に置いたままその背後に隠す。
//!
//! スレッド構成:
//! - ウィンドウはフックスレッドで作る ([`create_window`])。マウス・キー入力を
//!   受けるにはメッセージポンプを持つスレッドの所有でなければならないため。
//!   入力は `WmEvent` として Core スレッドへ送るだけで、状態は触らない。
//! - 表示・更新・非表示は Core スレッドから呼ぶ ([`show`] / [`update`] / [`hide`])。
//!   同一プロセス内の別スレッド所有ウィンドウへの SetWindowPos / DWM 呼び出しは可能。
//!   ただし SetWindowPos 等は所有スレッドへ同期メッセージを送るため、
//!   ウィンドウ呼び出しの間は WM_PAINT 側と共有するロックを保持してはならない。
//!
//! サムネイルは DWM がウィンドウ自身の描画の上に合成するので、背景・行・
//! ハイライトの輪は WM_PAINT で描き、輪はサムネイルの外側へはみ出させる。

use std::sync::atomic::{AtomicI32, AtomicIsize, Ordering};
use std::sync::Mutex;

use emakiwm_core::layout::Rect;
use emakiwm_core::overview::Crop;
use windows::core::{w, BOOL};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DwmRegisterThumbnail, DwmUnregisterThumbnail, DwmUpdateThumbnailProperties,
    DWM_THUMBNAIL_PROPERTIES, DWM_TNP_OPACITY, DWM_TNP_RECTDESTINATION, DWM_TNP_RECTSOURCE,
    DWM_TNP_SOURCECLIENTAREAONLY, DWM_TNP_VISIBLE,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateSolidBrush, DeleteDC,
    DeleteObject, EndPaint, FillRect, InvalidateRect, SelectObject, PAINTSTRUCT, SRCCOPY,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{VK_ESCAPE, VK_RETURN};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, GetWindowRect, LoadCursorW, RegisterClassW, SetWindowPos,
    ShowWindow, HWND_TOPMOST, IDC_ARROW, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW,
    SW_HIDE, WM_ERASEBKGND, WM_KEYDOWN, WM_LBUTTONUP, WM_PAINT, WNDCLASSW, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_POPUP,
};

use crate::events::{self, WmEvent};

/// 背景色 (COLORREF 0x00BBGGRR)。
const BACKDROP: u32 = 0x0020_2020;
/// ワークスペース行の色。active はわずかに明るくする。
const ROW_COLOR: u32 = 0x002A_2A2A;
const ROW_ACTIVE_COLOR: u32 = 0x0036_3636;
/// フォーカス枠の輪の太さ (px)。
const RING_THICKNESS: i32 = 4;
/// focused の枠色が未設定のときの輪の色 (#4c8bf5)。
pub const DEFAULT_RING: u32 = 0x00F5_8B4C;

/// 作成済みの Overview ウィンドウ。0 = 未作成。
static HWND_OVERVIEW: AtomicIsize = AtomicIsize::new(0);
/// ウィンドウ左上のスクリーン座標 (クライアント座標への変換用)。
static ORIGIN_X: AtomicI32 = AtomicI32::new(0);
static ORIGIN_Y: AtomicI32 = AtomicI32::new(0);

/// WM_PAINT が描く内容 (スクリーン座標)。Core スレッドが書き、フックスレッドが読む。
#[derive(Default)]
struct Paint {
    /// (矩形, active か)
    rows: Vec<(Rect, bool)>,
    highlight: Option<Rect>,
    ring: u32,
}
static PAINT: Mutex<Paint> = Mutex::new(Paint {
    rows: Vec::new(),
    highlight: None,
    ring: DEFAULT_RING,
});

/// 登録済みサムネイル 1 枚。`thumb` が None なら登録に失敗した窓
/// (毎フレーム再試行しないよう記録だけ残す)。
struct Thumb {
    hwnd: u64,
    thumb: Option<isize>,
    /// 不可視リサイズ枠を除いた可視部分のソース矩形 (ウィンドウ左上基準)
    source: RECT,
}
static THUMBS: Mutex<Vec<Thumb>> = Mutex::new(Vec::new());

/// 1 枚ぶんのサムネイル指定。`rect` はクリップ済みのスクリーン座標の描画先、
/// `crop` は元の可視部分のうち描く割合。
#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub hwnd: u64,
    pub rect: Rect,
    pub crop: Crop,
}

/// Overview ウィンドウを作る。フックスレッド (メッセージポンプを持つスレッド) から呼ぶこと。
pub fn create_window() {
    unsafe {
        let instance = GetModuleHandleW(None).unwrap_or_default();
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hInstance: instance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: w!("emakiwm_overview"),
            ..Default::default()
        };
        if RegisterClassW(&wc) == 0 {
            tracing::warn!("Overview ウィンドウクラスの登録に失敗しました");
            return;
        }
        // WS_EX_TOOLWINDOW で Alt+Tab・タスクバーから外す。自プロセスのウィンドウなので
        // WM のフィルタ (自プロセス除外) によりタイル・フロートの対象にもならない
        match CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            w!("emakiwm_overview"),
            w!("emakiwm overview"),
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
            Ok(h) => HWND_OVERVIEW.store(h.0 as isize, Ordering::SeqCst),
            Err(e) => tracing::warn!("Overview ウィンドウの作成に失敗しました: {e}"),
        }
    }
}

fn hwnd() -> Option<HWND> {
    let h = HWND_OVERVIEW.load(Ordering::SeqCst);
    (h != 0).then_some(HWND(h as _))
}

/// ウィンドウを `bounds` (全モニターの外接矩形) に合わせる。まだ表示しない。
/// 最初のサムネイルを登録してから [`show`] で出すことで、背景だけが一瞬見えるのを避ける。
/// Core スレッドから呼ぶ。
pub fn place(bounds: Rect) {
    let Some(h) = hwnd() else {
        return;
    };
    ORIGIN_X.store(bounds.x, Ordering::SeqCst);
    ORIGIN_Y.store(bounds.y, Ordering::SeqCst);
    unsafe {
        let _ = SetWindowPos(
            h,
            Some(HWND_TOPMOST),
            bounds.x,
            bounds.y,
            bounds.w,
            bounds.h,
            SWP_NOACTIVATE,
        );
    }
}

/// [`place`] 済みのウィンドウを最前面に表示し、キー入力を受けられるよう前面化する。
/// Core スレッドから呼ぶ。
pub fn show() {
    let Some(h) = hwnd() else {
        return;
    };
    unsafe {
        let _ = SetWindowPos(
            h,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
        );
    }
    // Esc / Enter を受けるため前面化する (失敗時の迂回は focus_window が持つ)
    crate::wm::focus_window(h.0 as isize);
}

/// 内容を更新する。`items` は奥から手前の順。`rows` は (スクリーン座標の矩形, active か)、
/// `highlight` はフォーカス中の窓の矩形、`ring` はその輪の色 (COLORREF)。
/// 呼び出し側が差分を取り、変化があったときだけ呼ぶこと。
pub fn update(items: &[Item], rows: &[(Rect, bool)], highlight: Option<Rect>, ring: u32) {
    let Some(h) = hwnd() else {
        return;
    };
    let origin = (
        ORIGIN_X.load(Ordering::SeqCst),
        ORIGIN_Y.load(Ordering::SeqCst),
    );
    {
        let mut p = PAINT.lock().unwrap_or_else(|e| e.into_inner());
        p.rows = rows.to_vec();
        p.highlight = highlight;
        p.ring = ring;
    }
    // ロックは WM_PAINT 側と共有するので、ウィンドウ呼び出しの前に手放している
    unsafe {
        let _ = InvalidateRect(Some(h), None, false);
    }

    let mut thumbs = THUMBS.lock().unwrap_or_else(|e| e.into_inner());
    // 未登録の窓が増えた、または手前・奥の順序が変わったら登録し直す
    // (後に登録したものが手前に描かれるため、フロートをタイルの後ろに登録し直す必要がある)
    let known: Vec<u64> = thumbs
        .iter()
        .map(|t| t.hwnd)
        .filter(|x| items.iter().any(|i| i.hwnd == *x))
        .collect();
    let wanted: Vec<u64> = items.iter().map(|i| i.hwnd).collect();
    if known != wanted {
        unregister_all(&mut thumbs);
        for it in items {
            thumbs.push(register(h, it.hwnd));
        }
    }
    for t in thumbs.iter() {
        let Some(id) = t.thumb else { continue };
        let item = items.iter().find(|i| i.hwnd == t.hwnd);
        let props = match item {
            Some(it) => {
                let (sw, sh) = (
                    (t.source.right - t.source.left) as f32,
                    (t.source.bottom - t.source.top) as f32,
                );
                let at = |base: i32, size: f32, frac: f32| base + (size * frac).round() as i32;
                DWM_THUMBNAIL_PROPERTIES {
                    dwFlags: DWM_TNP_RECTDESTINATION
                        | DWM_TNP_RECTSOURCE
                        | DWM_TNP_VISIBLE
                        | DWM_TNP_OPACITY
                        | DWM_TNP_SOURCECLIENTAREAONLY,
                    rcDestination: RECT {
                        left: it.rect.x - origin.0,
                        top: it.rect.y - origin.1,
                        right: it.rect.x + it.rect.w - origin.0,
                        bottom: it.rect.y + it.rect.h - origin.1,
                    },
                    rcSource: RECT {
                        left: at(t.source.left, sw, it.crop.left),
                        top: at(t.source.top, sh, it.crop.top),
                        right: at(t.source.left, sw, it.crop.right),
                        bottom: at(t.source.top, sh, it.crop.bottom),
                    },
                    opacity: 255,
                    fVisible: BOOL(1),
                    fSourceClientAreaOnly: BOOL(0),
                }
            }
            // 画面外へ出た窓は登録を残したまま隠す (再登録の連発を避ける)
            None => DWM_THUMBNAIL_PROPERTIES {
                dwFlags: DWM_TNP_VISIBLE,
                fVisible: BOOL(0),
                ..Default::default()
            },
        };
        if let Err(e) = unsafe { DwmUpdateThumbnailProperties(id, &props) } {
            tracing::debug!("DwmUpdateThumbnailProperties({:#x}) failed: {e}", t.hwnd);
        }
    }
}

/// サムネイルを登録する。失敗 (窓が消えた直後など) は debug ログのみで続行する。
fn register(dest: HWND, src: u64) -> Thumb {
    let src_hwnd = HWND(src as _);
    let thumb = match unsafe { DwmRegisterThumbnail(dest, src_hwnd) } {
        Ok(id) => Some(id),
        Err(e) => {
            tracing::debug!("DwmRegisterThumbnail({src:#x}) failed: {e}");
            None
        }
    };
    // 不可視リサイズ枠 (§8-2) を除いた可視部分をソースにする
    let mut win = RECT::default();
    let ok = unsafe { GetWindowRect(src_hwnd, &mut win).is_ok() };
    let (w, h) = (win.right - win.left, win.bottom - win.top);
    let source = match (ok, crate::wm::measure_border(src as isize)) {
        (true, Some((l, t, r, b))) => RECT {
            left: l,
            top: t,
            right: w - r,
            bottom: h - b,
        },
        _ => RECT {
            left: 0,
            top: 0,
            right: w,
            bottom: h,
        },
    };
    Thumb {
        hwnd: src,
        thumb,
        source,
    }
}

fn unregister_all(thumbs: &mut Vec<Thumb>) {
    for t in thumbs.drain(..) {
        if let Some(id) = t.thumb {
            let _ = unsafe { DwmUnregisterThumbnail(id) };
        }
    }
}

/// サムネイルを全て解除してウィンドウを隠す。Core スレッドから呼ぶ。
pub fn hide() {
    unregister_all(&mut THUMBS.lock().unwrap_or_else(|e| e.into_inner()));
    if let Some(h) = hwnd() {
        unsafe {
            let _ = ShowWindow(h, SW_HIDE);
        }
    }
    let mut p = PAINT.lock().unwrap_or_else(|e| e.into_inner());
    p.rows.clear();
    p.highlight = None;
}

fn send(ev: WmEvent) {
    if let Some(tx) = events::sender() {
        let _ = tx.send(ev);
    }
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            paint(hwnd);
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            // lParam は (符号付き) クライアント座標。Core にはスクリーン座標で渡す
            let x = (lp.0 & 0xFFFF) as u16 as i16 as i32;
            let y = ((lp.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
            send(WmEvent::OverviewClick(
                x + ORIGIN_X.load(Ordering::SeqCst),
                y + ORIGIN_Y.load(Ordering::SeqCst),
            ));
            LRESULT(0)
        }
        WM_KEYDOWN if wp.0 == VK_ESCAPE.0 as usize || wp.0 == VK_RETURN.0 as usize => {
            send(WmEvent::OverviewClose);
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

/// 背景・ワークスペース行・ハイライトの輪を、ちらつかないようメモリ DC へ描いて転送する。
fn paint(hwnd: HWND) {
    // 描画内容はロックの中で複製して手放す (描画中に Core スレッドを待たせない)
    let (rows, highlight, ring) = {
        let p = PAINT.lock().unwrap_or_else(|e| e.into_inner());
        (p.rows.clone(), p.highlight, p.ring)
    };
    let (ox, oy) = (
        ORIGIN_X.load(Ordering::SeqCst),
        ORIGIN_Y.load(Ordering::SeqCst),
    );
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        let mut client = RECT::default();
        let _ = GetWindowRect(hwnd, &mut client);
        let (cw, ch) = (client.right - client.left, client.bottom - client.top);

        let mem = CreateCompatibleDC(Some(hdc));
        let bmp = CreateCompatibleBitmap(hdc, cw.max(1), ch.max(1));
        let old = SelectObject(mem, bmp.into());

        let fill = |r: RECT, color: u32| {
            let brush = CreateSolidBrush(COLORREF(color));
            FillRect(mem, &r, brush);
            let _ = DeleteObject(brush.into());
        };
        let to_rect = |r: Rect| RECT {
            left: r.x - ox,
            top: r.y - oy,
            right: r.x + r.w - ox,
            bottom: r.y + r.h - oy,
        };
        fill(
            RECT {
                left: 0,
                top: 0,
                right: cw,
                bottom: ch,
            },
            BACKDROP,
        );
        for (r, active) in &rows {
            fill(
                to_rect(*r),
                if *active { ROW_ACTIVE_COLOR } else { ROW_COLOR },
            );
        }
        if let Some(r) = highlight {
            // サムネイルの外側にリングを描く (サムネイルは DWM がこの上に合成する)
            let t = RING_THICKNESS;
            let o = to_rect(Rect {
                x: r.x - t,
                y: r.y - t,
                w: r.w + 2 * t,
                h: r.h + 2 * t,
            });
            let i = to_rect(r);
            for edge in [
                RECT { bottom: i.top, ..o },
                RECT { top: i.bottom, ..o },
                RECT {
                    top: i.top,
                    bottom: i.bottom,
                    right: i.left,
                    ..o
                },
                RECT {
                    top: i.top,
                    bottom: i.bottom,
                    left: i.right,
                    ..o
                },
            ] {
                fill(edge, ring);
            }
        }

        let _ = BitBlt(hdc, 0, 0, cw, ch, Some(mem), 0, 0, SRCCOPY);
        SelectObject(mem, old);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(mem);
        let _ = EndPaint(hwnd, &ps);
    }
}
