# NetSense

[English](README.md) · [简体中文](README.zh.md) · [繁體中文](README.zh-TW.md) · [日本語](README.ja.md) · **한국어**

macOS, Windows, Linux용 크로스 플랫폼 네트워크 **Profile** 관리자.

각 Profile은 세 가지 질문에 답합니다: *나는 지금 어떤 네트워크에 있는가?* (`Rules` / `Conditions`),
*이 네트워크는 어떤 모습이어야 하는가?* (Profile이 일치하면 `THEN`, 일치하지 않으면 `ELSE`), *그다음에
무슨 일이 일어나야 하는가?* (자동화 동작). NetSense는 이 Profile들을 실제 네트워크에 대해 평가해
정확히 하나만 Active로 유지하고, 그 결과 설정을 시스템 자체 도구를 통해 적용합니다 — macOS는
`networksetup`, Windows는 PowerShell CIM + `netsh`, Linux는 `nmcli`.

트레이에서 동작합니다. 시작할 때 구성 편집기 창이 한 번 열립니다 — 앱이 실제로 실행되었다는
의심할 여지 없는 신호 — 그리고 닫는 것은 트레이로 숨는 것뿐입니다. 설정할 데몬 없음, 원격 측정 없음.

## 판단은 이렇게 내려집니다

```
             ┌────────────── 활성 Profile ──────────────┐
네트워크 ──▶ │  Rule = 활성 Conditions AND              │ ──▶ 1개 일치  ──▶ 해당 Profile이 Active
   변화      │  Rules 사이는 OR · 스냅샷마다 재확인     │ ──▶ 2+개 일치 ──▶ Conflict (창 표시, 아무것도 적용 안 함)
             └──────────────────────────────────────────┘ ──▶ 0개 일치  ──▶ 전역 fallback (Profile 아님)
```

- **일치는 네트워크 신원으로 판정합니다**: Wi‑Fi SSID, 게이트웨이 MAC, AP BSSID — 그래서 서로 다른
  장소의 같은 이름 SSID도, 사칭 핫스팟도 구분된 채로 남습니다. `network_interface` 조건도 준비되어
  있습니다. 유선 링크나, NIC 그 자체를 가리켜야 하는 경우에 씁니다.
- **Profile 우선순위는 없습니다** — 의도한 것입니다. 둘 이상 일치하면 정직한 답은 *Conflict*뿐입니다.
  NetSense는 그것을 보여주고 아무것도 적용하지 않으며, 몰래 승자를 고르지 않습니다.
- **적용은 하나의 장벽(3A)입니다**: IPv4 / 서브넷 마스크 / 게이트웨이 / DNS / IPv6와 정적 라우트가
  함께 내려갑니다. 분기가 `verify`를 설정했다면 이어서 시스템 상태를 다시 읽어 검증하고, 여기에
  ICMP/HTTP 헬스 체크를 선택적으로 더합니다 — 네트워크가 계속 실패하면 DHCP로 복구합니다.
- **자동화(3B)는 3A를 통과한 뒤에만 실행됩니다.** 일회성 동작(`launch_app`, `run_script`,
  `set_default_printer`)은 `priority` 단위로 묶어 실행합니다 — 낮은 것이 먼저, 같은 우선순위는 동시에,
  한 묶음이 끝난 뒤 다음 묶음으로 진행하고, 한 묶음 안의 실패가 뒤따르는 묶음을 막지 않습니다.
  기본 프린터를 지정하는 동작은 *해당 사용자*의 기본값만 바꾸므로, 네트워크를 전환한다고 권한 승인 창이
  뜨지 않습니다. 일치 실패(Conflict)와 실행 실패(Error)는 서로 다른 상태이며 별도로 보고됩니다.
- **감지는 Profile 단위입니다**: 네트워크 이벤트 반응, 주기적 폴링, 또는 둘 다. 각각 자체적인 변경 후
  지연을 두므로, 불안정한 재연결이 어댑터를 반복해 덮어쓰지 않습니다.
- **상시 동작이 지키는 것은 원하는 상태이지, 명령을 되풀이하는 것이 아닙니다.** 사용 켜진 동작마다
  워커 하나씩이 Active 프로필의 THEN 분기와 함께 시작되고, 새 구성을 적용하기 전에 반드시 멈춥니다.
  터널 점검에서 이미 올라 있음을 확인하면 명령을 하나도 내리지 않고, 워커가 승인 대화 상자를
  띄우지도 않습니다. (`periodic_script`는 확인할 상태가 없는 유일한 종류입니다 — 이 tick의 본체가
  스크립트 실행 그 자체이므로, 설계대로 반복됩니다.)

## 기능

- Profile별 정적 IP / DHCP / 사용자 지정 DNS / IPv6 (automatic, manual, off) / 정적 라우트. THEN과
  ELSE 분기를 따로 가집니다.
- 재읽기 검증과 헬스 모니터링은 적용 단계 안에서 분기별로 설정합니다: 시스템이 실제로 그렇다고
  확인해 줄 때에만 Profile이 "적용됨"을 보고합니다.
- 트레이 팝업 패널: 실제로 사용 중인 네트워크(인터페이스, SSID와 신호 강도, MAC,
  IPv4 / 마스크 / 게이트웨이 / IPv6 / DNS), 다른 활성 인터페이스, VPN 터널, 일치 배지가 있는
  원클릭 Profile 전환, 그리고 모든 진입점 - 설정 · 로그 · DHCP · 점검 · 업데이트 · 종료.
  아이콘을 클릭하면(왼쪽·오른쪽 모두) 열리고, 포커스를 잃으면 접힙니다; 네이티브 트레이 메뉴는 없습니다.
- 모델 전체를 다루는 구성 편집기 — Rules, Conditions, 3A, 라우트, 동작, ELSE, 전역 fallback — 그리고
  엔진의 실시간 판정을 그 자리에 그대로 표시합니다.
- 소프트웨어 설정 창 — 어떤 네트워크에도 해당하지 않는 설정을 모았습니다: 인터페이스 언어, 로그인 시 시작(창을 열 때마다 운영 체제에서 읽으며, 파일 속 사본에서는 읽지 않습니다), `config.json`과 로그의 위치, 로그 보관 일수.
- 될 수 있는 한 암호 없이: macOS는 `sudoers` 허용 목록을 한 번 설치, Windows는 관리자로 한 번 실행,
  Linux는 `sudo -n`. 그것이 안 될 때 NetSense는 실패하는 대신 시스템 인증 대화상자로 폴백합니다.
- 온라인 업그레이드: GitHub Releases를 확인하고 이 플랫폼의 자산을 고릅니다. Homebrew로 설치한
  경우는 `brew upgrade --cask`로 올리고 아무것도 내려받지 않습니다. 그 외에는 자산을 내려받아
  SHA256을 검증한 뒤, 그 Release의 `SHA256SUMS`와 일치할 때에만 설치합니다. 검증할 자료가 없으면
  (`SHA256SUMS`가 없거나, 그 자산이 적혀 있지 않거나, 가져오지 못하면) 설치를 멈추고 Release
  페이지로 안내합니다.
- 다섯 개의 UI 언어 (English, 简体中文, 繁體中文, 日本語, 한국어), 패리티 검증 완료. 기본값은
  English입니다. 세 플랫폼의 모든 창이 이 선택을 따릅니다 — 트레이 패널, 편집기, 설정, 로그 뷰어,
  트레이 툴팁과 네이티브 오류 대화상자까지 포함해. `scripts/validate.py`의 한 항목이 고정 텍스트를
  점검합니다.

## 기술 스택

Tauri v2 + Rust, 시스템 WebView, 프런트엔드는 순수 정적 HTML/CSS/JS (Node 빌드 체인 없음).
Core Engine (detection / conditions / 매칭 / network / automation)은 시스템 명령을 직접 호출하지
않습니다: 플랫폼 관련 처리는 모두 PAL 안의 한 trait 뒤에 있습니다
(`src-tauri/src/platform/{macos,windows,linux}.rs`). 컴파일 때 선택됩니다.

| 플랫폼 | 읽기 | 쓰기 | 권한 상승 |
|--------|------|------|-----------|
| macOS | `CoreWLAN` / `networksetup` / `ipconfig` / `arp` / `system_profiler` (`airport`은 아직 남아 있는 곳에서) | `networksetup` / `route` | sudoers 허용 목록, 아니면 `osascript` 인증 대화상자 |
| Windows | PowerShell CIM (`Get-NetAdapter` …) + `netsh` | `netsh` / `New-NetRoute` | 이미 관리자면 승인 없음, 아니면 UAC |
| Linux | `nmcli` / `ip neigh` | `nmcli con mod` / `ip route` | `sudo -n`, 아니면 `pkexec` |

## 시작하기

### 클라우드에서 빌드 (로컬 의존성 없음)

버전 태그를 푸시하면 CI가 네 개 대상에 대해 한 번에 실행되고
(`windows-x64` / `macos-arm64` / `macos-x64` / `linux-x64`), 설치 파일을 첨부한 **Draft** Release가
열립니다. 네 개 플랫폼의 자산이 모두 갖춰지고 `SHA256SUMS`까지 만들어지면 저절로 공개로 전환됩니다:

```bash
git tag v1.0.0 && git push origin v1.0.0
```

같은 워크플로를 GitHub → Actions → build → Run workflow에서 수동으로 시작할 수도 있습니다. 그러면
날것의 실행 파일만 만들어지고 Release는 생성되지 않습니다.

### 로컬 빌드

```bash
# Rust와 플랫폼별 빌드 의존성이 필요합니다
# (Windows는 MSVC C++ 빌드 도구와 WebView2가 추가로 필요합니다)

# Windows — 스크립트가 먼저 Rust/MSVC/SDK/WebView2/디스크를 점검하고 부족한 것을 알려줍니다
powershell -ExecutionPolicy Bypass -File scripts\build-windows.ps1            # 실행 파일
powershell -ExecutionPolicy Bypass -File scripts\build-windows.ps1 -Installer # msi / nsis

# macOS / Linux
cd src-tauri && cargo build --release   # 실행 파일
cd src-tauri && cargo tauri build       # 설치 파일 (tauri-cli 필요)

# macOS 선택: 암호 불필요한 권한 채널을 설치하면 네트워크를 바꿀 때마다
# 인증을 묻지 않게 됩니다
sh scripts/install-priv-helper.sh
```

### 검증 (컴파일 불필요, 어디서나 실행)

```bash
python3 scripts/validate.py    # 5개 언어의 JSON, i18n 패리티와 자리표시자, 양방향 key 사용 검사,
                               # PAL 경계, 3개 플랫폼 trait 커버리지, tauri.conf.json 필드, 버전 일관성,
                               # 문서와 코드 대조, 그리고 UI 문구가 모두 사전에서 오는지
node scripts/editor-smoke.mjs  # 편집기의 데이터 바인딩을 헤드리스로 실행 (Node가 필요하지만 빌드 단계는 없습니다)
cd src-tauri && cargo test     # 순수 bin crate — `--lib`가 아니라 `cargo test` 사용
```

NetSense는 트레이에 상주합니다: 아이콘을 클릭하면(왼쪽·오른쪽 모두) 패널이 열리고 모든 진입점이
그 패널에 있습니다 (설정 · 로그 폴더 열기 · 현재 네트워크를 DHCP로 · 지금 점검 · 종료; 이 버튼들
위에 사용 중인 NIC와 터널이 표시됩니다). 설정은 두 개의 파일로 나뉩니다. 이 두 종류의 설정은 공통점이 없기 때문입니다.
어떤 네트워크에 어떤 처리를 적용할지는 자동화 설정이며, 단일 `config.json`에 담기고 편집기 창에서 고릅니다.
앱 자체의 동작(인터페이스 언어, 로그 보관 일수, 로그인 시 시작)은 소프트웨어 설정인 `settings.json`에 담기고,
패널의 '설정'이 여는 창에서 고릅니다 — 이곳을 고쳐도 네트워크 설정이 다시 적용되지 않습니다.
`config.json`과 `settings.json`은 모두 사용자별 NetSense 디렉터리에 있습니다 — macOS는
`~/Library/Application Support/NetSense`, Windows는 `%APPDATA%\NetSense`, Linux는
`~/.config/netsense`. 실행 파일과 같은 위치가 아닙니다. 서명된 macOS 번들과 읽기 전용인
`Program Files`에 써서는 안 되기 때문입니다. 로그는 사용자별 NetSense 로그 디렉터리에 있습니다.
`config.example.json`은 완성된 실전 예시입니다.

> Windows에서 네트워크 설정 권한 상승은 **앱 실행당 UAC 승인 1회**로 줄어듭니다(최초 적용 시 상주 승격
> 헬퍼를 띄우고 이후 배치는 이를 거침; 승인을 거부하거나 헬퍼를 쓸 수 없으면 배치별 확인으로 돌아갑니다).
> 관리자로 실행하면 이 1회조차 불필요하며, 이후 권한 채널은 "승인 불필요"로 보고됩니다.

## 구성

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

알아 두면 좋은 관례가 두 가지 있습니다. **빈 문자열은 그 값을 지웁니다**, 그리고 **칸이 비어
있으면 그 값을 건드리지 않습니다**. `dns`도 같은 방식으로 만들어졌습니다 — 편집기의 DNS가 세
선택지인 이유가 이것입니다. "변경하지 않음"은 키를 지우는 것이므로 세 플랫폼 모두 DNS 명령을
내리지 않습니다. "시스템 자동"은 빈 문자열을 쓰므로 이미 설정된 DNS가 지워집니다.
"DNS Servers"는 입력한 값 그대로 적용합니다. 그리고 `run_script`는 `<config 디렉터리>/scripts` 안에 있거나 `allowed_scripts`에
등록된 경로만 실행합니다. 상대 경로는 프로세스가 시작된 위치가 아니라 config.json이 있는 디렉터리를
기준으로 해석됩니다.

`DEVELOPMENT.md`에 모델, 엔진 파이프라인, 플랫폼별 함정이 자세하게 정리되어 있습니다.

## 라이선스

MIT
