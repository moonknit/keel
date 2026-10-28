# 외부 계약

keel은 HTTP API를 열지 않는다. 대시보드는 TUI 패널이다. 이 문서는 프로세스가 지키는 바깥 계약이다.

## CLI

[srs.md](../planning/srs.md) REQ-3.21–3.23, REQ-3.30–3.33.

TTY 대화형: TUI. 승인 한 줄. `-p`는 텍스트 델타만. `keel serve`는 스케줄러만. `keel cron`은 잡 CRUD. stderr: 사람용 진단(키 없음). 디버그 로그는 `$KEEL_HOME/logs/keel-YYYYMMDD.log`.

슬래시 `/new` `/resume`은 같은 프로세스에서 session을 바꾼다. `/plan`은 plan mode 진입·계획 승인·거부. MCP 도구는 기동 시 config에서 등록하며 공개 이름은 `server__tool`. 서브에이전트 도구 이름은 `spawn` `wait`. 부모 `/last`는 위임 요약이다.

## 환경 변수

[REQ-3.8](../planning/srs.md). `.env.example`이 이름 목록의 공개본이다. 값은 저장소에 커밋하지 않는다.

## OpenAI Responses

- 메서드: `POST {LLM_BASE_URL}/responses`
- 인증: `Authorization: Bearer $OPENAI_API_KEY`
- 본문 고정: `store: false`, `stream: true`, `model: $LLM_MODEL`
- tools: 로컬 function 스키마 + (설정 시) `{ "type": "web_search", "user_location": { "type": "approximate", "country": "KR" } }`
- 입력: 시스템/개발자 텍스트 + 저장된 `raw` item + 새 user / `function_call_output`
- 출력 SSE (2026-10-27 `KEEL_LIVE=1` 실측):
  - 공통: `response.created`, `response.in_progress`, `response.output_item.added`, `response.output_item.done`, `response.completed`
  - 텍스트: `response.content_part.added`, `response.output_text.delta`, `response.output_text.done`, `response.content_part.done`, `response.output_text.annotation.added`
  - function: `response.function_call_arguments.delta`, `response.function_call_arguments.done`
  - web_search: `response.web_search_call.in_progress`, `response.web_search_call.searching`, `response.web_search_call.completed`
- `response.completed.response.output`이 재전송할 item 배열이다. SQLite TEXT로 왕복해도 필드가 유지된다.
- function `name`: ASCII 영숫자와 `_`. `server__tool`은 요청이 받아들여졌다. 점(`server.tool`)은 쓰지 않는다.

## Event (core → host)

| Event | 페이로드 |
|---|---|
| RunStart | session_id, chat_id |
| TurnStart | turn index |
| TextDelta | string |
| ToolStart | name, 요약 args |
| ToolEnd | name, ok/exit, 시간, 줄 수 |
| HostedTool | name (`web_search`) |
| ContextTrimmed | 생략한 chat 수 |
| Usage | input/output tokens |
| RunEnd | status: done / interrupted / error / limit |

## Store

`create_session`, `open_session`, `list_sessions`, `begin_chat`, `append`, `end_chat`, `rename`, `close`. 시그니처 상세는 구현 crate가 정하고, 의미는 SRS를 따른다.
