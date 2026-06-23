//! TOML 設定 (FR-7.1, FR-2.2, FR-6.2/6.3)。
//!
//! 場所: `%USERPROFILE%\.config\emakiwm\config.toml`。
//! ファイルがない・壊れている場合はデフォルトへフォールバックする (起動を止めない)。
//! キーバインドはデフォルト表へのマージ (値を "none" にすると無効化)。
//! 注意: キーバインドの反映には再起動が必要。gap / anim_ms / rules / hide はホットリロード可。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use emakiwm_core::filter::{Rule, RuleAction};
use serde::Deserialize;

use crate::events::{parse_command, Hotkey};

// RegisterHotKey の modifier 値 (winuser.h)
const MOD_ALT: u32 = 0x1;
const MOD_CONTROL: u32 = 0x2;
const MOD_SHIFT: u32 = 0x4;
const MOD_WIN: u32 = 0x8;

/// デフォルトキーバインド (§7)。
const DEFAULT_BINDS: &[(&str, &str)] = &[
    ("alt+h", "focus left"),
    ("alt+l", "focus right"),
    ("alt+j", "focus down"),
    ("alt+k", "focus up"),
    ("alt+shift+h", "move-column left"),
    ("alt+shift+l", "move-column right"),
    ("alt+shift+j", "move-window down"),
    ("alt+shift+k", "move-window up"),
    ("alt+u", "workspace down"),
    ("alt+i", "workspace up"),
    ("alt+shift+comma", "consume"),
    ("alt+shift+period", "expel"),
    ("alt+r", "cycle-width"),
    ("alt+f", "maximize"),
    ("alt+shift+f", "fullscreen"),
    ("alt+comma", "scroll left"),
    ("alt+period", "scroll right"),
    ("alt+shift+q", "close"),
    ("alt+shift+e", "quit"),
];

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct ConfigFile {
    gap: Option<i32>,
    anim_ms: Option<u64>,
    /// Viewport 外の隠蔽方式 (FR-3.3): "offscreen" (デフォルト) | "cloak"
    hide: Option<String>,
    /// 新規 Column のデフォルト幅 (Viewport に対する比率)。デフォルト 0.5。
    /// 0.48 のようにすると 2 枚 + 隣の列の端が見える niri 風レイアウトになる
    default_width_ratio: Option<f32>,
    /// フォーカス中ウィンドウの枠色 (FR-3.7): "#rrggbb" | "default" | "none"
    border_focused: Option<String>,
    /// 非フォーカスウィンドウの枠色
    border_unfocused: Option<String>,
    /// フォーカス枠の太さ (px)。0 = OS 標準の細枠のみ (オーバーレイなし)
    border_thickness: Option<i32>,
    /// 非フォーカスウィンドウの不透明度 (FR-3.8)。1.0 または未指定で無効
    unfocused_opacity: Option<f32>,
    /// toggle-opacity でピンしたウィンドウの不透明度。デフォルト 1.0 (不透明維持)
    pinned_opacity: Option<f32>,
    /// 画面の余白 px (FR-5.5)。CSS 風に [上,右,下,左] / [上下,左右] / [全辺]。
    /// バー (Zebar 等) のぶんを空けるのに使う
    margin: Option<Vec<i32>>,
    /// WebSocket 状態配信のポート (FR-7.5)。未指定で無効
    ws_port: Option<u16>,
    rules: Vec<RuleEntry>,
    keybinds: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct RuleEntry {
    exe: Option<String>,
    class: Option<String>,
    title: Option<String>,
    action: String,
}

pub struct Config {
    pub gap: i32,
    /// 0 でアニメーション無効 (FR-4.8)
    pub anim: Duration,
    /// true なら Viewport 外を shell cloak で隠す (FR-3.3 cloak モード)
    pub cloak: bool,
    /// 新規 Column のデフォルト幅比率 (0.1〜1.0)
    pub default_ratio: f32,
    /// DWMWA_BORDER_COLOR に渡す生値 (COLORREF / DEFAULT / NONE)。
    /// 両方 None なら枠色には触らない (FR-3.7)
    pub border_focused: Option<u32>,
    pub border_unfocused: Option<u32>,
    /// 0 より大きければ太枠オーバーレイを描く (要 border_focused)
    pub border_thickness: i32,
    /// 非フォーカスウィンドウのアルファ値 (FR-3.8)。None = 無効
    pub unfocused_alpha: Option<u8>,
    /// opacity ピン中のウィンドウのアルファ値 (FR-3.8)。デフォルト 255
    pub pinned_alpha: u8,
    /// 画面の余白 (top, right, bottom, left) px (FR-5.5)
    pub margin: (i32, i32, i32, i32),
    /// WebSocket 状態配信のポート (FR-7.5)。None = 無効。反映は再起動
    pub ws_port: Option<u16>,
    pub rules: Vec<Rule>,
    pub hotkeys: Vec<Hotkey>,
}

pub fn path() -> PathBuf {
    let home = std::env::var("USERPROFILE").unwrap_or_else(|_| ".".into());
    PathBuf::from(home)
        .join(".config")
        .join("emakiwm")
        .join("config.toml")
}

pub fn load() -> Config {
    let file = match std::fs::read_to_string(path()) {
        Ok(s) => toml::from_str::<ConfigFile>(&s).unwrap_or_else(|e| {
            tracing::warn!("config.toml の解析に失敗、デフォルトを使用: {e}");
            ConfigFile::default()
        }),
        Err(_) => {
            tracing::info!("config.toml なし ({})、デフォルトを使用", path().display());
            ConfigFile::default()
        }
    };

    let rules = file
        .rules
        .iter()
        .filter_map(|r| {
            let action = match r.action.as_str() {
                "ignore" => RuleAction::Ignore,
                "manage" => RuleAction::Manage,
                "float" => RuleAction::Float,
                other => {
                    tracing::warn!("不明な rule action \"{other}\" (ignore/manage/float)");
                    return None;
                }
            };
            let compile = |p: &Option<String>| -> Result<Option<regex::Regex>, regex::Error> {
                p.as_deref().map(regex::Regex::new).transpose()
            };
            match (compile(&r.exe), compile(&r.class), compile(&r.title)) {
                (Ok(exe), Ok(class), Ok(title)) => Some(Rule {
                    exe,
                    class,
                    title,
                    action,
                }),
                _ => {
                    tracing::warn!("rule の正規表現が不正なためスキップ: {r:?}");
                    None
                }
            }
        })
        .collect();

    let mut binds: BTreeMap<String, String> = DEFAULT_BINDS
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    for (k, v) in file.keybinds {
        binds.insert(k.to_lowercase(), v);
    }
    let hotkeys = binds
        .iter()
        .filter(|(_, v)| !v.is_empty() && v.as_str() != "none")
        .filter_map(|(k, v)| {
            let Some((mods, vk)) = parse_key(k) else {
                tracing::warn!("キー \"{k}\" を解釈できません");
                return None;
            };
            let Some(event) = parse_command(v) else {
                tracing::warn!("コマンド \"{v}\" を解釈できません (キー {k})");
                return None;
            };
            Some(Hotkey { mods, vk, event })
        })
        .collect();

    let cloak = match file.hide.as_deref() {
        None | Some("offscreen") => false,
        Some("cloak") => true,
        Some(other) => {
            tracing::warn!("不明な hide \"{other}\" (offscreen/cloak)、offscreen を使用");
            false
        }
    };

    Config {
        gap: file.gap.unwrap_or(8).clamp(0, 200),
        anim: Duration::from_millis(file.anim_ms.unwrap_or(180).min(2000)),
        cloak,
        default_ratio: file.default_width_ratio.unwrap_or(0.5).clamp(0.1, 1.0),
        border_focused: file.border_focused.as_deref().and_then(parse_color),
        border_unfocused: file.border_unfocused.as_deref().and_then(parse_color),
        border_thickness: file.border_thickness.unwrap_or(0).clamp(0, 50),
        unfocused_alpha: file
            .unfocused_opacity
            .filter(|r| *r < 1.0)
            .map(|r| (r.clamp(0.2, 1.0) * 255.0) as u8),
        pinned_alpha: file
            .pinned_opacity
            .map_or(255, |r| (r.clamp(0.2, 1.0) * 255.0) as u8),
        margin: parse_margin(file.margin.as_deref()),
        ws_port: file.ws_port,
        rules,
        hotkeys,
    }
}

/// 画面余白の指定 → (上, 右, 下, 左)。CSS 風の省略形に対応 (FR-5.5)。
fn parse_margin(m: Option<&[i32]>) -> (i32, i32, i32, i32) {
    let c = |v: i32| v.clamp(0, 1000);
    match m {
        None | Some([]) => (0, 0, 0, 0),
        Some([a]) => (c(*a), c(*a), c(*a), c(*a)),
        Some([v, h]) => (c(*v), c(*h), c(*v), c(*h)),
        Some([t, r, b, l]) => (c(*t), c(*r), c(*b), c(*l)),
        Some(other) => {
            tracing::warn!("margin は 1/2/4 要素で指定 ({other:?})、無視します");
            (0, 0, 0, 0)
        }
    }
}

/// 枠色指定 → DWMWA_BORDER_COLOR の生値。
/// "#rrggbb" は COLORREF (0x00BBGGRR) へ変換。"default" は OS 既定色、
/// "none" は枠線を消す (DWMWA_COLOR_DEFAULT / DWMWA_COLOR_NONE)。
fn parse_color(s: &str) -> Option<u32> {
    match s {
        "default" => Some(0xFFFF_FFFF),
        "none" => Some(0xFFFF_FFFE),
        _ => {
            let parsed = s
                .strip_prefix('#')
                .filter(|h| h.len() == 6)
                .and_then(|h| u32::from_str_radix(h, 16).ok());
            let Some(rgb) = parsed else {
                tracing::warn!("枠色 \"{s}\" を解釈できません (#rrggbb / default / none)");
                return None;
            };
            Some(((rgb & 0xFF) << 16) | (rgb & 0xFF00) | (rgb >> 16))
        }
    }
}

/// "alt+shift+h" 形式 → (modifier ビット和, 仮想キーコード)。
fn parse_key(s: &str) -> Option<(u32, u32)> {
    let parts: Vec<&str> = s.split('+').map(str::trim).collect();
    let (key, mod_parts) = parts.split_last()?;
    let mut mods = 0u32;
    for m in mod_parts {
        mods |= match *m {
            "alt" => MOD_ALT,
            "ctrl" | "control" => MOD_CONTROL,
            "shift" => MOD_SHIFT,
            "win" | "super" => MOD_WIN,
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

#[cfg(test)]
mod tests {
    use super::*;

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

    /// config.example.toml が実装とずれていないかの検証。
    /// TOML として妥当で、コメント中のデフォルトバインド一覧が
    /// DEFAULT_BINDS と一致することを確かめる。
    #[test]
    fn example_config_is_valid() {
        let s = include_str!("../../../examples/config.example.toml");
        let parsed: Result<ConfigFile, _> = toml::from_str(s);
        assert!(parsed.is_ok(), "{:?}", parsed.err());
        for (key, cmd) in DEFAULT_BINDS {
            let line = format!("#\"{key}\" = \"{cmd}\"");
            assert!(
                s.contains(&line),
                "config.example.toml にデフォルトバインド {line} が載っていない"
            );
        }
    }

    #[test]
    fn default_binds_all_parse() {
        for (key, cmd) in DEFAULT_BINDS {
            assert!(parse_key(key).is_some(), "key {key} must parse");
            assert!(
                crate::events::parse_command(cmd).is_some(),
                "command {cmd} must parse"
            );
        }
    }
}
