# Phase 06: MCP와 회귀

## 목적

MCP 서버 도구를 같은 레지스트리에 넣고, workspace 테스트를 한 번 돌린다.

## 선행 조건

Phase 05.

## Tasks

### TASK-012: MCP

- 목표: stdio/HTTP 서버를 등록하고 `server__tool`로 부르며, 한 서버 실패가 루프를 죽이지 않게 한다
- 관련 REQ: REQ-3.18 REQ-3.24 INV-9
- 결정 사항: External은 항상 승인. 연결 10초. 처음 보는 프로젝트 config는 MCP 기동 전 확인. 비밀은 `${VAR}`
- 변경 대상(힌트): `crates/keel-mcp`, config 로더
- 하지 말 것: hosted MCP, resources/prompts, `server.tool` 이름
- 선행 조건: TASK-011
- 검증: 통합
- 검증 범위: task-local
- 검증 명령(힌트): `cargo test -p keel-mcp`
- 산출물: `keel-mcp`

### TASK-013: 최종 회귀

- 목표: workspace 테스트와 ScriptedModel 종단이 통과한다
- 관련 REQ: REQ-4.3
- 결정 사항: 이번 계획 원인 실패는 고치고 다시 돌린다. 기존/환경 실패는 숨기지 않는다
- 변경 대상(힌트): 실패 수정만
- 하지 말 것: 범위 밖 리팩터로 빨갛게 만들지 않는다
- 선행 조건: TASK-012
- 검증: 회귀
- 검증 범위: final-regression
- 검증 명령(힌트): `cargo test --workspace`
- 산출물: 통과 또는 명시된 limitation

## 구현 메모

가짜 MCP 서버로 list/call/다운을 검증한다. 라이브 검색·실서버는 `KEEL_LIVE` 또는 limitation.

## 세부 실행 체크리스트

- 서버 하나가 죽어도 다른 도구 호출이 사는지
- `cargo test --workspace`가 이번 변경 원인으로 실패하지 않는지

## 하지 말 것

- MCP 세션을 커널 상태기계로 승격하지 않는다

## 검증 포인트

- `cargo test --workspace`

## Phase 완료 조건

TASK-012, TASK-013 완료.
