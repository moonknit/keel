# Phase 02: 루프와 저장

## 목적

커널이 ScriptedModel과 SQLite만으로 한 런을 닫고, 크래시 복구와 chat 생략이 검증된다.

## 선행 조건

Phase 01 workspace. spike가 저장 형식을 바꾸면 그 결정을 이 Phase 스키마에 반영한 뒤 시작한다.

## Tasks

### TASK-003: 커널 타입과 ReAct 루프

- 목표: Item/Event/trait과 루프가 텍스트 종료·직렬 tool·max_turns·취소 합성을 처리한다
- 관련 REQ: REQ-3.1 REQ-3.2 REQ-3.3 REQ-3.4 REQ-3.5 INV-4
- 결정 사항: v1 Inbox는 User만. tool은 직렬. Steer 없음
- 변경 대상(힌트): `crates/keel-core`
- 하지 말 것: 터미널, HTTP, rusqlite를 core에 넣지 않는다
- 선행 조건: TASK-001
- 검증: 단위
- 검증 범위: task-local
- 검증 명령(힌트): `cargo test -p keel-core`
- 산출물: 루프와 ScriptedModel 테스트

### TASK-004: MemoryStore와 context 조립

- 목표: 한도 근처에서 오래된 chat만 빼고 진행 중 chat은 그대로 둔다
- 관련 REQ: REQ-3.12 REQ-3.14 INV-5 INV-6
- 결정 사항: 요약 호출 없음. Notice는 저장하지 않음
- 변경 대상(힌트): `crates/keel-core`
- 하지 말 것: `/compact`, 메시지 중간 자르기
- 선행 조건: TASK-003
- 검증: 단위
- 검증 범위: task-local
- 검증 명령(힌트): `cargo test -p keel-core assembler`
- 산출물: ContextAssembler, MemoryStore

### TASK-005: SQLite Store와 복구

- 목표: 스키마 v1 write-through, running→interrupted, lock_pid
- 관련 REQ: REQ-3.11 REQ-3.13 REQ-5.1 REQ-5.2 INV-10
- 결정 사항: 끊긴 tool을 재실행하지 않고 interrupted result를 합성한다
- 변경 대상(힌트): `crates/keel-store`
- 하지 말 것: FTS, parent_id, allowlist 컬럼
- 선행 조건: TASK-004
- 검증: 통합
- 검증 범위: task-local
- 검증 명령(힌트): `cargo test -p keel-store`
- 산출물: `keel-store`

## 구현 메모

rusqlite는 전용 스레드. `user_version=1`. 식별자는 ULID, 시각은 unix ms.

## 세부 실행 체크리스트

- 짝 없는 tool_call을 연 뒤 모델에 보낼 입력이 INV-4를 만족하는지 확인
- 생략 Notice가 DB에 안 남는지 확인

## 하지 말 것

- JSONL 세션 파일로 SQLite를 대체하지 않는다

## 검증 포인트

- `cargo test -p keel-core -p keel-store`

## Phase 완료 조건

TASK-003–005 완료. fake만으로 루프 전 분기와 복구가 통과한다.
