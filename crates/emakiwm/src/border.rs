//! 管理ウィンドウの太枠オーバーレイ (FR-3.7 border_thickness)。
//!
//! DWMWA_BORDER_COLOR の枠線は太さが OS 固定のため、太い枠は自前の
//! クリックスルーなポップアップウィンドウで描く。topmost にはせず
//! Z オーダーで対象ウィンドウの直下に挿入する — 対象より上にある
//! ダイアログ等には枠が乗らない。
//!
//! 形状は per-pixel alpha (UpdateLayeredWindow): GDI+ で角丸リングを
//! アンチエイリアス描画した ARGB ビットマップを貼る。リングの内側は
//! 完全に透明なので、非フォーカスウィンドウの半透明化 (FR-3.8) でも
//! 背後に枠色が透けない。内縁の角丸半径は Win11 標準 (約 8px) に合わせる。
//!
//! 枠ウィンドウはプールとして使い回し、毎フレーム [`update_all`] で
//! 表示中の管理ウィンドウへ再割当てする。ウィンドウ作成はメッセージポンプを
//! 持つ専用スレッドでしか行えないため、message-only のマネージャへ
//! SendMessage (同期) で依頼する。プロセス終了で全部消えるため後始末は不要。

use std::ffi::c_void;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{Mutex, OnceLock};

use emakiwm_core::layout::Rect;
use windows::core::w;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC, SelectObject,
    AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, DIB_RGB_COLORS,
};
use windows::Win32::Graphics::GdiPlus::{
    FillModeAlternate, GdipAddPathArc, GdipClosePathFigure, GdipCreateBitmapFromScan0,
    GdipCreatePath, GdipCreatePen1, GdipDeleteGraphics, GdipDeletePath, GdipDeletePen,
    GdipDisposeImage, GdipDrawPath, GdipGetImageGraphicsContext, GdipSetSmoothingMode,
    GdiplusStartup, GdiplusStartupInput, GpBitmap, GpGraphics, GpImage, GpPath, GpPen,
    SmoothingModeAntiAlias, UnitPixel,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, RegisterClassW, SendMessageW,
    SetWindowPos, ShowWindow, UpdateLayeredWindow, HWND_MESSAGE, MSG, SWP_NOACTIVATE, SWP_NOSIZE,
    SWP_SHOWWINDOW, SW_HIDE, ULW_ALPHA, WM_APP, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT, WS_POPUP,
};

/// マネージャへの「枠ウィンドウを 1 枚作れ」依頼。LRESULT で hwnd が返る。
const WM_CREATE_BORDER: u32 = WM_APP + 1;

/// GDI+ の PixelFormat32bppPARGB (premultiplied ARGB)。
const PIXEL_FORMAT_32BPP_PARGB: i32 = 0xE200B;

/// Win11 標準ウィンドウの角丸半径 (96dpi で約 8px)。枠の内縁をこれに合わせる。
const WIN11_CORNER_RADIUS: f32 = 8.0;

/// 生成依頼を受ける message-only ウィンドウ (border スレッド所属)。0 = 未作成。
static MANAGER: AtomicIsize = AtomicIsize::new(0);

/// プール内の枠ウィンドウ 1 枚。形状・色は前回値を覚えて再描画を省く。
struct Slot {
    hwnd: isize,
    shape: (i32, i32, i32, u32),
}
static SLOTS: Mutex<Vec<Slot>> = Mutex::new(Vec::new());

/// 表示する枠 1 枚ぶんの指定。
pub struct Item {
    /// 枠を付ける対象ウィンドウ (Z オーダーの挿入位置にも使う)
    pub owner: u64,
    /// 対象の論理フレーム Rect
    pub rect: Rect,
    /// 枠色 (COLORREF)
    pub color: u32,
}

/// 表示中の管理ウィンドウ全部の枠を現在位置へ追従させる。毎フレーム呼んでよい。
/// 空スライスで全部隠す。
pub fn update_all(items: &[Item], thickness: i32) {
    // 出すものがなく、スレッドも未起動なら何もしない
    if items.is_empty() && MANAGER.load(Ordering::SeqCst) == 0 {
        return;
    }
    ensure_thread();
    let manager = MANAGER.load(Ordering::SeqCst);
    if manager == 0 {
        return; // スレッド起動直後でまだ作成中
    }
    let mut slots = SLOTS.lock().unwrap();
    // 足りない分は border スレッドに作らせる (SendMessage は同期実行)
    while slots.len() < items.len() {
        let h = unsafe { SendMessageW(HWND(manager as _), WM_CREATE_BORDER, None, None) }.0;
        if h == 0 {
            return;
        }
        slots.push(Slot {
            hwnd: h,
            shape: (0, 0, 0, 0),
        });
    }
    let t = thickness.max(1);
    for (slot, item) in slots.iter_mut().zip(items) {
        let hwnd = HWND(slot.hwnd as _);
        let r = item.rect;
        let (x, y, w, h) = (r.x - t, r.y - t, r.w + 2 * t, r.h + 2 * t);
        unsafe {
            // サイズ・太さ・色が変わったときだけビットマップを描き直す
            if slot.shape != (w, h, t, item.color) {
                slot.shape = (w, h, t, item.color);
                render(hwnd, x, y, w, h, t, item.color);
            }
            // 対象ウィンドウの直下へ挿入 — 対象より上のウィンドウには枠が乗らない
            let _ = SetWindowPos(
                hwnd,
                Some(HWND(item.owner as _)),
                x,
                y,
                0,
                0,
                SWP_NOACTIVATE | SWP_NOSIZE | SWP_SHOWWINDOW,
            );
        }
    }
    // 余りは隠してプールに残す (再利用)
    for slot in slots.iter().skip(items.len()) {
        unsafe {
            let _ = ShowWindow(HWND(slot.hwnd as _), SW_HIDE);
        }
    }
}

/// 角丸リングを ARGB で描いて UpdateLayeredWindow で貼る。
/// ペンの中心線を内縁半径 + 太さ/2 の丸角矩形に通すことで、
/// 内縁が対象ウィンドウの角丸に沿い、外縁はその外側を同心で回る。
unsafe fn render(hwnd: HWND, x: i32, y: i32, w: i32, h: i32, t: i32, color: u32) {
    // COLORREF (0x00BBGGRR) → GDI+ ARGB (0xAARRGGBB)
    let (r, g, b) = (color & 0xFF, (color >> 8) & 0xFF, (color >> 16) & 0xFF);
    let argb = 0xFF00_0000 | (r << 16) | (g << 8) | b;

    unsafe {
        let screen = GetDC(None);
        let mem = CreateCompatibleDC(Some(screen));
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut c_void = std::ptr::null_mut();
        let Ok(dib) = CreateDIBSection(Some(screen), &bmi, DIB_RGB_COLORS, &mut bits, None, 0)
        else {
            let _ = DeleteDC(mem);
            ReleaseDC(None, screen);
            return;
        };
        let old = SelectObject(mem, dib.into());

        // DIB のメモリへ GDI+ で直接描く (PARGB なのでアルファがそのまま載る)
        let mut bmp: *mut GpBitmap = std::ptr::null_mut();
        let _ = GdipCreateBitmapFromScan0(
            w,
            h,
            w * 4,
            PIXEL_FORMAT_32BPP_PARGB,
            Some(bits as *const u8),
            &mut bmp,
        );
        let mut gr: *mut GpGraphics = std::ptr::null_mut();
        let _ = GdipGetImageGraphicsContext(bmp as *mut GpImage, &mut gr);
        let _ = GdipSetSmoothingMode(gr, SmoothingModeAntiAlias);
        let mut pen: *mut GpPen = std::ptr::null_mut();
        let _ = GdipCreatePen1(argb, t as f32, UnitPixel, &mut pen);
        let mut path: *mut GpPath = std::ptr::null_mut();
        let _ = GdipCreatePath(FillModeAlternate, &mut path);

        let ht = t as f32 / 2.0;
        let rad = WIN11_CORNER_RADIUS + ht;
        let d = rad * 2.0;
        let (x0, y0) = (ht, ht);
        let (x1, y1) = (w as f32 - ht, h as f32 - ht);
        let _ = GdipAddPathArc(path, x0, y0, d, d, 180.0, 90.0);
        let _ = GdipAddPathArc(path, x1 - d, y0, d, d, 270.0, 90.0);
        let _ = GdipAddPathArc(path, x1 - d, y1 - d, d, d, 0.0, 90.0);
        let _ = GdipAddPathArc(path, x0, y1 - d, d, d, 90.0, 90.0);
        let _ = GdipClosePathFigure(path);
        let _ = GdipDrawPath(gr, pen, path);

        let _ = GdipDeletePath(path);
        let _ = GdipDeletePen(pen);
        let _ = GdipDeleteGraphics(gr);
        let _ = GdipDisposeImage(bmp as *mut GpImage);

        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let pt_dst = POINT { x, y };
        let size = SIZE { cx: w, cy: h };
        let pt_src = POINT { x: 0, y: 0 };
        let _ = UpdateLayeredWindow(
            hwnd,
            None,
            Some(&pt_dst),
            Some(&size),
            Some(mem),
            Some(&pt_src),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        );

        SelectObject(mem, old);
        let _ = DeleteObject(dib.into());
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
    }
}

fn ensure_thread() {
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        std::thread::spawn(|| unsafe { thread_main() });
    });
}

const CLASS_NAME: windows::core::PCWSTR = w!("emakiwm_border");

unsafe fn thread_main() {
    unsafe {
        // GDI+ 初期化 (プロセスで 1 回。render は他スレッドからも呼ばれる)
        let mut token = 0usize;
        let input = GdiplusStartupInput {
            GdiplusVersion: 1,
            ..Default::default()
        };
        let _ = GdiplusStartup(&mut token, &input, std::ptr::null_mut());

        let instance = GetModuleHandleW(None).unwrap_or_default();
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hInstance: instance.into(),
            lpszClassName: CLASS_NAME,
            ..Default::default()
        };
        if RegisterClassW(&wc) == 0 {
            tracing::error!("RegisterClassW(emakiwm_border) failed");
            return;
        }
        // 生成依頼の受け口 (message-only)
        let manager = match CreateWindowExW(
            Default::default(),
            CLASS_NAME,
            w!(""),
            WS_POPUP,
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(instance.into()),
            None,
        ) {
            Ok(h) => h,
            Err(e) => {
                tracing::error!("border manager の作成に失敗: {e}");
                return;
            }
        };
        MANAGER.store(manager.0 as isize, Ordering::SeqCst);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = DispatchMessageW(&msg);
        }
    }
}

/// 枠ウィンドウを 1 枚作る (border スレッドで実行される)。
/// 中身は UpdateLayeredWindow で貼るため WM_PAINT は不要。
unsafe fn create_border_window() -> isize {
    let instance = unsafe { GetModuleHandleW(None) }.unwrap_or_default();
    // クリックスルー (LAYERED + TRANSPARENT)・非アクティブ・Alt+Tab 非表示。
    // topmost にはしない (Z オーダーは update_all が対象の直下へ維持する)
    match unsafe {
        CreateWindowExW(
            WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TRANSPARENT | WS_EX_LAYERED,
            CLASS_NAME,
            w!(""),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance.into()),
            None,
        )
    } {
        Ok(h) => h.0 as isize,
        Err(e) => {
            tracing::error!("border window の作成に失敗: {e}");
            0
        }
    }
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_CREATE_BORDER => LRESULT(unsafe { create_border_window() }),
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}
