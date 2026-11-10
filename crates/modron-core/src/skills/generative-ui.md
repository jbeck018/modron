---
name: generative-ui
description: Use when the answer contains a table, chart, or a set of choices the user should fill in - emit a modron-ui fenced block so clients render it natively
---

# Generative UI

# generative-ui

Emit rich UI (tables, charts, forms) that modron's clients render
natively instead of prose.

## When to use

- A comparison or inventory the user will scan -> **table**.
- Quantities the user will compare -> **chart** (bar for magnitudes, pie
  for shares of a whole, line for trend series).
- You need structured answers back (choices, configuration) -> **form**;
  the GUI renders fillable inputs and the user's submit arrives as their
  next message, one `label: value` per line.

## How

End your reply with a fenced block tagged `modron-ui` containing ONE
JSON object:

~~~
```modron-ui
{"type": "table", "title": "Deps", "columns": ["crate", "version"],
 "rows": [["serde", "1.0"], ["tokio", "1"]]}
```
~~~

Forms:

~~~
```modron-ui
{"type": "form", "title": "Deploy", "fields": [
  {"name": "env", "label": "Environment", "kind": "select",
   "options": ["dev", "prod"]},
  {"name": "note", "kind": "text", "placeholder": "optional note"}]}
```
~~~

Charts:

~~~
```modron-ui
{"type": "chart", "kind": "bar", "points": [{"label": "rust", "value": 42},
 {"label": "ts", "value": 17}]}
```
~~~

## Rules

- One widget per block; several widgets = several blocks.
- Field kinds: `text`, `number`, `select` (with `options`), `check`.
- Chart kinds: `bar`, `line`, `pie`. Values are numbers.
- Keep the JSON on valid JSON (no comments, no trailing commas). A
  malformed block shows as raw text to the user.
- Surround the block with the prose that explains it; do not put
  explanations inside the JSON.
- If the user asks for "a table/chart/form of ...", prefer this over
  markdown approximations.

Ported as a modron built-in from the superpowers methodology (MIT, github.com/obra/superpowers).
