fn main() {
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        embed_icon();
    }
    println!("cargo:rerun-if-changed=build.rs");
}

/// アイコン (RT_GROUP_ICON, ID 1) を埋め込む。マニフェストは embed-manifest 側が担当する。
/// .rc は OUT_DIR に生成し、.ico は絶対パスで参照する
/// (rc.exe / windres のカレントディレクトリに依存しないため)。
fn embed_icon() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let ico = std::path::Path::new(&manifest_dir).join("../../assets/icon/emakiwm.ico");
    let ico = ico
        .canonicalize()
        .expect("assets/icon/emakiwm.ico not found");
    // rc は文字列中のバックスラッシュをエスケープとして扱うため '/' に統一する。
    // canonicalize の \\?\ 接頭辞 (Windows) は rc が解釈できないので除去する。
    let ico_str = ico.to_string_lossy().replace('\\', "/");
    let ico_str = ico_str.trim_start_matches("//?/");
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR");
    let rc = std::path::Path::new(&out_dir).join("icon.rc");
    std::fs::write(&rc, format!("1 ICON \"{ico_str}\"\n")).expect("failed to write icon.rc");
    embed_resource::compile(&rc, embed_resource::NONE)
        .manifest_required()
        .expect("failed to embed icon resource");
    println!("cargo:rerun-if-changed={}", ico.display());
}
