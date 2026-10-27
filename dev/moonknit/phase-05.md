# Phase 05: 스킬

## 목적

카탈로그만 프롬프트에 두고, 본문과 첨부 파일은 필요할 때 `skill`/`read`/`bash`로 연다. `AGENTS.md`를 붙인다.

## 선행 조건

Phase 04. `read`/`bash`와 session 저장이 있다.

## Tasks

### TASK-011: 스킬과 AGENTS.md

- 목표: 탐색·카탈로그·`skill` 도구·session read_roots 복원·AGENTS.md·`/skill:` `/skills`
- 관련 REQ: REQ-3.19 REQ-3.20
- 결정 사항: 먼저 찾은 이름이 이긴다. 카탈로그 4,000자, AGENTS 16,000자. `CLAUDE.md` 안 읽음. 읽기 범위는 session
- 변경 대상(힌트): `crates/keel-skills`, `crates/keel-cli`, `crates/keel-core` read_roots
- 하지 말 것: references 자동 전체 주입, standalone `.md` 스킬, `allowed-tools` 강제
- 선행 조건: TASK-010
- 검증: 단위
- 검증 범위: task-local
- 검증 명령(힌트): `cargo test -p keel-skills`
- 산출물: `keel-skills`와 슬래시 명령

## 구현 메모

탐색: cwd `.agents/skills/` → git 조상 → `~/.agents/skills/` → `.claude/skills` 호환. resume 시 skill 메시지로 루트를 다시 채운다.

## 세부 실행 체크리스트

- cwd 밖 스킬 파일을 `read`할 수 있고 그 루트에 `write`는 못 하는지

## 하지 말 것

- 스킬 전용 실행기를 만들지 않는다

## 검증 포인트

- 카탈로그만 시스템 프롬프트에 들어가는 테스트
- resume 후 read_roots 복원 테스트

## Phase 완료 조건

TASK-011 완료.
