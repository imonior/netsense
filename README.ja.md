# NetSense

> 現在のネットワーク識別情報（SSID / ゲートウェイ MAC / BSSID）に基づいてネットワークプロファイル（静的IP / DHCP / DNS / IPv6）を自動的に照合・適用するクロスプラットフォームのスタンドアロン デスクトップアプリ。ヘルスモニタリング（切断時にDHCPへフォールバック）とネットワーク起因の自動化（ルート設定 / アプリ起動 / スクリプト実行）を備える。

[hammerspoon-wifi-switcher](https://github.com/imonior/hammerspoon-wifi-switcher) から発展し、Hammerspoon に依存しない macOS / Windows / Linux 向けスタンドアロンアプリになった。

## 機能

- **ネットワーク識別照合**: SSID、ゲートウェイ MAC、AP BSSID はそれぞれ独立した条件として機能（単独または組み合わせ、AND 条件）。同名 SSID の複数シナリオやなりすましアクセスポイント（evil twin）も正しく判別。
- **SSIDごとのプロファイル**: 静的IP / DHCP / カスタムDNS / IPv6（automatic / manual / off）。
- **グローバルフォールバック**: `__DEFAULT__` は未設定の任意のネットワークに適用。
- **ヘルスモニタリング**: ICMP / HTTP / both プローブ。フォールバック有効時に連続失敗すると自動で DHCP へ戻り、接続を維持。
- **自動化**: ネットワーク起因で `route` / `launch` / `run` を発動（netsetman 型の付加アクション）、`on_apply` / `on_revert` でトリガ。スクリプトは許可リストで制約。
- **トレイ ポップアップパネル**: ステータスバー／トレイアイコンを左クリックでパネル表示（ステータス＋ワンクリック切替＋言語）。フォーカス消失で自動収縮。右クリックでネイティブメニュー。
- **3プラットフォームで同一コードベース**: プラットフォーム差はすべて PAL（`platform/{macos,windows,linux}.rs`）に集約。上位層は trait のみに依存。
- **権限昇格**: macOS は sudoers 許可リスト導入後はパスワード不要。Windows は管理者として一度起動すれば UAC なし。Linux は `sudo -n` 設定でプロンプトなし。不可時はシステム認可ダイアログへフォールバックし、機能は維持。
- **多言語（en / zh / zh-TW / ja / ko）**: 79 キー×5 言語、パリティ検証付き。

## 技術スタック

- **Tauri v2 (Rust)** + システム WebView（既存の HTML/CSS エディタを再利用。Node ビルドチェーン不要）
- Rust バックエンド: Core Engine（照合／適用／ヘルス／自動化）+ PAL（プラットフォーム抽象層）
- プラットフォーム実装（同一の上位コード、コンパイル時に選択）:

| プラットフォーム | 読取 | 書込 | 権限昇格 |
|------------------|------|------|----------|
| macOS | `networksetup` / `arp` / `airport` | `networksetup` / `route` | パスワード不要な sudoers 許可リスト、フォールバックは `osascript` 認可ダイアログ |
| Windows | PowerShell CIM（`Get-NetAdapter` / `Get-NetConnectionProfile` …）+ `netsh` | `netsh` / `New-NetRoute` | 既に管理者ならプロンプトなし、それ以外は UAC |
| Linux | `nmcli` / `ip neigh` | `nmcli con mod` / `ip route` | `sudo -n` 可ならプロンプトなし、それ以外は `pkexec` |

## クイックスタート

### 方法1: クラウドビルド（推奨、ローカルに依存なし）

タグをプッシュすると CI が 3 プラットフォーム分のインストーラと実行ファイルを一度に生成（`windows-x64` / `macos-arm64` / `macos-x64` / `linux-x64`）:

```bash
git tag v0.3.0 && git push origin v0.3.0
```

GitHub → Actions → build → Run workflow から手動でも起動可。

### 方法2: ローカルビルド

```bash
# 前提: Rust + 各プラットフォームのビルド依存（Windows は MSVC C++ ビルドツール + WebView2 も必要）

# Windows（スクリプトが Rust/MSVC/SDK/WebView2/ディスクを診断し、不足を通知）
powershell -ExecutionPolicy Bypass -File scripts\build-windows.ps1            # 裸の exe
powershell -ExecutionPolicy Bypass -File scripts\build-windows.ps1 -Installer # msi/nsis

# macOS / Linux
cd src-tauri && cargo build --release   # 裸の実行ファイル
cd src-tauri && cargo tauri build       # インストーラ（tauri-cli 要）

# macOS 任意: パスワード不要な権限チャネルを導入し、毎回の認可ダイアログを除去
sh scripts/install-priv-helper.sh
```

### 検証（コンパイル不要、任意のプラットフォームで実行可）

```bash
python scripts/validate.py     # JSON / 5言語 i18n パリティ / PAL 境界 / 3プラットフォーム trait カバレッジ
cd src-tauri && cargo test       # 本プロジェクトは純 bin crate（lib target なし）。cargo test を --lib ではなく使用
```

起動後はトレイに常駐（メインウィンドウは表示されない）: **トレイアイコンを左クリック** でポップアップパネルを開く。
右クリックでメニュー表示（エディタ表示／ログフォルダを開く／権限チャネル表示／終了）。

> **Windows は最初のネットワーク変更時に一度 UAC が出る。** 管理者として一度起動すれば以降は表示されない（以降、権限チャネルは「認可不要」と表示）。

## 設定

`config.json` を編集（`config.example.json` から複製）。主要構造:

```json
{
  "__DEFAULT__": { "mode": "dhcp", "dns": "", "v6mode": "automatic" },
  "Office_5G": {
    "match": { "ssid": "Office_5G", "gateway_mac": "aa:bb:cc:dd:ee:ff", "bssid": "00:11:22:33:44:55" },
    "mode": "manual", "ip": "192.168.1.100", "gateway": "192.168.1.1", "dns": "192.168.1.1,8.8.8.8",
    "health":   { "enabled": true, "fallback": { "enabled": true }, "mode": "both" },
    "automation": { "enabled": true, "on_apply": [ { "type": "route", "dest": "10.0.0.0/8", "gateway": "192.168.1.1" } ] }
  }
}
```

詳細は [DEVELOPMENT.md](DEVELOPMENT.md) を参照。

## ライセンス

MIT
