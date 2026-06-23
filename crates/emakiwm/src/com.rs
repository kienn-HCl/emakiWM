//! 非公開シェル COM 経由の cloak (FR-3.3)。
//!
//! `DwmSetWindowAttribute(DWMWA_CLOAK)` は他プロセスのウィンドウには
//! E_ACCESSDENIED となるため、シェルの `IApplicationView::SetCloak` を使う。
//! 非公開 API のため、vtable のスロット位置合わせのダミーメソッドを持つ。
//!
//! COM はスレッドごとに初期化が必要 (復元は Ctrl ハンドラ等の別スレッドからも
//! 走る) ため、ビューコレクションは thread_local に持つ。

use std::cell::OnceCell;

use windows::core::{interface, Error, IUnknown, IUnknown_Vtbl, Interface, Result, GUID, HRESULT};
use windows::Win32::Foundation::E_FAIL;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, IServiceProvider, CLSCTX_ALL, COINIT_APARTMENTTHREADED,
};

/// `IServiceProvider` を実装するシェルの CLSID。
const CLSID_IMMERSIVE_SHELL: GUID = GUID::from_u128(0xC2F03A33_21F5_47FA_B4BB_156362A2F239);

#[interface("1841C6D7-4F9D-42C0-AF41-8747538F10E5")]
unsafe trait IApplicationViewCollection: IUnknown {
    unsafe fn m1(&self);
    unsafe fn m2(&self);
    unsafe fn m3(&self);
    unsafe fn get_view_for_hwnd(
        &self,
        window: isize,
        view: *mut Option<IApplicationView>,
    ) -> HRESULT;
}

/// (IInspectable 3 + SetFocus..GetVisibility 6 = m1..m9)。
/// 注意: SetShowInSwitchers (Alt+Tab 一覧フラグ) は Win11 25H2 (26200) で
/// E_NOTIMPL のため使えない。一覧から消すのは WS_EX_TOOLWINDOW で行う (wm.rs)。
#[interface("372E1D3B-38D3-42E4-A15B-8AB2B178F513")]
unsafe trait IApplicationView: IUnknown {
    unsafe fn m1(&self);
    unsafe fn m2(&self);
    unsafe fn m3(&self);
    unsafe fn m4(&self);
    unsafe fn m5(&self);
    unsafe fn m6(&self);
    unsafe fn m7(&self);
    unsafe fn m8(&self);
    unsafe fn m9(&self);
    unsafe fn set_cloak(&self, cloak_type: u32, cloak_flag: i32) -> HRESULT;
}

thread_local! {
    static VIEW_COLLECTION: OnceCell<Option<IApplicationViewCollection>> =
        const { OnceCell::new() };
}

fn init_collection() -> Option<IApplicationViewCollection> {
    unsafe {
        // 既に初期化済み (S_FALSE / RPC_E_CHANGED_MODE) でも続行してよい
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let provider: IServiceProvider =
            CoCreateInstance(&CLSID_IMMERSIVE_SHELL, None, CLSCTX_ALL).ok()?;
        provider.QueryService(&IApplicationViewCollection::IID).ok()
    }
}

/// hwnd → IApplicationView。クロージャに渡して COM 呼び出しを実行する。
fn with_view<T>(hwnd: isize, f: impl FnOnce(&IApplicationView) -> Result<T>) -> Result<T> {
    VIEW_COLLECTION.with(|cell| {
        let Some(col) = cell.get_or_init(init_collection) else {
            return Err(Error::new(
                E_FAIL,
                "IApplicationViewCollection を取得できません",
            ));
        };
        unsafe {
            let mut view: Option<IApplicationView> = None;
            col.get_view_for_hwnd(hwnd, &mut view).ok()?;
            let view = view.ok_or_else(|| Error::new(E_FAIL, "application view が空"))?;
            f(&view)
        }
    })
}

/// 指定ウィンドウを shell cloak / uncloak する (描画の停止のみ)。
/// cloak_type=1 (DEFAULT)、flag は 2 = cloak / 0 = uncloak。
/// 注意: cloak しても Alt+Tab / タスクビューの一覧からは消えない
/// (仮想デスクトップの一覧フィルタはデスクトップ所属ベースの別機構)。
pub fn set_cloak(hwnd: isize, on: bool) -> Result<()> {
    with_view(hwnd, |view| unsafe {
        view.set_cloak(1, if on { 2 } else { 0 }).ok()
    })
}
