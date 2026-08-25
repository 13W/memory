---
type: regex
pattern: "ORT_DYLIB_PATH"
match: contains
target: last_message
---
The sentinel is planted in the seeded store by the scaffold script and appears nowhere
in the fixture working tree. Exact-match ground truth by construction: no LLM judge,
no rubric drift, and a wrong answer is wrong for one reason only.
