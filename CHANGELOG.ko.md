# 변경 로그

NetSense의 모든 주요 변경 사항을 여기에 기록합니다. 형식은 [Keep a Changelog](https://keepachangelog.com/)를 따르며, 이 프로젝트는 [의미론적 버전](https://semver.org/)을 준수합니다.

## [0.2.1] - 2026-09-19

### ✨ 추가
- **기본값이 영어인 다국어 문서**: `README.md`(en) 외에 `README.zh.md` / `README.zh-TW.md` / `README.ja.md` / `README.ko.md`; `CHANGELOG.md`(en) 외에 `CHANGELOG.zh.md` / `CHANGELOG.zh-TW.md` / `CHANGELOG.ja.md` / `CHANGELOG.ko.md`.
- 버전 번호의 단일 진짜 값으로 `VERSION` 파일을 추가. `scripts/bump_version.sh`가 이를 `tauri.conf.json`과 `Cargo.toml`에 동기화.

### 🔧 변경
- **Windows 설치 관리자가 컴퓨터 단위(per-machine) 설치로 변경**(`bundle.windows.nsis.installMode` / `wix.installMode` = `perMachine`). 앱은 `C:\Program Files\NetSense`에 설치되며 설치 시 관리자 권한 필요(이전에는 `%LOCALAPPDATA%` 아래 사용자 단위로 설치).
- 릴리스 노트는 해당 버전의 영어 `CHANGELOG.md` 섹션에서 생성되므로 게시 릴리스는 기본적으로 영어.

### 📝 문서
- `DEVELOPMENT.md`를 영어(기본값)로 재작성.

## [0.2.0] - 2026-09-18

### ✨ 추가
- **진정한 크로스 플랫폼 지원**(macOS / Windows / Linux)을 PAL(플랫폼 추상화 계층)로 구현: 모든 OS 차이는 `platform/{macos,windows,linux}.rs`에 수렴되고 `NetworkPlatform` trait 뒤에 배치되어 컴파일 시 선택. 상위 엔진은 trait만 의존.
- **4대상 CI 빌드 행렬**(`windows-x64` / `macos-arm64` / `macos-x64` / `linux-x64`). `v*` 태그를 푸시하면 Draft Release가 발동되어 각 플랫폼 설치 파일(Windows는 NSIS + MSI, macOS는 DMG, Linux는 DEB)과 실행 파일을 첨부.
- **앱 내 다국어 UI**(en / zh / zh-TW / ja / ko): 79키 × 5언어. `scripts/validate.py`로 패리티 검증.
- **권한 상승 채널**을 우아한 폴백과 함께 제공: macOS는 sudoers 허용 목록 설치 후 암호 불필요. Windows는 이미 관리자면 프롬프트 없음(아니면 UAC). Linux는 `sudo -n` 설정 시 프롬프트 없음(아니면 `pkexec`).
- **헬스 모니터링**: ICMP / HTTP / both 프로브. 폴백 활성화 상태에서 연속 실패하면 자동으로 DHCP로 되돌림.
- **네트워크 기반 자동화**: `route` / `launch` / `run`을 `on_apply` / `on_revert`에서 발동. 스크립트는 허용 목록으로 제약.

### 🐛 수정
- Windows CI 패키징: `windows-latest`에서 WiX + NSIS를 명시적으로 설치(기본 포함 안 됨). 번들 대상을 플랫폼별로 축소.

### 🛠 내부
- `scripts/validate.py`에 JSON 검증, 5언어 i18n 패리티(79×5), PAL 경계, 3플랫폼 trait 커버리지 검사 추가.
