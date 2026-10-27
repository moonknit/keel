# Phase 04: 코딩과 권한

## 목적

파일을 고치고 명령을 실행하며, 위험 동작은 묻고, 취소해도 transcript 짝이 맞는다.

## 선행 조건

Phase 03 CLI가 Event를 그릴 수 있다.

## Tasks

### TASK-009: 쓰기·실행 도구

- 목표: `write` `edit` `bash`가 cwd에서 안전하게 동작한다
- 관련 REQ: REQ-3.15 REQ-3.17 INV-7
- 결정 사항: edit는 old/new 정확 1회. bash는 호출마다 새 프로세스, 그룹 kill, 120s/600s, 30,000자
- 변경 대상(힌트): `crates/keel-tools`
- 하지 말 것: 상태 있는 셸, unified patch, Docker
- 선행 조건: TASK-007
- 검증: 단위
- 검증 범위: task-local
- 검증 명령(힌트): `cargo test -p keel-tools`
- 산출물: write/edit/bash

### TASK-010: 권한과 취소 UX

- 목표: once/always/deny와 위험 패턴, `--allow-edits`, `-p` 거부, Ctrl-C 취소가 동작한다
- 관련 REQ: REQ-3.16 REQ-3.3 REQ-3.23 INV-11 INV-12
- 결정 사항: allowlist는 메모리만. `--yolo` 없음. 위험 패턴은 휴리스틱
- 변경 대상(힌트): `crates/keel-tools`, `crates/keel-cli`, `crates/keel-core` Approver 연결
- 하지 말 것: 승인을 DB에 저장
- 선행 조건: TASK-008, TASK-009
- 검증: 통합
- 검증 범위: task-local
- 검증 명령(힌트): 거부·print 거부·취소 합성 테스트
- 산출물: 게이트와 승인 줄

## 구현 메모

경로 canonicalize 후 cwd/스킬 루트 검사. 스킬 루트 쓰기는 거부. 코딩 시나리오(테스트 고치기)는 ScriptedModel로 한 번 닫는다.

## 세부 실행 체크리스트

- 심링크가 cwd 밖으로 나가면 쓰기가 거절되는지
- 취소 후 다음 모델 입력이 INV-4인지

## 하지 말 것

- 네트워크 여부를 실제로 판별하려 하지 않는다. 패턴 목록만 쓴다

## 검증 포인트

- `cargo test -p keel-tools -p keel-cli`의 권한·취소 케이스

## Phase 완료 조건

TASK-009, TASK-010 완료. 승인 없이는 위험 bash가 실행되지 않는다.
