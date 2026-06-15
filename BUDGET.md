<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
# BUDGET.md — token, cost, and time statistics

> The resource side of the 19-day human–AI session this library was built in:
> what it cost, in tokens, dollars, and time. It is the shareable summary of a
> set of internal authorship records (a human-role write-up, an AI-role
> write-up, a first-person essay, and the raw session transcripts) that are
> kept private. 한국어 요약은 하단에.
>
> **Revision note**: the first version of this file overcounted every token
> figure by ~2.5× — the transcript logs one entry per *streaming chunk*, and
> entries sharing a `message.id` repeat the same `usage` block; summing
> entries double-counts. All numbers below are deduplicated by `message.id`.
> The error was caught when a follow-up question ("is a msg one prompt?")
> forced a look at message identity. Fitting, for this project.

## 쉬운 말 요약 / The bottom line, in plain language

**이 프로젝트(19일, 254 커밋, 약 36,000줄)를 만드는 데 쓰인 토큰은 API 정가
기준 약 $4,400어치다.** 환율을 1,400원으로 가정하면 약 620만 원. 단, Fable 5의
단가는 미공지라 Opus 단가로 가정한 추정이고, 구독 플랜으로 썼다면 실제 지불액은
이와 전혀 다를 수 있다(이 수치는 "작업의 명목 가치").

통계를 모르는 사람을 위한 해석:

1. **돈의 대부분(79%)은 "쓰기"가 아니라 "다시 읽기"에 들었다.** AI는 답을 하나
   만들 때마다 그동안의 대화·코드 전체 — 평균 44만 토큰, 두꺼운 책 한 권 분량 —
   를 다시 읽는다. 그걸 5,320번 했다. 새 글을 생성한 비용은 ~$400(9%)뿐이다.
   긴 프로젝트에서는 생각을 쓰는 것보다 **기억을 유지하는 게 비싸다.**
2. **에이전트가 실제로 일한 시간은 19일 중 약 43시간이다** (이벤트 간격 5분
   초과는 "대기"로 제외한 활성 시간; 임계값을 2~30분으로 바꿔도 37~62시간).
   처음엔 164시간으로 계산했었는데, 그건 사람이 자리 비운 25시간짜리 창까지
   "작업"으로 센 과대계상이었다 — 리뷰에서 빠졌다.
3. **평균(mean)과 중앙값(median)이 크게 다르면, 소수의 큰 값이 평균을 끌어올렸다는
   뜻이다.** 턴당 활성시간: 중앙값 1.5분 vs 평균 4.0분 — 보통의 턴은 1.5분 만에
   끝났고, 긴 구현 턴들이 평균을 끌어올렸다. **p95는 "100번 중 95번은 이 값
   이하"라는 뜻**: p95 = 16분 → 거의 모든 턴이 16분 안에 끝났고, 가장 길었던
   턴도 81분이다.
4. **비용은 소수 턴에 집중됐다.** 647개 턴을 비용순으로 줄 세우면, 싼 쪽
   절반(324개)의 비용을 전부 더해도 전체의 8.3%($366 ÷ $4,413)다 — 즉 **대화의
   절반을 통째로 지워도 청구서는 8%대만 줄어든다.** 반대로 비싼 상위 10%(64개)가 비용의
   **49%**를 쓴다. 턴의 절반은 $2.75 이하(커피 한 잔)였고, 가장 비싼 턴은
   **"이대로 진행"이라는 네 글자짜리 승인($117)** — 그 한마디가 푼 79회 호출의
   구현 체인 값이다. 이 협업의 리듬 그대로다: **잦고 싼 결정 + 드물고 비싼 실행.**
5. **단가 감각**: 에이전트 활성시간 1시간에 ~$102, 커밋 하나에 ~$17, 출하 코드
   한 줄에 ~$0.12꼴.

*In plain language: building this project (19 days, 254 commits, ~36 K lines)
consumed about **$4,400 worth of tokens at API list prices** (~₩6.2 M at an
assumed 1,400 KRW/USD; Fable 5 priced at the Opus assumption; a subscription
plan would have paid differently — this is nominal value). 79% of that went to
*re-reading* context (a book-sized ~440 K tokens before nearly every one of
5,320 replies), not to writing. The agent's *active* time was ~43 h of the 19
days (gaps > 5 min excluded; an earlier 164 h figure counted human absences
and was corrected). The median turn took 1.5 minutes and $2.75; the top 10%
of turns carried 49% of the cost — many cheap decisions, a few expensive
executions.*

## Units — what is a "message"?

| unit | count | definition |
| --- | ---: | --- |
| **Human prompts** (turns) | **643** | messages typed by the human |
| **API messages** (unique `message.id`) | **5,320** | one model call each; an agent turn chains several per prompt through tool use — **~8.3 API calls per human prompt** |
| Log entries | 10,690 | streaming chunks; same `message.id` repeated (the earlier overcount source) |
| Tool calls | 5,086 | `tool_use` blocks across all API messages |

## Methodology

- **Source**: `message.usage` of every assistant record in the raw session
  transcript (kept private), **deduplicated by `message.id`** (first
  occurrence; duplicates carry identical usage). Snapshot 2026-06-13 — the
  session continued, so figures are floors.
  Sub-agent (sidechain) calls logged in the same file are included.
- **Active working time**: events (user/assistant) are summed by inter-event
  gap, with any gap longer than **5 minutes counted as waiting and excluded**
  (the agent isn't computing for 25 h because a human left for work and a few
  wakeups trickled). Sensitivity: cap 2 min → 37.2 h, 5 min → **43.3 h**,
  10 min → 49.3 h, 30 min → 61.9 h — the conclusion is "roughly 40–60 h"
  regardless of cap. (An earlier version used prompt→last-event windows and
  reported 163.7 h; that counted human absences with sparse trailing events
  as work and was corrected on review.)
- **Cost**: Anthropic **API list prices** (table below). On a subscription
  plan the marginal spend differs entirely — this is the nominal API value of
  the work, not necessarily what was paid.

## Pricing used (USD per million tokens) and its basis

| model | input | output | cache read | cache write (5 min) | basis |
| --- | ---: | ---: | ---: | ---: | --- |
| claude-opus-4-7 | $15 | $75 | $1.50 | $18.75 | Anthropic API list price, Opus tier (as of the agent's knowledge, early 2026) |
| claude-opus-4-8 | $15 | $75 | $1.50 | $18.75 | same Opus tier |
| claude-fable-5 | $15 | $75 | $1.50 | $18.75 | **assumption** — Fable 5 pricing was not public at the time of writing; flagship (Opus) tier assumed |

Cache multipliers are the platform-standard ratios: read = 0.1 × input,
5-minute write = 1.25 × input. If Fable 5's real price differs, only its row
and the total scale accordingly.

## Turns & time

| metric | value |
| --- | ---: |
| Human prompts | **643** |
| Unique API messages | 5,320 (~8.3 per prompt) |
| Tool calls | 5,086 |
| **Agent active time** (gaps > 5 min excluded; see Methodology) | **~43.3 h** (range 37–62 h across gap caps) |
| Average active time per prompt | ~4.0 min |
| Calendar span / active days | 19 days / 16 days |

## Tokens & cost by model (deduplicated)

Verified main-loop timeline: **opus-4-7** (05-26 → 05-28, inception) →
**opus-4-8** (05-28 → 06-12, the bulk), with **fable-5** interleaved over the
final days (06-10 onward) and producing the latest turns — including this
analysis, while the in-context environment text still claimed Opus 4.8 (the
per-message `usage.model` is the ground truth; the *Fable 5* commit trailer
turned out to be the accurate artifact).

| model | msgs | input | output | cache read | cache write | est. cost |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| claude-fable-5 | 2,097 | 0.38 M | 1.95 M | 1.08 B | 6.3 M | $1,891 \* |
| claude-opus-4-8 | 2,493 | 0.17 M | **2.64 M** | 0.90 B | 12.6 M | $1,788 |
| claude-opus-4-7 | 727 | 0.00 M | 0.79 M | 0.34 B | 8.5 M | $734 |
| **total** | **5,320** | **0.55 M** | **5.38 M** | **2.33 B** | **27.3 M** | **~$4,413** |

\* Opus pricing assumed (see above).

## Per-message token distribution (unique API messages, n = 5,320)

| series | mean | std | variance | median | p95 | p99 | max |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| output tokens / msg | 1,011 | 1,526 | 2.33 M | 515 | 3,620 | 7,250 | 27,782 |
| context / msg (input + cache read + cache write) | 442 K | 294 K | 8.64 × 10¹⁰ | 449 K | 905 K | 961 K | 999 K |

Readings: output is heavily right-skewed (median 515 vs p99 7,250 — most
calls are short tool-step responses; the tail is long prose/code generation).
Context per call averages ~442 K tokens and the p99 sits at ~961 K — the
session habitually ran near a ~1 M-token context window, which is why cache
reads dominate the bill.

## Per-prompt distribution (human turns, n = 647 \*)

Everything one human prompt triggered until the next prompt — all API calls
(sub-agents included), summed per turn:

| series | mean | std | variance | median | p95 | p99 | max |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| output tokens / prompt | 8,328 | 13,854 | 1.92 × 10⁸ | 3,701 | 32,245 | 58,951 | 181,427 |
| API calls / prompt | 8.2 | 12.9 | 167 | 4 | 32 | 60 | 136 |
| est. cost ($) / prompt | 6.83 | 11.53 | 133 | 2.75 | 26.47 | 50.20 | 116.73 |
| active time (min) / prompt | 4.0 | 7.6 | 58 | 1.5 | 15.9 | 38 | 81 |

\* The turn detector counts a few interrupted/command-only prompts the
headline 643 excludes; 29 turns triggered zero API calls (interrupts,
consecutive inputs). Consistency: 647 × means reproduce the deduplicated
totals (output ≈ 5.39 M, calls ≈ 5.3 K, cost ≈ $4.4 K, active time ≈ 43 h). ✓

Readings: the per-prompt view is even more skewed than the per-message one —
the **median turn is 1.5 minutes, ~3.7 K output tokens, $2.75** (a quick
directive, a few tool steps). Cost concentration: the top 10% of turns carry
49% of the cost; the bottom half, 8.3% (~$366 of $4,413).

The two tails, identified (they are *different* turns):

- **Cost tail**: the priciest turns are all short "go" approvals that
  unleashed long tool chains — #1 is *"이대로 진행"* (05-28, $116.73, 79 API
  calls in 28 min; the cost is 79 near-full-context cache reads, not the
  $2.35 of output), followed by the C-ABI client directive ($100), *"cpp
  stock gRPC 는 TONIC"* ($97, 136 calls, the single biggest output turn at
  181 K), and *"구현으로 가자!!!!!!!"* (the mailbox implementation, $80).
- **Duration tail** (the reason the time metric was changed): under the old
  prompt→last-event window method, the longest "turn" was 1,538 min — but
  that was a compaction-boundary turn (06-05 02:49) with 39 sparse calls
  trickling across ~25 h of human absence, and the 1,530-min runner-up was
  the human leaving for work (05-30 *"출근해야할수있어..."*, 10 calls).
  Window duration measured "how long the agent was intermittently alive",
  not work. The table above therefore uses **active time** (gaps > 5 min
  excluded), under which the longest turn is a sane **81 minutes** and the
  total is ~43 h instead of the window-based ~164 h.

## Daily activity (output tokens, deduplicated)

```
05-26    442k ██████████▌     inception: core + FFI sprint
05-27     98k ██▌             backpressure design, review loop
05-28    257k ██████▌         C ABI tests, overnight agents
05-29     20k ▌               (mostly idle)
05-30     58k █▌              threading design discussion
05-31     28k ▌               thread-safety branch
06-01    108k ██▌             multithread verification
06-03    104k ██▌             cargo release prep
06-04    405k ██████████      CI, README, real-server migration
06-05    128k ███             release checklist, safe Rust crate
06-06    109k ██▌             tokio, bundled nghttp2, build guide
06-07     34k █               artifact boundary design
06-10  1,262k ███████████████████████████████▌
06-11    956k ███████████████████████▌        tests/example reorg, overnight loops
06-12  1,297k ████████████████████████████████  mailbox, health, forensics
06-13     72k █▌              authorship records (this file + companions)
```

## Unit economics (nominal, corrected)

| unit | value |
| --- | ---: |
| per active hour (~43.3 h) | ~$102 |
| per commit (254) | ~$17 |
| per shipped line (~36 K) | ~$0.12 |
| output tokens per commit | ~21 K |

## Structural observations

1. **Cache reads dominate the cost** — ~$3.5 K of ~$4.4 K (**~79%**) is the
   2.33 B cache-read tokens: a near-full (~1 M) context re-read on almost
   every of 5,320 calls. Actual generation (5.38 M output, ~$0.4 K) is a
   rounding error by comparison.
2. **The last three active days (06-10 → 06-12) produced ~65% of all output
   tokens** — verification, reorganization, documentation, and forensics
   out-tokened the core implementation.
3. **Three models, one session**: opus-4-7 (inception) → opus-4-8 (the bulk)
   with fable-5 interleaved at the end; the work is continuous across
   hand-offs, and an agent's in-context self-description can lag the actual
   serving model — `usage.model` is the ground truth.
4. **This file itself needed the project's own discipline**: its first
   version shipped a 2.5× overcount (streaming-chunk double-counting) and a
   wrong model timeline (inferred, not verified); both were corrected only
   under follow-up questioning. Measure, then claim — even about yourself.

---

## 한국어 요약

- **단위 구분**: 사람 프롬프트 **643** / 고유 API 메시지 **5,320**(프롬프트당
  ~8.3회 — 툴 루프) / 로그 엔트리 10,690(스트리밍 청크; 초판 과대계상의 원인).
- **토큰(dedup)**: input 0.55M / output **5.38M** / cache read **2.33B** /
  cache write 27.3M. **비용 ~$4,413** (API 정가; 단가표·근거 위 표 참조 —
  Opus 계열 $15/$75/M, cache read 0.1×, write 1.25×; **Fable 5는 단가 미공지로
  Opus 가정**; 구독 플랜이면 실지불과 무관한 명목 가치).
- **메시지 단위 분포** (n=5,320): output mean 1,011 / std 1,526 / var 2.33M /
  중앙값 515 / p95 3,620 / p99 7,250 / max 27,782 — 강한 우측 꼬리(대부분 짧은
  툴 스텝, 꼬리는 긴 생성). 컨텍스트는 호출당 평균 ~442K, p99 ~961K — 거의 1M
  컨텍스트를 상시 사용, cache read가 비용을 지배하는 이유.
- **프롬프트 단위 분포** (n=647): output/턴 mean 8,328·med 3,701·p99 58,951·max
  181K; API 호출/턴 mean 8.2·med 4·max 136; 비용/턴 mean $6.83·med $2.75·max
  $117; 활성시간/턴 mean 4.0분·**med 1.5분**·p95 16분·max 81분. 중앙값 턴은
  "1.5분, $2.75짜리 빠른 지시"이고 상위 1% 턴(밤샘 루프·대형 구현)이 무거운 일을
  함. 647×평균이 dedup 총계를 재현함(정합 ✓).
- **시간**: 활성 작업시간 ~43.3h (간격 5분 초과는 대기로 제외; cap 2~30분에서
  37~62h). 프롬프트당 평균 4.0분, 최장 턴 81분. 단위 경제: 활성시간당 ~$102,
  커밋당 ~$17, 줄당 ~$0.12.
- **정정 기록**: 이 문서 초판은 2.5× 과대계상(스트리밍 청크 중복 합산) + 틀린
  모델 타임라인(추정)이었고, 후속 질문에 의해 교정됨 — 자기 자신에 대해서도
  "측정 후 주장"이라는 이 프로젝트의 규율이 필요했다.
