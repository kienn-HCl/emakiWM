//! キーバインドの解釈 (FR-6.2 / FR-6.3)。
//!
//! キー文字列のパース、`[vars]` による `$name` 展開、デフォルト表とユーザー設定の
//! マージを担う純粋ロジック。Win32 には依存しない。

use std::collections::{BTreeMap, BTreeSet};

// RegisterHotKey の modifier 値 (winuser.h)
pub const MOD_ALT: u32 = 0x1;
pub const MOD_CONTROL: u32 = 0x2;
pub const MOD_SHIFT: u32 = 0x4;
pub const MOD_WIN: u32 = 0x8;
/// 無変換 (VK_NONCONVERT) を修飾キーとして使うバインド。独自ビットで、
/// RegisterHotKey には渡さない (WH_KEYBOARD_LL フックで処理する)
pub const MOD_MUHENKAN: u32 = 0x1_0000;

/// `mod` 変数の既定値
const DEFAULT_MOD: &str = "alt";

/// "alt+shift+h" 形式 → (modifier ビット和, 仮想キーコード)。
pub fn parse_key(s: &str) -> Option<(u32, u32)> {
    let s = s.to_lowercase();
    let parts: Vec<&str> = s.split('+').map(str::trim).collect();
    let (key, mod_parts) = parts.split_last()?;
    let mut mods = 0u32;
    for m in mod_parts {
        mods |= match *m {
            "alt" => MOD_ALT,
            "ctrl" | "control" => MOD_CONTROL,
            "shift" => MOD_SHIFT,
            "win" | "super" => MOD_WIN,
            "muhenkan" | "nonconvert" => MOD_MUHENKAN,
            _ => return None,
        };
    }
    if mods == 0 {
        return None; // modifier なしのグローバルキーは奪わない
    }
    let vk = match *key {
        k if k.len() == 1 && k.chars().next()?.is_ascii_alphanumeric() => {
            k.chars().next()?.to_ascii_uppercase() as u32
        }
        "comma" => 0xBC,
        "period" => 0xBE,
        "minus" => 0xBD,
        "slash" => 0xBF,
        "semicolon" => 0xBA,
        "space" => 0x20,
        "enter" | "return" => 0x0D,
        "tab" => 0x09,
        "esc" | "escape" => 0x1B,
        "backspace" => 0x08,
        "left" => 0x25,
        "up" => 0x26,
        "right" => 0x27,
        "down" => 0x28,
        "pageup" => 0x21,
        "pagedown" => 0x22,
        "home" => 0x24,
        "end" => 0x23,
        k if k.starts_with('f') => {
            let n: u32 = k[1..].parse().ok()?;
            if !(1..=24).contains(&n) {
                return None;
            }
            0x70 + n - 1
        }
        _ => return None,
    };
    Some((mods, vk))
}

/// キー文字列中の `$name` (name は `[A-Za-z0-9_]+`) を vars の値へ置換する。
/// 再帰展開はしない。未定義の変数があればその名前を Err で返す。
pub fn expand_vars(key: &str, vars: &BTreeMap<String, String>) -> Result<String, String> {
    let mut out = String::with_capacity(key.len());
    let mut rest = key;
    while let Some(pos) = rest.find('$') {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + 1..];
        let end = after
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(after.len());
        let name = &after[..end];
        if name.is_empty() {
            // 名前のない `$` はそのまま残す (後段のパースで弾かれる)
            out.push('$');
        } else {
            match vars.get(name) {
                Some(v) => out.push_str(v),
                None => return Err(name.to_string()),
            }
        }
        rest = &after[end..];
    }
    out.push_str(rest);
    Ok(out)
}

/// デフォルト表とユーザー設定を、展開・パース後のキー (mods, vk) 単位でマージする。
/// ユーザー設定が勝ち、コマンドが空 / "none" のものはマージ後に除く。
/// 戻り値は (mods, vk) 昇順の (mods, vk, コマンド) と警告メッセージ。
/// `mod` が vars に無ければ "alt" を補う。
pub fn resolve_binds(
    defaults: &[(&str, &str)],
    user: &BTreeMap<String, String>,
    vars: &BTreeMap<String, String>,
) -> (Vec<(u32, u32, String)>, Vec<String>) {
    let mut vars = vars.clone();
    vars.entry("mod".to_string())
        .or_insert_with(|| DEFAULT_MOD.to_string());

    let mut warnings = Vec::new();
    let mut merged: BTreeMap<(u32, u32), String> = BTreeMap::new();
    let resolve = |key: &str, warnings: &mut Vec<String>| -> Option<(u32, u32)> {
        let expanded = match expand_vars(key, &vars) {
            Ok(e) => e,
            Err(name) => {
                warnings.push(format!(
                    "キー \"{key}\" の変数 ${name} が [vars] にありません"
                ));
                return None;
            }
        };
        let parsed = parse_key(&expanded);
        if parsed.is_none() {
            warnings.push(format!(
                "キー \"{key}\" を解釈できません (展開後: \"{expanded}\")"
            ));
        }
        parsed
    };

    for (k, cmd) in defaults {
        if let Some(combo) = resolve(k, &mut warnings) {
            merged.insert(combo, cmd.to_string());
        }
    }
    // user は BTreeMap なのでキー昇順 = 決定的。同一の組み合わせは後勝ち
    let mut seen: BTreeSet<(u32, u32)> = BTreeSet::new();
    for (k, cmd) in user {
        let Some(combo) = resolve(k, &mut warnings) else {
            continue;
        };
        if !seen.insert(combo) {
            warnings.push(format!(
                "キー \"{k}\" は他のキーと同じ組み合わせです (後のものを採用)"
            ));
        }
        merged.insert(combo, cmd.clone());
    }

    let binds = merged
        .into_iter()
        .filter(|(_, c)| !c.is_empty() && c != "none")
        .map(|((m, vk), c)| (m, vk, c))
        .collect();
    (binds, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn find(binds: &[(u32, u32, String)], mods: u32, vk: u32) -> Option<&str> {
        binds
            .iter()
            .find(|(m, v, _)| *m == mods && *v == vk)
            .map(|(_, _, c)| c.as_str())
    }

    #[test]
    fn parse_key_basics() {
        assert_eq!(parse_key("alt+h"), Some((MOD_ALT, 'H' as u32)));
        assert_eq!(
            parse_key("alt+shift+comma"),
            Some((MOD_ALT | MOD_SHIFT, 0xBC))
        );
        assert_eq!(
            parse_key("ctrl+win+f5"),
            Some((MOD_CONTROL | MOD_WIN, 0x74))
        );
        assert_eq!(parse_key("alt+1"), Some((MOD_ALT, '1' as u32)));
    }

    #[test]
    fn parse_key_rejects_invalid() {
        assert_eq!(parse_key("h"), None); // modifier なしは奪わない
        assert_eq!(parse_key("alt+unknownkey"), None);
        assert_eq!(parse_key("foo+h"), None);
        assert_eq!(parse_key("alt+f99"), None);
    }

    #[test]
    fn parse_key_muhenkan() {
        assert_eq!(parse_key("muhenkan+l"), Some((MOD_MUHENKAN, 'L' as u32)));
        assert_eq!(
            parse_key("nonconvert+shift+l"),
            Some((MOD_MUHENKAN | MOD_SHIFT, 'L' as u32))
        );
    }

    #[test]
    fn expand_basic_and_multi_modifier() {
        let vars = map(&[("mod", "alt+shift"), ("m2", "ctrl")]);
        assert_eq!(expand_vars("$mod+l", &vars).unwrap(), "alt+shift+l");
        assert_eq!(
            expand_vars("$m2+$mod+l", &vars).unwrap(),
            "ctrl+alt+shift+l"
        );
        assert_eq!(expand_vars("alt+l", &vars).unwrap(), "alt+l");
        // 名前の区切りは [A-Za-z0-9_] 以外
        assert_eq!(expand_vars("$m2+l", &vars).unwrap(), "ctrl+l");
        assert_eq!(
            parse_key(&expand_vars("$mod+l", &vars).unwrap()).unwrap().0,
            MOD_ALT | MOD_SHIFT
        );
    }

    #[test]
    fn expand_unknown_var() {
        let vars = map(&[("mod", "alt")]);
        assert_eq!(expand_vars("$nope+l", &vars), Err("nope".to_string()));
    }

    #[test]
    fn expand_does_not_recurse() {
        let vars = map(&[("a", "$b"), ("b", "alt")]);
        assert_eq!(expand_vars("$a+l", &vars).unwrap(), "$b+l");
    }

    const DEFAULTS: &[(&str, &str)] = &[
        ("$mod+h", "focus left"),
        ("$mod+shift+h", "move-column left"),
        ("$mod+space", "focus-floating"),
    ];

    #[test]
    fn mod_defaults_to_alt() {
        let (b, w) = resolve_binds(DEFAULTS, &BTreeMap::new(), &BTreeMap::new());
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(find(&b, MOD_ALT, 'H' as u32), Some("focus left"));
        assert_eq!(b.len(), 3);
    }

    #[test]
    fn mod_moves_defaults() {
        let vars = map(&[("mod", "muhenkan")]);
        let (b, w) = resolve_binds(DEFAULTS, &BTreeMap::new(), &vars);
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(find(&b, MOD_MUHENKAN, 'H' as u32), Some("focus left"));
        assert_eq!(
            find(&b, MOD_MUHENKAN | MOD_SHIFT, 'H' as u32),
            Some("move-column left")
        );
        assert_eq!(find(&b, MOD_ALT, 'H' as u32), None);
    }

    #[test]
    fn user_overrides_by_parsed_key() {
        // 文字列は違うが同じ (mods, vk) になるキーが既定を上書きする
        let user = map(&[("shift+alt+h", "focus right")]);
        let (b, _) = resolve_binds(DEFAULTS, &user, &BTreeMap::new());
        assert_eq!(
            find(&b, MOD_ALT | MOD_SHIFT, 'H' as u32),
            Some("focus right")
        );
        assert_eq!(b.len(), 3); // 重複せず置き換わる
    }

    #[test]
    fn none_disables_default() {
        let user = map(&[("$mod+space", "none"), ("Alt+H", "")]);
        let (b, w) = resolve_binds(DEFAULTS, &user, &BTreeMap::new());
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(find(&b, MOD_ALT, 0x20), None);
        assert_eq!(find(&b, MOD_ALT, 'H' as u32), None);
        assert_eq!(b.len(), 1);
    }

    #[test]
    fn user_vars_and_unknown_var_warns() {
        let vars = map(&[("mod", "muhenkan"), ("hyper", "ctrl+alt")]);
        let user = map(&[("$hyper+x", "close"), ("$bogus+y", "close")]);
        let (b, w) = resolve_binds(DEFAULTS, &user, &vars);
        assert_eq!(find(&b, MOD_CONTROL | MOD_ALT, 'X' as u32), Some("close"));
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("bogus"));
    }

    #[test]
    fn duplicate_user_combo_is_deterministic_and_warns() {
        // ソート順で後ろの "alt+x" ("alt+x" > "$mod+x" は '$' < 'a' なので後ろ) が勝つ
        let user = map(&[("$mod+x", "close"), ("alt+x", "quit")]);
        let (b, w) = resolve_binds(&[], &user, &BTreeMap::new());
        assert_eq!(find(&b, MOD_ALT, 'X' as u32), Some("quit"));
        assert_eq!(w.len(), 1);
    }

    #[test]
    fn invalid_user_key_warns() {
        let user = map(&[("h", "close")]);
        let (b, w) = resolve_binds(DEFAULTS, &user, &BTreeMap::new());
        assert_eq!(b.len(), 3);
        assert_eq!(w.len(), 1);
    }
}
