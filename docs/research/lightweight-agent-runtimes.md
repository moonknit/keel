# 경량 에이전트 런타임 조사

조사일: 2026-10-27.
별 수와 파일 크기는 그날 GitHub API 기준이다.

필터는 두 개다.

1. 가벼운가. 코어를 한 자리에서 읽을 수 있는가. LangGraph, CrewAI, Letta, OpenCode는 여기서 뺀다.
2. keel과 맞는가. 턴 루프가 커널이고, 채널·도구·대시보드는 바깥인가.

이번 설계에 필요한 기능도 같이 봤다.

- ReAct 루프
- 코드 생성과 실행
- MCP와 도구. 검색, 코딩
- `SKILL.md` 스킬
- 대화 저장 이름. session vs context
- 로컬 저장. JSONL vs SQLite
- CLI 호스트와 코어 분리
- OpenAI Responses hosted `web_search`
- 스킬 첨부 파일은 `read` `bash`

## 한줄 결론

철학이 가장 가까운 구현은 TypeScript `pi-agent-core`다. 가장 짧은 정의는 Hugging Face tiny-agents다. Rust로 바로 가져다 쓸 가벼운 런타임은 없다. goose는 성숙한 Rust 제품이고, agentd는 런타임 카테고리는 맞지만 AGPL에 커뮤니티가 없다. `llm-agent-runtime` crate는 ReAct+MCP가 있지만 메모리·그래프까지 넣은 키친싱크다.

코딩 도구와 `SKILL.md`는 커널이 아니라 호스트 쪽에 두는 쪽이 공개 구현과 맞다. 저장된 대화의 이름은 session이다. context는 모델에 넣는 창이다.

## 요구 기능별 누가 뭘 하나

| 기능 | 짧은 구현 | 성숙한 구현 | keel이 가져올 것 | 가져오면 안 되는 것 |
|---|---|---|---|---|
| ReAct 루프 | tiny-agents `Agent.ts` 4.7KB. mini-swe `default.py` 8KB | pi-agent-core. goose `agent.rs` 98KB | native tool call을 Act로 쓰는 루프. 텍스트 `Action:` 파서는 기본이 아님 | smolagents CodeAgent. 모델이 파이썬을 써서 도구를 호출 |
| 코드 생성·실행 | mini-swe는 bash 하나 | goose, pi coding agent, OpenCode | `bash` `read` `write` `edit` `grep` `glob`. 실행은 bash | 커널 안 인터프리터. 86개 도구를 바이너리에 박기 |
| MCP | tiny-agents. mcp-agent | goose extensions | MCP는 Tool 소스. `rmcp` | MCP를 커널 상태기계로 만들기 |
| 검색 | OpenAI hosted `web_search` | goose 확장, MCP 검색 | Responses API에 hosted 도구를 붙임. 로컬 function으로 다시 만들지 않음 | 모델만 켠다고 검색된다고 가정 |
| 스킬 첨부 | pi: 루트 알리고 `read` | Agent Skills `scripts/` `references/` | 본문만 주입. 파일은 기존 `read` `bash` | 스킬 전용 실행기 |
| 스킬 | tiny-agents `AGENTS.md` | [Agent Skills](https://agentskills.io/specification). pi, goose, Claude Code | `SKILL.md` progressive disclosure | 스킬 본문을 시작부터 시스템 프롬프트에 넣기 |

## 프로젝트

숫자는 2026-10-27 GitHub.

| 프로젝트 | 언어 | 라이선스 | 별 | 코어 | 종류 |
|---|---|---|---:|---|---|
| [earendil-works/pi](https://github.com/earendil-works/pi) (`pi-agent-core`) | TS | MIT | 118075 | 6파일, 약 80KB | 루프 커널 |
| [huggingface.js mcp-client](https://github.com/huggingface/huggingface.js/tree/main/packages/mcp-client) | TS | Apache-2.0 | (모노레포) | `Agent.ts` 4.7KB | 루프 + MCP |
| [SWE-agent/mini-swe-agent](https://github.com/SWE-agent/mini-swe-agent) | Py | MIT | 8864 | `default.py` 8KB | 코딩 하네스 |
| [aaif-goose/goose](https://github.com/aaif-goose/goose) | Rust | Apache-2.0 | 55787 | `agent.rs` 98KB+ | 제품 런타임 |
| [agentd](https://agentd.dev/) ([source](https://github.com/agentd-dev/source-code)) | Rust | AGPL-3.0 | 0 | 모듈 맵 | 프로세스 런타임 |
| [0xPlaygrounds/rig](https://github.com/0xPlaygrounds/rig) | Rust | MIT | 8986 | Agent 루프 crate | 라이브러리 |
| [Mattbusel/agent-runtime](https://github.com/Mattbusel/agent-runtime) (`llm-agent-runtime`) | Rust | MIT | 6 | ReActLoop + MCP feature | 키친싱크 crate |
| [lastmile-ai/mcp-agent](https://github.com/lastmile-ai/mcp-agent) | Py | Apache-2.0 | 8568 | 워크플로 패턴 | 오케스트레이션 |
| [huggingface/smolagents](https://github.com/huggingface/smolagents) | Py | Apache-2.0 | 30083 | `agents.py` 81KB | 프레임워크 |
| [The-Pocket/PocketFlow](https://github.com/The-Pocket/PocketFlow) | Py | MIT | 11283 | 100줄 그래프 | 교육용 |
| [liquidos-ai/AutoAgents](https://github.com/liquidos-ai/AutoAgents) | Rust | Apache-2.0 | 769 | 멀티에이전트 | 프레임워크 |
| [bosun-ai/swiftide](https://github.com/bosun-ai/swiftide) | Rust | MIT | 796 | RAG + agent | 인덱싱이 본업 |

### pi-agent-core

가장 가깝다.

최근 코어에서 세션, durable, skills, compaction을 뺐다. 남은 것은 `Agent`, 루프, 스트림, 타입이다. 훅은 `beforeToolCall`, `afterToolCall`, `shouldStopAfterTurn`, `prepareNextTurn`. steering·follow-up 큐가 메시지다. TUI, 코딩 CLI, 채팅은 다른 패키지다. 서브에이전트와 plan mode는 일부러 없다.

스킬은 코어 밖, Pi coding agent / 하네스에 있다. [Pi Skills](https://pi.dev/docs/latest/skills)는 Agent Skills 스펙을 따른다. 시작 때 name·description만 넣고, 본문은 필요할 때 읽는다.

루프는 native tool call이다. 모델이 tool call을 내면 실행하고 `ToolResult`를 붙인 다음 다시 부른다. 텍스트 ReAct 파서가 아니다.

### tiny-agents

가장 짧다.

문장 하나가 정의다. MCP 클라이언트가 있으면 에이전트는 그 위의 while 루프다. 도구는 커널에 없다. 설정은 `agent.json`과 MCP 서버 목록. 프롬프트는 `AGENTS.md` 또는 `PROMPT.md`.

코딩 도구 세트나 `SKILL.md` 카탈로그는 없다. 패턴만 가져온다.

글: [Tiny Agents](https://huggingface.co/blog/tiny-agents), [Python 판](https://huggingface.co/blog/python-tiny-agents).

### mini-swe-agent

과설계 금지.

에이전트 클래스는 약 100줄이라고 적혀 있고, `default.py`는 8KB다. 도구는 bash 하나. 히스토리는 선형이다. SWE-bench Verified에서 74%를 넘긴다고 한다. 범용 런타임이 아니라 코딩 하네스다.

교훈은 두 개다. 코드 실행의 기본 경로는 셸이다. 스캐폴드보다 모델이 일을 한다. 검색·MCP·스킬은 이 프로젝트의 범위가 아니다.

### goose

성숙한 Rust 제품.

구조는 Interface / Agent / Extensions(MCP). 크론, 서브에이전트, 스케줄 도구가 있다. 스킬은 `.agents/skills/`와 `SKILL.md`를 읽고, `.claude/skills/`도 호환한다. ([using-skills](https://github.com/aaif-goose/goose/blob/main/documentation/docs/guides/context-engineering/using-skills.md))

`crates/goose/src/agents/agent.rs`는 98KB이고 주변에 MCP 클라이언트, 확장, 서브에이전트, 확인 UI가 붙어 있다. 아키텍처 문서만 읽고 포크하지 않는다.

### agentd

런타임 카테고리.

supervisor는 추론하지 않는다. 에이전트 루프는 자식 프로세스에만 있다. 도구는 MCP. 트리거와 타이머가 있다. 정적 바이너리 약 8.5MiB, idle RSS 약 5.5MiB라고 한다. AGPL-3.0. 별 0. 모듈 맵은 참고하고 의존하지 않는다.

### Rig

Rust 라이브러리.

`Agent`는 모델 + preamble + tools + 루프다. 데몬, 인박스, 채널, 스킬 카탈로그가 없다. 프로바이더 추상으로 쓸 수는 있다. 커널을 Rig에 맡기면 메시지 경계가 흐려진다.

### llm-agent-runtime

Rust crate. ReActLoop, native tool call, MCP(`rmcp`), 액션 훅이 있다. 동시에 에피소딕/시맨틱 메모리, knowledge graph, 서킷 브레이커, 멀티에이전트 버스가 기본에 가깝다. 별 6. 루프 API는 참고하고 crate를 통째로 쓰지 않는다.

### 나머지

- mcp-agent: Anthropic 워크플로 패턴 + MCP. 오케스트레이션. 커널이 아니다.
- smolagents: CodeAgent가 포인트다. `agents.py` 81KB. 이름보다 크다.
- PocketFlow: 100줄 그래프. Rust 포트는 별 44.
- AutoAgents: WASM 샌드박스, OTEL, 멀티에이전트. keel보다 크다.
- swiftide: RAG가 본업.

뺀 것: OpenCode(222k, 코딩 제품), Crush(29k, FSL 계열), Letta(25k, 메모리 플랫폼), PydanticAI / Atomic Agents(타입 좋은 라이브러리), Ryvos(별 8, 채널·UI·도구를 한 바이너리에 넣음).

## session과 context

저장된 한 대화를 context라고 부르는 구현은 거의 없다. 쓰는 말은 session이다.

| 제품 | 저장 단위 | 모델 입력 창 | 저장 형식 |
|---|---|---|---|
| Claude Code | session. `--continue` `--resume` | 히스토리 + 시스템 + 도구. compact 가능 | JSONL |
| Pi | session. `--continue` `--resume` `/session` | 문서가 Sessions and Context로 둘을 가름 | JSONL 트리 |
| goose | session. named session | 루프가 쓰는 메시지 | SQLite |

context는 이미 차 있다. context window, MCP context, 스킬 context. 디스크의 대화 행에 그 이름을 주면 설계가 섞인다.

한 session 안의 한 사용자 입력과 이어지는 런을 keel은 chat이라고 부른다. 공개 구현은 보통 이걸 turn 또는 user message로만 나눈다. chat을 중간 테이블로 두는 것은 목록과 CLI 접기를 쉽게 하려는 선택이다.

로컬 DB는 SQLite가 맞다. 목록, cwd 필터, 이어하기에 쿼리가 필요하다. JSONL은 Claude/Pi처럼 재생이 목표면 충분하다.

## 스킬 스펙

공개 표준은 [Agent Skills](https://agentskills.io/specification)다. 저장소 [agentskills/agentskills](https://github.com/agentskills/agentskills) 별 26674.

스킬은 디렉터리다. 최소 `SKILL.md` 하나.

```text
skill-name/
├── SKILL.md
├── scripts/
├── references/
└── assets/
```

frontmatter 필수: `name`, `description`. 선택: `license`, `compatibility`, `metadata`, `allowed-tools`(실험).

progressive disclosure.

1. 시작: name + description. 약 100 토큰.
2. 활성화: `SKILL.md` 본문. 5000 토큰 이하를 권장.
3. 필요 시: `scripts/`, `references/`, `assets/`.

pi와 goose와 Claude Code가 이 형식을 읽는다. 탐색 경로는 `.agents/skills/`, `~/.agents/skills/`, 호환으로 `.claude/skills/`.

스킬은 도구가 아니다. 지시문과 첨부 파일이다. 스크립트 실행은 bash 같은 코딩 도구로 한다.

## 토큰을 아끼고 읽는 순서

1. [tiny-agents 글](https://huggingface.co/blog/tiny-agents)과 `Agent.ts` 4.7KB.
2. mini-swe-agent `src/minisweagent/agents/default.py` 8KB.
3. pi-agent-core 6파일, 약 80KB. `packages/agent/src/`.
4. [Agent Skills specification](https://agentskills.io/specification).
5. goose architecture 문서만. crate는 열지 않는다.
6. agentd-core `lib.rs` 모듈 맵. [agentd embedding](https://agentd.dev/docs/embedding/).
7. 필요하면 [Rig Agents](https://rig.rs/docs/concepts/agent/).

goose, OpenCode, smolagents 본체를 clone 해서 컨텍스트에 넣지 않는다.

## 출처

- GitHub API, 2026-10-27.
- [pi-agent-core changelog](https://github.com/badlogic/pi-mono/blob/main/packages/agent/CHANGELOG.md) — 하네스 제거. 저장소는 `earendil-works/pi`로 옮겨졌다.
- [goose architecture](https://github.com/aaif-goose/goose/blob/main/documentation/docs/goose-architecture/goose-architecture.md)
- [Agent Skills spec](https://agentskills.io/specification)
- [Pi Skills](https://pi.dev/docs/latest/skills)
- [goose using-skills](https://github.com/aaif-goose/goose/blob/main/documentation/docs/guides/context-engineering/using-skills.md)
- [tiny-agents](https://huggingface.co/blog/tiny-agents)
- [llm-agent-runtime](https://github.com/Mattbusel/agent-runtime)
- [Claude Code sessions](https://code.claude.com/docs/en/sessions)
- [Pi Sessions and Context](https://pi.dev/docs/latest/sessions)
- [OpenAI web search](https://developers.openai.com/api/docs/guides/tools-web-search)
