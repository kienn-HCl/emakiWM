use embed_manifest::manifest::DpiAwareness;
use embed_manifest::{embed_manifest, new_manifest};

fn main() {
    // Per-Monitor v2 DPI awareness (FR-5.3)。
    // 宣言しないと GetWindowRect 等の座標が DPI 仮想化されて狂う。
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        embed_manifest(new_manifest("emakiWM.emakiwm").dpi_awareness(DpiAwareness::PerMonitorV2))
            .expect("failed to embed Windows manifest");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
