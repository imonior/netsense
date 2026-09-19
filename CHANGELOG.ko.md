# 변경 로그

NetSense의 모든 주요 변경 사항을 여기에 기록합니다. 형식은 [Keep a Changelog](https://keepachangelog.com/)를 따르며, 이 프로젝트는 [의미론적 버전](https://semver.org/)을 준수합니다.

## [0.2.1] - 2026-09-19

### ✨ 추가
- **기본값이 영어인 다국어 문서**: `README.md`(en) 외에 `README.zh.md` / `README.zh-TW.md` / `README.ja.md` / `README.ko.md`; `CHANGELOG.md`(en) 외에 `CHANGELOG.zh.md` / `CHANGELOG.zh-TW.md` / `CHANGELOG.ja.md` / `CHANGELOG.ko.md`.
- 버전 번호의 단일 진짜 값으로 `VERSION` 파일을 추가. `scripts/bump_version.sh`가 이를 `tauri.conf.json`과 `Cargo.toml`에 동기화.
- 앱 아이콘: `app-icon.png`(1024² 마스터)를 창·트레이·설치 관리자 아이콘의 원본으로 사용하며, `scripts/gen_icons.py`가 여기서 전체 세트를 생성 — 각 크기 PNG, 7프레임 ICO(16 → 256, BMP 프레임 + PNG 프레임), 8청크 ICNS(ic07–ic14).

### 🔧 변경
- **Windows 설치 관리자가 컴퓨터 단위(per-machine) 설치로 변경**(`bundle.windows.nsis.installMode` = `perMachine`). 앱은 `C:\Program Files\NetSense`에 설치되며 설치 시 관리자 권한 필요(이전에는 `%LOCALAPPDATA%` 아래 사용자 단위로 설치). MSI에는 해당 옵션이 없으므로(WiX는 원래 `%PROGRAMFILES%`에 설치) 더 이상 이 필드를 설정하지 않습니다.
- 릴리스 노트는 해당 버전의 영어 `CHANGELOG.md` 섹션에서 생성되므로 게시 릴리스는 기본적으로 영어.
- `scripts/gen_icons.py`가 더 이상 자리 표시자 도형을 그리지 않음: 마스터를 리샘플링하고 각 대상 크기에 둥근 모서리 마스크를 적용한 뒤 PNG/ICO/ICNS 컨테이너를 조립(순수 stdlib, 서드파티 의존성 없음).

### 🐛 수정
- **Windows: 불필요한 콘솔 창이 더 이상 나타나지 않음.** PAL이 `Command::output()`으로 `powershell.exe` / `netsh`를 호출하면서 생성 플래그를 지정하지 않아, 상태를 읽을 때마다 Windows가 보이는 콘솔(제목 표시줄 버튼 포함)을 할당했습니다 — 이제 `CREATE_NO_WINDOW`로 시작합니다.
- **Windows: 앱이 "실행되지 않은" 것처럼 보였음.** NetSense는 트레이 상주 앱이며 시작 시 두 창 모두 표시하지 않으므로, 콘솔 창이 사라지면 아무것도 보이지 않았습니다. 이제 시작 시 상태 패널을 열고 포커스를 잃거나 닫으면 트레이로 되돌립니다.
- CI: Windows의 `choco install wixtoolset nsis` 단계에 하드 타임아웃을 설정하여 다운로드가 멈춰도 러너 한도까지 매달리지 않습니다.

### 🔒 보안
- **모든 권한 상승 경로에 셸 인용 적용.** 설정 파일이나 OS에서 오는 값(프로필 이름, SSID, 게이트웨이 주소, 라우트)은 `osascript` / `sudo` / 셸로 전달됩니다. 이제 보간 지점에서 POSIX 작은따옴표로 감싸므로 `'`, `$(…)`, 백틱, `;`가 포함된 값이 root 셸에 명령을 주입할 수 없습니다.
- **macOS 상승 분기에도 동일하게 인용.** 비밀번호 없는 `osascript … with administrator privileges` 경로는 문자열 연결로 명령줄을 만들었으나, 이제 스크립트 경로와 인자를 토큰 단위로 인용합니다.
- **Windows `v6prefix`는 보간 대신 파싱.** OS가 반환한 접두사를 그대로 PowerShell 명령줄에 삽입했으나, 이제 정수로만 허용하고 그 외에는 옵션 자체를 버립니다.
- **자동화 스크립트 허용 목록을 경로 접두사로 우회할 수 없음.** `scripts2/` 같은 디렉터리가 단순 문자열 접두사 비교로 `scripts/`에 매칭되었으나, 이제 양쪽 경로를 정규화해 경로 구성 요소 단위로 비교합니다.

### 🛠 내부
- `scripts/validate.py`에 검사 **[7] `tauri.conf.json` 필드 유효성** 추가: `bundle` / `bundle.windows` / `nsis` / `wix` 하위 트리를 공식 Tauri v2 스키마로 검증하여, 잘못된 필드가 4개 플랫폼 빌드가 아니라 10분짜리 검증 작업에서 실패합니다.
- 릴리스 게시 단계를 태그 푸시로만 제한하여, 수동 `workflow_dispatch` 검증이 기존 Draft Release의 태그를 덮어쓰지 않습니다.

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
