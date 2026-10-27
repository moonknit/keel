# Invariants

테스트 이름에 `INV-n`을 그대로 적는다.

### INV-1
`keel-core`는 rusqlite, reqwest, rmcp, clap, rustyline, 파일/프로세스 도구 crate를 의존하지 않는다.

### INV-2
OpenAI 요청은 항상 `store: false`다. 서버 conversation id로 resume하지 않는다.

### INV-3
모델에 보내는, 마지막 user 메시지 이후 output item은 DB의 `raw` 그대로다. 필드를 고쳐 쓰지 않는다.

### INV-4
`tool_call`이 transcript에 있으면 같은 런을 모델에 보내기 전에 짝 `tool_result`가 있다. 취소·복구 시 합성한다.

### INV-5
진행 중 chat의 message는 잘리지 않는다. 생략은 끝난 chat 단위만이다.

### INV-6
생략 Notice는 저장하지 않는다. 원본 `chat`/`message` 행을 지우지 않는다.

### INV-7
`write`/`edit`의 최종 경로는 canonicalize 후 session cwd 아래여야 한다. 스킬 루트에는 쓰지 않는다.

### INV-8
비밀값(`OPENAI_API_KEY`, MCP `${VAR}` 치환 결과)은 config 파일과 기본 로그에 쓰지 않는다.

### INV-9
MCP 도구 공개 이름은 `<server>__<tool>`이다. 점(`.`) 구분 이름을 등록하지 않는다.

### INV-10
같은 session을 `lock_pid`가 살아있는 두 프로세스가 동시에 열지 않는다.

### INV-11
allowlist와 승인 결과는 디스크에 쓰지 않는다. 프로세스를 다시 열면 다시 묻는다.

### INV-12
print 모드(`-p`)에서 승인이 필요한 도구는 `--allow-edits`가 허용하는 Write를 제외하고 실행하지 않는다.

### INV-13
jail이 켜진 `bash`는 cwd와 임시 디렉터리 밖에 쓰지 못하고, 네트워크를 쓰지 못한다. jail 기동 실패는 호스트 실행으로 바꾸지 않는다.

### INV-14
같은 cwd의 `write`/`edit`/`bash`는 부모·자식을 포함해 동시에 둘 이상 실행하지 않는다.

### INV-15
부모 transcript의 위임 결과는 요약과 자식 session id만이다. 자식의 도구 로그를 부모 context에 펼치지 않는다.

### INV-16
자식은 새 session이다. 부모 session을 다시 열지 않는다. 자식은 `spawn`/`wait`를 갖지 않는다.

### INV-17
신뢰한 설정의 MCP HTTP 서버는 기동 시 연결한다. 파싱만 하고 건너뛰면 REQ-3.18을 충족한 것이 아니다.

### INV-18
훅은 루프 콜백이다. Inbox `Hook` 메시지 처리로 대체하지 않는다. `should_stop`으로 런을 끝낼 때도 짝 없는 `tool_call`에 result를 붙인다.
