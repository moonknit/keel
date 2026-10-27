# Error model

권위: [srs.md](../planning/srs.md) REQ-4.4, REQ-3.13, REQ-3.18, REQ-3.23.

## 분류

| 코드 | 언제 | 사용자에게 | chat status | 프로세스 |
|---|---|---|---|---|
| `auth` | 401, 키 없음 | 키/권한 안내. 키 값 출력 금지 | error | 실패 |
| `rate_limit` | 429, 백오프 3회 실패 | 잠시 후 재시도 | error | 실패 |
| `provider` | 5xx 3회 실패, 프로토콜 불명 | 제공자 오류 | error | 실패 |
| `overflow` | context 한도. 1회 재시도도 실패 | 새 session 권고 | error | 실패 |
| `cancelled` | Ctrl-C / token | 취소됨 | interrupted | REPL은 프롬프트로 |
| `locked` | 다른 프로세스 lock | pid 안내 | (열지 않음) | 명령 실패 |
| `denied` | 승인 거부, 경로 탈출, print 모드 거부 | 거부 이유 | 런 계속 또는 print 1 | 도구 observation |
| `tool` | bash 타임아웃, 비0, MCP 서버 다운 | 도구 오류를 observation으로 | 런 계속 | 루프 유지 |
| `limit` | `max_turns` | 한도 | done 아님, RunEnd limit | `-p` 종료 코드 2 |
| `config` | TOML 파싱, 필수 값 없음 | 경로와 필드 | (시작 전) | 실패 |

## 복구

- 열기: `running` → `interrupted` + 짝 없는 tool_call에 interrupted result. 도구를 다시 실행하지 않는다.
- MCP: 죽은 서버의 다음 `tools/call`만 `tool` 에러. 다른 도구와 루프는 산다.
- overflow: 오래된 chat을 더 빼고 한 번만 다시 부른다. 그래도 실패면 `overflow`.
- 취소: 진행 중 모델/도구를 끊고 짝을 맞춘 뒤 lock은 REPL이면 유지, 종료면 해제.

## print 모드 종료 코드

0 정상 완료, 1 위 표의 실패(`cancelled` 제외), 2 `limit`, 130 `cancelled`.
