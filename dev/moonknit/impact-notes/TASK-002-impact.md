# TASK-002 impact

## 실행 중 결정

- 기본 경로는 `reqwest` + SSE + `serde_json::Value`. typed SDK는 쓰지 않는다. 픽스처와 라이브 모두 unknown field가 SQLite TEXT 왕복 후 남았다 (RV-003 닫음).
- 라이브는 `KEEL_LIVE=1` + cwd `.env`. 본 루프 재전송은 `response.completed.response.output`을 그대로 넣는다.

## RV 판정

- RV-003: Value 왕복으로 충분. adopted.
- RV-004: 실측 이름을 `docs/technical/api-contract.md`에 기록. adopted.
- RV-005: `echo`, `server__tool` 허용. `server.tool`은 보내지 않음. adopted.

## 검증 한계

- 이번 라이브 호출에는 reasoning item이 나오지 않았다. `encrypted_content` 보존은 픽스처/유닛으로만 확인했다.
