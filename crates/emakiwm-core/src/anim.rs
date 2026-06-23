//! アニメーション補間 (FR-4.8)。
//!
//! レイアウト変化 (スクロール・列の詰め・挿入) はすべて
//! 「各ウィンドウの Rect が from → to へ動く」として表現し、ここで補間する。
//! 時間の供給は Win32 層 (Instant) が行い、ここは純粋ロジックのみ。

use crate::layout::Rect;

/// ease-out cubic。終端に向かって減速する。範囲外の t はクランプ。
pub fn ease_out_cubic(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

fn lerp(a: i32, b: i32, t: f32) -> i32 {
    a + ((b - a) as f32 * t).round() as i32
}

/// eased_t (0.0..=1.0、ease 適用済み) における中間 Rect。
/// 端点では厳密に from / to に一致する。
pub fn lerp_rect(from: Rect, to: Rect, eased_t: f32) -> Rect {
    Rect {
        x: lerp(from.x, to.x, eased_t),
        y: lerp(from.y, to.y, eased_t),
        w: lerp(from.w, to.w, eased_t),
        h: lerp(from.h, to.h, eased_t),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FROM: Rect = Rect {
        x: 100,
        y: 50,
        w: 600,
        h: 900,
    };
    const TO: Rect = Rect {
        x: 900,
        y: 50,
        w: 800,
        h: 900,
    };

    #[test]
    fn endpoints_are_exact() {
        assert_eq!(lerp_rect(FROM, TO, 0.0), FROM);
        assert_eq!(lerp_rect(FROM, TO, 1.0), TO);
    }

    #[test]
    fn ease_clamps_and_decelerates() {
        assert_eq!(ease_out_cubic(-1.0), 0.0);
        assert_eq!(ease_out_cubic(2.0), 1.0);
        // ease-out: 前半で半分以上進む
        assert!(ease_out_cubic(0.5) > 0.5);
        // 単調増加
        let mut prev = 0.0;
        for i in 0..=10 {
            let v = ease_out_cubic(i as f32 / 10.0);
            assert!(v >= prev);
            prev = v;
        }
    }

    #[test]
    fn midpoint_is_between() {
        let mid = lerp_rect(FROM, TO, ease_out_cubic(0.5));
        assert!(mid.x > FROM.x && mid.x < TO.x);
        assert!(mid.w > FROM.w && mid.w < TO.w);
        assert_eq!(mid.y, 50); // 変化しない成分は固定
    }

    #[test]
    fn backward_motion_works() {
        let mid = lerp_rect(TO, FROM, ease_out_cubic(0.5));
        assert!(mid.x < TO.x && mid.x > FROM.x);
    }
}
