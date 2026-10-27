# Phase 07: 스트림 출력

## 목적

모델 SSE와 CLI가 텍스트 델타를 런이 끝난 뒤에 한꺼번에 그리지 않게 한다.

## 선행 조건

Phase 06.

## Tasks

### TASK-014: 스트림 출력

- 목표: Responses SSE를 조각으로 파싱하고, REPL/`-p`가 `TextDelta`를 도착하는 대로 stdout에 쓴다
- 관련 REQ: REQ-3.22 REQ-3.23 REQ-3.6
- 결정 사항: `Model::stream`은 이벤트를 live 채널로 먼저 보낸다. `-p`는 텍스트만 스트리밍하고 도구 줄은 쓰지 않는다. ratatui는 여전히 없다
- 변경 대상(힌트): `keel-openai` SSE 파서, `keel-core` Model trait/Agent, `keel-cli` 이벤트 구독
- 하지 말 것: 마크다운 렌더, Steer, 토큰 단위 UI 장식
- 선행 조건: TASK-013
- 검증: 통합
- 검증 범위: task-local
- 검증 명령(힌트): `cargo test -p keel-core -p keel-openai -p keel-cli`
- 산출물: 델타가 런 종료 전에 Event로 나오고, `-p`/REPL이 그걸 바로 찍는다

## 구현 메모

지금은 `resp.text()`로 본문을 모은 뒤 CLI가 `run()` 종료 후 `try_recv`한다. 두 층을 같이 고친다.

## 세부 실행 체크리스트

- 픽스처 SSE를 조각으로 넣어도 델타 순서가 같은지
- 느린 모델이 첫 `TextDelta`를 `run()` 반환 전에 내는지

## 하지 말 것

전체 화면 TUI를 넣지 않는다.

## 검증 포인트

- `cargo test -p keel-core -p keel-openai -p keel-cli`

## Phase 완료 조건

TASK-014 완료.
