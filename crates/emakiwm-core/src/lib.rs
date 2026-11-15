//! emakiwm の純粋ロジック層。
//!
//! このクレートは Win32 (windows crate) に依存してはならない。
//! HWND 等の OS ハンドルを持ち込まず、全ロジックを単体テスト可能に保つ
//! (REQUIREMENTS.md §10 テスト戦略)。

pub mod anim;
pub mod cmdline;
pub mod filter;
pub mod gesture;
pub mod keybind;
pub mod layout;
pub mod modkey;
pub mod origin;
pub mod overview;
pub mod state;
