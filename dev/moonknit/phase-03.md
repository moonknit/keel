# Phase 03: 대화

## 목적

OpenAI와 읽기 도구로 실제로 대화하고, CLI에서 session을 열고 `-p`로 끝낼 수 있다.

## 선행 조건

Phase 02. api-contract의 실측 SSE 이름을 Model 구현에 쓴다.

## Tasks

### TASK-006: OpenAI Model 구현

- 목표: `LLM_*`로 Responses 스트림을 받아 Item/Event로 바꾸고 raw를 왕복한다
- 관련 REQ: REQ-3.6 REQ-3.7 REQ-3.8 REQ-4.4 INV-2 INV-3
- 결정 사항: `SMALL_LLM_*` 미사용. 키 마스킹. 401 즉시 실패, 429/5xx 3회, overflow 1회 재시도
- 변경 대상(힌트): `crates/keel-openai`
- 하지 말 것: Chat Completions, `previous_response_id`
- 선행 조건: TASK-002, TASK-003
- 검증: 단위
- 검증 범위: task-local
- 검증 명령(힌트): `cargo test -p keel-openai`
- 산출물: Model impl

### TASK-007: 읽기 도구

- 목표: `read` `grep` `glob`이 cwd와 허용 루트만 연다
- 관련 REQ: REQ-3.15 REQ-3.16 INV-7
- 결정 사항: 스킬 전용 read 없음
- 변경 대상(힌트): `crates/keel-tools`
- 하지 말 것: 쓰기/bash를 이 Task에 넣지 않는다 (Phase 04)
- 선행 조건: TASK-003
- 검증: 단위
- 검증 범위: task-local
- 검증 명령(힌트): `cargo test -p keel-tools`
- 산출물: 읽기 도구
- 비고: TASK-006과 병렬 가능

### TASK-008: CLI 세션과 대화

- 목표: 명령·REPL·`-p`가 Event를 그리고 session을 연다
- 관련 REQ: REQ-3.21 REQ-3.22 REQ-3.23 REQ-3.5
- 결정 사항: 마크다운 렌더/ratatui/Steer 없음. `-p` 종료 코드 0/1/2/130
- 변경 대상(힌트): `crates/keel-cli`
- 하지 말 것: 승인 UI와 bash를 이 Task에서 끝내지 않는다
- 선행 조건: TASK-005, TASK-006, TASK-007
- 검증: 통합
- 검증 범위: task-local
- 검증 명령(힌트): ScriptedModel로 `keel -p` 테스트
- 산출물: `keel` 바이너리

## 구현 메모

web_search는 이 Phase에서 Model 요청에 붙이고, 인용 URL은 최종 텍스트 아래에 그린다. 승인 없는 읽기만 REPL에 연다.

## 세부 실행 체크리스트

- `-p`가 최종 텍스트만 stdout에 쓰는지
- 키 없이 실행하면 키 값을 출력하지 않는지

## 하지 말 것

- CLI에 루프를 복제하지 않는다. core Event만 구독한다

## 검증 포인트

- ScriptedModel `-p` 통과
- `cargo test -p keel-openai -p keel-tools -p keel-cli`

## Phase 완료 조건

TASK-006–008 완료. 읽기 도구와 모델로 한 chat을 끝낼 수 있다.
