# fornax-ci

Turns Git events into CI pipelines (C-003): reads `.fornax/` at the pushed commit through
`fornax-git`, evaluates triggers and `if:` conditions with `fornax-spec`, and creates
pipelines and jobs through `fornax-db`. Defines the versioned job payload (`JobPayload`)
that the runner server turns into protocol messages.

Boundary: no SQL and no `git` processes of its own — it only calls `fornax-db` and
`fornax-git`.
