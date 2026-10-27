# keel v1 개발 계획

## 개요

가벼운 Rust 에이전트 런타임 v1을 구현한다. 턴 루프가 커널이고, OpenAI Responses로 ReAct를 돌리며, 코딩 도구·hosted 검색·MCP·`SKILL.md`를 CLI에서 쓴다. 대화는 로컬 SQLite session이다.

요구 정본: [docs/planning/srs.md](../../docs/planning/srs.md).
설계: [docs/design/runtime.md](../../docs/design/runtime.md).
근거: [docs/reference/261027-140721-blueprint.md](../../docs/reference/261027-140721-blueprint.md).

## 범위

한다: SRS REQ-3.1–3.24, REQ-4.*, REQ-5.*, crate 7개, `keel` CLI와 `-p`.

하지 않는다: 채널, 대시보드, 크론, 서브에이전트, Docker, ratatui, Steer, 요약 compaction, 다른 LLM 제공자, `SMALL_LLM_*`, hosted shell/code_interpreter/hosted MCP, `--yolo`.
한다(추가): `bash` OS jail. macOS Seatbelt, Linux `bwrap`. 없으면 승인 후 호스트.

## 가정 / 결정 사항

- Act는 native tool call. 텍스트 ReAct 파서 없음.
- 대화 상태는 `store: false`. `message.raw`를 손대지 않고 재전송.
- 모델 식별자·한도·키는 환경 변수(`LLM_*`, `OPENAI_API_KEY`). 설정 파일에 비밀을 두지 않음.
- `async-openai`는 쓰지 않는다. Responses는 `reqwest` + `Value` (TASK-002에서 확정).
- MCP는 `rmcp` 대신 newline JSON-RPC stdio 클라이언트 (TASK-012).
- context 축소는 끝난 chat 통째 생략. 진행 중 chat은 자르지 않음.
- 스킬 읽기 범위는 session. MCP 이름은 `server__tool`.
- 위험 패턴 감지는 휴리스틱이지 보안 경계가 아님.

## 실행 규칙

- 한 번에 Task 하나. 상태는 `dev-state.md`만 권위.
- snapshot을 고치지 않는다. 비밀값을 문서·로그 예제에 쓰지 않는다.
- 테스트 이름에 관련 `REQ-*` / `INV-*`를 적는다.
- 실행 절차의 일반 규칙은 dev-planning `dev-workflow` / `coding-discipline`을 따른다.

## 파일 배치 규칙

```text
crates/keel-core/      루프, trait, Event
crates/keel-store/     SQLite
crates/keel-openai/    Responses 클라이언트
crates/keel-tools/     bash read write edit grep glob, 권한
crates/keel-mcp/       MCP → Tool
crates/keel-skills/    SKILL.md, AGENTS.md
crates/keel-cli/       clap, rustyline, 바이너리 keel
```

테스트는 해당 crate `tests/` 또는 `#[cfg(test)]`. 라이브 API는 `KEEL_LIVE=1` 가드. 픽스처 SSE는 `crates/keel-openai/tests/fixtures/`.

## 플로우

```yaml
flow:
  - step: phase-01
    next: phase-02
  - step: phase-02
    next: phase-03
  - step: phase-03
    next: phase-04
  - step: phase-04
    next: phase-05
  - step: phase-05
    next: phase-06
  - step: phase-06
    next: phase-07
  - step: phase-07
    next: DONE
```

## Phase 목록

### Phase 1 → `phase-01.md`

- 목적: workspace와 Responses 계약 실측(M0)
- 포함 Task: TASK-001, TASK-002
- 의존성: -

### Phase 2 → `phase-02.md`

- 목적: 루프와 저장이 fake/SQLite로 닫힘(M1)
- 포함 Task: TASK-003, TASK-004, TASK-005
- 의존성: Phase 1의 workspace. spike 실패 시 저장 형식부터 재검토

### Phase 3 → `phase-03.md`

- 목적: 실제 모델과 읽기 도구로 대화 가능(M2)
- 포함 Task: TASK-006, TASK-007, TASK-008
- 의존성: Phase 2. TASK-006과 TASK-007은 병렬 가능

### Phase 4 → `phase-04.md`

- 목적: 코드를 고치고 실행하고 막을 수 있음(M3)
- 포함 Task: TASK-009, TASK-010
- 의존성: Phase 3

### Phase 5 → `phase-05.md`

- 목적: 스킬과 AGENTS.md(M4)
- 포함 Task: TASK-011
- 의존성: Phase 4

### Phase 6 → `phase-06.md`

- 목적: MCP와 workspace 회귀(M5)
- 포함 Task: TASK-012, TASK-013
- 의존성: Phase 5

### Phase 7 → `phase-07.md`

- 목적: 호스트가 텍스트 델타를 도착하는 대로 그린다
- 포함 Task: TASK-014
- 의존성: Phase 6

## 전체 검증 전략

- 일반 Task: 해당 crate `cargo test -p …` (task-local).
- 라이브 OpenAI: `KEEL_LIVE=1`일 때만. 없으면 limitation 기록.
- 최종: TASK-013에서 `cargo test --workspace`.
- canonical full-test: `cargo test --workspace`.

## 오픈 이슈 / 보류 항목

- RV-003/004/005: Phase 1 spike에서 닫는다.
- RV-006 요약 compaction, RV-001 Steer, RV-015 ratatui: v2.
- RV-016 프로젝트 config 신뢰: TASK-012에서 첫 확인 UX만 넣고 정책은 유지.
- `SMALL_LLM_*`: v1 미사용.
