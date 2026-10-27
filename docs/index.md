# 문서 목록

조사·요구·설계·기술 문서의 입구. 사용법과 실행 예는 루트 [README.md](../README.md)에 있다.
REPL이 코드를 어떻게 타는지 초심자용 안내는 [guide/repl.html](guide/repl.html)이다.

| 문서 | 역할 |
|---|---|
| [guide/repl.html](guide/repl.html) | `keel` REPL 경로를 코드와 같이 설명. |
| [planning/srs.md](planning/srs.md) | v1 요구 정본. |
| [technical/domain-model.md](technical/domain-model.md) | session / chat / message와 메모리 타입. |
| [technical/invariants.md](technical/invariants.md) | 깨지면 안 되는 조건. |
| [technical/error-model.md](technical/error-model.md) | 오류 분류와 복구. |
| [technical/api-contract.md](technical/api-contract.md) | CLI·환경 변수·OpenAI·Event 계약. |
| [research/lightweight-agent-runtimes.md](research/lightweight-agent-runtimes.md) | 공개 경량 런타임 조사. 사실과 출처. |
| [research/lightweight-sandboxes.md](research/lightweight-sandboxes.md) | 로컬 코드 실행 격리. Seatbelt/bwrap부터 microVM까지. |
| [design/runtime.md](design/runtime.md) | 그 조사와 요구사항으로 잡은 keel 설계. |
| [reference/261027-140721-blueprint.md](reference/261027-140721-blueprint.md) | v1 청사진. 결정 근거, 기각한 대안, 마일스톤, 재검토 항목. |

조사는 설계의 근거이고, 구현이 어긋나면 설계를 고친다. 조사 문서를 구현 스펙처럼 읽지 않는다.
