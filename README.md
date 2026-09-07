# SnipChord

**macOS-style screenshots for Linux.**

Linux에서도 macOS 스크린샷처럼 빠르고 명확한 캡처 경험을 만드는 독립 데스크톱 앱.
macOS와 동일한 구현이나 완전한 기능 복제를 주장하지 않습니다. 현재는 Rust로 구현한 X11 전용 0.1 프로토타입입니다.

## 제품의 중심

- 기다림이 짧은 영역 선택과 안정적인 시각적 피드백
- 화면을 어둡게 하지 않고 캡처 시작 시점의 화면 위에 선택 테두리 표시
- 선명한 흰 테두리, 네 모서리 표시, 픽셀 크기 표시
- 마우스를 놓으면 즉시 캡처. 드래그 전에 `Space`를 누르면 포인터 아래 창을 선택하고,
  드래그 중 `Space`를 누르면 선택 영역을 이동
- 흰 선과 어두운 외곽선을 함께 그려 밝은 화면과 어두운 화면 모두에서 선택 범위를 표시
- `Esc` 또는 우클릭으로 취소; 단순 클릭은 전체 창을 뜻하지 않음
- 완료 후 모니터 오른쪽 아래에 4초 동안 작은 이미지 전용 thumbnail 표시
- thumbnail을 클릭하면 저장 모드에서는 저장된 PNG를, 클립보드 모드에서는 임시 PNG를 기본 이미지 뷰어로 열며 기존 클립보드와 저장 동작은 그대로 유지
- 기본은 클립보드 복사. 명시적 파일 저장과 저장 폴더 설정도 지원
- 단일 Rust 프로세스가 X11 연결과 클립보드 소유권을 유지하여 반복 초기화를 줄임

Wine 전용 앱이 아닙니다. 클립보드에 PNG와 BMP를 함께 제공하여 Linux 앱과 Wine 앱에서
사용할 수 있도록 설계했습니다. Wine 카카오톡의 실제 붙여넣기 호환성은 사용 환경에서 검증해야 합니다.

## 현재 지원 범위

| 항목 | 0.1 상태 |
| --- | --- |
| Linux X11 영역 / 전체 데스크톱 캡처 | Rust 구현 (X11); 최종 통합 검증 완료 |
| 여러 모니터를 가로지르는 영역 선택 | X11 전체 화면 좌표 기반 |
| 드래그 전 `Space` | 포인터 아래 X11 창 선택 |
| 드래그 중 `Space` / `Esc` / 크기 표시 | 선택 영역 이동 / 취소 / 크기 표시 |
| 이미지 thumbnail | 4초 표시, 최대 220×140, 둥근 모서리, 5px 테두리, 클릭 시 기본 이미지 뷰어로 열기 |
| 클립보드 / PNG 저장 | `--clipboard` 또는 `--save`로 목적지 선택 |
| 저장 폴더 | `--save-dir PATH`로 설정; 기본값은 XDG Pictures/Screenshots |
| GNOME 단축키 | `Ctrl+Alt+Shift+4`/`Alt+Shift+4` 영역, `Ctrl+Alt+Shift+3`/`Alt+Shift+3` 전체 |
| Wayland | 미지원: 포털 또는 데스크톱별 통합 필요 |
| 캡처 도구 막대 | 후속 작업 |
| 주석 편집 / 녹화 / 스크롤 캡처 | 미구현 |
| macOS의 미리보기 드래그로 다른 앱에 전달 | 미구현 |
| 혼합 배율 모니터 | 별도 실기기 검증 필요 |

## 개발 환경

Rust 1.85 이상과 Cargo로 빌드합니다. 실행 시 Python, GTK, 외부 캡처 명령은 필요하지 않습니다.

```sh
cargo build --release --locked
./bin/snipchord --daemon       # 선택 사항: 미리 실행
./bin/snipchord --region       # 또는 인자 없이 실행
./bin/snipchord --fullscreen
./bin/snipchord --region --clipboard
./bin/snipchord --region --save
./bin/snipchord --fullscreen --clipboard
./bin/snipchord --fullscreen --save
./bin/snipchord --save-dir ~/Downloads
./bin/snipchord --preferences
./bin/snipchord --quit
```

`bin/snipchord`는 `target/release/snipchord`를 실행하는 개발용 실행기입니다. Rust 단위 테스트는
다음처럼 실행합니다.

```sh
cargo test --locked
```

`--demo`는 실제 화면 대신 단색의 가상 데스크톱으로 선택 UI를 테스트합니다.
완료 시 thumbnail만 표시하고 클립보드 및 자동 저장은 변경하지 않습니다.

```sh
./bin/snipchord --demo
```

앱은 캡처 후에도 Rust 프로세스와 X11 연결을 유지합니다. 이는 다음 캡처의 초기화를 줄이고
X11 클립보드 이미지의 소유권을 유지하기 위해서입니다. `--quit` 이후 이미지가 유지되는지는
클립보드 관리자에 달려 있습니다.

## 설치

```sh
cargo build --release --locked
python3 tools/install.py
# 네 개의 GNOME 단축키를 설치하려면:
python3 tools/install.py --shortcuts
# 로그인할 때 준비 상태로 실행하려면:
python3 tools/install.py --autostart
```

`tools/install.py`는 빌드 후 Rust 릴리스 바이너리와 아이콘을 사용자 경로에 복사하는 설치
도구입니다. 기본 실행은 단축키를 변경하지 않으며, `--shortcuts`를 지정할 때만 SnipChord의
네 개 항목을 설치하거나 갱신합니다. 관련 없는 GNOME 단축키는 보존합니다. 실행 중에는 Python이
필요하지 않습니다.

최종 Rust 바이너리는 기본 사용자 설치 경로에 반영했고, `--shortcuts`로 네 개의 SnipChord
GNOME 단축키를 설정했습니다. 관련 없는 사용자 단축키는 보존합니다. 저장 위치는
`--save-dir /home/shane/Downloads`로 설정했습니다.

설치 후 앱 목록에서 SnipChord를 실행할 수 있습니다. 설치 프로그램은 기존 캡처 단축키를 변경하지 않습니다.
GNOME 사용자 지정 단축키의 명령으로 아래를 지정할 수 있습니다.

```text
/home/<user>/.local/bin/snipchord --region
```

기존 캡처 단축키를 재사용하려면 해당 단축키의 명령을 위 실행기로 변경합니다.
이 앱은 이전 scrot 기반 캡처 스크립트나 별도 클립보드 감시 서비스 없이 동작합니다.
기본 설치는 단축키를 변경하지 않으며, `--shortcuts`를 지정할 때만 네 개의 SnipChord 항목을
설정합니다. 관련 없는 단축키와 설정은 보존합니다. 로그인 자동 실행은 `--autostart`를 명시했을
때만 추가됩니다.

저장 단축키는 영역 클립보드(`Ctrl+Alt+Shift+4`), 영역 파일(`Alt+Shift+4`), 전체 데스크톱
클립보드(`Ctrl+Alt+Shift+3`), 전체 데스크톱 파일(`Alt+Shift+3`) 순서로 설치됩니다.
파일 저장 위치를 Downloads로 쓰려면 다음 명령을 한 번 실행합니다.

```sh
/home/<user>/.local/bin/snipchord --save-dir ~/Downloads
```

이 설정은 `--save`와 자동 저장에 적용되며, 설정 파일에 저장됩니다.

설정은 `$XDG_CONFIG_HOME/snipchord/settings.json` (기본 `~/.config/snipchord/settings.json`),
저장 이미지는 기본적으로 XDG Pictures 폴더 아래 `Screenshots/`에 위치합니다. `--save-dir PATH`를
설정하면 지정한 폴더를 사용합니다.

설치 경로:

- `~/.local/bin/snipchord`: 설치한 Rust 실행 파일
- `~/.local/share/snipchord/`: 아이콘과 설치 관리 표식
- `~/.local/share/applications/io.github.shane.snipchord.desktop`: 앱 목록 항목
- `~/.config/autostart/io.github.shane.snipchord.desktop`: 선택적 로그인 자동 시작

제거하려면 앱을 종료한 뒤 위 설치 항목만 제거합니다. 설정 및 저장 이미지는 자동 삭제하지 않습니다.

## 구현

`src/main.rs`의 Rust 바이너리가 명령행과 상주 프로세스를 담당하고, `app`, `x11`, `ui`,
`window_capture`, `server_capture`, `image`, `clipboard`, `geometry`, `settings`, `storage` 모듈이 기능을 나눠 가집니다. `x11rb`로 X11 루트
화면과 창 정보를 직접 다루고 네이티브 X11 창으로 선택 오버레이, thumbnail, 환경설정을 표시합니다. 화면
버퍼와 선택 영역은 Rust 메모리로 처리하며, PNG 인코딩에는 `png`, 설정 파일에는
`serde_json`을 사용합니다. GTK 또는 Python 런타임은 필요하지 않습니다.

영역 선택에서는 X11 서버 쪽 pixmap 하나에 화면을 고정하고, 사용자가 선택을 확정한 영역만
클라이언트로 읽습니다. 화면을 어둡게 만드는 Render 처리와 별도 배경 복사본은 사용하지 않습니다.
서버 캡처를 준비할 수 없는 경우에는 기존 클라이언트 버퍼 경로로 전환합니다. 창 선택은 포인터 아래 X11 창의 root 좌표를 확인하고 Composite 경로를
우선 사용하며, Composite를 사용할 수 없거나 창의 알파 경계처럼 직접 합성이 적합하지 않은
경우에는 캡처 시작 때 고정한 화면의 visible crop으로 보완합니다. 단축키가 키보드를 잠시 잡고
있으면 선택 화면을 버리지 않고 입력 권한을 다시 시도합니다. 측정 방법은
[지연 측정](docs/latency-measurement.md), 실험 결과는 [개선 실험](docs/latency-experiments.md)에 남깁니다.

클립보드는 X11 선택 소유권을 직접 관리하고 PNG와 BMP 대상을 함께 제공합니다. 외부 scrot /
ImageMagick / xclip 프로세스가 필요하지 않습니다. 클립보드 모드에서는 기본 뷰어를 빠르게 열 수 있도록 임시 PNG를 백그라운드에서 미리 준비합니다.
임시 이미지는 `$XDG_CACHE_HOME/snipchord/previews` (기본 `~/.cache/snipchord/previews`)에
최근 5개만 유지하며 앱을 다시 시작해도 이 기준으로 정리합니다. 직접 저장한 스크린샷은 정리하지 않습니다.

화면 데이터는 메모리와 로컬 클립보드에서 처리합니다. 네트워크 통신, 업로드, 분석 수집은 없습니다.
로그에는 선택 화면 준비 시간과 캡처 크기만 남기며 픽셀 데이터나 파일 내용은 출력하지 않습니다.
