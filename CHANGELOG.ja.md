# 変更履歴

NetSense のすべての重要な変更をここに記録する。形式は [Keep a Changelog](https://keepachangelog.com/) に基づき、本プロジェクトは [セマンティックバージョニング](https://semver.org/) に準拠する。

## [0.2.1] - 2026-09-19

### ✨ 追加
- **英語をデフォルトとする多言語ドキュメント**: `README.md`（en）に加え `README.zh.md` / `README.zh-TW.md` / `README.ja.md` / `README.ko.md`；`CHANGELOG.md`（en）に加え `CHANGELOG.zh.md` / `CHANGELOG.zh-TW.md` / `CHANGELOG.ja.md` / `CHANGELOG.ko.md`。
- バージョン番号の単一真値として `VERSION` ファイルを追加。`scripts/bump_version.sh` がこれを `tauri.conf.json` と `Cargo.toml` に同期する。
- アプリアイコン：`app-icon.png`（1024² のマスター）をウィンドウ・トレイ・インストーラのアイコン原版とし、`scripts/gen_icons.py` がそこから一式を生成する — 各サイズの PNG、7 フレームの ICO（16 → 256、BMP フレーム + PNG フレーム）と 8 チャンクの ICNS（ic07–ic14）。

### 🔧 変更
- **Windows インストーラがマシン単位（per-machine）に**:（`bundle.windows.nsis.installMode` / `wix.installMode` = `perMachine`）。アプリは `C:\Program Files\NetSense` にインストールされ、インストール時に管理者権限が必要（以前は `%LOCALAPPDATA%` 配下にユーザー単位でインストールされていた）。
- リリースノートは対応バージョンの英語 `CHANGELOG.md` セクションから生成され、公開リリースはデフォルトで英語になる。
- `scripts/gen_icons.py` はプレースホルダーの図形を描かなくなった：マスターを再サンプリングし、各ターゲットサイズで角丸マスクを適用して PNG/ICO/ICNS コンテナを組み立てる（純粋な stdlib のみ、サードパーティ依存なし）。

### 🐛 修正
- **Windows：不要なコンソールウィンドウが表示されなくなった。** PAL は `Command::output()` で `powershell.exe` / `netsh` を起動する際に作成フラグを指定していなかったため、状態を読み取るたびに Windows が可視コンソール（タイトルバーのボタン付き）を割り当てていた — 現在は `CREATE_NO_WINDOW` で起動する。
- **Windows：アプリが「起動していない」ように見えた。** NetSense はトレイ常駐アプリで、起動時にどちらのウィンドウも表示しないため、コンソールウィンドウが消えると何も見えなかった。起動時にステータスパネルを開き、フォーカスを失うか閉じるとトレイに戻すようにした。
- CI：Windows の `choco install wixtoolset nsis` ステップにハードタイムアウトを設定し、ダウンロードが停滞してもランナーの上限まで待ち続けないようにした。

### 📝 ドキュメント
- `DEVELOPMENT.md` を英語（デフォルト）に書き直し。

## [0.2.0] - 2026-09-18

### ✨ 追加
- **真のクロスプラットフォーム対応**（macOS / Windows / Linux）を PAL（Platform Abstraction Layer）で実現: すべての OS 差異は `platform/{macos,windows,linux}.rs` に集約され、`NetworkPlatform` trait の背後に置かれコンパイル時に選択。上位エンジンは trait のみに依存。
- **4ターゲット CI ビルド行列**（`windows-x64` / `macos-arm64` / `macos-x64` / `linux-x64`）。`v*` タグをプッシュすると Draft Release が発動し、各プラットフォームのインストーラ（Windows は NSIS + MSI、macOS は DMG、Linux は DEB）と裸の実行ファイルを添付。
- **アプリ内多言語 UI**（en / zh / zh-TW / ja / ko）: 79キー×5言語。`scripts/validate.py` でパリティ検証。
- **権限昇格チャネル**を任意のフォールバック付きで提供: macOS は sudoers 許可リスト導入後はパスワード不要。Windows は既に管理者ならプロンプトなし（それ以外は UAC）。Linux は `sudo -n` 設定でプロンプトなし（それ以外は `pkexec`）。
- **ヘルスモニタリング**: ICMP / HTTP / both プローブ。フォールバック有効時に連続失敗すると自動で DHCP へ戻る。
- **ネットワーク起因の自動化**: `route` / `launch` / `run` を `on_apply` / `on_revert` で発動。スクリプトは許可リストで制約。

### 🐛 修正
- Windows CI パッケージジング: `windows-latest` で WiX + NSIS を明示的にインストール（プリインストールされていない）。バンドルターゲットをプラットフォームごとに絞り込み。

### 🛠 内部
- `scripts/validate.py` に JSON 検証、5言語 i18n パリティ（79×5）、PAL 境界、3プラットフォーム trait カバレッジのチェックを追加。
