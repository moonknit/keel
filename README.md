# keel

가벼운 Rust 에이전트 런타임.

턴 루프 하나가 커널이다. 훅, 크론, 서브에이전트는 그 루프에 넣는 메시지다. 채널과 도구, 대시보드는 커널 밖에 둔다.

조사와 설계는 [docs/index.md](docs/index.md)에 있다.
REPL이 코드를 어떻게 도는지는 [docs/guide/repl.html](docs/guide/repl.html)에 있다. Rust를 다시 익히며 같은 코드를 읽고 싶다면 [docs/guide/repl-rust.html](docs/guide/repl-rust.html)을 본다.

## 준비

저장소 루트에 `.env`가 있어야 한다. 키는 커밋하지 않는다. 이름은 [`.env.example`](.env.example)을 본다.

```bash
cp .env.example .env
# OPENAI_API_KEY, LLM_MODEL 을 채운다. 기본 모델 이름은 gpt-6-luna.
```

DB를 홈에 남기지 않으려면 `KEEL_HOME`을 임시 디렉터리로 둔다.

```bash
export KEEL_HOME="$(mktemp -d)"
cargo build -p keel-cli
```

## 사용 예

### print 모드 (`-p`)

한 번 돌리고 어시스턴트 텍스트를 도착하는 대로 찍는다. 승인이 필요한 도구는 거부한다. `read` / `grep` / `glob`과 hosted `web_search`는 통과한다. `write` / `edit`는 `--allow-edits`가 있을 때만. `bash`는 `-p`에서 막힌다.

```bash
# 텍스트만. 도구를 부르지 말라고 못 박는다.
cargo run -p keel-cli -- -p "Reply with exactly the word pong and nothing else. Do not call tools."

# 로컬 read. Cargo.toml 을 읽고 workspace members 만 답한다.
cargo run -p keel-cli -- -p "Read Cargo.toml with the read tool. Reply with only the workspace members list, one line."

# hosted web_search. OpenAI가 검색을 실행한다.
cargo run -p keel-cli -- -p "What year is it? Use web_search once and answer in one short sentence."

# write. print 모드에서는 --allow-edits 가 필요하다.
cargo run -p keel-cli -- --allow-edits -p "Create scratch-keel.txt with the text hello using the write tool, then reply with the word done."
```

2026-10-27 실측: 위 텍스트/`read` 두 예는 exit 0 이었다. `pong`, 그리고 `crates/keel-*` members 목록이 나왔다.

### REPL

승인 프롬프트와 `bash` jail은 여기 있다. 안전한 `cargo test` 같은 명령은 Seatbelt/`bwrap` 안에서 자동 허용된다. `curl` / `git push` 는 묻는다.

```bash
cargo run -p keel-cli
```

```text
> List Rust files matching **/*.rs with glob, then say how many you found.
> Run `echo hi` with bash.
> /skills
> /session
> /exit
```

처음 뜨면 cwd의 최근 session을 열고, 없으면 만든다. `--new`는 새 session, `--resume <id>`는 지정 session.

### session

```bash
cargo run -p keel-cli -- session list
cargo run -p keel-cli -- session show <id>
cargo run -p keel-cli -- session rm <id>
```

### 스킬

`<cwd>/.agents/skills/<name>/SKILL.md` 를 두면 기동 시 카탈로그에 오른다. REPL에서 `/skill:name` 또는 모델이 `skill` 도구를 부른다.

## 테스트

`keel`을 그냥 실행하면 `.env`의 실제 모델을 부른다. 가짜 모델(`ScriptedModel`, `KEEL_SCRIPTED=1`)은 **테스트와 스크립트 검증**용이다.

`cargo test`는 기본적으로 가짜 모델·픽스처만 쓴다. 실제 API를 치는 테스트는 `KEEL_LIVE=1`일 때만 나간다.

```bash
cargo test --workspace

KEEL_LIVE=1 cargo test -p keel-openai --test spike -- req_3_6_live_store_false_roundtrip --nocapture
KEEL_LIVE=1 cargo test -p keel-cli --test print -- req_3_23_print_mode_live --nocapture
```

`cargo test --workspace`만으로는 모델이 도구를 고르는 경로를 보지 못한다. 그 경로는 위의 `KEEL_LIVE` 명령과 이 문서의 `-p` / REPL 예로 확인한다.
