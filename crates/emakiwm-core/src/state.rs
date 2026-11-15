//! WM の論理状態とイベント処理 (§5 の reducer)。
//!
//! 「どのモニターのどのワークスペースに何があり、どれがフォーカスか」を持ち、
//! イベント・コマンドに対する状態遷移をここで完結させる。Win32 層 (wm.rs) は
//! OS への問い合わせ結果を引数で渡し、戻り値のフォーカス要求を実行するだけ。
//! 時刻は単調増加の ms を引数で受け取る (テストで時間を操作できるように)。

use crate::layout::{scale_width, FocusDir, Placement, Rect, Stack, WindowId};

/// モニター跨ぎのフォーカス追従を「閉じたことによる OS フォールバック」と
/// みなして巻き戻す猶予 (ms)。
const CROSS_FOLLOW_MS: u64 = 800;

/// 閉じ処理で後継を選んだ直後、別モニターへの前面化を stale とみなす時間 (ms)。
const FOCUS_GUARD_MS: u64 = 500;

/// 手動リサイズで受け入れる Column 幅の下限 (px)。
const MIN_COLUMN_WIDTH: i32 = 100;

/// Win32 層が列挙したモニター 1 枚ぶんの領域。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonitorGeom {
    /// モニター識別子 (HMONITOR 値)。Core は不透明な ID として扱う
    pub hmonitor: isize,
    /// 実効作業領域 (margin 適用後)
    pub work: Rect,
    /// モニター全面 (fullscreen 用)
    pub monitor: Rect,
}

/// 1 物理モニターぶんの状態。
#[derive(Debug, Clone)]
pub struct Monitor {
    pub hmonitor: isize,
    pub work: Rect,
    pub monitor: Rect,
    /// work.w - 2 * gap
    pub viewport_w: i32,
    /// 新規 Column のデフォルト幅
    pub default_width: i32,
    /// このモニターのワークスペース列
    pub stack: Stack,
}

impl Monitor {
    fn new(g: MonitorGeom, gap: i32, ratio: f32) -> Self {
        let mut m = Monitor {
            hmonitor: g.hmonitor,
            work: g.work,
            monitor: g.monitor,
            viewport_w: 0,
            default_width: 0,
            stack: Stack::default(),
        };
        m.set_geom(g, gap, ratio);
        m
    }

    fn set_geom(&mut self, g: MonitorGeom, gap: i32, ratio: f32) {
        self.work = g.work;
        self.monitor = g.monitor;
        let old_vw = self.viewport_w;
        self.viewport_w = g.work.w - 2 * gap;
        self.default_width = ((self.viewport_w - gap) as f32 * ratio) as i32;
        // 解像度などで viewport 幅が変わったら、列幅は比率を保って追従させる
        for s in &mut self.stack.strips {
            s.rescale(old_vw, self.viewport_w);
        }
    }
}

/// [`State::remove`] の結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Removal {
    /// 後継としてフォーカスすべきウィンドウ
    pub focus: Option<WindowId>,
    /// 直前のモニター跨ぎ追従を巻き戻した (ログ用)
    pub rolled_back: bool,
}

/// WM 全体の論理状態。
/// 不変条件: `monitors` は空にならず、`active_monitor` は常に有効な添字。
#[derive(Debug, Clone)]
pub struct State {
    /// 左→右 (同じ x なら上→下) 順
    pub monitors: Vec<Monitor>,
    pub active_monitor: usize,
    /// フォーカス中の管理ウィンドウ。管理外の窓が前面でも直前の値を保つ
    pub focused: Option<WindowId>,
    pub fullscreen: Option<WindowId>,
    /// toggle-floating でレイアウトから外したウィンドウ (末尾ほど新しい)
    pub floating: Vec<WindowId>,
    /// WM が動かしてはいないが浮いているとわかっている窓 (ダイアログ・ルールの float 等)
    pub passive_floats: Vec<WindowId>,
    gap: i32,
    default_ratio: f32,
    /// 直前のモニター跨ぎフォーカス追従の記録 (前フォーカス窓, 時刻)。
    /// 追従直後にその前フォーカス窓が消えた場合、それは「閉じたことによる
    /// OS の Z 順フォールバックへの追従」だったと分かるので巻き戻す
    /// (Firefox のように前面を手放してから遅れて窓を破棄するアプリ対策)
    cross_follow: Option<(WindowId, u64)>,
    /// 閉じイベントで後継フォーカスを選んだ直後の防衛 (後継窓, 時刻)。
    /// 閉じたアプリが複数の削除系イベントを出す場合、後継決定の後から OS の
    /// Z 順フォールバック前面化が届くことがある。防衛時間内の別モニターへの
    /// 前面化は stale とみなして無視し、後継を再主張する
    focus_guard: Option<(WindowId, u64)>,
    /// 最後に前面だった管理外のフロート窓 (focus-floating の戻り先)
    float_focus: Option<WindowId>,
    /// 現在キーボードフォーカスを持つ toggle-floating 済みの窓
    float_active: Option<WindowId>,
}

impl State {
    /// `geoms` は 1 枚以上であること。
    pub fn new(geoms: &[MonitorGeom], gap: i32, default_ratio: f32) -> Self {
        assert!(!geoms.is_empty(), "モニターが 1 枚もありません");
        let mut monitors: Vec<Monitor> = geoms
            .iter()
            .map(|g| Monitor::new(*g, gap, default_ratio))
            .collect();
        monitors.sort_by_key(|m| (m.work.x, m.work.y));
        State {
            monitors,
            active_monitor: 0,
            focused: None,
            fullscreen: None,
            floating: Vec::new(),
            passive_floats: Vec::new(),
            float_focus: None,
            float_active: None,
            gap,
            default_ratio,
            cross_follow: None,
            focus_guard: None,
        }
    }

    /// WindowId がどのモニターの Stack に含まれるか。
    pub fn monitor_index_of(&self, id: WindowId) -> Option<usize> {
        self.monitors.iter().position(|m| m.stack.contains(id))
    }

    /// 管理中のウィンドウ総数。
    pub fn window_count(&self) -> usize {
        self.monitors
            .iter()
            .flat_map(|m| m.stack.strips.iter())
            .flat_map(|s| s.columns.iter())
            .map(|c| c.tiles.len())
            .sum()
    }

    fn active_strip_mut(&mut self) -> (&mut crate::layout::Strip, i32) {
        let ms = &mut self.monitors[self.active_monitor];
        (ms.stack.active_mut(), ms.viewport_w)
    }

    /// 起動時スキャンの取り込み (FR-1.1): 指定モニターの末尾に追加する。
    pub fn adopt_initial(&mut self, mi: usize, id: WindowId) {
        let gap = self.gap;
        let ms = &mut self.monitors[mi];
        let dw = ms.default_width;
        ms.stack.active_mut().insert_column_after(None, id, dw, gap);
        ms.stack.normalize();
    }

    /// 起動時の前面ウィンドウが管理対象ならフォーカスとして記録する。
    pub fn set_initial_focus(&mut self, id: WindowId) {
        if let Some(mi) = self.monitor_index_of(id) {
            self.active_monitor = mi;
            self.focused = Some(id);
            let strip = self.monitors[mi].stack.active_mut();
            strip.set_active(id);
            strip.last_focused = Some(id);
        }
    }

    /// 新規ウィンドウの取り込み (FR-1.3): 物理的な出現位置ではなく、
    /// フォーカスのあるモニターのフォーカス Column の右隣へ挿入する。
    /// 呼び出し側で未管理かつ管理対象であることを確認済みであること。
    pub fn adopt(&mut self, id: WindowId) -> Option<WindowId> {
        let gap = self.gap;
        let ms = &mut self.monitors[self.active_monitor];
        let (dw, vw) = (ms.default_width, ms.viewport_w);
        let anchor = self.focused.filter(|f| ms.stack.contains(*f));
        ms.stack
            .active_mut()
            .insert_column_after(anchor, id, dw, gap);
        ms.stack.normalize();
        self.focused = Some(id);
        let strip = ms.stack.active_mut();
        strip.set_active(id);
        strip.last_focused = Some(id);
        strip.ensure_visible(id, vw);
        Some(id)
    }

    /// ウィンドウの消滅・最小化 (FR-1.4)。未管理なら None。
    pub fn remove(&mut self, id: WindowId, now: u64) -> Option<Removal> {
        let mi = self.monitor_index_of(id)?;
        let gap = self.gap;
        // 消えた窓が「直前のモニター跨ぎ追従の前フォーカス窓」なら、
        // その追従は OS フォールバックだった → 巻き戻し対象
        let rolled_back = self
            .cross_follow
            .is_some_and(|(p, t)| p == id && now.saturating_sub(t) < CROSS_FOLLOW_MS);
        let was_focused = self.focused == Some(id) || rolled_back;
        if self.cross_follow.is_some_and(|(p, _)| p == id) {
            self.cross_follow = None;
        }
        let mut focus = None;
        let ms = &mut self.monitors[mi];
        let vw = ms.viewport_w;
        if let Some(si) = ms.stack.strip_index_of(id) {
            let strip = &mut ms.stack.strips[si];
            // 後継候補: 同じ位置の列 (末尾なら左隣) の active Tile。
            // 縦スタック内で消えた場合は同列の残りが繰り上がる
            let col_hint = strip.column_index_of(id);
            strip.remove_window(id, gap);
            strip.clamp_offset(vw);
            if strip.last_focused == Some(id) {
                strip.last_focused = None;
            }
            let succ = was_focused
                .then(|| {
                    let cols = &strip.columns;
                    col_hint
                        .and_then(|i| cols.get(i.min(cols.len().checked_sub(1)?)))
                        .and_then(|c| c.active())
                })
                .flatten();
            ms.stack.normalize();
            // フォーカス窓の消滅はモニター内で完結させる:
            // 同モニターの後継へフォーカスを移す (OS の Z 順
            // フォールバックで別モニターへ飛ばさない)
            if was_focused {
                let strip = ms.stack.active_mut();
                let succ = succ
                    .filter(|s| strip.contains(*s))
                    .or_else(|| strip.last_focused.filter(|f| strip.contains(*f)))
                    .or_else(|| strip.columns.first().and_then(|c| c.active()));
                if let Some(s) = succ {
                    self.active_monitor = mi;
                    self.focused = Some(s);
                    strip.set_active(s);
                    strip.last_focused = Some(s);
                    strip.ensure_visible(s, vw);
                    self.focus_guard = Some((s, now));
                    focus = Some(s);
                }
            }
        }
        if self.fullscreen == Some(id) {
            self.fullscreen = None;
        }
        if self.focused == Some(id) {
            self.focused = None;
        }
        Some(Removal { focus, rolled_back })
    }

    /// 前面ウィンドウの変化 (EVENT_SYSTEM_FOREGROUND)。
    /// `focused_dead`: 現在の `focused` が既に消滅/最小化しているか (OS に問い合わせた結果)。
    /// 戻り値は再主張すべきフォーカス (stale なフォールバックを打ち消す場合)。
    pub fn foreground(&mut self, id: WindowId, focused_dead: bool, now: u64) -> Option<WindowId> {
        let owner = self.monitor_index_of(id);
        let other_monitor = owner.is_some_and(|mi| mi != self.active_monitor);
        // 後継フォーカスの防衛: 閉じ処理で後継を選んだ直後に届く
        // 別モニターへの stale なフォールバック前面化は無視して再主張
        let mut reassert = None;
        if let Some((succ, t)) = self.focus_guard {
            if now.saturating_sub(t) >= FOCUS_GUARD_MS || id == succ {
                self.focus_guard = None;
            } else if other_monitor {
                reassert = Some(succ);
            }
        }
        // フォーカス窓が消滅/最小化した直後に OS が Z 順で別モニターの
        // 窓を前面化するフォールバックには追従しない
        // (remove 側がモニター内の後継へフォーカスを移す)
        let os_fallback = other_monitor && self.focused.is_some() && focused_dead;
        if reassert.is_some() || os_fallback {
            return reassert;
        }
        let mi = owner?;
        if mi != self.active_monitor {
            self.cross_follow = self.focused.map(|f| (f, now));
        }
        self.active_monitor = mi;
        let ms = &mut self.monitors[mi];
        // 別ワークスペースのウィンドウなら切替えて追従
        if let Some(si) = ms.stack.strip_index_of(id) {
            ms.stack.active = si;
        }
        self.focused = Some(id);
        let vw = ms.viewport_w;
        let strip = ms.stack.active_mut();
        strip.set_active(id);
        strip.last_focused = Some(id);
        strip.ensure_visible(id, vw);
        None
    }

    /// toggle-floating でフロート化されているか。
    pub fn is_floating(&self, id: WindowId) -> bool {
        self.floating.contains(&id)
    }

    /// タイルをフロート化する (レイアウトから外して記録する)。
    /// タイル側の `focused` は後継へ移るが、フロート化した窓が前面のままなので
    /// 前面化は要求しない。戻り値: フロート化したか (未管理なら false)。
    pub fn float(&mut self, id: WindowId, now: u64) -> bool {
        if self.remove(id, now).is_none() {
            return false;
        }
        // remove の後継防衛は「閉じた直後」用。フロート化では窓が残るので不要
        self.focus_guard = None;
        self.floating.push(id);
        self.float_focus = Some(id);
        // フロート化した窓は前面のまま
        self.float_active = Some(id);
        true
    }

    /// WM が浮かせたわけではないフロート窓 (ダイアログ・ルールの float 等) を記録する。
    /// タイル・フロート化済み・記録済みなら何もしない。
    pub fn add_passive_float(&mut self, id: WindowId) {
        if self.monitor_index_of(id).is_some()
            || self.floating.contains(&id)
            || self.passive_floats.contains(&id)
        {
            return;
        }
        self.passive_floats.push(id);
    }

    /// toggle-floating 済み、または受動的なフロート窓か。
    pub fn is_float(&self, id: WindowId) -> bool {
        self.floating.contains(&id) || self.passive_floats.contains(&id)
    }

    /// 全フロート窓 (toggle-floating 済み → 受動の順)。
    pub fn floats(&self) -> impl Iterator<Item = WindowId> + '_ {
        self.floating
            .iter()
            .chain(self.passive_floats.iter())
            .copied()
    }

    /// 前面窓がフロート窓かをシェルから受け取る。
    /// 前面窓がフロート窓なら Some(id)、それ以外 (タイル・管理外) なら None。
    pub fn set_float_foreground(&mut self, id: Option<WindowId>) {
        self.float_active = id;
    }

    /// 「フォーカスされた見た目」にすべき窓。フロート窓が前面ならそれ、なければタイルの focused。
    pub fn highlighted(&self) -> Option<WindowId> {
        self.float_active.or(self.focused)
    }

    /// フロート窓をタイルへ戻す。記録のない管理外ウィンドウの取り込みにも使う。
    /// 戻り値は前面化すべきウィンドウ (既に管理中なら None)。
    pub fn unfloat(&mut self, id: WindowId) -> Option<WindowId> {
        self.forget_floating(id);
        if self.monitor_index_of(id).is_some() {
            return None;
        }
        self.adopt(id)
    }

    /// フロート窓の記録を消す (ウィンドウの破棄時)。
    pub fn forget_floating(&mut self, id: WindowId) {
        self.floating.retain(|f| *f != id);
        self.passive_floats.retain(|f| *f != id);
        if self.float_focus == Some(id) {
            self.float_focus = None;
        }
        if self.float_active == Some(id) {
            self.float_active = None;
        }
    }

    /// 管理外のフロート窓 (ダイアログ等を含む) が前面になったことを記録する。
    pub fn note_float_focus(&mut self, id: WindowId) {
        self.float_focus = Some(id);
        if let Some(i) = self.floating.iter().position(|f| *f == id) {
            let f = self.floating.remove(i);
            self.floating.push(f);
        }
        if let Some(i) = self.passive_floats.iter().position(|f| *f == id) {
            let f = self.passive_floats.remove(i);
            self.passive_floats.push(f);
        }
    }

    /// focus-floating の切替先候補を優先順に返す。`on_tile`: 現在の前面がタイルか。
    /// タイルからは最後に前面だったフロート窓、次に toggle-floating 済みの窓 (新しい順)、
    /// 最後に受動フロート窓 (新しい順)。それ以外からはタイル側のフォーカスへ。
    /// 生存確認は呼び出し側で行う。
    pub fn focus_toggle_candidates(&self, on_tile: bool) -> Vec<WindowId> {
        if !on_tile {
            return self.focused.into_iter().collect();
        }
        let mut out: Vec<WindowId> = self.float_focus.into_iter().collect();
        for &f in self
            .floating
            .iter()
            .rev()
            .chain(self.passive_floats.iter().rev())
        {
            if !out.contains(&f) {
                out.push(f);
            }
        }
        out
    }

    /// フォーカス移動 (FR-4.1)。上下の端ではワークスペース切替へ続く。
    pub fn focus(&mut self, dir: FocusDir) -> Option<WindowId> {
        let focused = self.focused;
        let (strip, vw) = self.active_strip_mut();
        let target = match focused.filter(|f| strip.contains(*f)) {
            Some(cur) => strip.neighbor(cur, dir),
            None => strip.columns.first().and_then(|c| c.active()),
        };
        match target {
            Some(t) => {
                strip.set_active(t);
                strip.last_focused = Some(t);
                strip.ensure_visible(t, vw);
                self.focused = Some(t);
                Some(t)
            }
            None if dir == FocusDir::Down => self.switch_workspace(true),
            None if dir == FocusDir::Up => self.switch_workspace(false),
            None => None,
        }
    }

    /// ワークスペース切替 (FR-5.4)。切替先で最後にフォーカスしていた窓へ戻る。
    pub fn switch_workspace(&mut self, down: bool) -> Option<WindowId> {
        let stack = &mut self.monitors[self.active_monitor].stack;
        if !stack.switch(down) {
            return None;
        }
        let strip = stack.active_mut();
        self.focused = strip
            .last_focused
            .filter(|f| strip.contains(*f))
            .or_else(|| strip.columns.first().and_then(|c| c.active()));
        if let Some(f) = self.focused {
            strip.set_active(f);
        }
        self.focused
    }

    /// Column の並べ替え (FR-4.4)。
    pub fn move_column(&mut self, dir: FocusDir) {
        let (Some(f), gap) = (self.focused, self.gap) else {
            return;
        };
        let (strip, vw) = self.active_strip_mut();
        if strip.move_column(f, dir, gap) {
            strip.ensure_visible(f, vw);
        }
    }

    /// Tile を右隣へ押し出す (FR-4.5)。
    pub fn expel(&mut self) {
        let (Some(f), gap) = (self.focused, self.gap) else {
            return;
        };
        let (strip, vw) = self.active_strip_mut();
        if strip.expel(f, gap) {
            strip.ensure_visible(f, vw);
        }
    }

    /// 右隣の Tile を取り込む (FR-4.5)。
    pub fn consume(&mut self) {
        let (Some(f), gap) = (self.focused, self.gap) else {
            return;
        };
        self.active_strip_mut().0.consume_right(f, gap);
    }

    /// フォーカスウィンドウを下/上のワークスペースへ移動 (FR-5.4)。
    pub fn move_to_workspace(&mut self, down: bool) {
        let (Some(f), gap) = (self.focused, self.gap) else {
            return;
        };
        let ms = &mut self.monitors[self.active_monitor];
        let (vw, dw) = (ms.viewport_w, ms.default_width);
        if ms.stack.move_window(f, down, dw, vw, gap) {
            let strip = ms.stack.active_mut();
            strip.set_active(f);
            strip.ensure_visible(f, vw);
        }
    }

    /// Column 幅プリセットのサイクル (FR-3.4)。
    pub fn cycle_width(&mut self) {
        let (Some(f), gap) = (self.focused, self.gap) else {
            return;
        };
        let (strip, vw) = self.active_strip_mut();
        if strip.cycle_width(f, vw, gap) {
            strip.ensure_visible(f, vw);
        }
    }

    /// maximize-column トグル (FR-4.6)。
    pub fn maximize_column(&mut self) {
        let (Some(f), gap) = (self.focused, self.gap) else {
            return;
        };
        let (strip, vw) = self.active_strip_mut();
        if strip.toggle_maximize(f, vw, gap) {
            strip.ensure_visible(f, vw);
        }
    }

    /// fullscreen トグル (FR-4.7)。
    pub fn toggle_fullscreen(&mut self) {
        if let Some(f) = self.focused {
            self.fullscreen = (self.fullscreen != Some(f)).then_some(f);
        }
    }

    /// 明示的スクロール (FR-4.3)。
    pub fn scroll(&mut self, forward: bool) {
        let (strip, vw) = self.active_strip_mut();
        strip.scroll_columnwise(forward, vw);
    }

    /// ユーザーの手動リサイズ後の幅を Column 幅として採用する (FR-1.2 MOVESIZEEND)。
    /// maximize 状態は解除する。戻り値は幅が変わったか。
    pub fn resize_column(&mut self, id: WindowId, width: i32) -> bool {
        self.set_column_width(id, width.max(MIN_COLUMN_WIDTH), false)
    }

    /// 最小サイズ制約で要求より広くなったウィンドウの実幅を Column 幅へ
    /// 読み戻す (FR-3.6, §8-4)。広げる方向にしか動かさない。戻り値は幅が変わったか。
    pub fn grow_column(&mut self, id: WindowId, actual_width: i32) -> bool {
        self.set_column_width(id, actual_width, true)
    }

    fn set_column_width(&mut self, id: WindowId, width: i32, only_grow: bool) -> bool {
        let gap = self.gap;
        let Some(mi) = self.monitor_index_of(id) else {
            return false;
        };
        let ms = &mut self.monitors[mi];
        let vw = ms.viewport_w;
        let Some(si) = ms.stack.strip_index_of(id) else {
            return false;
        };
        let strip = &mut ms.stack.strips[si];
        let Some(ci) = strip.column_index_of(id) else {
            return false;
        };
        let col = &mut strip.columns[ci];
        if col.width == width || (only_grow && width < col.width) {
            return false;
        }
        col.width = width;
        if !only_grow {
            col.saved_width = None;
        }
        strip.relayout(gap);
        strip.clamp_offset(vw);
        // 幅の変化でフォーカス Column が viewport からはみ出さないようにする
        let anchor = self.focused.filter(|f| strip.contains(*f)).unwrap_or(id);
        strip.ensure_visible(anchor, vw);
        true
    }

    fn neighbor_monitor(&self, from: usize, forward: bool) -> Option<usize> {
        let n = self.monitors.len();
        if n <= 1 {
            return None;
        }
        Some(if forward {
            (from + 1) % n
        } else {
            (from + n - 1) % n
        })
    }

    /// 隣のモニターへフォーカスを移す (FR-5.1)。
    pub fn focus_monitor(&mut self, forward: bool) -> Option<WindowId> {
        self.active_monitor = self.neighbor_monitor(self.active_monitor, forward)?;
        let (strip, vw) = self.active_strip_mut();
        let f = strip
            .last_focused
            .filter(|f| strip.contains(*f))
            .or_else(|| strip.columns.first().and_then(|c| c.active()));
        if let Some(f) = f {
            strip.set_active(f);
            strip.ensure_visible(f, vw);
        }
        self.focused = f;
        f
    }

    /// フォーカスウィンドウを隣のモニターへ移動する (FR-5.1)。
    pub fn move_to_monitor(&mut self, forward: bool) -> Option<WindowId> {
        let f = self.focused?;
        let src = self.monitor_index_of(f)?;
        let dst = self.neighbor_monitor(src, forward)?;
        let gap = self.gap;
        // 移動元での幅 (maximize 中は元の幅)。宛先では viewport 比を保つ
        let mut src_width = None;
        let src_vw = self.monitors[src].viewport_w;
        // ソース側から除去
        {
            let ms = &mut self.monitors[src];
            let vw = ms.viewport_w;
            if let Some(si) = ms.stack.strip_index_of(f) {
                let strip = &mut ms.stack.strips[si];
                src_width = strip
                    .column_index_of(f)
                    .map(|i| &strip.columns[i])
                    .map(|c| c.saved_width.unwrap_or(c.width));
                strip.remove_window(f, gap);
                strip.clamp_offset(vw);
                if strip.last_focused == Some(f) {
                    strip.last_focused = None;
                }
            }
            ms.stack.normalize();
        }
        // 宛先側へ追加
        let ms = &mut self.monitors[dst];
        let vw = ms.viewport_w;
        let width = match src_width {
            Some(w) => scale_width(w, src_vw, vw).min(vw),
            None => ms.default_width,
        };
        let strip = ms.stack.active_mut();
        let anchor = strip.last_focused;
        strip.insert_column_after(anchor, f, width, gap);
        strip.last_focused = Some(f);
        ms.stack.normalize();
        let strip = ms.stack.active_mut();
        strip.set_active(f);
        strip.ensure_visible(f, vw);
        self.active_monitor = dst;
        Some(f)
    }

    /// モニター構成・作業領域・設定 (gap / デフォルト幅) の変更を反映する
    /// (FR-5.2, FR-5.5)。消えたモニターのウィンドウは active モニターへ退避する。
    /// `geoms` が空 (列挙失敗) の場合は何もしない。
    pub fn reconfigure(&mut self, geoms: &[MonitorGeom], gap: i32, default_ratio: f32) {
        if geoms.is_empty() {
            return;
        }
        self.gap = gap;
        self.default_ratio = default_ratio;

        // HMONITOR は構成変更で振り直されることがある。ハンドルが
        // 消えても同じ物理領域のモニターが残っていれば同一とみなして
        // 付け替える (全ウィンドウが孤立扱いになるのを防ぐ)
        for i in 0..self.monitors.len() {
            if geoms
                .iter()
                .any(|g| g.hmonitor == self.monitors[i].hmonitor)
            {
                continue;
            }
            let same_area = geoms.iter().find(|g| {
                g.monitor == self.monitors[i].monitor
                    && !self.monitors.iter().any(|m| m.hmonitor == g.hmonitor)
            });
            if let Some(g) = same_area {
                self.monitors[i].hmonitor = g.hmonitor;
            }
        }
        // インデックスは retain / sort でずれるので HMONITOR で引き直す
        let active_hmon = self.monitors[self.active_monitor].hmonitor;

        // 消えたモニターのウィンドウを回収
        let mut orphaned: Vec<WindowId> = Vec::new();
        self.monitors.retain(|ms| {
            let alive = geoms.iter().any(|g| g.hmonitor == ms.hmonitor);
            if !alive {
                for strip in &ms.stack.strips {
                    for col in &strip.columns {
                        orphaned.extend_from_slice(&col.tiles);
                    }
                }
            }
            alive
        });

        for g in geoms {
            match self.monitors.iter_mut().find(|m| m.hmonitor == g.hmonitor) {
                Some(ms) => ms.set_geom(*g, gap, default_ratio),
                None => self.monitors.push(Monitor::new(*g, gap, default_ratio)),
            }
        }
        self.monitors.sort_by_key(|m| (m.work.x, m.work.y));
        self.active_monitor = self
            .monitors
            .iter()
            .position(|m| m.hmonitor == active_hmon)
            .unwrap_or_else(|| self.active_monitor.min(self.monitors.len() - 1));

        let ms = &mut self.monitors[self.active_monitor];
        let dw = ms.default_width;
        for id in orphaned {
            ms.stack.active_mut().insert_column_after(None, id, dw, gap);
        }
        ms.stack.normalize();

        for ms in &mut self.monitors {
            for s in &mut ms.stack.strips {
                s.relayout(gap);
                s.clamp_offset(ms.viewport_w);
            }
        }
    }

    /// 全ウィンドウのターゲット Rect (最終的に置くべき画面座標)。
    /// active ワークスペースは通常配置、非 active は上下にずらし、
    /// Viewport 外の Column は全モニターの外へ退避する。
    pub fn targets(&self) -> Vec<(WindowId, Rect)> {
        let bounds = self.virtual_bounds();
        let mut out = Vec::new();
        for ms in &self.monitors {
            // ずらし幅は仮想デスクトップ全体の高さより大きく取り、他モニターと重ねない
            let step = bounds.h + ms.work.h + 200;
            for (i, strip) in ms.stack.strips.iter().enumerate() {
                let dy = (i as i32 - ms.stack.active as i32) * step;
                for (id, p) in strip.project(ms.work, self.gap) {
                    let r = if Some(id) == self.fullscreen && i == ms.stack.active {
                        ms.monitor
                    } else {
                        let base = park(p, ms.work, bounds);
                        Rect {
                            y: base.y + dy,
                            ..base
                        }
                    };
                    out.push((id, r));
                }
            }
        }
        out
    }

    /// 全モニターの物理領域を結合したバウンディングボックス。
    /// 退避座標はこの外に取る — モニターが上下・左右どう配置されても
    /// 非 active ワークスペースや offscreen Column が他モニターに写り込まない。
    fn virtual_bounds(&self) -> Rect {
        let mut it = self.monitors.iter().map(|ms| ms.monitor);
        let first = it.next().unwrap_or_default();
        it.fold(first, |acc, r| {
            let x = acc.x.min(r.x);
            let y = acc.y.min(r.y);
            let right = (acc.x + acc.w).max(r.x + r.w);
            let bottom = (acc.y + acc.h).max(r.y + r.h);
            Rect {
                x,
                y,
                w: right - x,
                h: bottom - y,
            }
        })
    }
}

/// Offscreen Column の退避先。仮想デスクトップ全体 (bounds) の外へ出し、
/// 左右に別モニターがある配置でも隣のモニターに写り込まないようにする。
fn park(p: Placement, work: Rect, bounds: Rect) -> Rect {
    match p {
        Placement::Visible(r) => r,
        Placement::Offscreen(r) => {
            let x = if r.x < work.x {
                bounds.x - r.w - 100
            } else {
                bounds.x + bounds.w + 100
            };
            Rect { x, ..r }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// 2 モニター。左 (index 0) に 1,2、右 (index 1) に 3 を置き、2 にフォーカス。
    fn two_monitors() -> State {
        let mut s = State::new(&[geom(20, 1920), geom(10, 0)], GAP, 0.5);
        s.adopt_initial(0, w(1));
        s.adopt_initial(0, w(2));
        s.adopt_initial(1, w(3));
        s.set_initial_focus(w(2));
        s
    }

    #[test]
    fn monitors_are_sorted_left_to_right() {
        let s = two_monitors();
        assert_eq!(s.monitors[0].hmonitor, 10);
        assert_eq!(s.monitors[1].hmonitor, 20);
        assert_eq!(s.window_count(), 3);
    }

    #[test]
    fn adopt_inserts_right_of_focused_and_focuses() {
        let mut s = two_monitors();
        s.foreground(w(1), false, 0);
        assert_eq!(s.adopt(w(9)), Some(w(9)));
        let cols: Vec<_> = s.monitors[0].stack.strips[0]
            .columns
            .iter()
            .map(|c| c.tiles[0])
            .collect();
        assert_eq!(cols, [w(1), w(9), w(2)]);
        assert_eq!(s.focused, Some(w(9)));
    }

    #[test]
    fn remove_unmanaged_is_none() {
        let mut s = two_monitors();
        assert_eq!(s.remove(w(99), 0), None);
    }

    #[test]
    fn removing_focused_picks_successor_on_same_monitor() {
        let mut s = two_monitors();
        let r = s.remove(w(2), 0).unwrap();
        // 末尾の列が消えたので左隣が後継
        assert_eq!(r.focus, Some(w(1)));
        assert_eq!(s.focused, Some(w(1)));
        assert_eq!(s.active_monitor, 0);
    }

    #[test]
    fn removing_unfocused_keeps_focus() {
        let mut s = two_monitors();
        let r = s.remove(w(1), 0).unwrap();
        assert_eq!(r.focus, None);
        assert_eq!(s.focused, Some(w(2)));
    }

    #[test]
    fn removing_last_window_clears_focus() {
        let mut s = two_monitors();
        s.foreground(w(3), false, 0);
        let r = s.remove(w(3), 5000).unwrap();
        assert_eq!(r.focus, None);
        assert_eq!(s.focused, None);
    }

    #[test]
    fn removing_fullscreen_window_clears_fullscreen() {
        let mut s = two_monitors();
        s.toggle_fullscreen();
        assert_eq!(s.fullscreen, Some(w(2)));
        s.remove(w(2), 0);
        assert_eq!(s.fullscreen, None);
    }

    #[test]
    fn stale_fallback_after_close_is_ignored_and_reasserted() {
        let mut s = two_monitors();
        s.remove(w(2), 1000);
        // 閉じた直後に OS が別モニターの 3 を前面化 → 無視して 1 を再主張
        assert_eq!(s.foreground(w(3), false, 1100), Some(w(1)));
        assert_eq!(s.focused, Some(w(1)));
        assert_eq!(s.active_monitor, 0);
    }

    #[test]
    fn focus_guard_expires() {
        let mut s = two_monitors();
        s.remove(w(2), 1000);
        // 防衛時間を過ぎた前面化はユーザー操作として追従する
        assert_eq!(s.foreground(w(3), false, 1000 + FOCUS_GUARD_MS), None);
        assert_eq!(s.focused, Some(w(3)));
        assert_eq!(s.active_monitor, 1);
    }

    #[test]
    fn os_fallback_to_other_monitor_is_ignored_while_focused_is_dead() {
        let mut s = two_monitors();
        // 2 が消滅済み (Gone 未着) の状態で別モニターの 3 が前面化
        assert_eq!(s.foreground(w(3), true, 0), None);
        assert_eq!(s.focused, Some(w(2)));
        assert_eq!(s.active_monitor, 0);
    }

    #[test]
    fn cross_monitor_follow_is_rolled_back_when_previous_focus_dies() {
        let mut s = two_monitors();
        // 前面を手放してから遅れて破棄するアプリ: 先に 3 への追従が起きる
        assert_eq!(s.foreground(w(3), false, 1000), None);
        assert_eq!(s.active_monitor, 1);
        let r = s.remove(w(2), 1200).unwrap();
        assert!(r.rolled_back);
        assert_eq!(r.focus, Some(w(1)));
        assert_eq!(s.active_monitor, 0);
    }

    #[test]
    fn cross_monitor_follow_is_kept_after_grace_period() {
        let mut s = two_monitors();
        s.foreground(w(3), false, 1000);
        let r = s.remove(w(2), 1000 + CROSS_FOLLOW_MS).unwrap();
        assert!(!r.rolled_back);
        assert_eq!(r.focus, None);
        assert_eq!(s.focused, Some(w(3)));
        assert_eq!(s.active_monitor, 1);
    }

    #[test]
    fn foreground_of_unmanaged_window_keeps_focus() {
        let mut s = two_monitors();
        assert_eq!(s.foreground(w(99), false, 0), None);
        assert_eq!(s.focused, Some(w(2)));
    }

    #[test]
    fn foreground_follows_to_other_workspace() {
        let mut s = two_monitors();
        s.move_to_workspace(true); // 2 を下のワークスペースへ
        assert_eq!(s.monitors[0].stack.active, 1);
        s.foreground(w(1), false, 0);
        assert_eq!(s.monitors[0].stack.active, 0);
        assert_eq!(s.focused, Some(w(1)));
    }

    #[test]
    fn focus_stops_at_horizontal_edge_and_falls_to_workspace_vertically() {
        let mut s = two_monitors();
        assert_eq!(s.focus(FocusDir::Right), None);
        assert_eq!(s.focused, Some(w(2)));
        assert_eq!(s.focus(FocusDir::Left), Some(w(1)));
        s.move_to_workspace(true); // 1 を下へ。上に 2 が残る
        assert_eq!(s.monitors[0].stack.active, 1);
        assert_eq!(s.focus(FocusDir::Up), Some(w(2)));
        assert_eq!(s.monitors[0].stack.active, 0);
    }

    #[test]
    fn focus_monitor_wraps_and_restores_last_focus() {
        let mut s = two_monitors();
        assert_eq!(s.focus_monitor(true), Some(w(3)));
        assert_eq!(s.active_monitor, 1);
        assert_eq!(s.focus_monitor(true), Some(w(2)));
        assert_eq!(s.active_monitor, 0);
    }

    #[test]
    fn move_to_monitor_carries_focus() {
        let mut s = two_monitors();
        assert_eq!(s.move_to_monitor(true), Some(w(2)));
        assert_eq!(s.active_monitor, 1);
        assert_eq!(s.monitor_index_of(w(2)), Some(1));
        assert_eq!(s.focused, Some(w(2)));
    }

    #[test]
    fn resize_column_adopts_width_and_shifts_neighbors() {
        let mut s = two_monitors();
        assert!(s.resize_column(w(1), 500));
        let cols = &s.monitors[0].stack.strips[0].columns;
        assert_eq!(cols[0].width, 500);
        assert_eq!(cols[1].x, 500 + GAP);
        // 同じ幅なら変化なし、極端に小さい幅は下限で止める
        assert!(!s.resize_column(w(1), 500));
        s.resize_column(w(1), 3);
        assert_eq!(
            s.monitors[0].stack.strips[0].columns[0].width,
            MIN_COLUMN_WIDTH
        );
        assert!(!s.resize_column(w(99), 500));
    }

    #[test]
    fn grow_column_only_widens() {
        let mut s = two_monitors();
        let width = s.monitors[0].stack.strips[0].columns[0].width;
        assert!(!s.grow_column(w(1), width - 50));
        assert!(s.grow_column(w(1), width + 50));
        let cols = &s.monitors[0].stack.strips[0].columns;
        assert_eq!(cols[0].width, width + 50);
        assert_eq!(cols[1].x, width + 50 + GAP);
        // 2 回目は安定 (読み戻しがループしない)
        assert!(!s.grow_column(w(1), width + 50));
    }

    #[test]
    fn float_removes_from_layout_and_keeps_tile_focus_on_successor() {
        let mut s = two_monitors();
        assert!(s.float(w(2), 0));
        assert!(s.is_floating(w(2)));
        assert_eq!(s.monitor_index_of(w(2)), None);
        // タイル側のフォーカスは後継へ。フロート窓から戻る先になる
        assert_eq!(s.focused, Some(w(1)));
        assert_eq!(s.focus_toggle_candidates(false), [w(1)]);
        assert_eq!(s.focus_toggle_candidates(true), [w(2)]);
        // フロート化直後に別モニターの窓を前面化しても stale 扱いしない
        assert_eq!(s.foreground(w(3), false, 10), None);
        assert_eq!(s.focused, Some(w(3)));
        assert!(!s.float(w(99), 0));
    }

    #[test]
    fn unfloat_returns_window_to_layout_next_to_focus() {
        let mut s = two_monitors();
        s.float(w(1), 0);
        assert_eq!(s.unfloat(w(1)), Some(w(1)));
        assert!(!s.is_floating(w(1)));
        let cols: Vec<_> = s.monitors[0].stack.strips[0]
            .columns
            .iter()
            .map(|c| c.tiles[0])
            .collect();
        assert_eq!(cols, [w(2), w(1)]);
        assert_eq!(s.focused, Some(w(1)));
        // 既にタイルなら何もしない
        assert_eq!(s.unfloat(w(1)), None);
    }

    #[test]
    fn highlighted_follows_float_foreground_else_tile_focus() {
        let mut s = two_monitors();
        assert_eq!(s.highlighted(), Some(w(2)));
        // フロート化した窓は前面のまま強調される。タイル側の focused は後継へ
        s.float(w(2), 0);
        assert_eq!(s.highlighted(), Some(w(2)));
        assert_eq!(s.focused, Some(w(1)));
        // タイルが前面になれば focused へ戻る
        s.set_float_foreground(None);
        assert_eq!(s.highlighted(), Some(w(1)));
        s.set_float_foreground(Some(w(2)));
        assert_eq!(s.highlighted(), Some(w(2)));
        // unfloat / 破棄で強調を手放す
        s.unfloat(w(2));
        assert_eq!(s.highlighted(), s.focused);
        s.float(w(1), 0);
        s.forget_floating(w(1));
        assert_eq!(s.highlighted(), s.focused);
    }

    #[test]
    fn passive_float_is_tracked_and_highlighted_without_being_user_floated() {
        let mut s = two_monitors();
        s.add_passive_float(w(50));
        // 二重登録・タイル・フロート化済みは無視
        s.add_passive_float(w(50));
        s.add_passive_float(w(1));
        s.float(w(2), 0);
        s.add_passive_float(w(2));
        assert_eq!(s.passive_floats, [w(50)]);
        assert!(s.is_float(w(50)) && s.is_float(w(2)) && !s.is_float(w(1)));
        // toggle-floating 済みを示す is_floating は受動を含まない
        assert!(!s.is_floating(w(50)));
        assert_eq!(s.floats().collect::<Vec<_>>(), [w(2), w(50)]);
        s.set_float_foreground(Some(w(50)));
        assert_eq!(s.highlighted(), Some(w(50)));
        // 破棄・タイル化で受動の記録も消える
        assert_eq!(s.unfloat(w(50)), Some(w(50)));
        assert!(!s.is_float(w(50)));
        assert_eq!(s.highlighted(), s.focused);
        s.add_passive_float(w(60));
        s.forget_floating(w(60));
        assert!(!s.is_float(w(60)));
    }

    #[test]
    fn focus_toggle_includes_passive_floats_after_user_floats() {
        let mut s = two_monitors();
        s.float(w(1), 0);
        s.add_passive_float(w(50));
        s.add_passive_float(w(51));
        // 最後に前面だった窓 → フロート化済み (新しい順) → 受動 (新しい順)
        s.note_float_focus(w(50));
        assert_eq!(s.focus_toggle_candidates(true), [w(50), w(1), w(51)]);
        // 受動窓も前面になるたびに MRU の先頭へ
        s.note_float_focus(w(51));
        s.note_float_focus(w(50));
        assert_eq!(s.passive_floats, [w(51), w(50)]);
        s.note_float_focus(w(1));
        assert_eq!(s.focus_toggle_candidates(true), [w(1), w(50), w(51)]);
        // 前面記録がなくても受動窓へ戻れる
        let mut s = two_monitors();
        s.add_passive_float(w(50));
        assert_eq!(s.focus_toggle_candidates(true), [w(50)]);
    }

    #[test]
    fn focus_toggle_prefers_last_focused_float() {
        let mut s = two_monitors();
        s.float(w(1), 0);
        s.float(w(2), 0);
        assert_eq!(s.focus_toggle_candidates(true), [w(2), w(1)]);
        s.note_float_focus(w(1));
        assert_eq!(s.focus_toggle_candidates(true), [w(1), w(2)]);
        // フロート化していない管理外の窓 (ダイアログ等) も戻り先になる
        s.note_float_focus(w(50));
        assert_eq!(s.focus_toggle_candidates(true), [w(50), w(1), w(2)]);
        s.forget_floating(w(1));
        assert_eq!(s.focus_toggle_candidates(true), [w(50), w(2)]);
        // タイルが 1 つもなければ戻り先なし
        s.remove(w(3), 0);
        assert_eq!(s.focus_toggle_candidates(false), []);
    }

    #[test]
    fn single_monitor_ignores_monitor_commands() {
        let mut s = State::new(&[geom(10, 0)], GAP, 0.5);
        s.adopt_initial(0, w(1));
        s.set_initial_focus(w(1));
        assert_eq!(s.focus_monitor(true), None);
        assert_eq!(s.move_to_monitor(true), None);
        assert_eq!(s.focused, Some(w(1)));
    }

    #[test]
    fn reconfigure_keeps_active_monitor_when_order_changes() {
        let mut s = two_monitors();
        s.foreground(w(3), false, 0);
        assert_eq!(s.active_monitor, 1);
        // 左に新しいモニターが増えて添字がずれる
        s.reconfigure(&[geom(30, -1920), geom(10, 0), geom(20, 1920)], GAP, 0.5);
        assert_eq!(s.monitors[s.active_monitor].hmonitor, 20);
        assert_eq!(s.monitor_index_of(w(3)), Some(s.active_monitor));
    }

    #[test]
    fn reconfigure_rebinds_changed_handles_by_area() {
        let mut s = two_monitors();
        // 同じ領域のまま HMONITOR だけ振り直された
        s.reconfigure(&[geom(11, 0), geom(21, 1920)], GAP, 0.5);
        assert_eq!(s.monitor_index_of(w(1)), Some(0));
        assert_eq!(s.monitor_index_of(w(3)), Some(1));
        assert_eq!(s.monitors[0].stack.strips[0].columns.len(), 2);
    }

    #[test]
    fn reconfigure_moves_orphans_to_active_monitor() {
        let mut s = two_monitors();
        s.reconfigure(&[geom(10, 0)], GAP, 0.5);
        assert_eq!(s.monitors.len(), 1);
        assert_eq!(s.active_monitor, 0);
        assert_eq!(s.monitor_index_of(w(3)), Some(0));
        assert_eq!(s.window_count(), 3);
    }

    #[test]
    fn reconfigure_ignores_empty_enumeration() {
        let mut s = two_monitors();
        s.reconfigure(&[], GAP, 0.5);
        assert_eq!(s.monitors.len(), 2);
    }

    #[test]
    fn reconfigure_applies_new_gap() {
        let mut s = two_monitors();
        s.reconfigure(&[geom(10, 0), geom(20, 1920)], 20, 0.5);
        assert_eq!(s.monitors[0].viewport_w, 1920 - 40);
        let cols = &s.monitors[0].stack.strips[0].columns;
        assert_eq!(cols[1].x, cols[0].width + 20);
    }

    #[test]
    fn reconfigure_keeps_column_ratio_on_resolution_change() {
        let mut s = two_monitors();
        let before = s.monitors[0].stack.strips[0].columns[0].width;
        assert_eq!(before, s.monitors[0].default_width);
        // 左モニターが 1920 → 2560 幅に (右モニターは隣へずれる)
        let wide = Rect {
            x: 0,
            y: 0,
            w: 2560,
            h: 1440,
        };
        let left = MonitorGeom {
            hmonitor: 10,
            work: wide,
            monitor: wide,
        };
        s.reconfigure(&[left, geom(20, 2560)], GAP, 0.5);
        let ms = &s.monitors[0];
        let cols = &ms.stack.strips[0].columns;
        // 半分幅の 2 列は新しい解像度でも viewport にちょうど収まる
        assert!((cols[0].width - ms.default_width).abs() <= 4);
        assert!(cols[1].x + cols[1].width <= ms.viewport_w);
    }

    #[test]
    fn move_to_monitor_keeps_width_ratio() {
        let small = Rect {
            x: 1920,
            y: 0,
            w: 1280,
            h: 720,
        };
        let right = MonitorGeom {
            hmonitor: 20,
            work: small,
            monitor: small,
        };
        let mut s = State::new(&[geom(10, 0), right], GAP, 0.5);
        s.adopt_initial(0, w(1));
        s.set_initial_focus(w(1));
        s.cycle_width(); // 1/2 → 2/3
        let src_vw = s.monitors[0].viewport_w;
        assert_eq!(
            s.monitors[0].stack.strips[0].columns[0].width,
            src_vw * 2 / 3
        );
        s.move_to_monitor(true);
        let dst = &s.monitors[1];
        let width = dst.stack.strips[0].columns[0].width;
        assert!((width - dst.viewport_w * 2 / 3).abs() <= 1);
    }

    #[test]
    fn targets_park_offscreen_outside_all_monitors() {
        let mut s = two_monitors();
        s.adopt(w(4));
        s.adopt(w(5)); // 左モニターに 4 列 → 先頭側が viewport 外
        let t = s.targets();
        let rect = |id| t.iter().find(|(i, _)| *i == w(id)).unwrap().1;
        // フォーカス中の 5 は左モニター内
        assert!(rect(5).x >= 0 && rect(5).x + rect(5).w <= 1920);
        // viewport 外の 1 は仮想デスクトップ全体 (0..3840) の外
        assert!(rect(1).x + rect(1).w <= 0 || rect(1).x >= 3840);
    }

    #[test]
    fn targets_shift_inactive_workspaces_and_expand_fullscreen() {
        let mut s = two_monitors();
        s.toggle_fullscreen();
        s.foreground(w(1), false, 0);
        s.move_to_workspace(true); // 1 を下へ。active は下のワークスペース
        let t = s.targets();
        let rect = |id| t.iter().find(|(i, _)| *i == w(id)).unwrap().1;
        assert!(rect(1).y >= 0 && rect(1).y < 1080);
        // 上のワークスペースに残った 2 は fullscreen 指定でも画面外
        assert!(rect(2).y + rect(2).h <= 0);
        s.switch_workspace(false);
        let t = s.targets();
        let full = t.iter().find(|(i, _)| *i == w(2)).unwrap().1;
        assert_eq!(full, s.monitors[0].monitor);
    }
}
