---
name: writing-skills
description: Use when creating or editing SKILL.md skills
---

# Writing Skills

A skill is a trigger + a procedure. It earns its index line by being
found at the right moment and obeyed.

## Rules

1. **Frontmatter:** `name` (kebab-case) and a `description` that says
   WHEN to use it ("Use when..."), not what it is - the description is
   the model's retrieval cue; make it match the words the moment uses.
2. **Body = procedure:** imperative steps, the reasoning behind each
   constraint, and the failure modes the rules prevent. No essays.
3. **Progressive disclosure:** the body loads on demand via the `skill`
   tool; put references/examples toward the end so the operative rules
   come first.
4. **One skill, one job.** Two triggers? Two skills.
5. **Test it:** does a fresh session pick the skill at the right moment
   from the description alone? Rewrite until it does.

Repo-local skills live in `.modron/skills/<name>/SKILL.md` (or
`.claude/skills/`); repo skills with the same name OVERRIDE the
built-ins.

Adapted from Jesse Vincent's superpowers (MIT): github.com/obra/superpowers
