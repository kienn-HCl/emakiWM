//! Overview (全ワークスペースを縮小して一覧する表示、FR-4.11) の純粋なシーン計算。
//!
//! 実ウィンドウは縮小できないため、Win32 層は DWM サムネイルで「縮小した絵」を
//! 描く。ここでは各ウィンドウ・各ワークスペースを画面上のどこへ描くか (論理座標) だけを
//! 計算する。`zoom == 1.0` のとき active ワークスペースのタイルは通常配置の
//! 論理位置と厳密に一致する (開閉アニメーションを継ぎ目なくつなぐための性質)。
//!
//! 各モニターのワークスペース列は縦に積み、active を作業領域の中央に置く。
//! 他モニターへはみ出さないよう、描画は常にそのモニターの領域 (`clip`) でクリップする。

use crate::layout::{Rect, WindowId};
use crate::state::State;

/// ワークスペース行の間隔 = 作業領域の高さ / この値 (縮小前の寸法)。
const ROW_GAP_DIV: i32 = 10;

/// 縮小表示したワークスペース 1 枚 (モニターの領域でクリップ済み)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverviewRow {
    pub monitor: usize,
    pub workspace: usize,
    pub rect: Rect,
    pub active: bool,
}

/// 縮小表示したウィンドウ 1 枚。`rect` は未クリップの縮小後の矩形で、
/// 実際に描ける範囲は `rect ∩ clip` (サムネイルの切り出しは [`clip_crop`])。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverviewTile {
    pub id: WindowId,
    pub monitor: usize,
    pub workspace: usize,
    pub rect: Rect,
    /// 描画を許す領域 (属するモニターの全面)
    pub clip: Rect,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OverviewScene {
    pub rows: Vec<OverviewRow>,
    pub tiles: Vec<OverviewTile>,
}

/// 当たり判定の結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    Float(WindowId),
    Tile(WindowId),
    Row { monitor: usize, workspace: usize },
    None,
}

/// `center` を中心に位置とサイズを `zoom` 倍する。zoom == 1.0 なら恒等。
/// 辺ごとに丸めるので、隣り合う矩形の間に隙間・重なりが生じない。
pub fn scale_about(r: Rect, center: (i32, i32), zoom: f32) -> Rect {
    let z = zoom as f64;
    let s = |v: i32, c: i32| c + ((v - c) as f64 * z).round() as i32;
    let left = s(r.x, center.0);
    let top = s(r.y, center.1);
    Rect {
        x: left,
        y: top,
        w: s(r.x + r.w, center.0) - left,
        h: s(r.y + r.h, center.1) - top,
    }
}

fn center_of(r: Rect) -> (i32, i32) {
    (r.x + r.w / 2, r.y + r.h / 2)
}

/// 2 つの矩形の共通部分。重ならなければ None。
pub fn intersect(a: Rect, b: Rect) -> Option<Rect> {
    let x = a.x.max(b.x);
    let y = a.y.max(b.y);
    let r = (a.x + a.w).min(b.x + b.w);
    let bt = (a.y + a.h).min(b.y + b.h);
    (r > x && bt > y).then_some(Rect {
        x,
        y,
        w: r - x,
        h: bt - y,
    })
}

/// 全モニター・全ワークスペースの縮小配置を計算する。
/// タイルは Strip 内の論理位置 (viewport 外の Column も含む) を使い、
/// モニター領域でクリップされて見えなくなるものは含めない。
pub fn overview_scene(state: &State, zoom: f32) -> OverviewScene {
    let gap = state.gap();
    let mut scene = OverviewScene::default();
    for (mi, ms) in state.monitors.iter().enumerate() {
        let c = center_of(ms.work);
        let step = ms.work.h + ms.work.h / ROW_GAP_DIV;
        let active = ms.stack.active;
        for (i, strip) in ms.stack.strips.iter().enumerate() {
            let dy = (i as i32 - active as i32) * step;
            let place = |r: Rect| scale_about(Rect { y: r.y + dy, ..r }, c, zoom);
            if let Some(rect) = intersect(place(ms.work), ms.monitor) {
                scene.rows.push(OverviewRow {
                    monitor: mi,
                    workspace: i,
                    rect,
                    active: i == active,
                });
            }
            for (id, p) in strip.project(ms.work, gap) {
                let logical = match p {
                    crate::layout::Placement::Visible(r)
                    | crate::layout::Placement::Offscreen(r) => r,
                };
                let logical = if Some(id) == state.fullscreen && i == active {
                    ms.monitor
                } else {
                    logical
                };
                let rect = place(logical);
                if intersect(rect, ms.monitor).is_some() {
                    scene.tiles.push(OverviewTile {
                        id,
                        monitor: mi,
                        workspace: i,
                        rect,
                        clip: ms.monitor,
                    });
                }
            }
        }
    }
    scene
}

/// フロート窓 (実際のフレーム `frame`) の縮小表示。中心を含むモニター
/// (なければ active モニター) の作業領域の中心を基準に拡縮する。
pub fn overview_float(state: &State, id: WindowId, frame: Rect, zoom: f32) -> OverviewTile {
    let (cx, cy) = center_of(frame);
    let mi = state
        .monitors
        .iter()
        .position(|m| {
            cx >= m.monitor.x
                && cx < m.monitor.x + m.monitor.w
                && cy >= m.monitor.y
                && cy < m.monitor.y + m.monitor.h
        })
        .unwrap_or(state.active_monitor);
    let ms = &state.monitors[mi];
    OverviewTile {
        id,
        monitor: mi,
        workspace: ms.stack.active,
        rect: scale_about(frame, center_of(ms.work), zoom),
        clip: ms.monitor,
    }
}

/// サムネイルの切り出し範囲。元の矩形に対する割合 (0.0..=1.0) で、
/// 見える部分が `left..right` × `top..bottom`。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Crop {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

/// `rect` を `clip` で切ったときの描画先と、元サムネイルのうち見える割合。
/// 重ならなければ None。
pub fn clip_crop(rect: Rect, clip: Rect) -> Option<(Rect, Crop)> {
    let dest = intersect(rect, clip)?;
    let (w, h) = (rect.w.max(1) as f32, rect.h.max(1) as f32);
    let crop = Crop {
        left: (dest.x - rect.x) as f32 / w,
        top: (dest.y - rect.y) as f32 / h,
        right: (dest.x + dest.w - rect.x) as f32 / w,
        bottom: (dest.y + dest.h - rect.y) as f32 / h,
    };
    Some((dest, crop))
}

fn contains(r: Rect, x: i32, y: i32) -> bool {
    x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h
}

fn hits(t: &OverviewTile, x: i32, y: i32) -> bool {
    intersect(t.rect, t.clip).is_some_and(|r| contains(r, x, y))
}

/// 画面座標 (x, y) に何があるか。手前から: フロート → タイル (後ろ勝ち) → ワークスペース行。
/// `floats` は描画順 (後ろほど手前) のフロート窓。
pub fn hit_test(scene: &OverviewScene, floats: &[OverviewTile], x: i32, y: i32) -> Hit {
    if let Some(t) = floats.iter().rev().find(|t| hits(t, x, y)) {
        return Hit::Float(t.id);
    }
    if let Some(t) = scene.tiles.iter().rev().find(|t| hits(t, x, y)) {
        return Hit::Tile(t.id);
    }
    if let Some(r) = scene.rows.iter().find(|r| contains(r.rect, x, y)) {
        return Hit::Row {
            monitor: r.monitor,
            workspace: r.workspace,
        };
    }
    Hit::None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::MonitorGeom;
    use std::collections::HashMap;

    const GAP: i32 = 8;

    fn geom(hmonitor: isize, x: i32) -> MonitorGeom {
        let r = Rect {
            x,
            y: 0,
            w: 1920,
            h: 1080,
        };
        MonitorGeom {
            hmonitor,
            work: r,
            monitor: r,
        }
    }

    fn w(n: u64) -> WindowId {
        WindowId(n)
    }

    fn tile(s: &OverviewScene, id: u64) -> OverviewTile {
        *s.tiles.iter().find(|t| t.id == w(id)).unwrap()
    }

    /// 1 モニター: ワークスペース 0 に 1,2、ワークスペース 1 に 3。active は 0。
    fn one_monitor() -> State {
        let mut s = State::new(&[geom(1, 0)], GAP, 0.5);
        s.adopt_initial(0, w(1));
        s.adopt_initial(0, w(2));
        s.adopt_initial(0, w(3));
        s.set_initial_focus(w(3));
        s.move_to_workspace(true);
        s.switch_workspace(false);
        s
    }

    #[test]
    fn scale_about_center_and_identity() {
        let r = Rect {
            x: 100,
            y: 200,
            w: 400,
            h: 300,
        };
        assert_eq!(scale_about(r, (960, 540), 1.0), r);
        // 中心そのものを含む矩形は中心が動かない
        let c = Rect {
            x: 860,
            y: 440,
            w: 200,
            h: 200,
        };
        assert_eq!(
            scale_about(c, (960, 540), 0.5),
            Rect {
                x: 910,
                y: 490,
                w: 100,
                h: 100
            }
        );
        assert_eq!(
            scale_about(r, (0, 0), 0.5),
            Rect {
                x: 50,
                y: 100,
                w: 200,
                h: 150
            }
        );
    }

    #[test]
    fn zoom_one_matches_normal_layout_of_active_workspace() {
        let s = one_monitor();
        let scene = overview_scene(&s, 1.0);
        let ms = &s.monitors[0];
        for (id, p) in ms.stack.strips[ms.stack.active].project(ms.work, GAP) {
            let r = match p {
                crate::layout::Placement::Visible(r) | crate::layout::Placement::Offscreen(r) => r,
            };
            let t = tile(&scene, id.0);
            assert_eq!(t.rect, r);
            assert_eq!(t.workspace, 0);
        }
        // active 行は作業領域そのもの
        let row = scene.rows.iter().find(|r| r.active).unwrap();
        assert_eq!(row.rect, ms.work);
    }

    #[test]
    fn zoom_scales_about_work_center() {
        let s = one_monitor();
        let full = tile(&overview_scene(&s, 1.0), 1).rect;
        let half = tile(&overview_scene(&s, 0.5), 1).rect;
        let (cx, cy) = (960, 540);
        assert_eq!(half.x, cx + (full.x - cx) / 2);
        assert_eq!(half.y, cy + (full.y - cy) / 2);
        assert_eq!(half.w, full.w / 2);
        assert_eq!(half.h, full.h / 2);
    }

    #[test]
    fn rows_stack_vertically_with_active_centered() {
        let s = one_monitor();
        let scene = overview_scene(&s, 0.5);
        let row = |i| scene.rows.iter().find(|r| r.workspace == i).unwrap();
        // active (0) は中央、1 (下) は active の下、末尾の空ワークスペース (2) はさらに下
        let (r0, r1) = (row(0), row(1));
        assert!(r0.active && !r1.active);
        assert_eq!(r0.rect.y + r0.rect.h / 2, 540);
        assert!(r1.rect.y > r0.rect.y + r0.rect.h, "行の間に隙間がある");
        // 下のワークスペースのタイルは下の行の中にある
        let t3 = tile(&scene, 3);
        assert_eq!(t3.workspace, 1);
        assert!(t3.rect.y >= r1.rect.y);
        // active を下へ移すと、以前 active だった行は上へ回る
        let mut s2 = s.clone();
        s2.switch_workspace(true);
        let scene2 = overview_scene(&s2, 0.5);
        let up = scene2.rows.iter().find(|r| r.workspace == 0).unwrap();
        let cur = scene2.rows.iter().find(|r| r.workspace == 1).unwrap();
        assert!(cur.active);
        assert_eq!(cur.rect.y + cur.rect.h / 2, 540);
        assert!(up.rect.y + up.rect.h < cur.rect.y);
    }

    #[test]
    fn fullscreen_uses_monitor_rect() {
        let mut s = one_monitor();
        s.toggle_fullscreen();
        let id = s.fullscreen.unwrap();
        let t = tile(&overview_scene(&s, 1.0), id.0);
        assert_eq!(t.rect, s.monitors[0].monitor);
    }

    #[test]
    fn two_monitors_are_independent_and_clipped() {
        let mut s = State::new(&[geom(10, 0), geom(20, 1920)], GAP, 0.5);
        s.adopt_initial(0, w(1));
        s.adopt_initial(1, w(2));
        s.set_initial_focus(w(1));
        let scene = overview_scene(&s, 0.5);
        let (a, b) = (tile(&scene, 1), tile(&scene, 2));
        assert_eq!((a.monitor, b.monitor), (0, 1));
        // 各モニターの中心を基準に縮小される
        assert!(a.rect.x < 960 && a.rect.x + a.rect.w <= 1920);
        assert!(b.rect.x >= 1920 && b.rect.x + b.rect.w <= 3840);
        assert_eq!(b.clip, s.monitors[1].monitor);
        // 全行が属するモニターの内側に収まる
        for r in &scene.rows {
            let m = s.monitors[r.monitor].monitor;
            assert!(r.rect.x >= m.x && r.rect.x + r.rect.w <= m.x + m.w);
            assert!(r.rect.y >= m.y && r.rect.y + r.rect.h <= m.y + m.h);
        }
    }

    #[test]
    fn offscreen_columns_are_clipped_not_leaked_to_other_monitor() {
        let mut s = State::new(&[geom(10, 0), geom(20, 1920)], GAP, 0.5);
        for i in 1..=4 {
            s.adopt_initial(0, w(i));
        }
        s.set_initial_focus(w(4));
        s.scroll(false);
        let scene = overview_scene(&s, 1.0);
        // 左モニターの viewport 外の列でも、描画領域は左モニター内に限られる
        for t in scene.tiles.iter().filter(|t| t.monitor == 0) {
            let (dest, _) = clip_crop(t.rect, t.clip).unwrap();
            assert!(dest.x + dest.w <= 1920);
        }
    }

    #[test]
    fn clip_crop_fractions() {
        let rect = Rect {
            x: -100,
            y: 0,
            w: 400,
            h: 200,
        };
        let clip = Rect {
            x: 0,
            y: 0,
            w: 1000,
            h: 100,
        };
        let (dest, c) = clip_crop(rect, clip).unwrap();
        assert_eq!(
            dest,
            Rect {
                x: 0,
                y: 0,
                w: 300,
                h: 100
            }
        );
        assert_eq!((c.left, c.top, c.right, c.bottom), (0.25, 0.0, 1.0, 0.5));
        assert!(clip_crop(rect, Rect { x: 500, ..clip }).is_none());
    }

    #[test]
    fn float_scaled_about_its_monitor() {
        let s = State::new(&[geom(10, 0), geom(20, 1920)], GAP, 0.5);
        let frame = Rect {
            x: 2500,
            y: 100,
            w: 400,
            h: 300,
        };
        let t = overview_float(&s, w(9), frame, 1.0);
        assert_eq!((t.rect, t.monitor), (frame, 1));
        let t = overview_float(&s, w(9), frame, 0.5);
        // モニター 1 の作業領域の中心 (2880, 540) 基準
        assert_eq!(t.rect.x, 2880 + (2500 - 2880) / 2);
        assert_eq!(t.rect.w, 200);
    }

    #[test]
    fn hit_test_priority() {
        let s = one_monitor();
        let scene = overview_scene(&s, 0.5);
        let t1 = tile(&scene, 1);
        let (px, py) = (t1.rect.x + 5, t1.rect.y + 5);
        assert_eq!(hit_test(&scene, &[], px, py), Hit::Tile(w(1)));
        // フロートが重なっていればフロートが優先
        let fl = OverviewTile {
            id: w(99),
            rect: Rect {
                x: px - 2,
                y: py - 2,
                w: 50,
                h: 50,
            },
            ..t1
        };
        assert_eq!(hit_test(&scene, &[fl], px, py), Hit::Float(w(99)));
        // タイルのない行の上は行
        let row = scene.rows.iter().find(|r| r.workspace == 1).unwrap();
        let occupied: HashMap<_, _> = scene.tiles.iter().map(|t| (t.id, t.rect)).collect();
        let (rx, ry) = (row.rect.x + 1, row.rect.y + 1);
        assert!(occupied.values().all(|r| !contains(*r, rx, ry)));
        assert_eq!(
            hit_test(&scene, &[], rx, ry),
            Hit::Row {
                monitor: 0,
                workspace: 1
            }
        );
        assert_eq!(hit_test(&scene, &[], -50, -50), Hit::None);
    }
}
