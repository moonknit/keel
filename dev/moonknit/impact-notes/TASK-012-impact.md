# TASK-012 impact

## 실행 중 결정

MCP 클라이언트는 `rmcp` 대신 newline JSON-RPC stdio를 직접 구현했다. crate API가 빠르게 바뀌고, 필요한 동작은 initialize / tools/list / tools/call / 실패 격리뿐이기 때문이다. HTTP transport는 v1에서 설정 키만 받고, 연결은 stdio부터 연다.

## 계획 보정

`keel-mcp`의 의존에서 `rmcp`를 빼도 INV-9와 REQ-3.18은 그대로다.
