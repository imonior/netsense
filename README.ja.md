# NetSense

[English](README.md) · [简体中文](README.zh.md) · [繁體中文](README.zh-TW.md) · **日本語** · [한국어](README.ko.md)

macOS、Windows、Linux 向けのクロスプラットフォームなネットワーク **Profile** マネージャー。

各 Profile は 3 つの問いに答えます。*自分はどのネットワークにいるのか?*（`Rules` / `Conditions`）、
*このネットワークはどのようになっていられるべきか?*（Profile に一致した時は `THEN`、一致しない時は
`ELSE`）、*その後に何が起きるべきか?*（自動化アクション）。NetSense はこれらの Profile を実際の
ネットワークに対して評価し、必ず 1 つだけを Active に保ったまま、結果の設定をシステム自身のツール
経由で適用します —— macOS では `networksetup`、Windows では PowerShell CIM + `netsh`、Linux では
`nmcli`。

トレイで動きます。起動時に一度だけ設定エディタのウィンドウが開きます —— アプリが実際に起動した
ことを疑いなく示すサインです —— 閉じてもアプリは終了せず、トレイに引っ込むだけです。
設定の要るデーモンなし、テレメトリなし。

## 判断はどのように下されるか

```
                 ┌───────────── 有効な Profile ─────────────┐
ネットワーク ──▶ │  Rule = 有効な Conditions を AND         │ ──▶ 1 件一致  ──▶ その Profile が Active
   変化          │  Rules 間は OR · スナップショットで再評価│ ──▶ 2+ 件一致 ──▶ Conflict（ダイアログ、適用なし）
                 └──────────────────────────────────────────┘ ──▶ 0 件一致  ──▶ グローバル fallback（Profile ではない）
```

- **照合はネットワークの身元で行います**: Wi‑Fi SSID、ゲートウェイ MAC、AP の BSSID ——
  だから場所の違う同名 SSID も、なりすましホットスポットも、区別されたままになります。
  `network_interface` の条件も使えます。有線リンクや、NIC そのものを指したい場合向けの手段です。
- **Profile の優先順位はありません** —— あえてそうしています。複数一致した場合、正直な答えは
  *Conflict* だけです。NetSense はそれを表示し、何も適用しません。勝者をこっそり選ぶことはしません。
- **適用は 1 つのバリア（3A）です**: IPv4 / サブネットマスク / ゲートウェイ / DNS / IPv6 と静的ルートが
  まとめて適用されます。分岐が `verify` を設定している場合は、続いてシステムの状態を読み戻して検証され、
  さらに ICMP/HTTP のヘルスチェックを追加できます（任意）。ネットワークが失敗し続けるなら DHCP に
  復帰します。
- **自動化（3B）は 3A を通過した後だけに実行されます。** 単発アクション（`launch_app`、`run_script`、
  `set_default_printer`）は `priority` ごとにバッチで実行されます —— 低いものが先、同じ優先度は並行、
  あるバッチが終わってから次のバッチへ進み、1 つのバッチ内の失敗が後のバッチを止めることはありません。
  既定プリンタの設定は *そのユーザー* の既定だけを変えるので、ネットワークを切り替えても認可ダイアログは
  出ません。一致の失敗（Conflict）と実行の失敗（Error）は別の状態であり、分けて報告されます。
- **検出は Profile 単位です**: ネットワークイベントへの反応、間隔を空けたポーリング、またはその両方。
  それぞれに独自の変化後の待機時間があり、不安定な再接続がアダプタを何度も書き換えることを防ぎます。
- **常駐アクションが保つのは期待状態で、コマンドを繰り返すことではありません。** 有効なアクション
  ごとにワーカー 1 本が動き、Active プロファイルの THEN 分岐と同時に始まり、新しい設定を適用する前に
  必ず止まります。トンネルを確認してすでに上がっていると分かったらコマンドを 1 つも発行せず、
  ワーカーが承認ダイアログを出すこともありません。（`periodic_script` だけが「確認すべき状態」を持たない
  種別です。この tick の本体はスクリプトを実行することそのものなので、設計どおり繰り返されます。）

## 機能

- Profile ごとの静的 IP / DHCP / カスタム DNS / IPv6（automatic、manual、off）/ 静的ルート。THEN と
  ELSE の分岐を別々に持てます。
- 読み戻し検証とヘルスモニタリングは適用ステップの中で分岐ごとに設定します: システムが実際の一歩を
  認めた時だけ、Profile は「適用済み」を報告します。
- トレイのポップアップパネル: 実際に使われているネットワーク（インターフェース、SSID と電波強度、
  MAC、IPv4 / マスク / ゲートウェイ / IPv6 / DNS）、その他の有効なインターフェース、VPN トンネル、
  一致バッジ付きのワンクリック Profile 切り替え、そしてすべての入口 —— 設定・ログ・DHCP・診断・
  アップデート・終了。アイコンのクリック（左右どちらでも）で開き、フォーカスを外れると収まります;
  ネイティブなトレイメニューはありません。
- モデル全体をカバーする設定エディタ —— Rules、Conditions、3A、ルート、アクション、ELSE、グローバル
  fallback —— エンジンのリアルタイムの判定をその場に表示します。
- アプリの設定ウィンドウ — どのネットワークにも関係しない設定をまとめた場所です: 表示言語、ログイン時の起動（ウィンドウを開くたびに OS から読みます。ファイル内の副本からは読みません）、`config.json` とログの場所、そしてログの保存日数。
- できる限りパスワード不要で: macOS では sudoers 許可リストを一度導入、Windows では管理者として一度
  実行、Linux では `sudo -n`。それができない場合、NetSense は失敗する代わりにシステムの認可ダイアログへ
  フォールバックします。
- オンラインアップグレード: GitHub Releases を確認し、このプラットフォームのアセットを選びます。
  Homebrew からのインストールは `brew upgrade --cask` で行い、何もダウンロードしません。それ以外は
  アセットをダウンロードし、その SHA256 が Release の `SHA256SUMS` と一致する場合に限りインストール
  します —— 一致を確かめられない（`SHA256SUMS` が無い、このアセットが載っていない、取得できない）場合も
  インストールせず、Release ページを開く案内に切り替えます。
- 5 つの UI 言語（English、简体中文、繁體中文、日本語、한국어）、パリティを検証済み。既定は English です。
  3 つのプラットフォームのすべてのウィンドウがこの選択に従います —— トレーパネル、エディタ、設定、
  ログビューア、トレーのツールチップとネイティブのエラーダイアログも含めて。`scripts/validate.py` の
  1 項目が固定テキストを検出しています。

## 技術スタック

Tauri v2 + Rust、システム WebView、フロントエンドは素の静的 HTML/CSS/JS（Node ビルドチェーンなし）。
Core Engine（detection / conditions / 照合 / network / automation）はシステムコマンドを直接呼び出すこと
がなく、プラットフォーム固有のものはすべて PAL 内の 1 つの trait の後ろに置かれています
（`src-tauri/src/platform/{macos,windows,linux}.rs`）。コンパイル時に選択されます。

| プラットフォーム | 読み取り | 書き込み | 権限昇格 |
|------------------|----------|----------|----------|
| macOS | `CoreWLAN` / `networksetup` / `ipconfig` / `arp` / `system_profiler`（`airport` はまだ生きているところで） | `networksetup` / `route` | sudoers 許可リスト、それ以外は `osascript` の認可ダイアログ |
| Windows | PowerShell CIM（`Get-NetAdapter` …）+ `netsh` | `netsh` / `New-NetRoute` | 既に管理者なら承認不要、それ以外は UAC |
| Linux | `nmcli` / `ip neigh` | `nmcli con mod` / `ip route` | `sudo -n`、それ以外は `pkexec` |

## クイックスタート

### クラウドでビルド（ローカル依存なし）

バージョンタグをプッシュすると、CI が 4 つのターゲットすべてに対して一度に実行され
（`windows-x64` / `macos-arm64` / `macos-x64` / `linux-x64`）、インストーラを添付した **Draft** Release が
作成されます。すべてのアセットが揃って `SHA256SUMS` も生成されると、勝手に公開へ進みます:

```bash
git tag v1.0.0 && git push origin v1.0.0
```

同じワークフローは GitHub → Actions → build → Run workflow から手動でも起動できます。この場合は
むき出しの実行ファイルだけが生成され、Release は作成されません。

### ローカルでビルド

```bash
# Rust と、各プラットフォームのビルド依存が必要です
#（Windows では MSVC C++ ビルドツールと WebView2 も必要です）

# Windows —— スクリプトが最初に Rust/MSVC/SDK/WebView2/ディスクを確認し、不足を報告します
powershell -ExecutionPolicy Bypass -File scripts\build-windows.ps1            # 実行ファイル
powershell -ExecutionPolicy Bypass -File scripts\build-windows.ps1 -Installer # msi / nsis

# macOS / Linux
cd src-tauri && cargo build --release   # 実行ファイル
cd src-tauri && cargo tauri build       # インストーラ（tauri-cli が必要）

# macOS（任意）: パスワード不要の権限チャネルを導入すると、ネットワークを変えるたびに
# 承認を求められなくなります
sh scripts/install-priv-helper.sh
```

### 検証（コンパイルなし、どこでも実行可能）

```bash
python3 scripts/validate.py    # 5 言語の JSON、i18n パリティとプレースホルダ、双方向の key 使用検査、
                               # PAL 境界、3 プラットフォームの trait カバレッジ、tauri.conf.json の
                               # フィールド、バージョンの一貫性、ドキュメントとコードの突き合わせ、
                               # UI 文字列がすべて辞書由来かどうか
node scripts/editor-smoke.mjs  # エディタのデータバインディングをヘッドレスで実行（Node は要りますがビルドステップはありません）
cd src-tauri && cargo test     # 純粋な bin crate —— `cargo test` を使用（`--lib` ではない）
```

NetSense はトレイに常駐します: アイコンをクリック（左右どちらでも）するとパネルが開き、すべての入口が
そのパネルにあります（設定 · ログフォルダを開く · 現在のネットワークを DHCP にする · いま確認 · 終了。
これらの上に使用中の NIC とトンネルが表示されます）。設定は2つのファイルに分かれています。この2種類の設定には共通点がないからです。
どのネットワークにどの処理を適用するかは自動化の設定で、1 つの `config.json` にあり、エディタウィンドウで編集します。
アプリ自体の振る舞い（表示言語、ログ保存日数、ログイン時の起動）はアプリの設定で、`settings.json` にあり、
パネルの「設定」が開くウィンドウで編集します。ここを変更してもネットワーク設定が再適用されることはありません。
`config.json` と `settings.json` はどちらもユーザーごとの NetSense ディレクトリに
置かれます —— macOS は `~/Library/Application Support/NetSense`、Windows は `%APPDATA%\NetSense`、
Linux は `~/.config/netsense`。実行ファイルと同じ場所には置きません。署名済み macOS バンドルと
読み取り専用の `Program Files` に書き込むべきではないからです。ログはユーザーごとの NetSense ログディレクトリにあります。
`config.example.json` は完全な実例です。

> Windows では最初のネットワーク変更時に一度だけ UAC が求められます。管理者として一度実行すれば解消し、
> 以降は権限チャネルが「承認不要」と表示されます。

## 設定

```jsonc
{
  "schema": 1,
  "allowed_scripts": ["/opt/ops/office-init.sh"],
  "profiles": [{
    "id": "office", "name": "Office_5G", "enabled": true,
    "detection": { "mode": "network_events_and_polling", "poll_interval_secs": 30 },
    "rules": [{
      "id": "r1", "enabled": true,
      "conditions": [
        { "id": "c1", "enabled": true, "type": "wifi_ssid",   "value": "Office_5G" },
        { "id": "c2", "enabled": true, "type": "gateway_mac", "value": "aa:bb:cc:dd:ee:ff" }
      ]
    }],
    "then": {
      "network": {
        "mode": "manual", "ip": "192.168.1.100", "netmask": "255.255.255.0",
        "gateway": "192.168.1.1", "dns": "192.168.1.1,8.8.8.8", "v6mode": "off",
        "routes": [{ "dest": "10.0.0.0/8", "gateway": "192.168.1.1", "metric": 0 }],
        "verify": { "readback": true,
                    "health": { "enabled": true, "mode": "both",
                                "icmp_target": "192.168.1.1",
                                "http_target": "http://192.168.1.1/",
                                "interval": 30, "retries": 3, "timeout": 5,
                                "fallback": { "enabled": true } } }
      },
      "one_shot": [
        { "id": "a1", "enabled": true, "priority": 1,
          "action": { "type": "run_script", "path": "/opt/ops/office-init.sh", "elevated": false } }
      ]
    },
    "else": { "network": { "mode": "dhcp", "dns": "", "routes": [{ "dest": "10.0.0.0/8", "delete": true }] } }
  }],
  "fallback": { "enabled": true, "network": { "mode": "dhcp", "dns": "", "v6mode": "automatic" } }
}
```

知っておくと良い慣習が 2 つあります。**空文字列はそのフィールドをクリアし、書かれていない
フィールドには一切触らない**。`dns` もこの作りで、エディタの DNS が三択になっているのも
そのためです ——「変更しない」はキーを消すことなので、どのプラットフォームも DNS のコマンドを出ません、
「システム自動」は空文字列を書くので設定済みの DNS はクリアされます、「DNS Servers」は入力された値を
そのまま適用します。そして `run_script` が実行するのは、`<設定ファイルのディレクトリ>/scripts`
の中か `allowed_scripts` に登録されたパスだけです。相対パスはプロセスを起動したディレクトリではなく、
config.json のあるディレクトリを基準に解決されます。

`DEVELOPMENT.md` にモデル、エンジンのパイプライン、プラットフォーム別の落とし穴を詳しく書いてあります。

## ライセンス

MIT
