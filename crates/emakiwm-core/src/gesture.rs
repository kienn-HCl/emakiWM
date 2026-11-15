//! タッチパッドの 3 本指スワイプ判定。
//!
//! Precision Touchpad の HID レポートから取り出した観測値
//! (接触本数 + 第 1 接触点の正規化座標) の列を受け取り、
//! 「3 本指で一定距離動いた」を方向つきで検出する状態機械。
//! Win32 依存の解析 (Raw Input / HidP) は emakiwm 側 touchpad.rs が担う。

/// スワイプの指の移動方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwipeDir {
    Up,
    Down,
    Left,
    Right,
}

/// レポートがこれ以上途切れたら別ジェスチャとみなす (ms)。
/// 接触ありのタッチパッドは数 ms 間隔でレポートし続けるので、
/// 途切れ = 全指リフトの検出として十分。
const GESTURE_GAP_MS: u32 = 200;

/// 3 本指スワイプの検出器。
pub struct SwipeTracker {
    /// 現在のジェスチャの基準点 (正規化座標)。None = 3 本指未成立
    origin: Option<(f32, f32)>,
    /// 最後に観測したレポートの時刻 (ms)
    last_ms: u32,
    /// 発火に必要な移動量 (パッド辺長に対する比率)
    threshold: f32,
}

impl SwipeTracker {
    pub fn new(threshold: f32) -> Self {
        Self {
            origin: None,
            last_ms: 0,
            threshold,
        }
    }

    /// HID レポート 1 件ぶんの観測を与え、スワイプ確定時に方向を返す。
    ///
    /// - `contacts`: レポートの Contact Count。ハイブリッドモードの継続
    ///   パケット (値 0) は接触スロットの割当が先頭パケットと異なるため、
    ///   位置は見ずタイムスタンプの更新のみ行う
    /// - `x`, `y`: 第 1 接触点の正規化座標 (0.0..=1.0、y は下向き正)
    /// - `now_ms`: レポート時刻 (ms、循環タイムスタンプ可)
    ///
    /// threshold を超えるたびに発火して基準点を現在位置へ移すので、
    /// 長いスワイプ 1 回で複数回発火する (ワークスペースの連続送り)。
    pub fn update(&mut self, contacts: u32, x: f32, y: f32, now_ms: u32) -> Option<SwipeDir> {
        if now_ms.wrapping_sub(self.last_ms) > GESTURE_GAP_MS {
            self.origin = None;
        }
        self.last_ms = now_ms;
        if contacts == 0 {
            return None;
        }
        if contacts != 3 {
            self.origin = None;
            return None;
        }
        let (ox, oy) = *self.origin.get_or_insert((x, y));
        let (dx, dy) = (x - ox, y - oy);
        if dx.abs() < self.threshold && dy.abs() < self.threshold {
            return None;
        }
        self.origin = Some((x, y));
        Some(if dx.abs() >= dy.abs() {
            if dx > 0.0 {
                SwipeDir::Right
            } else {
                SwipeDir::Left
            }
        } else if dy > 0.0 {
            SwipeDir::Down
        } else {
            SwipeDir::Up
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracker() -> SwipeTracker {
        SwipeTracker::new(0.15)
    }

    #[test]
    fn three_finger_swipe_right_fires() {
        let mut t = tracker();
        assert_eq!(t.update(3, 0.30, 0.50, 0), None);
        assert_eq!(t.update(3, 0.40, 0.50, 10), None); // 閾値未満
        assert_eq!(t.update(3, 0.50, 0.51, 20), Some(SwipeDir::Right));
    }

    #[test]
    fn vertical_swipe_up_fires() {
        let mut t = tracker();
        assert_eq!(t.update(3, 0.50, 0.80, 0), None);
        assert_eq!(t.update(3, 0.51, 0.60, 10), Some(SwipeDir::Up));
    }

    #[test]
    fn two_fingers_never_fire() {
        let mut t = tracker();
        assert_eq!(t.update(2, 0.20, 0.50, 0), None);
        assert_eq!(t.update(2, 0.90, 0.50, 10), None);
    }

    #[test]
    fn long_swipe_fires_repeatedly() {
        let mut t = tracker();
        assert_eq!(t.update(3, 0.10, 0.50, 0), None);
        assert_eq!(t.update(3, 0.30, 0.50, 10), Some(SwipeDir::Right));
        assert_eq!(t.update(3, 0.40, 0.50, 20), None); // 基準点リセット後は閾値未満
        assert_eq!(t.update(3, 0.50, 0.50, 30), Some(SwipeDir::Right));
    }

    #[test]
    fn finger_count_change_resets_origin() {
        let mut t = tracker();
        assert_eq!(t.update(3, 0.10, 0.50, 0), None);
        assert_eq!(t.update(4, 0.20, 0.50, 10), None); // 4 本指で解除
        assert_eq!(t.update(3, 0.30, 0.50, 20), None); // ここが新しい基準点
        assert_eq!(t.update(3, 0.38, 0.50, 30), None); // 0.30 からは閾値未満
        assert_eq!(t.update(3, 0.50, 0.50, 40), Some(SwipeDir::Right));
    }

    #[test]
    fn gap_starts_new_gesture() {
        let mut t = tracker();
        assert_eq!(t.update(3, 0.10, 0.50, 0), None);
        // 200ms 超の空白 → 全指リフト扱い。離れた位置でも発火しない
        assert_eq!(t.update(3, 0.90, 0.50, 500), None);
    }

    #[test]
    fn hybrid_continuation_packets_are_ignored() {
        let mut t = tracker();
        assert_eq!(t.update(3, 0.10, 0.50, 0), None);
        // 継続パケット (contacts=0) は別スロットの座標なので位置を見ない
        assert_eq!(t.update(0, 0.90, 0.90, 5), None);
        assert_eq!(t.update(3, 0.12, 0.50, 10), None); // 基準点は 0.10 のまま
        assert_eq!(t.update(3, 0.30, 0.50, 20), Some(SwipeDir::Right));
    }

    #[test]
    fn dominant_axis_wins() {
        let mut t = tracker();
        assert_eq!(t.update(3, 0.50, 0.50, 0), None);
        // 斜め移動は移動量の大きい軸で判定
        assert_eq!(t.update(3, 0.60, 0.75, 10), Some(SwipeDir::Down));
    }
}
