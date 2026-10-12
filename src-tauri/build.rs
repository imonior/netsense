fn main() {
    // 发布签名公钥（CI 注入）透传给编译期常量；空串则等同于「未签名 / dev 构建」。
    println!(
        "cargo:rustc-env=NETSENSE_UPDATE_PUBKEY={}",
        std::env::var("NETSENSE_UPDATE_PUBKEY").unwrap_or_default()
    );
    // B+ 全套：root 公钥（离线根密钥的公钥）作为唯一编译期信任锚，空串表示 dev / 本地构建。
    // 信任清单 UPDATE-TRUST.json 由 root 私钥离线签发，App 用此公钥验证清单，
    // 再从中取出有效 signing_key 验证 SHA256SUMS。root 私钥永不进 CI / git。
    println!(
        "cargo:rustc-env=NETSENSE_ROOT_PUBKEY={}",
        std::env::var("NETSENSE_ROOT_PUBKEY").unwrap_or_default()
    );
    // 仅当 CI 显式置位 NETSENSE_RELEASE_SIGN 时，本构建才「要求签名验过才安装」。
    if std::env::var("NETSENSE_RELEASE_SIGN").map(|v| !v.is_empty()).unwrap_or(false) {
        println!("cargo:rustc-env=NETSENSE_RELEASE_SIGN=1");
    }
    println!("cargo:rerun-if-env-changed=NETSENSE_UPDATE_PUBKEY");
    println!("cargo:rerun-if-env-changed=NETSENSE_ROOT_PUBKEY");
    println!("cargo:rerun-if-env-changed=NETSENSE_RELEASE_SIGN");
    tauri_build::build()
}
