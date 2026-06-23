//! Strip / Column レイアウトの純粋ロジック (FR-1.3, FR-1.4, FR-3.1〜3.3, §6)。
//!
//! HWND を持ち込まず [`WindowId`] で抽象化する。座標は物理 px。
//! 不変条件: 挿入・削除で既存 Column の幅は決して変わらない (niri モデルの核心)。

/// 実ウィンドウへの不透明なハンドル。Win32 層で HWND と相互変換する。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WindowId(pub u64);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// Strip 上の縦区画。`x` は Strip 座標 (gap を含まない左端)。
#[derive(Debug, Clone)]
pub struct Column {
    pub x: i32,
    pub width: i32,
    pub tiles: Vec<WindowId>,
    /// この Column で最後にフォーカスされていた Tile。
    /// 左右移動で入ってきたときの着地先 (niri の挙動)。
    pub active_tile: usize,
    /// maximize-column 中の元の幅 (FR-4.6)。Some = maximize 中
    pub saved_width: Option<i32>,
}

impl Column {
    /// active_tile を tiles の現在の範囲にクランプして返す。
    pub fn active(&self) -> Option<WindowId> {
        self.tiles
            .get(self.active_tile.min(self.tiles.len().saturating_sub(1)))
            .copied()
    }
}

/// フォーカス移動の方向 (FR-4.1)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusDir {
    Left,
    Right,
    Up,
    Down,
}

/// モニタ 1 枚ぶんの無限ストリップとビューポート。
#[derive(Debug, Clone, Default)]
pub struct Strip {
    pub columns: Vec<Column>,
    /// Viewport 左端が映している Strip 座標
    pub offset_x: i32,
    /// この Workspace で最後にフォーカスされていた Tile (切替時の復帰先)
    pub last_focused: Option<WindowId>,
}

/// モニタ 1 枚の縦ワークスペース列 (FR-5.4、niri の動的モデル)。
/// 不変条件: 末尾に常に空の Strip が 1 つあり、それ以外に空の Strip はない。
#[derive(Debug, Clone)]
pub struct Stack {
    pub strips: Vec<Strip>,
    pub active: usize,
}

impl Default for Stack {
    fn default() -> Self {
        Stack {
            strips: vec![Strip::default()],
            active: 0,
        }
    }
}

impl Stack {
    pub fn active_mut(&mut self) -> &mut Strip {
        &mut self.strips[self.active]
    }

    pub fn strip_index_of(&self, id: WindowId) -> Option<usize> {
        self.strips.iter().position(|s| s.contains(id))
    }

    pub fn contains(&self, id: WindowId) -> bool {
        self.strip_index_of(id).is_some()
    }

    /// 不変条件の回復: 空 Strip を除去し、末尾に空を 1 つ補う。
    /// active は「直前に active だった Strip」を追跡する (消えた場合は近傍へ)。
    pub fn normalize(&mut self) {
        let mut new_active = 0;
        let mut kept: Vec<Strip> = Vec::with_capacity(self.strips.len() + 1);
        for (i, s) in std::mem::take(&mut self.strips).into_iter().enumerate() {
            let was_active = i == self.active;
            if !s.columns.is_empty() {
                if was_active {
                    new_active = kept.len();
                }
                kept.push(s);
            } else if was_active {
                // active が空で消える → 次の非空 (なければ末尾の空) へ落ちる
                new_active = kept.len();
            }
        }
        kept.push(Strip::default());
        self.active = new_active.min(kept.len() - 1);
        self.strips = kept;
    }

    /// ワークスペース切替 (上下)。端でクランプ。戻り値は移動したか。
    pub fn switch(&mut self, down: bool) -> bool {
        let new = if down {
            (self.active + 1).min(self.strips.len() - 1)
        } else {
            self.active.saturating_sub(1)
        };
        let changed = new != self.active;
        self.active = new;
        changed
    }

    /// ウィンドウを下/上のワークスペースへ移動し、active も追従する (FR-5.4)。
    /// 最下段 (末尾の空) へ移すと normalize が新しい空を下に補う = 動的作成。
    /// 最上段から上へは no-op。
    pub fn move_window(
        &mut self,
        id: WindowId,
        down: bool,
        width: i32,
        viewport_w: i32,
        gap: i32,
    ) -> bool {
        let Some(src) = self.strip_index_of(id) else {
            return false;
        };
        let dst = if down {
            src + 1
        } else {
            match src.checked_sub(1) {
                Some(d) => d,
                None => return false,
            }
        };
        if dst >= self.strips.len() {
            return false; // 末尾の空 Strip 不変条件があるため通常到達しない
        }
        self.strips[src].remove_window(id, gap);
        self.strips[src].clamp_offset(viewport_w);
        let anchor = self.strips[dst].last_focused;
        self.strips[dst].insert_column_after(anchor, id, width, gap);
        self.strips[dst].last_focused = Some(id);
        self.active = dst;
        self.normalize();
        true
    }
}

/// project() の出力。Viewport と交差しない Column のタイルは Offscreen。
/// Offscreen も射影上の Rect を持つ — 退避時にサイズを変えてはならない
/// (niri 核心仕様 3「既存ウィンドウは決してリサイズされない」)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    Visible(Rect),
    Offscreen(Rect),
}

impl Strip {
    pub fn column_index_of(&self, id: WindowId) -> Option<usize> {
        self.columns.iter().position(|c| c.tiles.contains(&id))
    }

    pub fn contains(&self, id: WindowId) -> bool {
        self.column_index_of(id).is_some()
    }

    /// 新規ウィンドウを anchor の属する Column の右隣に新 Column として挿入する
    /// (FR-1.3)。anchor が None / 未管理なら末尾に追加。
    /// 挿入位置より右の Column は `width + gap` ぶん右へシフトする (§6)。
    pub fn insert_column_after(
        &mut self,
        anchor: Option<WindowId>,
        id: WindowId,
        width: i32,
        gap: i32,
    ) {
        let idx = anchor
            .and_then(|a| self.column_index_of(a))
            .map(|i| i + 1)
            .unwrap_or(self.columns.len());

        let x = if idx == 0 {
            0
        } else {
            let prev = &self.columns[idx - 1];
            prev.x + prev.width + gap
        };

        for col in &mut self.columns[idx..] {
            col.x += width + gap;
        }
        self.columns.insert(
            idx,
            Column {
                x,
                width,
                tiles: vec![id],
                active_tile: 0,
                saved_width: None,
            },
        );
    }

    /// 全 Column の x を先頭から振り直す。順序・幅の変更後に呼ぶ。
    pub fn relayout(&mut self, gap: i32) {
        let mut x = 0;
        for c in &mut self.columns {
            c.x = x;
            x += c.width + gap;
        }
    }

    /// フォーカス Column を左右の隣と入れ替える (FR-4.4)。端では no-op。
    pub fn move_column(&mut self, id: WindowId, dir: FocusDir, gap: i32) -> bool {
        let Some(i) = self.column_index_of(id) else {
            return false;
        };
        let j = match dir {
            FocusDir::Left => i.checked_sub(1),
            FocusDir::Right => (i + 1 < self.columns.len()).then_some(i + 1),
            _ => None,
        };
        let Some(j) = j else {
            return false;
        };
        self.columns.swap(i, j);
        self.relayout(gap);
        true
    }

    /// Tile を Column から押し出し、右隣に独立した Column にする (FR-4.5)。
    /// 単独 Tile の Column では no-op。
    pub fn expel(&mut self, id: WindowId, gap: i32) -> bool {
        let Some(i) = self.column_index_of(id) else {
            return false;
        };
        if self.columns[i].tiles.len() < 2 {
            return false;
        }
        let col = &mut self.columns[i];
        let pos = col.tiles.iter().position(|&t| t == id).unwrap();
        col.tiles.remove(pos);
        col.active_tile = col.active_tile.min(col.tiles.len() - 1);
        let width = col.width;
        self.columns.insert(
            i + 1,
            Column {
                x: 0, // relayout で確定
                width,
                tiles: vec![id],
                active_tile: 0,
                saved_width: None,
            },
        );
        self.relayout(gap);
        true
    }

    /// 右隣 Column の active Tile を自 Column へ取り込み縦スタックする (FR-4.5)。
    /// 右隣が空になったら Column ごと詰める。右端では no-op。
    pub fn consume_right(&mut self, id: WindowId, gap: i32) -> bool {
        let Some(i) = self.column_index_of(id) else {
            return false;
        };
        if i + 1 >= self.columns.len() {
            return false;
        }
        let right = &mut self.columns[i + 1];
        let take = right.active_tile.min(right.tiles.len() - 1);
        let taken = right.tiles.remove(take);
        if right.tiles.is_empty() {
            self.columns.remove(i + 1);
        } else {
            right.active_tile = right.active_tile.min(right.tiles.len() - 1);
        }
        self.columns[i].tiles.push(taken);
        self.relayout(gap);
        true
    }

    /// Column 幅プリセットのサイクル (FR-3.4): viewport の 1/3 → 1/2 → 2/3。
    /// 現在幅に最も近いプリセットの「次」へ。maximize 状態は解除する。
    pub fn cycle_width(&mut self, id: WindowId, viewport_w: i32, gap: i32) -> bool {
        let Some(i) = self.column_index_of(id) else {
            return false;
        };
        let col = &mut self.columns[i];
        col.saved_width = None;
        let presets = [viewport_w / 3, viewport_w / 2, viewport_w * 2 / 3];
        let nearest = presets
            .iter()
            .enumerate()
            .min_by_key(|(_, &p)| (p - col.width).abs())
            .map(|(i, _)| i)
            .unwrap();
        col.width = presets[(nearest + 1) % presets.len()];
        self.relayout(gap);
        true
    }

    /// maximize-column トグル (FR-4.6): Viewport 全幅 ⇔ 元の幅。
    pub fn toggle_maximize(&mut self, id: WindowId, viewport_w: i32, gap: i32) -> bool {
        let Some(i) = self.column_index_of(id) else {
            return false;
        };
        let col = &mut self.columns[i];
        match col.saved_width.take() {
            Some(w) => col.width = w,
            None => {
                col.saved_width = Some(col.width);
                col.width = viewport_w;
            }
        }
        self.relayout(gap);
        true
    }

    /// Viewport を 1 Column ぶん左右へスクロールする (FR-4.3)。フォーカスは変えない。
    pub fn scroll_columnwise(&mut self, forward: bool, viewport_w: i32) {
        let cur = self.offset_x;
        let target = if forward {
            self.columns.iter().map(|c| c.x).filter(|&x| x > cur).min()
        } else {
            self.columns.iter().map(|c| c.x).filter(|&x| x < cur).max()
        };
        if let Some(t) = target {
            self.offset_x = t;
        }
        self.clamp_offset(viewport_w);
    }

    /// フォーカス移動先の Tile を返す (FR-4.1)。端ではラップせず None。
    /// 左右は隣 Column の active Tile、上下は同一 Column 内の隣 Tile。
    pub fn neighbor(&self, from: WindowId, dir: FocusDir) -> Option<WindowId> {
        let col_idx = self.column_index_of(from)?;
        let col = &self.columns[col_idx];
        match dir {
            FocusDir::Left => col_idx
                .checked_sub(1)
                .and_then(|i| self.columns[i].active()),
            FocusDir::Right => self.columns.get(col_idx + 1).and_then(|c| c.active()),
            FocusDir::Up => {
                let t = col.tiles.iter().position(|&t| t == from)?;
                t.checked_sub(1).map(|i| col.tiles[i])
            }
            FocusDir::Down => {
                let t = col.tiles.iter().position(|&t| t == from)?;
                col.tiles.get(t + 1).copied()
            }
        }
    }

    /// フォーカスが移った Tile を記録する (Column の active_tile 更新)。
    pub fn set_active(&mut self, id: WindowId) {
        if let Some(col_idx) = self.column_index_of(id) {
            let col = &mut self.columns[col_idx];
            if let Some(t) = col.tiles.iter().position(|&t| t == id) {
                col.active_tile = t;
            }
        }
    }

    /// FR-4.2 / §6 focus_follow: id の属する Column 全体が見える最小移動量で
    /// offset_x を調整する。Viewport より幅広の Column は左端を優先する。
    /// 戻り値は offset_x が変化したか。
    pub fn ensure_visible(&mut self, id: WindowId, viewport_w: i32) -> bool {
        let Some(col_idx) = self.column_index_of(id) else {
            return false;
        };
        let col = &self.columns[col_idx];
        let old = self.offset_x;
        if col.x < self.offset_x {
            self.offset_x = col.x;
        } else if col.x + col.width > self.offset_x + viewport_w {
            // 右端合わせ。ただし Column が viewport より広いなら左端合わせ
            self.offset_x = (col.x + col.width - viewport_w).min(col.x);
        }
        self.offset_x != old
    }

    /// ウィンドウを除去する (FR-1.4)。Column が空になったら詰める
    /// (右側の Column を `width + gap` ぶん左へシフト)。
    /// 戻り値は除去できたかどうか。
    pub fn remove_window(&mut self, id: WindowId, gap: i32) -> bool {
        let Some(idx) = self.column_index_of(id) else {
            return false;
        };
        let col = &mut self.columns[idx];
        col.tiles.retain(|t| *t != id);
        if col.tiles.is_empty() {
            let removed_width = col.width;
            self.columns.remove(idx);
            for col in &mut self.columns[idx..] {
                col.x -= removed_width + gap;
            }
        }
        true
    }

    /// Strip 全体の幅 (最後の Column の右端)。
    pub fn content_width(&self) -> i32 {
        self.columns.last().map(|c| c.x + c.width).unwrap_or(0)
    }

    /// offset_x を [0, content_width - viewport_w] にクランプする (§6 削除時)。
    pub fn clamp_offset(&mut self, viewport_w: i32) {
        let max = (self.content_width() - viewport_w).max(0);
        self.offset_x = self.offset_x.clamp(0, max);
    }

    /// Strip 座標 → 実画面座標へ射影する (§6)。
    /// Viewport と交差する Column のみ Visible。Tile は Column 内で縦等分割 (FR-3.1)。
    /// gap は Column 間・Tile 間・画面端すべてに適用する (FR-3.5)。
    pub fn project(&self, work: Rect, gap: i32) -> Vec<(WindowId, Placement)> {
        let viewport_w = work.w - 2 * gap;
        let inner_h = work.h - 2 * gap;
        let mut out = Vec::new();

        for col in &self.columns {
            let intersects =
                col.x < self.offset_x + viewport_w && col.x + col.width > self.offset_x;

            let screen_x = work.x + gap + (col.x - self.offset_x);
            let n = col.tiles.len() as i32;
            let tile_h = (inner_h - (n - 1) * gap) / n;
            for (i, &id) in col.tiles.iter().enumerate() {
                let i = i as i32;
                let y = work.y + gap + i * (tile_h + gap);
                // 最後の Tile に丸め誤差を吸収させ、Column 下端を揃える
                let h = if i == n - 1 {
                    inner_h - i * (tile_h + gap)
                } else {
                    tile_h
                };
                let rect = Rect {
                    x: screen_x,
                    y,
                    w: col.width,
                    h,
                };
                out.push((
                    id,
                    if intersects {
                        Placement::Visible(rect)
                    } else {
                        Placement::Offscreen(rect)
                    },
                ));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GAP: i32 = 8;
    const WORK: Rect = Rect {
        x: 0,
        y: 0,
        w: 1920,
        h: 1080,
    };

    fn id(n: u64) -> WindowId {
        WindowId(n)
    }

    fn strip_with(widths: &[i32]) -> Strip {
        let mut s = Strip::default();
        for (i, &w) in widths.iter().enumerate() {
            s.insert_column_after(None, id(i as u64), w, GAP);
        }
        s
    }

    #[test]
    fn insert_into_empty_starts_at_origin() {
        let s = strip_with(&[600]);
        assert_eq!(s.columns[0].x, 0);
    }

    #[test]
    fn sequential_inserts_are_gapped() {
        let s = strip_with(&[600, 400]);
        assert_eq!(s.columns[1].x, 600 + GAP);
        assert_eq!(s.content_width(), 600 + GAP + 400);
    }

    #[test]
    fn insert_after_anchor_shifts_right_columns_without_resizing() {
        let mut s = strip_with(&[600, 400, 500]);
        let before: Vec<i32> = s.columns.iter().map(|c| c.width).collect();

        s.insert_column_after(Some(id(0)), id(9), 300, GAP);

        // 挿入位置は anchor の右隣
        assert_eq!(s.columns[1].tiles, vec![id(9)]);
        assert_eq!(s.columns[1].x, 600 + GAP);
        // 以降の Column は width+gap ぶんシフト
        assert_eq!(s.columns[2].x, 600 + GAP + 300 + GAP);
        // 既存 Column の幅は不変 (niri 核心仕様 3)
        let after: Vec<i32> = s
            .columns
            .iter()
            .filter(|c| c.tiles != vec![id(9)])
            .map(|c| c.width)
            .collect();
        assert_eq!(before, after);
    }

    #[test]
    fn insert_with_unknown_anchor_appends() {
        let mut s = strip_with(&[600]);
        s.insert_column_after(Some(id(42)), id(9), 300, GAP);
        assert_eq!(s.columns.len(), 2);
        assert_eq!(s.columns[1].tiles, vec![id(9)]);
    }

    #[test]
    fn remove_middle_column_shifts_left() {
        let mut s = strip_with(&[600, 400, 500]);
        assert!(s.remove_window(id(1), GAP));
        assert_eq!(s.columns.len(), 2);
        assert_eq!(s.columns[1].x, 600 + GAP); // 500px の Column が詰まった
        assert_eq!(s.columns[1].width, 500); // 幅は不変
    }

    #[test]
    fn remove_tile_from_stacked_column_keeps_column() {
        let mut s = strip_with(&[600]);
        s.columns[0].tiles.push(id(9)); // 縦スタック (Phase 3 相当の状態)
        assert!(s.remove_window(id(9), GAP));
        assert_eq!(s.columns.len(), 1);
        assert_eq!(s.columns[0].tiles, vec![id(0)]);
    }

    #[test]
    fn remove_unknown_window_is_noop() {
        let mut s = strip_with(&[600]);
        assert!(!s.remove_window(id(42), GAP));
        assert_eq!(s.columns.len(), 1);
    }

    #[test]
    fn project_places_visible_column_with_gaps() {
        let s = strip_with(&[600]);
        let placements = s.project(WORK, GAP);
        assert_eq!(
            placements,
            vec![(
                id(0),
                Placement::Visible(Rect {
                    x: GAP,
                    y: GAP,
                    w: 600,
                    h: 1080 - 2 * GAP,
                })
            )]
        );
    }

    #[test]
    fn project_marks_out_of_viewport_columns_offscreen() {
        // viewport 幅 1904px に対し 3 列目は右端からはみ出す
        let s = strip_with(&[1000, 800, 600]);
        let placements = s.project(WORK, GAP);
        assert!(matches!(placements[0].1, Placement::Visible(_)));
        // 2 列目: x=1008, 右端 1808 < 1904 → 可視
        assert!(matches!(placements[1].1, Placement::Visible(_)));
        // 3 列目: x=1816 < 1904 → 部分的に交差するので可視 (はみ出して配置)
        assert!(matches!(placements[2].1, Placement::Visible(_)));

        // offset を進めずさらに列を足すと完全に外れる
        let mut s = s;
        s.insert_column_after(None, id(9), 600, GAP);
        let placements = s.project(WORK, GAP);
        assert!(matches!(placements[3], (w, Placement::Offscreen(r)) if w == id(9) && r.w == 600));
    }

    #[test]
    fn project_splits_stacked_tiles_evenly() {
        let mut s = strip_with(&[600]);
        s.columns[0].tiles.push(id(9));
        s.columns[0].tiles.push(id(10));
        let placements = s.project(WORK, GAP);
        let rects: Vec<Rect> = placements
            .iter()
            .map(|(_, p)| match p {
                Placement::Visible(r) => *r,
                Placement::Offscreen(_) => panic!("expected visible"),
            })
            .collect();
        // 高さの合計 + tile 間 gap = column 内側の高さ
        let total: i32 = rects.iter().map(|r| r.h).sum();
        assert_eq!(total + 2 * GAP, 1080 - 2 * GAP);
        // 隣接 Tile は gap を挟んで連続する
        assert_eq!(rects[1].y, rects[0].y + rects[0].h + GAP);
        assert_eq!(rects[2].y, rects[1].y + rects[1].h + GAP);
        // 下端が work_area の gap 内側に揃う
        assert_eq!(rects[2].y + rects[2].h, 1080 - GAP);
    }

    #[test]
    fn project_respects_offset_x() {
        let mut s = strip_with(&[600, 600]);
        s.offset_x = 608;
        let placements = s.project(WORK, GAP);
        // 1 列目は viewport 左外 → offscreen (サイズは維持)
        assert!(matches!(placements[0].1, Placement::Offscreen(r) if r.w == 600));
        // 2 列目が viewport 左端 (gap 位置) に来る
        assert_eq!(
            placements[1].1,
            Placement::Visible(Rect {
                x: GAP,
                y: GAP,
                w: 600,
                h: 1080 - 2 * GAP,
            })
        );
    }

    #[test]
    fn clamp_offset_bounds() {
        let mut s = strip_with(&[600, 600]);
        s.offset_x = 99999;
        s.clamp_offset(1904);
        assert_eq!(s.offset_x, 0); // content 1208 < viewport 1904 → 0
        s.offset_x = -5;
        s.clamp_offset(1904);
        assert_eq!(s.offset_x, 0);

        let mut s = strip_with(&[1000, 1000, 1000]);
        s.offset_x = 99999;
        s.clamp_offset(1904);
        assert_eq!(s.offset_x, 3 * 1000 + 2 * GAP - 1904);
    }

    #[test]
    fn neighbor_left_right_stops_at_edges() {
        let s = strip_with(&[600, 600, 600]);
        assert_eq!(s.neighbor(id(0), FocusDir::Left), None);
        assert_eq!(s.neighbor(id(0), FocusDir::Right), Some(id(1)));
        assert_eq!(s.neighbor(id(2), FocusDir::Right), None);
        assert_eq!(s.neighbor(id(2), FocusDir::Left), Some(id(1)));
    }

    #[test]
    fn neighbor_up_down_within_column() {
        let mut s = strip_with(&[600]);
        s.columns[0].tiles.push(id(9));
        assert_eq!(s.neighbor(id(0), FocusDir::Down), Some(id(9)));
        assert_eq!(s.neighbor(id(9), FocusDir::Up), Some(id(0)));
        assert_eq!(s.neighbor(id(0), FocusDir::Up), None);
        assert_eq!(s.neighbor(id(9), FocusDir::Down), None);
    }

    #[test]
    fn lateral_move_lands_on_active_tile() {
        let mut s = strip_with(&[600, 600]);
        s.columns[1].tiles.push(id(9));
        // 2 列目の下段 Tile をアクティブにしてから左→右と移動すると下段に着地
        s.set_active(id(9));
        assert_eq!(s.neighbor(id(0), FocusDir::Right), Some(id(9)));
    }

    #[test]
    fn neighbor_of_unknown_window_is_none() {
        let s = strip_with(&[600]);
        assert_eq!(s.neighbor(id(42), FocusDir::Left), None);
    }

    #[test]
    fn ensure_visible_scrolls_right_minimally() {
        // viewport 1904px、3 列目 (x=2016, w=1000) は右にはみ出している
        let mut s = strip_with(&[1000, 1000, 1000]);
        let changed = s.ensure_visible(id(2), 1904);
        assert!(changed);
        // 右端合わせ: offset = 2016 + 1000 - 1904
        assert_eq!(s.offset_x, 1112);
        // この offset で 3 列目は完全に可視
        let placements = s.project(WORK, GAP);
        assert!(matches!(placements[2].1, Placement::Visible(_)));
    }

    #[test]
    fn ensure_visible_scrolls_left_to_column_start() {
        let mut s = strip_with(&[1000, 1000]);
        s.offset_x = 800;
        assert!(s.ensure_visible(id(0), 1904));
        assert_eq!(s.offset_x, 0); // 左端合わせ
    }

    #[test]
    fn ensure_visible_noop_when_fully_visible() {
        let mut s = strip_with(&[600, 600]);
        assert!(!s.ensure_visible(id(1), 1904));
        assert_eq!(s.offset_x, 0);
    }

    #[test]
    fn ensure_visible_prefers_left_edge_for_oversized_column() {
        let mut s = strip_with(&[1000, 3000]);
        assert!(s.ensure_visible(id(1), 1904));
        // 幅 3000 > viewport 1904 → 左端合わせ (右端合わせだと左が切れる)
        assert_eq!(s.offset_x, 1008);
    }

    #[test]
    fn move_column_swaps_and_relayouts() {
        let mut s = strip_with(&[600, 400, 500]);
        assert!(s.move_column(id(0), FocusDir::Right, GAP));
        let order: Vec<_> = s.columns.iter().map(|c| c.tiles[0]).collect();
        assert_eq!(order, vec![id(1), id(0), id(2)]);
        // x は幅順に振り直される (幅は不変)
        assert_eq!(s.columns[0].x, 0);
        assert_eq!(s.columns[1].x, 400 + GAP);
        assert_eq!(s.columns[2].x, 400 + GAP + 600 + GAP);
        // 端では no-op
        assert!(!s.move_column(id(2), FocusDir::Right, GAP));
        assert!(!s.move_column(id(1), FocusDir::Left, GAP));
    }

    #[test]
    fn expel_splits_stacked_tile_to_right_column() {
        let mut s = strip_with(&[600, 500]);
        s.columns[0].tiles.push(id(9));
        assert!(s.expel(id(9), GAP));
        assert_eq!(s.columns.len(), 3);
        assert_eq!(s.columns[1].tiles, vec![id(9)]); // 元 Column の右隣
        assert_eq!(s.columns[1].width, 600); // 幅は元 Column を引き継ぐ
        assert_eq!(s.columns[2].x, 600 + GAP + 600 + GAP);
        // 単独 Tile は押し出せない
        assert!(!s.expel(id(1), GAP));
    }

    #[test]
    fn consume_right_stacks_and_removes_empty_column() {
        let mut s = strip_with(&[600, 500, 400]);
        assert!(s.consume_right(id(0), GAP));
        assert_eq!(s.columns.len(), 2);
        assert_eq!(s.columns[0].tiles, vec![id(0), id(1)]);
        // 右の Column (幅 400) が詰まる
        assert_eq!(s.columns[1].x, 600 + GAP);
        // 右端では no-op
        assert!(!s.consume_right(id(2), GAP));
    }

    #[test]
    fn expel_then_consume_roundtrips() {
        let mut s = strip_with(&[600]);
        s.columns[0].tiles.push(id(9));
        assert!(s.expel(id(9), GAP));
        assert!(s.consume_right(id(0), GAP));
        assert_eq!(s.columns.len(), 1);
        assert_eq!(s.columns[0].tiles, vec![id(0), id(9)]);
    }

    #[test]
    fn cycle_width_walks_presets() {
        let vw = 1904;
        let mut s = strip_with(&[vw / 2]);
        assert!(s.cycle_width(id(0), vw, GAP));
        assert_eq!(s.columns[0].width, vw * 2 / 3); // 1/2 → 2/3
        assert!(s.cycle_width(id(0), vw, GAP));
        assert_eq!(s.columns[0].width, vw / 3); // 2/3 → 1/3 (循環)
        assert!(s.cycle_width(id(0), vw, GAP));
        assert_eq!(s.columns[0].width, vw / 2); // 1/3 → 1/2
    }

    #[test]
    fn cycle_width_relayouts_following_columns() {
        let vw = 1904;
        let mut s = strip_with(&[vw / 2, 500]);
        s.cycle_width(id(0), vw, GAP);
        assert_eq!(s.columns[1].x, vw * 2 / 3 + GAP);
        assert_eq!(s.columns[1].width, 500); // 隣はリサイズされない
    }

    #[test]
    fn toggle_maximize_roundtrips() {
        let vw = 1904;
        let mut s = strip_with(&[600, 500]);
        assert!(s.toggle_maximize(id(0), vw, GAP));
        assert_eq!(s.columns[0].width, vw);
        assert_eq!(s.columns[1].x, vw + GAP); // 隣は押し出される
        assert!(s.toggle_maximize(id(0), vw, GAP));
        assert_eq!(s.columns[0].width, 600); // 元の幅へ復帰
        assert_eq!(s.columns[1].x, 600 + GAP);
    }

    fn stack_with(ids: &[u64]) -> Stack {
        let mut st = Stack::default();
        for &i in ids {
            st.strips[0].insert_column_after(None, id(i), 600, GAP);
        }
        st.normalize();
        st
    }

    #[test]
    fn stack_starts_with_single_empty_strip() {
        let st = Stack::default();
        assert_eq!(st.strips.len(), 1);
        assert_eq!(st.active, 0);
    }

    #[test]
    fn move_window_down_creates_workspace_dynamically() {
        let mut st = stack_with(&[0, 1]);
        assert_eq!(st.strips.len(), 2); // [ws0(2窓), 末尾空]

        assert!(st.move_window(id(1), true, 600, 1904, GAP));
        // ws1 が生まれ、その下に新しい空が補われる
        assert_eq!(st.strips.len(), 3);
        assert_eq!(st.strips[0].columns.len(), 1);
        assert_eq!(st.strips[1].columns[0].tiles, vec![id(1)]);
        assert!(st.strips[2].columns.is_empty());
        assert_eq!(st.active, 1); // フォーカス追従
    }

    #[test]
    fn move_window_up_from_top_is_noop() {
        let mut st = stack_with(&[0]);
        assert!(!st.move_window(id(0), false, 600, 1904, GAP));
        assert_eq!(st.active, 0);
    }

    #[test]
    fn emptied_middle_workspace_collapses() {
        let mut st = stack_with(&[0, 1]);
        st.move_window(id(1), true, 600, 1904, GAP); // ws1 = [1]
        st.move_window(id(1), true, 600, 1904, GAP); // ws1 が空に → 消滅
        assert_eq!(st.strips.len(), 3); // [ws0(0), ws1(1), 空]
        assert_eq!(st.strips[1].columns[0].tiles, vec![id(1)]);
        assert_eq!(st.active, 1);
    }

    #[test]
    fn move_window_up_returns_to_previous_workspace() {
        let mut st = stack_with(&[0, 1]);
        st.move_window(id(1), true, 600, 1904, GAP);
        assert!(st.move_window(id(1), false, 600, 1904, GAP));
        assert_eq!(st.strips.len(), 2); // 元通り [ws0(2窓), 空]
        assert_eq!(st.strips[0].columns.len(), 2);
        assert_eq!(st.active, 0);
    }

    #[test]
    fn switch_clamps_at_both_ends() {
        let mut st = stack_with(&[0]);
        assert!(!st.switch(false)); // 上端
        assert!(st.switch(true)); // 末尾の空へ
        assert_eq!(st.active, 1);
        assert!(!st.switch(true)); // 下端 (空より下はない)
        assert!(st.switch(false));
        assert_eq!(st.active, 0);
    }

    #[test]
    fn normalize_after_removal_collapses_and_tracks_active() {
        let mut st = stack_with(&[0, 1]);
        st.move_window(id(1), true, 600, 1904, GAP); // active = ws1
        st.strips[1].remove_window(id(1), GAP); // ws1 が空に (ウィンドウ消滅相当)
        st.normalize();
        assert_eq!(st.strips.len(), 2); // [ws0, 空]
        assert_eq!(st.active, 1); // 空だった active は次の位置 (末尾空) へ
    }

    #[test]
    fn scroll_columnwise_moves_by_column_boundary() {
        let mut s = strip_with(&[1000, 1000, 1000]);
        let vw = 1904;
        s.scroll_columnwise(true, vw);
        assert_eq!(s.offset_x, 1008); // 2 列目の x
        s.scroll_columnwise(true, vw);
        // 3 列目の x=2016 だが max offset (3016-1904=1112) でクランプ
        assert_eq!(s.offset_x, 1112);
        s.scroll_columnwise(false, vw);
        assert_eq!(s.offset_x, 1008);
        s.scroll_columnwise(false, vw);
        assert_eq!(s.offset_x, 0);
        s.scroll_columnwise(false, vw);
        assert_eq!(s.offset_x, 0); // 左端で停止
    }
}
