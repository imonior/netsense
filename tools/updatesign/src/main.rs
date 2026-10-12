//! NetSense 发布签名工具（Ed25519）。
//!
//! 与 wireguide-plus/tools/updatesign 同格式：`.sig` 为**原始 64 字节**，无编码。
//! App 侧 `update.rs` 用同一套 `ed25519-dalek` 验签，确保格式一致。
//!
//! 子命令：
//!   gen                                         打印 priv/pub 的 hex（priv 务必离线、单独保存）
//!   pub  -k <privhex>                           由私钥派生公钥 hex
//!   sign -i SHA256SUMS -o SHA256SUMS.sig -k <privhex>
//!   verify -i SHA256SUMS -s SHA256SUMS.sig -p <pubhex>
//!   issue-trust -r <rootpriv> -k <signpub> [-s <seq>] -o <trust.json>
//!   verify-trust -i <trust.json> -p <rootpubhex>
//!
//! 密钥分层（精简版 B+）：
//!   - root 密钥对：离线生成、永不进 CI/git；root 私钥仅用于签发「信任清单」。
//!   - signing 密钥对：CI 用（UPDATE_SIGNING_KEY），仅用于签 SHA256SUMS。
//! 信任清单 UPDATE-TRUST.json 由 root 私钥签名，含当前有效 signing_key 与 seq（防清单
//! 回滚，也是「轮换能生效」的前提）。App 内嵌 root 公钥验证该清单，再从中取出
//! signing_key 验证 SHA256SUMS —— 因此 signing 私钥泄露时用 root 签一份指向新密钥的
//! 新清单即可轮换，无需重发二进制。
//!
//! CI 用法见 .github/workflows/build.yml：各平台腿把 repo 变量 `NETSENSE_ROOT_PUBKEY`（root
//! **公钥**）注入二进制，`checksums` 作业用 secret `UPDATE_SIGNING_KEY` 跑 `sign` 产出
//! SHA256SUMS.sig；信任清单由维护者在离线机器上 `issue-trust` 签发后提交到仓库 `update/`
//! 路径（CI 不持有 root 私钥，无法代签）。

use std::io::Read;
use std::process::exit;

// `Signer` 需要 import 才有 `sk.sign(..)`；验签走 `VerifyingKey::verify_strict`
// （固有方法，不需要 `Verifier` trait）。
use ed25519_dalek::{SecretKey, Signer, SigningKey};
use hex::FromHex;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        usage();
        exit(2);
    }
    match args[1].as_str() {
        "gen" => cmd_gen(),
        "pub" => cmd_pub(&args),
        "sign" => cmd_sign(&args),
        "verify" => cmd_verify(&args),
        "issue-trust" => cmd_issue_trust(&args),
        "verify-trust" => cmd_verify_trust(&args),
        _ => {
            usage();
            exit(2);
        }
    }
}

fn usage() {
    eprintln!("usage:");
    eprintln!("  updatesign gen");
    eprintln!("  updatesign pub -k <privhex>");
    eprintln!("  updatesign sign -i <sums> -o <sig> -k <privhex>");
    eprintln!("  updatesign verify -i <sums> -s <sig> -p <pubhex>");
    eprintln!("  updatesign issue-trust -r <rootpriv> -k <signpub> [-s <seq>] -o <trust.json>");
    eprintln!("  updatesign verify-trust -i <trust.json> -p <rootpubhex>");
}

fn fail(msg: &str) -> ! {
    eprintln!("error: {msg}");
    exit(1);
}

/// 取 `-flag <value>` 形式的参数值。
fn parse_opt<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    let mut i = 2;
    while i < args.len() {
        if args[i] == flag {
            return args.get(i + 1).map(|s| s.as_str());
        }
        i += 1;
    }
    None
}

fn secret_from_hex(hexstr: &str) -> [u8; 32] {
    let bytes = Vec::from_hex(hexstr.trim()).unwrap_or_else(|e| fail(&format!("invalid private key hex: {e}")));
    if bytes.len() != 32 {
        fail(&format!("private key must be 32 bytes, got {}", bytes.len()));
    }
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    arr
}

fn pub_from_hex(hexstr: &str) -> [u8; 32] {
    let bytes = Vec::from_hex(hexstr.trim()).unwrap_or_else(|e| fail(&format!("invalid public key hex: {e}")));
    if bytes.len() != 32 {
        fail(&format!("public key must be 32 bytes, got {}", bytes.len()));
    }
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    arr
}

fn read_file(path: &str) -> Vec<u8> {
    let mut f = std::fs::File::open(path).unwrap_or_else(|e| fail(&format!("cannot open {path}: {e}")));
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).unwrap_or_else(|e| fail(&format!("cannot read {path}: {e}")));
    buf
}

fn utc_now() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn cmd_gen() {
    use rand::rngs::OsRng;
    let mut csprng = OsRng;
    let sk = SigningKey::generate(&mut csprng);
    println!("priv={}", hex::encode(sk.to_bytes()));
    println!("pub={}", hex::encode(sk.verifying_key().to_bytes()));
}

fn cmd_pub(args: &[String]) {
    let k = parse_opt(args, "-k").unwrap_or_else(|| fail("pub: missing -k <privhex>"));
    let sk = secret_from_hex(k);
    let sk = SigningKey::from(SecretKey::from(sk));
    println!("{}", hex::encode(sk.verifying_key().to_bytes()));
}

fn cmd_sign(args: &[String]) {
    let inp = parse_opt(args, "-i").unwrap_or_else(|| fail("sign: missing -i <sums>"));
    let out = parse_opt(args, "-o").unwrap_or_else(|| fail("sign: missing -o <sig>"));
    let k = parse_opt(args, "-k").unwrap_or_else(|| fail("sign: missing -k <privhex>"));
    let sk = secret_from_hex(k);
    let sk = SigningKey::from(SecretKey::from(sk));
    let msg = read_file(inp);
    let sig: ed25519_dalek::Signature = sk.sign(&msg);
    std::fs::write(out, sig.to_bytes()).unwrap_or_else(|e| fail(&format!("cannot write {out}: {e}")));
    eprintln!("signed {inp} -> {out}");
}

fn cmd_verify(args: &[String]) {
    let inp = parse_opt(args, "-i").unwrap_or_else(|| fail("verify: missing -i <sums>"));
    let sigp = parse_opt(args, "-s").unwrap_or_else(|| fail("verify: missing -s <sig>"));
    let pp = parse_opt(args, "-p").unwrap_or_else(|| fail("verify: missing -p <pubhex>"));
    let pk = pub_from_hex(pp);
    let vk = ed25519_dalek::VerifyingKey::from_bytes(&pk).unwrap_or_else(|_| fail("verify: invalid public key"));
    let msg = read_file(inp);
    let sigbytes = read_file(sigp);
    if sigbytes.len() != 64 {
        fail(&format!("signature must be 64 bytes, got {}", sigbytes.len()));
    }
    let mut sig_arr = [0u8; 64];
    sig_arr.copy_from_slice(&sigbytes);
    let sig = ed25519_dalek::Signature::from_bytes(&sig_arr);
    match vk.verify_strict(&msg, &sig) {
        Ok(()) => {
            eprintln!("OK: signature verifies");
            exit(0);
        }
        Err(e) => {
            eprintln!("FAIL: {e}");
            exit(1);
        }
    }
}

/// 信任清单的**被签名主体**。
///
/// ⚠️ 字段顺序即规范：serde 按声明顺序序列化，App 侧（`src-tauri/src/update.rs`）用同序的
/// `TrustBody` 重放同一份字节再验签。这里**绝不能**改字段顺序或增删字段，否则两端算出的
/// 字节不一致，验证恒定失败。`signature` 不在本结构里——它是对其余字段的签名本身。
///
/// 同理也**不能**改成 `serde_json::Map`：Map 的键序由其内部实现（`BTreeMap` / `IndexMap`）
/// 决定，跨版本不保证，签出来与验出来的字节可能对不上。
#[derive(serde::Serialize)]
struct TrustBody<'a> {
    schema: u64,
    seq: u64,
    signing_key: &'a str,
    issued_at: &'a str,
}

/// 用 root 私钥签发「信任清单」UPDATE-TRUST.json。
///
/// 清单由 root 私钥签名，App 用内嵌的 root 公钥验证该签名，再从中取出 `signing_key` 验证
/// `SHA256SUMS.sig`。因此 signing 私钥泄露时，只要 root 私钥还在，就能签一份指向新
/// `signing_key`、**seq 更大**的新清单完成轮换 —— **无需重新分发二进制**，用户下次更新
/// 即自动切换信任。
///
/// ⚠️ `seq` 必须比当前清单大，轮换才生效：App 只接受单调不减的 seq，否则攻击者把轮换前
/// 那份同样合法签过的旧清单反复投喂就能让旧密钥复活。
///
/// -r <rootpriv>  root 私钥 hex（离线保管，永不进 CI）
/// -k <signpub>   当前有效的 signing 公钥 hex
/// -s <seq>       单调序号，防「清单回滚」；轮换时必须 +1（默认 1）
/// -o <path>      输出路径
fn cmd_issue_trust(args: &[String]) {
    let root_priv = parse_opt(args, "-r").unwrap_or_else(|| fail("issue-trust: missing -r <rootprivhex>"));
    let sign_pub = parse_opt(args, "-k").unwrap_or_else(|| fail("issue-trust: missing -k <signpubhex>"));
    let out = parse_opt(args, "-o").unwrap_or_else(|| fail("issue-trust: missing -o <trust.json>"));
    let seq: u64 = parse_opt(args, "-s")
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);

    // 校验公钥 hex 合法（32 字节），避免把垃圾写进清单。
    let _ = pub_from_hex(sign_pub);
    let root_sk = secret_from_hex(root_priv);
    let root_sk = SigningKey::from(SecretKey::from(root_sk));

    let issued_at = utc_now();
    let body = TrustBody {
        schema: 1,
        seq,
        signing_key: sign_pub,
        issued_at: &issued_at,
    };
    // 被签名的字节 = TrustBody 的紧凑序列化，App 端按同序重放。
    let body_bytes = serde_json::to_vec(&body).unwrap_or_else(|e| fail(&format!("serialize trust body: {e}")));
    let sig: ed25519_dalek::Signature = root_sk.sign(&body_bytes);

    // 输出把 signature 摊平到顶层，方便人眼阅读；App 反序列化时会先剥掉它再重放主体。
    let mut out_map = serde_json::Map::new();
    out_map.insert("schema".into(), serde_json::json!(body.schema));
    out_map.insert("seq".into(), serde_json::json!(body.seq));
    out_map.insert("signing_key".into(), serde_json::json!(body.signing_key));
    out_map.insert("issued_at".into(), serde_json::json!(body.issued_at));
    out_map.insert("signature".into(), serde_json::json!(hex::encode(sig.to_bytes())));
    let out_val = serde_json::Value::Object(out_map);
    let out_str =
        serde_json::to_string_pretty(&out_val).unwrap_or_else(|e| fail(&format!("serialize trust: {e}")));
    std::fs::write(out, &out_str).unwrap_or_else(|e| fail(&format!("cannot write {out}: {e}")));
    eprintln!("issued trust manifest: {out} (seq={seq} signing_key={sign_pub}, root-signed)");
}

/// 本地校验一份信任清单：确认 root 签名有效，且字段结构符合预期。
/// 提交前自检用 —— 免得推上去的清单在客户端恒定验签失败。
fn cmd_verify_trust(args: &[String]) {
    let pp = parse_opt(args, "-p").unwrap_or_else(|| fail("verify-trust: missing -p <rootpubhex>"));
    let path = parse_opt(args, "-i").unwrap_or_else(|| fail("verify-trust: missing -i <trust.json>"));
    let raw = read_file(path);
    let manifest: serde_json::Value =
        serde_json::from_slice(&raw).unwrap_or_else(|e| fail(&format!("{path} is not valid JSON: {e}")));
    let obj = manifest
        .as_object()
        .unwrap_or_else(|| fail(&format!("{path}: expected a JSON object")));
    let sig_hex = obj
        .get("signature")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| fail(&format!("{path}: missing \"signature\"")));
    let str_field = |k: &str| obj.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let seq = obj.get("seq").and_then(|v| v.as_u64()).unwrap_or(0);
    let schema = obj.get("schema").and_then(|v| v.as_u64()).unwrap_or(0);

    // 剥掉 signature 后按声明顺序重放主体。
    let signing_key = str_field("signing_key");
    let issued_at = str_field("issued_at");
    let body = TrustBody {
        schema,
        seq,
        signing_key: &signing_key,
        issued_at: &issued_at,
    };
    let body_bytes = serde_json::to_vec(&body).unwrap_or_else(|e| fail(&format!("serialize trust body: {e}")));
    let pk = pub_from_hex(pp);
    let vk = ed25519_dalek::VerifyingKey::from_bytes(&pk).unwrap_or_else(|_| fail("verify-trust: invalid public key"));
    let sigbytes = Vec::from_hex(sig_hex.trim())
        .unwrap_or_else(|e| fail(&format!("signature is not hex: {e}")));
    if sigbytes.len() != 64 {
        fail(&format!("signature must be 64 bytes, got {}", sigbytes.len()));
    }
    let mut sig_arr = [0u8; 64];
    sig_arr.copy_from_slice(&sigbytes);
    let sig = ed25519_dalek::Signature::from_bytes(&sig_arr);
    match vk.verify_strict(&body_bytes, &sig) {
        Ok(()) => {
            eprintln!("OK: {path} verifies against root pubkey (schema={schema} seq={seq} signing_key={signing_key})");
            exit(0);
        }
        Err(e) => {
            eprintln!("FAIL: {path} signature mismatch: {e}");
            exit(1);
        }
    }
}
