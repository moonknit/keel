# Phase 01: workspace와 Responses 계약

## 목적

컴파일되는 crate 뼈대를 만들고, OpenAI Responses의 무손실 왕복을 실측한다. 이 계약이 실패하면 이후 저장 형식을 다시 정한다.

## 선행 조건

- SRS와 `.env.example`이 있다. 라이브 확인에는 로컬 `.env`의 `OPENAI_API_KEY`와 `LLM_*`가 필요하다. 없어도 픽스처 파싱은 진행한다.

## Tasks

### TASK-001: workspace와 설정 뼈대

- 목표: 7 crate가 빌드되고 core 의존 금지가 컴파일러로 막히게 한다
- 관련 REQ: REQ-3.8 REQ-4.1 INV-1 INV-8
- 결정 사항: `async-openai`를 workspace 의존에 넣지 않는다. 키를 예제에 넣지 않는다
- 변경 대상(힌트): `Cargo.toml`, `crates/keel-{core,store,openai,tools,mcp,skills,cli}/`
- 하지 말 것: 루프·도구 구현을 이 Task에 섞지 않는다
- 선행 조건: -
- 검증: 단위
- 검증 범위: task-local
- 검증 명령(힌트): `cargo test -p keel-core`
- 산출물: 빈 라이브러리와 dotenv 로더

### TASK-002: Responses spike

- 목표: `store: false` + function call + web_search + reasoning이 SQLite JSON 왕복 후에도 성공하는지 확인하고 SSE·함수 이름을 실측한다
- 관련 REQ: REQ-3.6 REQ-3.7 REQ-3.9
- 결정 사항: typed SDK 채택은 무손실 확인 전 금지. 실측 결과는 `docs/technical/api-contract.md`에 반영
- 변경 대상(힌트): `crates/keel-openai`, `docs/technical/api-contract.md`, `dev/moonknit/impact-notes/`
- 하지 말 것: 루프 전체에 클라이언트를 붙이지 않는다
- 선행 조건: TASK-001
- 검증: 통합
- 검증 범위: task-local
- 검증 명령(힌트): `KEEL_LIVE=1 cargo test -p keel-openai -- spike`
- 산출물: spike 테스트, 갱신된 계약, RV-003/004/005 판정

## 구현 메모

모델 호출은 `LLM_BASE_URL` + `LLM_MODEL`. 값은 로그에 남기지 않는다. 라이브가 안 되면 픽스처만 고정하고 `docs/testing/test-limitations.md`에 이유를 적는다.

## 세부 실행 체크리스트

- crate 그래프가 INV-1을 깨면 다음 Phase로 가지 않는다
- spike가 raw 왕복에 실패하면 `message.raw` 전략을 다시 합의한다

## 하지 말 것

- Chat Completions로 spike를 대체하지 않는다
- 서버 `previous_response_id`에 기대지 않는다

## 검증 포인트

- `cargo test -p keel-core` 통과
- api-contract에 실측 SSE 이름이 채워짐

## Phase 완료 조건

TASK-001, TASK-002 완료. 라이브 실패는 limitation으로 명시돼 있고 파싱 계약은 남아 있다.
