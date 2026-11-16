//! Overview (FR-4.11) の駆動: 開閉アニメーション・フレーム組み立て・クリック判定。
//! ウィンドウ自体 (サムネイル描画) は `overview.rs`。

use std::time::{Duration, Instant};

use emakiwm_core::anim::ease_out_cubic;
use emakiwm_core::layout::Rect;
use emakiwm_core::overview::{
    clip_crop, hit_test, intersect, overview_float, overview_scene, Hit, OverviewScene,
    OverviewTile,
};
use emakiwm_core::state::{Monitor, State};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{IsIconic, IsWindowVisible};

use crate::decor::border_colors;
use crate::wm::frame_rect;
use crate::{config, overview};

/// Overview (FR-4.11) の開閉アニメーション。`Some` の間は Overview ウィンドウが
/// 出ている (開き切った静止状態を含む)。`t` は 0 (通常) .. 1 (縮小表示)。
struct OverviewAnim {
    from: f32,
    to: f32,
    started: Instant,
    /// 補間中か。false なら `to` で静止していて、メインループはポーリングしない
    running: bool,
    /// Overview ウィンドウを表示済みか。最初のサムネイル登録を終えてから出すため
    shown: bool,
}

impl OverviewAnim {
    fn opening() -> Self {
        OverviewAnim {
            from: 0.0,
            to: 1.0,
            started: Instant::now(),
            running: true,
            shown: false,
        }
    }

    /// 現在の補間位置。duration が 0 なら即座に終点。
    fn t(&self, dur: Duration) -> f32 {
        if !self.running {
            return self.to;
        }
        let p = if dur.is_zero() {
            1.0
        } else {
            (self.started.elapsed().as_secs_f32() / dur.as_secs_f32()).min(1.0)
        };
        self.from + (self.to - self.from) * p
    }

    /// 開く/閉じる向きへ切り替える。途中からでも現在位置から折り返す。
    fn retarget(&mut self, open: bool, dur: Duration) {
        let to = if open { 1.0 } else { 0.0 };
        if self.to == to {
            return;
        }
        self.from = self.t(dur);
        self.to = to;
        self.started = Instant::now();
        self.running = true;
    }

    fn is_closing(&self) -> bool {
        self.to == 0.0
    }
}

/// Overview 1 フレームぶんの描画内容。前回送ったものと比べて差分があるときだけ反映する。
#[derive(PartialEq)]
struct OvFrame {
    items: Vec<overview::Item>,
    rows: Vec<(Rect, bool)>,
    highlight: Option<Rect>,
    ring: u32,
}

/// 全モニターの `monitor` 領域の外接矩形 (Overview ウィンドウの位置)。
fn monitors_bounds(monitors: &[Monitor]) -> Rect {
    let mut it = monitors.iter().map(|m| m.monitor);
    let first = it.next().unwrap_or_default();
    it.fold(first, |a, r| {
        let (x, y) = (a.x.min(r.x), a.y.min(r.y));
        let (right, bottom) = ((a.x + a.w).max(r.x + r.w), (a.y + a.h).max(r.y + r.h));
        Rect {
            x,
            y,
            w: right - x,
            h: bottom - y,
        }
    })
}

/// Overview の 1 フレームを組み立てる。タイル → フロートの順 (後ろほど手前)。
/// フロート窓は動き得るので、毎回実際のフレームを問い合わせて縮小する。
/// 返す `OverviewScene` / フロート一覧はクリックの当たり判定に使う。
fn build_overview(
    state: &State,
    cfg: &config::Config,
    zoom: f32,
) -> (OvFrame, OverviewScene, Vec<OverviewTile>) {
    let scene = overview_scene(state, zoom);
    let mut floats = Vec::new();
    for fl in state.floats() {
        let hwnd = HWND(fl.0 as _);
        if unsafe { !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() } {
            continue;
        }
        if let Some(frame) = frame_rect(fl.0 as isize) {
            floats.push(overview_float(state, fl, frame, zoom));
        }
    }
    let all = || scene.tiles.iter().chain(floats.iter());
    let items = all()
        .filter_map(|t| {
            clip_crop(t.rect, t.clip).map(|(rect, crop)| overview::Item {
                hwnd: t.id.0,
                rect,
                crop,
            })
        })
        .collect();
    let highlight = state
        .highlighted()
        .and_then(|id| all().find(|t| t.id == id))
        .and_then(|t| intersect(t.rect, t.clip));
    let ring = border_colors(cfg)
        .map(|c| c.0)
        .filter(|c| *c < 0xFFFF_FFFE)
        .unwrap_or(overview::DEFAULT_RING);
    let frame = OvFrame {
        items,
        rows: scene.rows.iter().map(|r| (r.rect, r.active)).collect(),
        highlight,
        ring,
    };
    (frame, scene, floats)
}

/// Overview の駆動状態一式。開閉状態、前回 Overview へ送った内容、
/// クリック判定用の直近シーンをまとめて持つ。
#[derive(Default)]
pub(crate) struct OvCtl {
    anim: Option<OverviewAnim>,
    sent: Option<OvFrame>,
    hit: Option<(OverviewScene, Vec<OverviewTile>)>,
}

impl OvCtl {
    /// Overview ウィンドウが出ているか (開き切った静止状態・閉じ途中を含む)。
    pub(crate) fn is_open(&self) -> bool {
        self.anim.is_some()
    }

    /// 開閉アニメーション中か (メインループがフレーム間隔で回る必要があるか)。
    pub(crate) fn is_running(&self) -> bool {
        self.anim.as_ref().is_some_and(|o| o.running)
    }

    /// 開閉の切替。開いている/開く途中なら閉じ始め、閉じる途中なら開き直す。
    pub(crate) fn toggle(&mut self, monitors: &[Monitor], dur: Duration) {
        match self.anim.as_mut() {
            Some(o) => o.retarget(o.is_closing(), dur),
            None => {
                overview::place(monitors_bounds(monitors));
                self.anim = Some(OverviewAnim::opening());
            }
        }
    }

    /// 閉じ始める (開いていなければ何もしない)。
    pub(crate) fn close(&mut self, dur: Duration) {
        if let Some(o) = self.anim.as_mut() {
            o.retarget(false, dur);
        }
    }

    /// クリック選択。結果は State へ反映するだけで、実際のフォーカスは閉じ終わりに適用する。
    pub(crate) fn click(&mut self, state: &mut State, x: i32, y: i32, dur: Duration) {
        if let Some(o) = self.anim.as_mut().filter(|o| !o.is_closing()) {
            let hit = self
                .hit
                .as_ref()
                .map_or(Hit::None, |(scene, floats)| hit_test(scene, floats, x, y));
            match hit {
                Hit::Float(id) => {
                    state.note_float_focus(id);
                    state.set_float_foreground(Some(id));
                }
                Hit::Tile(id) => {
                    state.focus_window_by_id(id);
                }
                Hit::Row { monitor, workspace } => {
                    state.activate_workspace(monitor, workspace);
                }
                Hit::None => {}
            }
            o.retarget(false, dur);
        }
    }

    /// 開閉アニメーションを 1 コマ進め、シーンが変わったときだけサムネイルを更新する。
    /// 閉じ終わった周回で true を返す (呼び出し側が保留していたフォーカスを適用する)。
    pub(crate) fn tick(&mut self, state: &State, cfg: &config::Config) -> bool {
        let Some(o) = self.anim.as_mut() else {
            return false;
        };
        let t = o.t(cfg.anim);
        if o.running && o.started.elapsed() >= cfg.anim {
            o.running = false;
            o.from = o.to;
        }
        if !o.running && o.is_closing() {
            overview::hide();
            self.anim = None;
            self.sent = None;
            self.hit = None;
            tracing::debug!("overview closed");
            true
        } else {
            let zoom = 1.0 + (cfg.overview_zoom - 1.0) * ease_out_cubic(t);
            let (frame, scene, floats) = build_overview(state, cfg, zoom);
            self.hit = Some((scene, floats));
            if self.sent.as_ref() != Some(&frame) {
                overview::update(&frame.items, &frame.rows, frame.highlight, frame.ring);
                self.sent = Some(frame);
            }
            if !o.shown {
                o.shown = true;
                overview::show();
                tracing::debug!("overview opened");
            }
            false
        }
    }
}
