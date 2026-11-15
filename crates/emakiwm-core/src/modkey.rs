//! 無変換 (VK_NONCONVERT) を修飾キーとして扱う状態機械。
//!
//! RegisterHotKey は Alt/Ctrl/Shift/Win しか修飾にできないため、無変換を含む
//! バインドは WH_KEYBOARD_LL フックで処理する。ここではフックから渡される
//! キーイベント列を見て「通す / 食う / 発火 / 単打の再送」を決める。
//! Win32 には依存せず、時刻は呼び出し側が ms 単位で渡す (wrapping 前提)。

use crate::keybind::MOD_MUHENKAN;

/// 無変換キーの仮想キーコード
pub const VK_NONCONVERT: u32 = 0x1D;

/// 保持状態が無操作のままこれを超えたら、無変換が離されたものとみなして解除する (ms)。
///
/// 無変換の key-up を取り逃すと (管理者権限ウィンドウが前面、ロック画面、UAC 等)、
/// 保持状態が残って素の h/j/k/l を食い続けてしまう。その保険。
/// GetAsyncKeyState による実状態の確認は使えない: フックが無変換自体を食うため、
/// システム側のキー状態には無変換が一度も押されたことにならない。
pub const HOLD_IDLE_MS: u32 = 5000;

/// フックが取るべき動作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
    /// 何もせず次のフックへ渡す
    Pass,
    /// イベントを食う (アプリへ渡さない)
    Swallow,
    /// i 番目のバインドを発火し、イベントは食う
    Fire(usize),
    /// 無変換の単打: イベントは食い、無変換の down+up を IME 向けに再送する
    ReplayTap,
}

#[derive(Debug, Default)]
pub struct ModKeyTracker {
    /// 無変換を押下中とみなしているか
    held: bool,
    /// 保持中に他キーが押された (単打ではない)
    used: bool,
    /// 保持中に最後にイベントを処理した時刻 (ms)
    last_ms: u32,
    /// 発火済みで押下中のキー。リピートと key-up を食うために覚える
    down: Vec<u32>,
}

fn is_modifier_vk(vk: u32) -> bool {
    matches!(vk, 0x10..=0x12 | 0xA0..=0xA5 | 0x5B | 0x5C)
}

impl ModKeyTracker {
    pub const fn new() -> Self {
        Self {
            held: false,
            used: false,
            last_ms: 0,
            down: Vec::new(),
        }
    }

    /// キーイベント 1 件を処理する。
    /// `other_mods` は無変換以外で押下中の修飾 (MOD_SHIFT 等)。
    /// `lookup` は (mods | MOD_MUHENKAN, vk) に対応するバインドの添字を返す。
    pub fn on_key(
        &mut self,
        vk: u32,
        down: bool,
        now_ms: u32,
        other_mods: u32,
        lookup: impl Fn(u32, u32) -> Option<usize>,
    ) -> KeyAction {
        // 先に期限切れを判定する (key-up 取り逃しの救済)
        if self.held && now_ms.wrapping_sub(self.last_ms) > HOLD_IDLE_MS {
            self.held = false;
            self.used = false;
            self.down.clear();
        }
        if self.held {
            self.last_ms = now_ms;
        }

        if vk == VK_NONCONVERT {
            if down {
                if !self.held {
                    self.held = true;
                    self.used = false;
                }
                self.last_ms = now_ms;
                return KeyAction::Swallow; // リピートも含めて食う
            }
            if !self.held {
                return KeyAction::Pass; // 状態不明 / 期限切れ
            }
            self.held = false;
            // down は消さない: 押しっぱなしのキーの key-up / リピートを食うため
            return if self.used {
                KeyAction::Swallow
            } else {
                KeyAction::ReplayTap
            };
        }

        if is_modifier_vk(vk) {
            return KeyAction::Pass; // 修飾キーの操作は「使用」に数えない
        }

        if down {
            if self.down.contains(&vk) {
                return KeyAction::Swallow; // オートリピート (MOD_NOREPEAT 相当)
            }
            if !self.held {
                return KeyAction::Pass;
            }
            self.used = true;
            return match lookup(other_mods | MOD_MUHENKAN, vk) {
                Some(i) => {
                    self.down.push(vk);
                    KeyAction::Fire(i)
                }
                None => KeyAction::Pass, // 未割当はアプリへ素通し (IME 再送も抑止)
            };
        }

        if let Some(pos) = self.down.iter().position(|&k| k == vk) {
            self.down.remove(pos);
            return KeyAction::Swallow;
        }
        KeyAction::Pass
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keybind::MOD_SHIFT;

    const L: u32 = 'L' as u32;
    const H: u32 = 'H' as u32;

    fn lookup(mods: u32, vk: u32) -> Option<usize> {
        match (mods, vk) {
            (m, L) if m == MOD_MUHENKAN => Some(0),
            (m, H) if m == MOD_MUHENKAN => Some(1),
            (m, L) if m == MOD_MUHENKAN | MOD_SHIFT => Some(2),
            _ => None,
        }
    }

    fn k(t: &mut ModKeyTracker, vk: u32, down: bool, now: u32) -> KeyAction {
        t.on_key(vk, down, now, 0, lookup)
    }

    #[test]
    fn tap_alone_replays() {
        let mut t = ModKeyTracker::new();
        assert_eq!(k(&mut t, VK_NONCONVERT, true, 0), KeyAction::Swallow);
        assert_eq!(k(&mut t, VK_NONCONVERT, true, 30), KeyAction::Swallow); // リピート
        assert_eq!(k(&mut t, VK_NONCONVERT, false, 60), KeyAction::ReplayTap);
    }

    #[test]
    fn combo_fires_once_and_release_swallows() {
        let mut t = ModKeyTracker::new();
        k(&mut t, VK_NONCONVERT, true, 0);
        assert_eq!(k(&mut t, L, true, 10), KeyAction::Fire(0));
        assert_eq!(k(&mut t, L, false, 20), KeyAction::Swallow);
        assert_eq!(k(&mut t, VK_NONCONVERT, false, 30), KeyAction::Swallow);
    }

    #[test]
    fn autorepeat_does_not_refire() {
        let mut t = ModKeyTracker::new();
        k(&mut t, VK_NONCONVERT, true, 0);
        assert_eq!(k(&mut t, L, true, 10), KeyAction::Fire(0));
        assert_eq!(k(&mut t, L, true, 50), KeyAction::Swallow);
        assert_eq!(k(&mut t, L, true, 90), KeyAction::Swallow);
    }

    #[test]
    fn second_combo_while_held_fires() {
        let mut t = ModKeyTracker::new();
        k(&mut t, VK_NONCONVERT, true, 0);
        assert_eq!(k(&mut t, L, true, 10), KeyAction::Fire(0));
        assert_eq!(k(&mut t, L, false, 20), KeyAction::Swallow);
        assert_eq!(k(&mut t, H, true, 30), KeyAction::Fire(1));
        // 修飾つきは other_mods を渡して別バインドを引く
        assert_eq!(t.on_key(L, true, 40, MOD_SHIFT, lookup), KeyAction::Fire(2));
    }

    #[test]
    fn unbound_combo_passes_and_suppresses_replay() {
        let mut t = ModKeyTracker::new();
        k(&mut t, VK_NONCONVERT, true, 0);
        assert_eq!(k(&mut t, 'Z' as u32, true, 10), KeyAction::Pass);
        assert_eq!(k(&mut t, 'Z' as u32, false, 20), KeyAction::Pass);
        assert_eq!(k(&mut t, VK_NONCONVERT, false, 30), KeyAction::Swallow);
    }

    #[test]
    fn modifier_keys_do_not_count_as_use() {
        let mut t = ModKeyTracker::new();
        k(&mut t, VK_NONCONVERT, true, 0);
        assert_eq!(k(&mut t, 0xA0, true, 10), KeyAction::Pass); // LShift
        assert_eq!(k(&mut t, 0xA0, false, 20), KeyAction::Pass);
        assert_eq!(k(&mut t, VK_NONCONVERT, false, 30), KeyAction::ReplayTap);
    }

    #[test]
    fn keyup_of_consumed_key_swallowed_after_release() {
        let mut t = ModKeyTracker::new();
        k(&mut t, VK_NONCONVERT, true, 0);
        assert_eq!(k(&mut t, L, true, 10), KeyAction::Fire(0));
        assert_eq!(k(&mut t, VK_NONCONVERT, false, 20), KeyAction::Swallow);
        // 無変換を先に離しても、リピートと key-up はアプリへ漏らさない
        assert_eq!(k(&mut t, L, true, 30), KeyAction::Swallow);
        assert_eq!(k(&mut t, L, false, 40), KeyAction::Swallow);
        assert_eq!(k(&mut t, L, false, 50), KeyAction::Pass);
    }

    #[test]
    fn plain_key_passes_when_not_held() {
        let mut t = ModKeyTracker::new();
        assert_eq!(k(&mut t, L, true, 0), KeyAction::Pass);
        assert_eq!(k(&mut t, L, false, 10), KeyAction::Pass);
    }

    #[test]
    fn mk_up_when_not_held_passes() {
        let mut t = ModKeyTracker::new();
        assert_eq!(k(&mut t, VK_NONCONVERT, false, 0), KeyAction::Pass);
    }

    #[test]
    fn stale_hold_expires() {
        let mut t = ModKeyTracker::new();
        k(&mut t, VK_NONCONVERT, true, 1000);
        // 期限内なら発火する
        assert_eq!(k(&mut t, L, true, 1000 + HOLD_IDLE_MS), KeyAction::Fire(0));
        assert_eq!(k(&mut t, L, false, 1000 + HOLD_IDLE_MS), KeyAction::Swallow);
        // 最後のイベントから HOLD_IDLE_MS 超で解除され、素のキーは通る
        let late = 1000 + HOLD_IDLE_MS + HOLD_IDLE_MS + 1;
        assert_eq!(k(&mut t, L, true, late), KeyAction::Pass);
        // 取り逃した key-up は通す
        assert_eq!(k(&mut t, VK_NONCONVERT, false, late + 1), KeyAction::Pass);
    }

    #[test]
    fn activity_refreshes_hold() {
        let mut t = ModKeyTracker::new();
        k(&mut t, VK_NONCONVERT, true, 0);
        // リピートが続く限り期限切れにならない
        for i in 1..=10 {
            k(&mut t, VK_NONCONVERT, true, i * 3000);
        }
        assert_eq!(k(&mut t, L, true, 30_100), KeyAction::Fire(0));
    }

    #[test]
    fn wrapping_timestamps() {
        let mut t = ModKeyTracker::new();
        let start = u32::MAX - 100;
        k(&mut t, VK_NONCONVERT, true, start);
        // wrap をまたいでも 200ms 後は保持中
        assert_eq!(
            k(&mut t, L, true, start.wrapping_add(200)),
            KeyAction::Fire(0)
        );
        // wrap をまたいで期限超過
        let mut t = ModKeyTracker::new();
        k(&mut t, VK_NONCONVERT, true, start);
        assert_eq!(
            k(&mut t, L, true, start.wrapping_add(HOLD_IDLE_MS + 1)),
            KeyAction::Pass
        );
    }
}
