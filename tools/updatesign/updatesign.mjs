#!/usr/bin/env node
/**
 * NetSense 发布签名工具链 —— **零依赖的 Node 版**。
 *
 * 存在的理由：`cargo run --manifest-path tools/updatesign/...` 是最直接的路径，但它要求本机
 * 装了 Rust 工具链。而 root 私钥的诞生环境往往恰恰是那台**离线、精简、不想装编译器的机器**
 * —— 所以这里用 Node 内置的 `crypto`（OpenSSL 之上的 Ed25519）把同一套契约重做一遍，
 * 不装任何 npm 包（Node ≥ 12 自带 Ed25519 支持）。
 *
 * 与 Rust 版 `tools/updatesign` **字节兼容**，两边的输出可互换：
 *   私钥   32 字节 seed，小写 hex（64 字符）
 *   公钥   32 字节压缩点，小写 hex（64 字符）
 *   签名   64 原始字节，无编码
 *
 * ⚠️ 信任清单的**被签名字节**必须与 App 侧 `src-tauri/src/update.rs` 的 `TrustBody`
 * 逐字节相同 —— 即「除 signature 外全部字段，按固定顺序紧凑序列化」。这里显式拼字符串而
 * 不是用 JSON.stringify：对象的键序在 JS 里虽是插入序、但依赖引擎实现，显式拼接是唯一
 * 不会漂移的写法。字段顺序：schema, seq, signing_key, issued_at。
 *
 * 子命令：
 *   gen                                   打印 priv/pub 的 hex
 *   pub  -k <privhex>                     由私钥推导公钥 hex
 *   sign -i <sums> -o <sig> -k <privhex>  对文件签名（原始 64 字节）
 *   verify -i <sums> -s <sig> -p <pubhex>
 *   issue-trust -r <rootpriv> -k <signpub> [-s seq] -o <file>
 *   verify-trust -i <file> -p <rootpubhex>
 *
 * 用法：node tools/updatesign/updatesign.mjs <子命令> [参数]
 */

import crypto from 'node:crypto';
import fs from 'node:fs';

// RFC 8410：Ed25519 的 PKCS#8 与 SPKI DER 结构是定长的，raw 密钥拼上固定前缀即可还原。
// 这两个前缀是标准的一部分，不是本项目的发明，所以任何实现都能互通。
const PKCS8_PREFIX = Buffer.from('302e020100300506032b657004220420', 'hex');
const SPKI_PREFIX = Buffer.from('302a300506032b6570032100', 'hex');

const HEX32 = /^[0-9a-fA-F]{64}$/;

function fail(msg) {
  process.stderr.write(`error: ${msg}\n`);
  process.exit(1);
}

function usage() {
  process.stderr.write(`usage:
  updatesign gen
  updatesign pub -k <privhex>
  updatesign sign -i <sums> -o <sig> -k <privhex>
  updatesign verify -i <sums> -s <sig> -p <pubhex>
  updatesign issue-trust -r <rootpriv> -k <signpub> [-s <seq>] -o <file>
  updatesign verify-trust -i <file> -p <rootpubhex>
`);
}

/**
 * 解析 `-flag <value>` 形式的参数。
 *
 * 从整个 argv 起扫（不跳过任何位置）：本文件传进来的是 `process.argv.slice(2)`，其
 * `args[0]` 是子命令名、`args[1]` 起才是参数。若照搬 Rust 版那种「从 index 2 起扫」的写法，
 * 第一个参数会被整条跳过 —— 表现为 `pub: missing -k` 这类莫名其妙的报错。
 */
function parseOpt(args, flag) {
  for (let i = 0; i < args.length; i++) {
    if (args[i] === flag) return args[i + 1];
  }
  return undefined;
}

/** hex 串 → 32 字节 Buffer。与 Rust 的 `secret_from_hex` / `pub_from_hex` 同样严格。 */
function hexTo32(s, what) {
  const t = String(s).trim();
  if (t.length !== 64) fail(`${what} must be 64 hex chars (32 bytes), got ${t.length}`);
  if (!HEX32.test(t)) fail(`${what} is not valid hex`);
  return Buffer.from(t, 'hex');
}

function privKeyFromRaw(priv32) {
  return crypto.createPrivateKey({
    key: Buffer.concat([PKCS8_PREFIX, priv32]),
    format: 'der',
    type: 'pkcs8',
  });
}

function pubKeyFromRaw(pub32) {
  return crypto.createPublicKey({
    key: Buffer.concat([SPKI_PREFIX, pub32]),
    format: 'der',
    type: 'spki',
  });
}

/** raw 32 字节私钥 → raw 32 字节公钥（走一次公钥派生，与 Rust 的 verifying_key() 等价）。 */
function derivePub(privHex) {
  const key = privKeyFromRaw(hexTo32(privHex, 'private key'));
  return crypto.createPublicKey(key).export({ type: 'spki', format: 'der' }).subarray(-32);
}

function cmdGen() {
  // Node 的 generateKeyPairSync 走 OpenSSL CSPRNG —— 与 Rust 的 OsRng 同源（都是操作系统熵池）。
  const { privateKey, publicKey } = crypto.generateKeyPairSync('ed25519');
  const priv = privateKey.export({ type: 'pkcs8', format: 'der' }).subarray(-32);
  const pub = publicKey.export({ type: 'spki', format: 'der' }).subarray(-32);
  process.stdout.write(`priv=${priv.toString('hex')}\npub=${pub.toString('hex')}\n`);
}

function cmdPub(args) {
  const k = parseOpt(args, '-k') ?? fail('pub: missing -k <privhex>');
  process.stdout.write(`${derivePub(k).toString('hex')}\n`);
}

function cmdSign(args) {
  const inp = parseOpt(args, '-i') ?? fail('sign: missing -i <sums>');
  const out = parseOpt(args, '-o') ?? fail('sign: missing -o <sig>');
  const k = parseOpt(args, '-k') ?? fail('sign: missing -k <privhex>');
  const key = privKeyFromRaw(hexTo32(k, 'private key'));
  const sig = crypto.sign(null, fs.readFileSync(inp), key); // ed25519：digest 参数必须是 null
  if (sig.length !== 64) fail(`unexpected signature length ${sig.length}`);
  fs.writeFileSync(out, sig);
  process.stderr.write(`signed ${inp} -> ${out}\n`);
}

function cmdVerify(args) {
  const inp = parseOpt(args, '-i') ?? fail('verify: missing -i <sums>');
  const sigp = parseOpt(args, '-s') ?? fail('verify: missing -s <sig>');
  const p = parseOpt(args, '-p') ?? fail('verify: missing -p <pubhex>');
  const key = pubKeyFromRaw(hexTo32(p, 'public key'));
  const sig = fs.readFileSync(sigp);
  if (sig.length !== 64) fail(`signature must be 64 bytes, got ${sig.length}`);
  const ok = crypto.verify(null, fs.readFileSync(inp), key, sig);
  if (!ok) {
    process.stderr.write('FAIL: signature does not verify\n');
    process.exit(1);
  }
  process.stderr.write('OK: signature verifies\n');
}

/**
 * 信任清单的**被签名字节**。
 *
 * ⚠️ 顺序即规范：必须与 App 侧 `TrustBody` 的字段声明顺序逐字对应，且不含 `signature`
 * （签名是对其余字段的签名本身）。改动这里必须同步改 update.rs，否则客户端恒定验签失败。
 *
 * 故意不�� JSON.stringify：显式拼接是唯一不受 JS 引擎键序实现影响的写法。
 */
function canonicalBody({ schema, seq, signingKey, issuedAt }) {
  return (
    `{"schema":${schema},"seq":${seq},` +
    `"signing_key":${JSON.stringify(signingKey)},` +
    `"issued_at":${JSON.stringify(issuedAt)}}`
  );
}

/**
 * RFC 3339 UTC 时间戳，毫秒精度，与 Rust 侧 `chrono::Utc::now().to_rfc3339()` 同形
 * （`2026-10-11T06:36:48.296Z`）。
 *
 * `toISOString()` 已经返回 `…Z`，**不要**再 replace 一次 —— 那会拼出 `…296.000Z`
 * 这种双小数点的畸形时间戳。它只影响可读性、不影响验签（清单自洽），但清单是要入库、
 * 要被人审的，格式不能将就。
 */
function utcNow() {
  return new Date().toISOString();
}

function cmdIssueTrust(args) {
  const rootPriv = parseOpt(args, '-r') ?? fail('issue-trust: missing -r <rootprivhex>');
  const signPub = parseOpt(args, '-k') ?? fail('issue-trust: missing -k <signpubhex>');
  const out = parseOpt(args, '-o') ?? fail('issue-trust: missing -o <file>');
  const seq = parseOpt(args, '-s') ?? '1';
  if (!/^\d+$/.test(seq)) fail(`-s must be a non-negative integer, got ${seq}`);

  // 先校验公钥是合法 32B hex，避免把垃圾写进清单。
  hexTo32(signPub, 'signing public key');
  const key = privKeyFromRaw(hexTo32(rootPriv, 'root private key'));

  const issuedAt = utcNow();
  const signingKey = signPub.trim().toLowerCase();
  const body = canonicalBody({
    schema: 1,
    seq: Number(seq),
    signingKey,
    issuedAt,
  });
  const sig = crypto.sign(null, Buffer.from(body, 'utf8'), key);
  const manifest = {
    schema: 1,
    seq: Number(seq),
    signing_key: signingKey,
    issued_at: issuedAt,
    signature: sig.toString('hex'),
  };
  fs.writeFileSync(out, `${JSON.stringify(manifest, null, 2)}\n`);
  process.stderr.write(
    `issued trust manifest: ${out} (seq=${seq} signing_key=${signingKey}, root-signed)\n`
  );
}

function cmdVerifyTrust(args) {
  const p = parseOpt(args, '-p') ?? fail('verify-trust: missing -p <rootpubhex>');
  const path = parseOpt(args, '-i') ?? fail('verify-trust: missing -i <file>');
  let manifest;
  try {
    manifest = JSON.parse(fs.readFileSync(path, 'utf8'));
  } catch (e) {
    fail(`${path} is not valid JSON: ${e.message}`);
  }
  if (typeof manifest.signature !== 'string') fail(`${path}: missing "signature"`);
  const key = pubKeyFromRaw(hexTo32(p, 'root public key'));
  // 用清单自己的字段重放被签名的字节 —— 与签发时拼的是同一个函数、同一顺序。
  const body = canonicalBody({
    schema: manifest.schema ?? 0,
    seq: manifest.seq ?? 0,
    signingKey: manifest.signing_key ?? '',
    issuedAt: manifest.issued_at ?? '',
  });
  let sig;
  try {
    sig = Buffer.from(manifest.signature, 'hex');
  } catch {
    fail('signature is not hex');
  }
  if (sig.length !== 64) fail(`signature must be 64 bytes, got ${sig.length}`);
  if (!crypto.verify(null, Buffer.from(body, 'utf8'), key, sig)) {
    process.stderr.write(`FAIL: ${path} signature mismatch\n`);
    process.exit(1);
  }
  process.stderr.write(
    `OK: ${path} verifies against root pubkey (schema=${manifest.schema} seq=${manifest.seq} signing_key=${manifest.signing_key})\n`
  );
}

const args = process.argv.slice(2);
if (args.length < 1) {
  usage();
  process.exit(2);
}
switch (args[0]) {
  case 'gen': cmdGen(); break;
  case 'pub': cmdPub(args); break;
  case 'sign': cmdSign(args); break;
  case 'verify': cmdVerify(args); break;
  case 'issue-trust': cmdIssueTrust(args); break;
  case 'verify-trust': cmdVerifyTrust(args); break;
  default: usage(); process.exit(2);
}
