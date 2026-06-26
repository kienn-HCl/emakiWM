use embed_manifest::manifest::ExecutionLevel;
use embed_manifest::{embed_manifest, new_manifest};

fn main() {
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        embed_manifest(
            new_manifest("emakiWM.emakiwm-setup")
                .requested_execution_level(ExecutionLevel::AsInvoker),
        )
        .expect("failed to embed Windows manifest");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
