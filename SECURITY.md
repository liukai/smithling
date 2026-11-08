# Security

## What tracedb is built to be

A **single-user, local** tool. Its safety model is "nothing leaves your machine", not "safe to expose":

- It listens on `127.0.0.1` by default and makes no outbound connections (checked by `scripts/no-outbound.sh`).
- It has **no login and no access control**. Anyone who can reach the port can read every trace and upload new ones.
  Don't bind it to a public interface (`--bind 0.0.0.0`) or put it behind a public proxy without adding your own
  authentication in front.
- Traces often contain prompts, tool outputs and sometimes credentials your agent handled. They are stored as sent,
  in plain files under `--data` (or your S3 bucket and Postgres in cloud mode). Protect that folder like the data in
  it. Delete and retention aren't built yet (roadmap backlog B1).
- Passwords in `--metastore` URLs and `AWS_*` values are redacted from logs; the LangSmith API key the SDK sends is
  ignored and never stored.

## Reporting a vulnerability

Please use GitHub's **private vulnerability reporting** ("Security" tab → "Report a vulnerability") rather than a
public issue. Include what you ran, what happened and what you expected.
