//! Precision Touchpad の Raw Input 購読と 3 本指スワイプ → コマンド変換。
//!
//! OS のジェスチャ設定 (キー割当) を経由せず、HID レポートを直接読んで
//! ワークスペース切替等を発火する。タッチパッドは HID Usage Page 0x0D /
//! Usage 0x05 の Raw Input としてアプリにも生データを配るので、
//! events.rs の不可視ウィンドウで WM_INPUT を受けて解析する。
//! スワイプ判定の状態機械は emakiwm_core::gesture (純粋ロジック) に置く。
//!
//! 注意: Raw Input は観測のみで OS のジェスチャ処理は止まらない。
//! 二重反応を避けるには Windows 設定で 3 本指ジェスチャを「なし」にする。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use emakiwm_core::gesture::{SwipeDir, SwipeTracker};
use windows::Win32::Devices::HumanInterfaceDevice::{
    HidP_GetCaps, HidP_GetUsageValue, HidP_GetValueCaps, HidP_Input, HIDP_CAPS,
    HIDP_STATUS_SUCCESS, HIDP_VALUE_CAPS, PHIDP_PREPARSED_DATA,
};
use windows::Win32::Foundation::{HANDLE, HWND, LPARAM};
use windows::Win32::UI::Input::{
    GetRawInputData, GetRawInputDeviceInfoW, RegisterRawInputDevices, HRAWINPUT, RAWINPUT,
    RAWINPUTDEVICE, RAWINPUTHEADER, RIDEV_INPUTSINK, RIDI_PREPARSEDDATA, RID_INPUT, RIM_TYPEHID,
};

use crate::events::WmEvent;

// HID Usage (Digitizer Page / Generic Desktop Page)
const PAGE_DIGITIZER: u16 = 0x0D;
const USAGE_TOUCHPAD: u16 = 0x05;
const USAGE_CONTACT_COUNT: u16 = 0x54;
const PAGE_GENERIC: u16 = 0x01;
const USAGE_X: u16 = 0x30;
const USAGE_Y: u16 = 0x31;

/// 発火に必要な移動量 (パッド辺長に対する比率)。
/// 10cm 幅のパッドで約 1.5cm。誤発火とのバランスで決めた値
const SWIPE_THRESHOLD: f32 = 0.15;

/// 3 本指スワイプ 4 方向への割当 (config [gestures] から構築)。
#[derive(Debug, Clone, Default)]
pub struct GestureBinds {
    pub up: Option<WmEvent>,
    pub down: Option<WmEvent>,
    pub left: Option<WmEvent>,
    pub right: Option<WmEvent>,
}

impl GestureBinds {
    pub fn is_empty(&self) -> bool {
        self.up.is_none() && self.down.is_none() && self.left.is_none() && self.right.is_none()
    }
}

/// タッチパッド 1 台ぶんの HID 解析情報。
/// preparsed データはデバイスごとに固定なので初回 WM_INPUT で取得して保持する。
struct Device {
    /// RIDI_PREPARSEDDATA の生バッファ (HidP_* が参照する)
    preparsed: Vec<u8>,
    /// Contact Count (0x0D:0x54) の link collection
    link_count: u16,
    /// 第 1 接触点の X/Y (0x01:0x30/0x31) の link collection
    link_xy: u16,
    /// 正規化用の論理レンジ
    x_min: f32,
    x_range: f32,
    y_min: f32,
    y_range: f32,
}

struct State {
    binds: GestureBinds,
    tracker: SwipeTracker,
    /// hDevice → 解析情報。None = タッチパッドとして解析できなかったデバイス
    devices: HashMap<isize, Option<Device>>,
}

/// WM_INPUT はフックスレッドの wndproc からのみ触るが、
/// static に置くため Mutex で包む (競合は実質発生しない)。
static STATE: OnceLock<Mutex<State>> = OnceLock::new();

/// タッチパッドの Raw Input を hwnd 宛に登録する。
/// RIDEV_INPUTSINK で非フォアグラウンドでも受け取る (WM は前面を持たないため)。
pub fn register(hwnd: HWND, binds: GestureBinds) {
    let rid = RAWINPUTDEVICE {
        usUsagePage: PAGE_DIGITIZER,
        usUsage: USAGE_TOUCHPAD,
        dwFlags: RIDEV_INPUTSINK,
        hwndTarget: hwnd,
    };
    match unsafe { RegisterRawInputDevices(&[rid], std::mem::size_of::<RAWINPUTDEVICE>() as u32) } {
        Ok(()) => {
            let _ = STATE.set(Mutex::new(State {
                binds,
                tracker: SwipeTracker::new(SWIPE_THRESHOLD),
                devices: HashMap::new(),
            }));
            tracing::debug!("touchpad raw input registered");
        }
        Err(e) => tracing::warn!("タッチパッド Raw Input の登録に失敗しました: {e}"),
    }
}

/// WM_INPUT ハンドラ。HID レポートを解析してスワイプ判定へ流し、
/// 確定したら対応コマンドを Core へ送る。now_ms はメッセージ時刻 (ms)。
pub fn handle_input(lp: LPARAM, now_ms: u32) {
    let Some(state) = STATE.get() else { return };

    // 可変長の RAWINPUT を 2 段階で取得する
    let hri = HRAWINPUT(lp.0 as *mut _);
    let header_size = std::mem::size_of::<RAWINPUTHEADER>() as u32;
    let mut size = 0u32;
    unsafe { GetRawInputData(hri, RID_INPUT, None, &mut size, header_size) };
    if size == 0 {
        return;
    }
    let mut buf = vec![0u8; size as usize];
    let got = unsafe {
        GetRawInputData(
            hri,
            RID_INPUT,
            Some(buf.as_mut_ptr() as *mut _),
            &mut size,
            header_size,
        )
    };
    if got == 0 || got == u32::MAX {
        return;
    }
    let ri = unsafe { &*(buf.as_ptr() as *const RAWINPUT) };
    if ri.header.dwType != RIM_TYPEHID.0 {
        return;
    }
    let hid = unsafe { &ri.data.hid };
    // bRawData は可変長配列の先頭。バッファ内オフセットで各レポートを切り出す
    let data_off =
        unsafe { std::ptr::addr_of!(ri.data.hid.bRawData) as usize - buf.as_ptr() as usize };
    let report_size = hid.dwSizeHid as usize;
    let count = hid.dwCount as usize;
    if report_size == 0 || data_off + report_size * count > buf.len() {
        return;
    }

    let mut st = state.lock().unwrap_or_else(|p| p.into_inner());
    let State {
        binds,
        tracker,
        devices,
    } = &mut *st;
    let dev = devices
        .entry(ri.header.hDevice.0 as isize)
        .or_insert_with(|| load_device(ri.header.hDevice));
    let Some(dev) = dev else { return };

    for i in 0..count {
        let report = &buf[data_off + i * report_size..data_off + (i + 1) * report_size];
        let pp = PHIDP_PREPARSED_DATA(dev.preparsed.as_ptr() as isize);
        let Some(contacts) = usage_value(
            pp,
            PAGE_DIGITIZER,
            dev.link_count,
            USAGE_CONTACT_COUNT,
            report,
        ) else {
            continue;
        };
        // 継続パケット (contacts=0) は座標を読まない (gesture.rs 参照)
        let (mut x, mut y) = (0.0, 0.0);
        if contacts != 0 {
            let (Some(rx), Some(ry)) = (
                usage_value(pp, PAGE_GENERIC, dev.link_xy, USAGE_X, report),
                usage_value(pp, PAGE_GENERIC, dev.link_xy, USAGE_Y, report),
            ) else {
                continue;
            };
            x = ((rx as f32 - dev.x_min) / dev.x_range).clamp(0.0, 1.0);
            y = ((ry as f32 - dev.y_min) / dev.y_range).clamp(0.0, 1.0);
        }
        if let Some(dir) = tracker.update(contacts, x, y, now_ms) {
            let ev = match dir {
                SwipeDir::Up => &binds.up,
                SwipeDir::Down => &binds.down,
                SwipeDir::Left => &binds.left,
                SwipeDir::Right => &binds.right,
            };
            tracing::debug!("swipe3 {dir:?}");
            if let (Some(ev), Some(tx)) = (ev, crate::events::sender()) {
                let _ = tx.send(ev.clone());
            }
        }
    }
}

/// HID レポートから値を 1 つ取り出す。usage がレポートにない場合は None。
fn usage_value(
    pp: PHIDP_PREPARSED_DATA,
    page: u16,
    link: u16,
    usage: u16,
    report: &[u8],
) -> Option<u32> {
    let mut v = 0u32;
    let status =
        unsafe { HidP_GetUsageValue(HidP_Input, page, Some(link), usage, &mut v, pp, report) };
    (status == HIDP_STATUS_SUCCESS).then_some(v)
}

/// デバイスの preparsed データを取得し、Contact Count と第 1 接触点の
/// X/Y の在り処 (link collection) と論理レンジを調べる。
fn load_device(h: HANDLE) -> Option<Device> {
    unsafe {
        let mut size = 0u32;
        GetRawInputDeviceInfoW(Some(h), RIDI_PREPARSEDDATA, None, &mut size);
        if size == 0 {
            return None;
        }
        let mut preparsed = vec![0u8; size as usize];
        let got = GetRawInputDeviceInfoW(
            Some(h),
            RIDI_PREPARSEDDATA,
            Some(preparsed.as_mut_ptr() as *mut _),
            &mut size,
        );
        if got == 0 || got == u32::MAX {
            return None;
        }
        let pp = PHIDP_PREPARSED_DATA(preparsed.as_ptr() as isize);

        let mut caps = HIDP_CAPS::default();
        if HidP_GetCaps(pp, &mut caps) != HIDP_STATUS_SUCCESS {
            return None;
        }
        let mut n = caps.NumberInputValueCaps;
        let mut vcaps = vec![HIDP_VALUE_CAPS::default(); n as usize];
        if HidP_GetValueCaps(HidP_Input, vcaps.as_mut_ptr(), &mut n, pp) != HIDP_STATUS_SUCCESS {
            return None;
        }
        vcaps.truncate(n as usize);

        let has_usage = |c: &HIDP_VALUE_CAPS, page: u16, usage: u16| {
            c.UsagePage == page
                && if c.IsRange {
                    c.Anonymous.Range.UsageMin <= usage && usage <= c.Anonymous.Range.UsageMax
                } else {
                    c.Anonymous.NotRange.Usage == usage
                }
        };
        let count_cap = vcaps
            .iter()
            .find(|c| has_usage(c, PAGE_DIGITIZER, USAGE_CONTACT_COUNT))?;
        // 接触点は指ごとに別 collection に並ぶ。最小の collection を第 1 接触点とする
        let x_cap = vcaps
            .iter()
            .filter(|c| has_usage(c, PAGE_GENERIC, USAGE_X))
            .min_by_key(|c| c.LinkCollection)?;
        let y_cap = vcaps.iter().find(|c| {
            has_usage(c, PAGE_GENERIC, USAGE_Y) && c.LinkCollection == x_cap.LinkCollection
        })?;
        let x_range = (x_cap.LogicalMax - x_cap.LogicalMin) as f32;
        let y_range = (y_cap.LogicalMax - y_cap.LogicalMin) as f32;
        if x_range <= 0.0 || y_range <= 0.0 {
            return None;
        }
        tracing::debug!(
            "touchpad device: x={}..{} y={}..{} (link {})",
            x_cap.LogicalMin,
            x_cap.LogicalMax,
            y_cap.LogicalMin,
            y_cap.LogicalMax,
            x_cap.LinkCollection
        );
        Some(Device {
            link_count: count_cap.LinkCollection,
            link_xy: x_cap.LinkCollection,
            x_min: x_cap.LogicalMin as f32,
            x_range,
            y_min: y_cap.LogicalMin as f32,
            y_range,
            preparsed,
        })
    }
}
