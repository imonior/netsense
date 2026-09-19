# NetSense

> 현재 네트워크 신원(SSID / 게이트웨이 MAC / BSSID)에 따라 네트워크 프로필(고정 IP / DHCP / DNS / IPv6)을 자동으로 매칭·적용하는 크로스 플랫폼 독립 데스크톱 앱. 연결 끊김 시 DHCP로 폴백하는 헬스 모니터링과 네트워크 기반 자동화(라우트 설정 / 앱 실행 / 스크립트 실행)를 제공.

[hammerspoon-wifi-switcher](https://github.com/imonior/hammerspoon-wifi-switcher)에서 발전하여 Hammerspoon에 의존하지 않는 macOS / Windows / Linux용 독립 앱이 됨.

## 기능

- **네트워크 신원 매칭**: SSID, 게이트웨이 MAC, AP BSSID 각각을 독립 조건으로 사용(단독 또는 조합, AND 관계). 동일 SSID의 여러 환경이나 사칭 핫스팟(evil twin)도 정확히 구분.
- **SSID별 프로필**: 고정 IP / DHCP / 사용자 지정 DNS / IPv6(automatic / manual / off).
- **전역 폴백**: `__DEFAULT__`는 설정되지 않은 모든 네트워크에 적용.
- **헬스 모니터링**: ICMP / HTTP / both 프로브. 폴백 활성화 상태에서 연속 실패하면 자동으로 DHCP로 되돌려 연결 유지.
- **자동화**: 네트워크 기반으로 `route` / `launch` / `run` 발동(netsetman 형 부가 동작), `on_apply` / `on_revert` 트리거. 스크립트는 허용 목록으로 제약.
- **트레이 팝업 패널**: 상태 표시줄/트레이 아이콘 좌클릭 시 패널 표시(상태 + 원클릭 프로필 전환 + 언어). 포커스 이탈 시 자동 접힘. 우클릭은 네이티브 메뉴.
- **세 플랫폼 단일 코드베이스**: 플랫폼 차이는 모두 PAL(`platform/{macos,windows,linux}.rs`)에 수렴. 상위 계층은 trait만 의존.
- **권한 상승**: macOS는 sudoers 허용 목록 설치 후 암호 불필요. Windows는 관리자로 한 번 실행하면 UAC 없음. Linux는 `sudo -n` 설정 시 프롬프트 없음. 불가 시 시스템 인증 대화상자로 폴백하며 기능 유지.
- **다국어(en / zh / zh-TW / ja / ko)**: 79키 × 5언어, 패리티 검증 포함.

## 기술 스택

- **Tauri v2 (Rust)** + 시스템 WebView(기존 HTML/CSS 편집기 재사용, Node 빌드 체인 불필요)
- Rust 백엔드: Core Engine(매칭/적용/헬스/자동화) + PAL(플랫폼 추상화 계층)
- 플랫폼 구현(동일 상위 코드, 컴파일 시 선택):

| 플랫폼 | 읽기 | 쓰기 | 권한 상승 |
|--------|------|------|-----------|
| macOS | `networksetup` / `arp` / `airport` | `networksetup` / `route` | 암호 불필요한 sudoers 허용 목록, 폴백은 `osascript` 인증 대화상자 |
| Windows | PowerShell CIM(`Get-NetAdapter` / `Get-NetConnectionProfile` …) + `netsh` | `netsh` / `New-NetRoute` | 이미 관리자면 프롬프트 없음, 아니면 UAC |
| Linux | `nmcli` / `ip neigh` | `nmcli con mod` / `ip route` | `sudo -n` 가능하면 프롬프트 없음, 아니면 `pkexec` |

## 빠른 시작

### 방식 1: 클라우드 빌드(권장, 로컬 무의존)

태그를 푸시하면 CI가 세 플랫폼 설치 파일과 실행 파일을 한 번에 생성(`windows-x64` / `macos-arm64` / `macos-x64` / `linux-x64`):

```bash
git tag v0.3.0 && git push origin v0.3.0
```

GitHub → Actions → build → Run workflow에서 수동 실행도 가능.

### 방식 2: 로컬 빌드

```bash
# 사전조건: Rust + 플랫폼별 빌드 의존성(Windows는 MSVC C++ 빌드 도구 + WebView2 추가 필요)

# Windows(스크립트가 Rust/MSVC/SDK/WebView2/디스크를 점검하고 부족분을 알림)
powershell -ExecutionPolicy Bypass -File scripts\build-windows.ps1            # 실행 파일만
powershell -ExecutionPolicy Bypass -File scripts\build-windows.ps1 -Installer # msi/nsis

# macOS / Linux
cd src-tauri && cargo build --release   # 실행 파일
cd src-tauri && cargo tauri build       # 설치 파일(tauri-cli 필요)

# macOS 선택: 암호 불필요한 권한 채널 설치로 매번 인증 대화상자 제거
sh scripts/install-priv-helper.sh
```

### 검증(컴파일 불필요, 모든 플랫폼 실행 가능)

```bash
python scripts/validate.py     # JSON / 5언어 i18n 패리티 / PAL 경계 / 3플랫폼 trait 커버리지
cd src-tauri && cargo test       # 본 프로젝트는 순수 bin crate(lib target 없음). --lib 대신 cargo test 사용
```

실행 후 앱은 트레이에 상주(메인 창 미표시): **트레이 아이콘 좌클릭**으로 팝업 패널 열기.
우클릭은 메뉴(편집기 표시 / 로그 폴더 열기 / 권한 채널 보기 / 종료).

> **Windows는 첫 네트워크 변경 시 UAC가 한 번 표시됨.** 관리자로 한 번 실행하면 이후 사라짐(이후 권한 채널은 "인증 불필요" 표시).

## 구성

`config.json` 편집(`config.example.json`에서 복사). 주요 구조:

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

자세한 내용은 [DEVELOPMENT.md](DEVELOPMENT.md) 참조.

## 라이선스

MIT
