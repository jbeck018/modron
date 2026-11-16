# Reasoning replay: live check against the OpenAI Responses API (H7b-3)

Provider under test: `openai-responses` (API key, `OPENAI_BASE_URL`), the same
request and stream code as `chatgpt`, with its own block scope. Model
`gpt-5-mini`, `reasoning.effort = low`, `--yolo`, `store: false`.
Traffic went through the loopback forwarder (127.0.0.1:18090/v1); the key
was never read or printed. Debug capture used `MODRON_DEBUG_REQUEST_DIR`
(debug only, off by default, writes the JSON body only, never headers).

## What was run

1. Run 1: task "read a.txt, write its line count into b.txt, read b.txt and
   say what it says". Result: exit 0, 10 model turns, 8 tool calls. All 10
   requests accepted (HTTP 200), 10 request bodies captured.
2. Run 2: `--resume=<id>` in a new process, one turn, no tools. Result:
   exit 0, answer "5". The first request of the new process carried all the
   earlier reasoning items, in order.
3. Control A (as sent) and control B (reasoning items removed, function
   calls and outputs kept), both from captured request 0002 (8 input items),
   POSTed directly to the forwarder with curl.

## Observed wire shapes (trimmed)

Request (API key): `POST /v1/responses`, `Authorization: Bearer <key>` only.
No `chatgpt-account-id`, `OpenAI-Beta` or `originator` header is sent.
Body keys: include, input, instructions, max_output_tokens, model,
reasoning, store, stream, tool_choice, tools.

Input order of a later request (run 1, request 0009):

    message(user) R FC FO R FC FO ... R message(assistant) message(user) R FC FO message(user)

R = reasoning item, FC = function_call, FO = function_call_output.
Each R sits directly before the FC it preceded. Byte check: 9 of 9 reasoning
items sent in request 0009 are byte-equal to the blocks stored in the session
JSONL.

Reasoning item (sent back as-is; keys observed):

    {"type":"reasoning","id":"rs_...","summary":[],"content":[],
     "encrypted_content":"<2-3k chars>"}

No `status` key. `summary` was empty in these runs (no summary requested).

Response stream: `response.output_item.done` with `item.type=reasoning` at
`output_index 0`, then `function_call` items with `call_id`. `response.completed`
usage: `input_tokens`, `input_tokens_details.cached_tokens`, `output_tokens`,
`output_tokens_details.reasoning_tokens`.

## Results

- (i) Session JSONL: 10 assistant messages with `provider_blocks`, kind
  `reasoning`, provider `openai-responses`, api `responses`, and an
  `encrypted_content` payload. Accepted.
- (ii) Requests 0001 to 0009: earlier reasoning items present and in order
  before their function calls. Accepted.
- (iii) No 400 about reasoning items, missing items or ordering in any of the
  10 requests. The run completed.
- (iv) Resume in a new process: the first request carried 32 items including
  all earlier R items, accepted, completed.
- (v) Negative control: request 0002 as sent returned 200. With the reasoning
  items removed (function calls kept), it also returned 200. So for this
  shape the API does not require replay. Replay is not proven required here.
- (vi) Usage: control A and control B both reported input_tokens 2843 and
  cached_tokens 2816. Output reasoning_tokens was 64 (A) and 0 (B). One sample
  each, and the cache order was not randomized, so this is not a conclusion
  about cost or quality.

## Bugs found

None in the replay path. The only change needed was adding the API-key mode.
The `chatgpt` path is unchanged, and its tests pass without edits.

## Not verified

- Anthropic thinking-block replay: no Anthropic key is available.
- The `chatgpt` backend against the live service (needs a browser sign-in).
- Reordering control (reasoning placed after its call) and a reasoning-free
  multi-turn A/B on the full task: not run.
- Whether replay changes answer quality or reasoning token use; the only
  data is the single-sample control above.
- Long sessions and compaction with replayed items.
