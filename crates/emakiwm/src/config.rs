//! TOML 設定 (FR-7.1, FR-2.2, FR-6.2/6.3)。
//!
//! 場所: `%USERPROFILE%\.config\emakiwm\config.toml`。
//! ファイルがない・壊れている場合はデフォルトへフォールバックする (起動を止めない)。
//! キーバインドはデフォルト表へのマージ (値を "none" にすると無効化)。
//! ws_port と startup 以外は reload で反映される (キーバインド・ジェスチャを含む)。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use emakiwm_core::filter::{Rule, RuleAction};
use emakiwm_core::keybind::resolve_binds;
use serde::Deserialize;

use crate::events::{parse_command, Hotkey};
use crate::touchpad::GestureBinds;

/// デフォルトキーバインド (§7)。`$mod` は [vars] の mod (未設定なら alt) に展開される。
const DEFAULT_BINDS: &[(&str, &str)] = &[
    ("$mod+h", "focus left"),
    ("$mod+l", "focus right"),
    ("$mod+j", "focus down"),
    ("$mod+k", "focus up"),
    ("$mod+shift+h", "move-column left"),
    ("$mod+shift+l", "move-column right"),
    ("$mod+shift+j", "move-window down"),
    ("$mod+shift+k", "move-window up"),
    ("$mod+u", "workspace down"),
    ("$mod+i", "workspace up"),
    ("$mod+shift+comma", "consume"),
    ("$mod+shift+period", "expel"),
    ("$mod+r", "cycle-width"),
    ("$mod+f", "maximize"),
    ("$mod+shift+f", "fullscreen"),
    ("$mod+comma", "scroll left"),
    ("$mod+period", "scroll right"),
    ("$mod+shift+space", "toggle-floating"),
    ("$mod+space", "focus-floating"),
    ("$mod+o", "toggle-overview"),
    ("$mod+shift+q", "close"),
    ("$mod+shift+e", "quit"),
];

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct ConfigFile {
    startup: Vec<String>,
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
    /// Alt+ホイールでフォーカス左右移動。デフォルト false
    mouse_scroll_focus: Option<bool>,
    /// Overview の縮小率 (FR-4.11)。デフォルト 0.5、範囲 0.1〜0.9
    overview_zoom: Option<f32>,
    rules: Vec<RuleEntry>,
    /// キー中の `$name` を置換する変数。`mod` は未設定なら "alt"
    vars: BTreeMap<String, String>,
    keybinds: BTreeMap<String, String>,
    /// 3 本指スワイプの割当 (FR-6.5): "swipe3-up" 等 → コマンド構文。
    /// 空なら Raw Input を購読しない
    gestures: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct RuleEntry {
    exe: Option<String>,
    class: Option<String>,
    title: Option<String>,
    action: String,
}

pub struct Config {
    /// WM 起動時に自動実行するコマンド一覧。run() 初回のみ、reload では走らない
    pub startup: Vec<String>,
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
    /// Alt+ホイールでフォーカス左右移動。reload で反映
    pub mouse_scroll_focus: bool,
    /// Overview の縮小率 (0.1〜0.9)。reload で反映
    pub overview_zoom: f32,
    pub rules: Vec<Rule>,
    pub hotkeys: Vec<Hotkey>,
    /// 3 本指スワイプの割当 (FR-6.5)。全方向 None なら機能無効
    pub gestures: GestureBinds,
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

    // 変数展開 → (mods, vk) 単位でデフォルトとマージ (keybind.rs)
    let (binds, warnings) = resolve_binds(DEFAULT_BINDS, &file.keybinds, &file.vars);
    for w in warnings {
        tracing::warn!("{w}");
    }
    let hotkeys = binds
        .into_iter()
        .filter_map(|(mods, vk, cmd)| {
            let Some(event) = parse_command(&cmd) else {
                tracing::warn!(
                    "コマンド \"{cmd}\" を解釈できません (mods={mods:#x}, vk={vk:#04x})"
                );
                return None;
            };
            Some(Hotkey { mods, vk, event })
        })
        .collect();

    let mut gestures = GestureBinds::default();
    for (k, v) in &file.gestures {
        if v.is_empty() || v == "none" {
            continue;
        }
        let Some(event) = parse_command(v) else {
            tracing::warn!("コマンド \"{v}\" を解釈できません (ジェスチャ {k})");
            continue;
        };
        match k.as_str() {
            "swipe3-up" => gestures.up = Some(event),
            "swipe3-down" => gestures.down = Some(event),
            "swipe3-left" => gestures.left = Some(event),
            "swipe3-right" => gestures.right = Some(event),
            other => tracing::warn!("不明なジェスチャ \"{other}\" (swipe3-up/down/left/right)"),
        }
    }

    let cloak = match file.hide.as_deref() {
        None | Some("offscreen") => false,
        Some("cloak") => true,
        Some(other) => {
            tracing::warn!("不明な hide \"{other}\" (offscreen/cloak)、offscreen を使用");
            false
        }
    };

    Config {
        startup: file.startup,
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
        mouse_scroll_focus: file.mouse_scroll_focus.unwrap_or(false),
        overview_zoom: file.overview_zoom.unwrap_or(0.5).clamp(0.1, 0.9),
        rules,
        hotkeys,
        gestures,
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

#[cfg(test)]
mod tests {
    use super::*;

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
        let (b, w) = resolve_binds(DEFAULT_BINDS, &BTreeMap::new(), &BTreeMap::new());
        assert!(w.is_empty(), "{w:?}");
        // 展開後に衝突して潰れているデフォルトがない
        assert_eq!(b.len(), DEFAULT_BINDS.len());
        for (key, cmd) in DEFAULT_BINDS {
            assert!(b.iter().any(|(_, _, c)| c == cmd), "key {key} must resolve");
            assert!(
                crate::events::parse_command(cmd).is_some(),
                "command {cmd} must parse"
            );
        }
    }
}
