//! 管理対象判定 (FR-2.1)。
//!
//! Win32 から収集した属性のスナップショット [`WindowInfo`] を受け取り、
//! 管理対象かどうかを純粋関数 [`decide`] で判定する。

/// Win32 ウィンドウスタイル定数。
/// windows crate へ依存しないためローカルに定義する (値は winuser.h 準拠)。
pub const WS_POPUP: u32 = 0x8000_0000;
pub const WS_CAPTION: u32 = 0x00C0_0000; // WS_BORDER | WS_DLGFRAME
pub const WS_THICKFRAME: u32 = 0x0004_0000;
pub const WS_EX_TOOLWINDOW: u32 = 0x0000_0080;

/// 1 トップレベルウィンドウの属性スナップショット。Win32 層が収集する。
#[derive(Debug, Clone, Default)]
pub struct WindowInfo {
    pub title: String,
    pub class_name: String,
    /// exe ファイル名 (パスを除く)。プロセスを開けない場合 None
    pub exe_name: Option<String>,
    pub style: u32,
    pub ex_style: u32,
    pub is_visible: bool,
    /// DWMWA_CLOAKED ≠ 0 (UWP ゴースト・他仮想デスクトップ)
    pub is_cloaked: bool,
    /// GW_OWNER を持つ (ダイアログ等)
    pub has_owner: bool,
    pub is_own_process: bool,
}

/// 管理対象判定の結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// タイリング管理対象
    Manage,
    /// 位置を触らないが存在は認識する (FR-2.1 のフローティング扱い)
    Float(&'static str),
    /// 一覧にも載せない
    Ignore(&'static str),
}

/// 設定ファイル由来のルール (FR-2.2)。最初にマッチしたものを適用する。
#[derive(Debug, Clone)]
pub struct Rule {
    /// 指定されたパターンすべてにマッチしたら適用 (AND)。全部 None のルールは無効
    pub exe: Option<regex::Regex>,
    pub class: Option<regex::Regex>,
    pub title: Option<regex::Regex>,
    pub action: RuleAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleAction {
    Ignore,
    Manage,
    Float,
}

impl Rule {
    fn matches(&self, w: &WindowInfo) -> bool {
        if self.exe.is_none() && self.class.is_none() && self.title.is_none() {
            return false;
        }
        self.exe
            .as_ref()
            .is_none_or(|re| w.exe_name.as_deref().is_some_and(|e| re.is_match(e)))
            && self
                .class
                .as_ref()
                .is_none_or(|re| re.is_match(&w.class_name))
            && self.title.as_ref().is_none_or(|re| re.is_match(&w.title))
    }
}

/// FR-2.1 の除外ルール + FR-2.2 の設定ルールを適用する。
/// 不可視・自プロセス・cloaked は force-manage でも覆せない
/// (他仮想デスクトップのウィンドウを掴む事故を防ぐ)。
pub fn decide(w: &WindowInfo, rules: &[Rule]) -> Decision {
    if !w.is_visible {
        return Decision::Ignore("invisible");
    }
    if w.is_own_process {
        return Decision::Ignore("own process");
    }
    if w.is_cloaked {
        return Decision::Ignore("cloaked (UWP ghost / other virtual desktop)");
    }
    for rule in rules {
        if rule.matches(w) {
            return match rule.action {
                RuleAction::Ignore => Decision::Ignore("rule: ignore"),
                RuleAction::Manage => Decision::Manage,
                RuleAction::Float => Decision::Float("rule: float"),
            };
        }
    }
    if w.ex_style & WS_EX_TOOLWINDOW != 0 {
        return Decision::Float("WS_EX_TOOLWINDOW");
    }
    if w.has_owner {
        return Decision::Float("owned window (dialog)");
    }
    if w.style & WS_POPUP != 0 && w.style & WS_CAPTION != WS_CAPTION {
        return Decision::Float("WS_POPUP without caption");
    }
    if w.style & WS_THICKFRAME == 0 {
        return Decision::Float("not resizable (no WS_THICKFRAME)");
    }
    Decision::Manage
}

#[cfg(test)]
mod tests {
    use super::*;

    /// メモ帳相当: 可視・キャプション・リサイズ可
    fn normal_window() -> WindowInfo {
        WindowInfo {
            is_visible: true,
            style: WS_CAPTION | WS_THICKFRAME,
            ..Default::default()
        }
    }

    #[test]
    fn normal_window_is_managed() {
        assert_eq!(decide(&normal_window(), &[]), Decision::Manage);
    }

    #[test]
    fn invisible_is_ignored() {
        let w = WindowInfo {
            is_visible: false,
            ..normal_window()
        };
        assert!(matches!(decide(&w, &[]), Decision::Ignore(_)));
    }

    #[test]
    fn own_process_is_ignored() {
        let w = WindowInfo {
            is_own_process: true,
            ..normal_window()
        };
        assert!(matches!(decide(&w, &[]), Decision::Ignore(_)));
    }

    #[test]
    fn cloaked_is_ignored() {
        let w = WindowInfo {
            is_cloaked: true,
            ..normal_window()
        };
        assert!(matches!(decide(&w, &[]), Decision::Ignore(_)));
    }

    #[test]
    fn toolwindow_floats() {
        let w = WindowInfo {
            ex_style: WS_EX_TOOLWINDOW,
            ..normal_window()
        };
        assert!(matches!(decide(&w, &[]), Decision::Float(_)));
    }

    #[test]
    fn owned_dialog_floats() {
        let w = WindowInfo {
            has_owner: true,
            ..normal_window()
        };
        assert!(matches!(decide(&w, &[]), Decision::Float(_)));
    }

    #[test]
    fn bare_popup_floats() {
        let w = WindowInfo {
            is_visible: true,
            style: WS_POPUP | WS_THICKFRAME,
            ..Default::default()
        };
        assert!(matches!(decide(&w, &[]), Decision::Float(_)));
    }

    #[test]
    fn popup_with_caption_and_thickframe_is_managed() {
        // 一部アプリ (Chromium 系等) は WS_POPUP とキャプションを併用する
        let w = WindowInfo {
            is_visible: true,
            style: WS_POPUP | WS_CAPTION | WS_THICKFRAME,
            ..Default::default()
        };
        assert_eq!(decide(&w, &[]), Decision::Manage);
    }

    #[test]
    fn non_resizable_floats() {
        let w = WindowInfo {
            is_visible: true,
            style: WS_CAPTION,
            ..Default::default()
        };
        assert_eq!(
            decide(&w, &[]),
            Decision::Float("not resizable (no WS_THICKFRAME)")
        );
    }
}
