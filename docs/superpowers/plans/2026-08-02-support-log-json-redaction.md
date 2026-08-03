# Support Log JSON Redaction

## Goal

Keep redacted diagnostic JSONL valid when a string contains a Windows drive path serialized with
doubled backslashes.

## Test-Driven Steps

1. Add a failing support-export regression that redacts a serialized JSON log record containing a
   Windows path, reparses it, and proves the private path is gone.
2. Make the shared path redactor consume one or more adjacent backslashes at every Windows path
   separator so the same rule handles plain text and JSON-escaped text.
3. Run the focused support test, the application tests, full workspace tests, formatting, and
   warning-denied workspace Clippy.
4. Update `handoff.md`, commit the fix, and relaunch the development app.

## Constraints

- Preserve the current support bundle schema and JSONL structure.
- Do not weaken path, identity, credential, email, or URL-query redaction.
- Add no dependency or parallel JSONL implementation for a regex boundary bug.
- Keep the unrelated untracked `paseo.json` out of every commit.
