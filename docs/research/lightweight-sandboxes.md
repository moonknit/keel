# 경량 실행 샌드박스 조사

조사일 2026-10-27. keel의 `bash`(그리고 그 자식 프로세스)를 OS가 강제하는 경계 안에 둘 수 있는지 본다. 권한 확인·위험 패턴 휴리스틱은 유지하되, 그것만으로는 격리가 아니다.

현재 구현: cwd 검사 + allowlist + 타임아웃 + 출력 상한. 설계는 `Sandbox` trait만 예약하고 Docker는 v2로 미뤘다 (RV-009).

## 무엇을 막아야 하는가

keel은 사용자 기기에서, 그 cwd의 파일을 다룬다. 위협은 멀티테넌트 공격자가 아니라 **모델이 만든 명령**이다.

막아야 할 것.

- 워크스페이스 밖 쓰기 (`rm`, `~/.ssh` 덮어쓰기)
- 자격 증명 읽기 (`~/.ssh`, `~/.aws`, `~/.cargo/credentials`)
- 원치 않는 네트워크 (`curl | sh`, `git push`)
- 자식 프로세스까지 같은 규칙 (휴리스틱 문자열 검사는 여기 실패)

허용해야 할 것.

- `cargo test`처럼 로컬 툴체인 사용
- 워크스페이스 읽기/쓰기
- 스킬 루트 읽기
- 시작이 수 ms~수십 ms. Docker pull/boot는 이 루프에 안 맞음

## 계층

| 계층 | 대표 | 시작 | 커널 공유 | macOS | Linux | keel에 |
|---|---|---|---|---|---|---|
| 프로세스 래퍼 | cwd/타임아웃 (지금) | 0 | 예 | 예 | 예 | 이미 있음. 격리 아님 |
| OS jail | Seatbelt, bubblewrap, Landlock, nsjail | 수 ms | 예 | Seatbelt | bwrap/Landlock | **다음 단계로 적합** |
| 컨테이너 | Docker, Podman | 초 단위 + 이미지 | 예(보통) | 무거움 | 예 | v2 후보. 지금 아님 |
| microVM | Firecracker, microsandbox | ~50–200 ms + 이미지 | 아니오 | libkrun 계열 | KVM | 무인·강한 격리 때 |
| 사용자 모드 커널 | gVisor | 컨테이너급 | 부분 | 아니오 | 예 | 클라우드용 |
| Wasm | wasmtime | 매우 빠름 | 해당 없음 | 예 | 예 | `cargo`/`bash` 불가 |

공유 커널 jail은 커널 0-day까지 막지 않는다. 로컬 에이전트에는 보통 충분하다. 서로 불신하는 테넌트면 microVM이 맞다.

## 로컬 에이전트가 실제로 쓰는 것

로컬 에이전트 업계 기본은 **macOS Seatbelt + Linux bubblewrap**이다. 컨테이너를 기본으로 두지 않는다.

- Claude Code Bash 샌드박스: macOS Seatbelt, Linux/WSL2 bubblewrap(+ socat 네트워크 프록시). 파일 도구·MCP는 샌드박스 밖. [문서](https://code.claude.com/docs/en/sandboxing). 같은 스택을 [`@anthropic-ai/sandbox-runtime`](https://github.com/anthropics/sandbox-runtime)으로 공개.
- OpenAI Codex: Linux에서 Landlock을 기본으로 고른 사례가 2026 비교 글에 나온다.
- Cusp, Helix local-sandbox, SuperQode: 같은 Seatbelt/`bwrap` 패턴. 쓰기=워크스페이스, 네트워크=기본 차단.

## 도구별

### Seatbelt / `sandbox-exec` (macOS)

- 설치 없음. 프로파일을 만들어 `sandbox-exec -p '…' -- cmd`.
- 쓰기와 네트워크는 잘 막는다. 읽기 차단은 시스템 경로를 하나하나 열어야 해서 허술한 구현이 많다 (Helix: 기본 프로필은 광역 읽기 허용).
- Apple은 `sandbox-exec`를 비공식/레거시로 두지만, 로컬 에이전트는 여전히 이걸 쓴다.
- keel 개발 머신(darwin)에서 바로 쓸 수 있다.

### bubblewrap (`bwrap`) (Linux)

- Flatpak이 쓰는 비특권 user-namespace 래퍼. 단일 바이너리, 배포판 패키지, MIT.
- `--bind` cwd, `--ro-bind` 툴체인, `--unshare-net`, `--die-with-parent`. 시작은 fork+exec 수준.
- 마운트하지 않은 경로는 보이지 않아서 읽기 격리가 Seatbelt보다 쉽다.
- 컨테이너 안·user namespace가 꺼진 CI에서는 실패. 탐침 후 폴백이 필요하다.
- Claude Code Linux 경로.

### Landlock (Linux 5.13+, 네트워크는 6.7+)

- 프로세스가 스스로 규칙셋을 걸고 되돌리지 않는다. 루트·이미지 불필요.
- Codex류가 선호. 툴링(`island`)은 아직 젊다.
- 네임스페이스/바인드 마운트는 안 한다. 숨기기보다 접근 거부.
- 그린필드 Linux면 후보. macOS는 없음.

### nsjail

- 네임스페이스 + seccomp + rlimit + cgroup. CTF·멀티테넌트에 강함.
- 설정이 크고, 로컬 CLI 한 줄 래핑에는 bwrap가 더 작다.

### firejail

- 데스크톱 앱 격리. 표면이 넓다. 에이전트 기본값으로 추천하지 않음.

### Docker / Podman

- 이미지·데몬·볼륨. `cargo test`마다 워크스페이스를 맞추는 비용이 큼.
- 이미 RV-009. 강한 재현/CI용 뒤로 둔다.

### microsandbox

- Rust SDK, OCI 이미지, Linux/macOS/Windows 주장, 부팅 <100ms 주장, 하드웨어 격리.
- KVM/libkrun/이미지 캐시가 필요하다. keel의 “호스트에서 cargo 그대로”와는 결이 다르다.
- 게스트에 툴체인을 넣거나 이미지를 키워야 한다.

### Firecracker / gVisor / Kata

- 클라우드·멀티테넌트. 로컬 REPL 한 명령에는 과함.

### Wasm

- 계산 스니펫용. 호스트 `bash`/`cargo`를 대체하지 못한다.

### Sandlock (연구, 2026)

- Landlock + seccomp. 시작 ~5ms, Docker보다 수십 배 빠르다고 주장. 프로덕션 기본값으로 쓰기엔 이름·생태계가 이르다.

## keel에 대한 판정

1. **다음 구현은 OS jail.** `Sandbox` trait을 채운다. macOS `sandbox-exec`, Linux `bwrap`. 대상은 `bash`와 그 자식. `read`/`write`/`edit`는 지금처럼 프로세스 안 경로 검사.
2. **Docker는 기본이 아니다.** 이미 미룬 결정과 같다.
3. **microsandbox/Firecracker는 더 뒤.** 호스트 툴체인과 떨어지거나 이미지가 생긴다.
4. **없어도 동작, 있으면 강제.** 바이너리/커널이 없으면 지금처럼 휴리스틱 + 승인으로 폴백할지, 실행을 거절할지는 제품 결정. Claude Code는 Linux에서 의존이 없으면 패널로 알린다.
5. **권한 게이트와 겹친다.** 샌드박스 안에서는 워크스페이스 `cargo test`를 자동 허용할 수 있다. 샌드박스 밖·네트워크·cwd 밖은 지금처럼 묻는다.

## 출처

- [Claude Code sandboxing](https://code.claude.com/docs/en/sandboxing)
- [Anthropic sandbox-runtime](https://github.com/anthropics/sandbox-runtime) (Apache-2.0)
- [Choosing a sandbox 2026](https://tanayshah.dev/blog/choosing-agent-sandbox-2026/)
- [ai-jail sandbox alternatives](https://github.com/akitaonrails/ai-jail/blob/master/docs/sandbox-alternatives.md)
- [Cusp sandbox](https://cusp.run/docs/concepts/sandbox)
- [Helix local-sandbox](https://agents.open-source.onhelix.ai/workspaces/local-sandbox.html)
- [microsandbox](https://github.com/superradcompany/microsandbox)
- [Sandlock](https://arxiv.org/html/2605.26298v1)
