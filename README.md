# gym-mcp

A remote MCP server that turns Claude or ChatGPT into your gym logbook. You say *"I arrived at the NDSM Trainmore"*, send a photo plus *"12 on 13kg"*, and it logs the workout. Ask *"what should I do next?"* and it recommends from your history and what that gym has.

Rust (rmcp, axum, sqlx) on plain Postgres (no extensions). Your Authelia handles OAuth.

## Model

| | |
|---|---|
| **Gym** | name + aliases, `equipment` (free text: "2 leg presses (max 300 kg), dumbbells 2–40 kg, no sled") and notes |
| **Exercise** | `kind` (strength or cardio), muscle groups from a fixed list, `equipment` it needs (free text; empty = bodyweight) and a description |
| **Workout** | one visit to one gym; at most one open at a time |
| **Set** | reps / weight / duration, cardio `distance_m` and free-text `intensity` ("level 12"), optional effort (`rpe` 1–10 in half steps) and `rest_s`; `performed_at` (when it happened) is separate from `logged_at` |
| **Settings** | timezone and training `goal` (strength / hypertrophy / endurance / general) |

**Recommendations.** `training_overview` gives Claude the gym's equipment text and, per exercise, its equipment text and the whole last session (reps, weight, RPE, rest), plus your goal. Claude judges what's possible at the gym by reading the two texts, and suggests progression from the last session.

**Weights** are the number on the implement: per dumbbell or kettlebell; barbell = total including the bar; machine = the stack setting.

**Time.** Every timestamp is stored as `timestamptz`. Each request runs with the Postgres session `TimeZone` set to the user's timezone (`settings_update`, default `DEFAULT_TIMEZONE`). As a result:
- local input like `2026-11-02 19:40` is read in that timezone;
- "today" is the local date;
- weeks are ISO weeks (starting Monday) and months are calendar months, both local;
- output is RFC 3339 with the local offset.

**Workout lifecycle**
- `workout_start` at the gym that already has the open workout resumes it.
- `workout_start` at a different gym is refused unless you pass `finish_open: true`.
- A workout is stale when its last activity is older than `WORKOUT_STALE_AFTER`. The next workout action (start, finish, `set_add`) closes a stale workout first: one with sets is finished at its last set; one with no sets and no notes is deleted. Read tools (`workout_current`, `training_overview`) show the workout as closed but don't save that.
- Past sessions are entered with `workout_add_past`, then `set_add` with a `workout_id`.

## Tools

| Group | Tools |
|---|---|
| Setup | `settings_get/update`, `gym_create/update/get/list`, `exercise_create/update/search` |
| Workout | `workout_start/finish/update/current/add_past`, `workout_list`, `set_add/update/delete` |
| Analysis | `training_overview`, `search`, `set_search`, `progress`, `personal_records`, `exercise_alternatives` |

Every tool carries MCP annotations, so clients can auto-approve reads and warn on deletes: a title, `readOnlyHint` (read tools run in a `READ ONLY` Postgres transaction, so the database enforces it), and `destructiveHint`/`idempotentHint` on the rest. All tools are `openWorldHint: false`, since they only touch this database.

Wherever a tool takes a gym or exercise, you can give an id or a name. Names are matched in any word order and with typos tolerated ("trianmore ndsm"). An ambiguous or unknown name returns an error listing the candidates, so the model asks you instead of guessing.

## Run locally

```sh
docker compose up --build        # Postgres + server on :8080, auth disabled (DEV_SUB=dev)
npx @modelcontextprotocol/inspector   # connect to http://localhost:8080/mcp
```

Tests need a Postgres user that can create databases (the compose one can):

```sh
docker compose up -d db
DATABASE_URL=postgres://gym:gym@localhost:5432/gym cargo test
```

## Configuration

| Env | |
|---|---|
| `DATABASE_URL` | required; migrations run on startup. For an encrypted-only Postgres add `?sslmode=verify-full` (or `require` to skip certificate checks). A private CA goes in `&sslrootcert=/path/ca.crt`, or set `SSL_CERT_FILE`; otherwise the system CA store is used. The default `prefer` falls back to plaintext |
| `PUBLIC_URL` | required; e.g. `https://gym.example.com`. Used as the OAuth `resource`, the required token `aud`, and the allowed `Host` |
| `OIDC_ISSUER` | required unless `DEV_SUB` is set; your Authelia URL, e.g. `https://auth.example.com` |
| `BIND_ADDR` | default `0.0.0.0:8080` |
| `DEFAULT_TIMEZONE` | default `Europe/Amsterdam` |
| `WORKOUT_STALE_AFTER` | default `3h` (`90m` and `45s` also work) |
| `DEV_SUB` | **local only**: turns off auth and acts as this user |

Endpoints: `/mcp` (Streamable HTTP, stateless), `/.well-known/oauth-protected-resource`, `/healthz`. Put TLS in front, e.g. your ingress.

## Authelia

The server is an OAuth *resource server*. It validates Authelia's JWT access tokens (signature via JWKS, `iss`, `aud` = `PUBLIC_URL`, `exp`) and keys all data by the token's `sub`. Authelia has no dynamic client registration, so register one client per assistant and paste its ID and secret into the connector's advanced settings.

```yaml
identity_providers:
  oidc:
    clients:
      - client_id: 'gym-mcp-claude'
        client_name: 'Gym logbook (Claude)'
        client_secret: '$pbkdf2-sha512$...'      # authelia crypto hash generate pbkdf2
        authorization_policy: 'two_factor'
        require_pkce: true
        pkce_challenge_method: 'S256'
        redirect_uris: ['https://claude.ai/api/mcp/auth_callback']
        scopes: ['openid', 'offline_access']
        grant_types: ['authorization_code', 'refresh_token']
        response_types: ['code']
        audience: ['https://gym.example.com']   # = PUBLIC_URL
        requested_audience_mode: 'implicit'      # Claude sends `resource`, not `audience`
        access_token_signed_response_alg: 'RS256' # JWT access tokens instead of opaque ones
        token_endpoint_auth_method: 'client_secret_post'
      # ChatGPT: the same, with client_id 'gym-mcp-chatgpt' and
      # redirect_uris: ['https://chatgpt.com/connector_platform_oauth_redirect']
```

This needs Authelia ≥ 4.39 for JWT access tokens. Check the redirect URIs against Claude's and ChatGPT's current docs.

Then, in Claude: **Settings → Connectors → Add custom connector**, with URL `https://gym.example.com/mcp` and the client ID/secret under advanced settings.

## Releases

Commits on `main` follow [Conventional Commits](https://www.conventionalcommits.org/). On every push to `main`, CI tests, lints, and builds the image for amd64 and arm64. If that passes, [release-plz](https://release-plz.dev):
1. opens or updates a release PR with the version bump and `CHANGELOG.md`;
2. when that PR is merged, tags `vX.Y.Z` and creates the GitHub release;
3. then dispatches `release.yml`, which pushes the multi-arch image with build provenance.

Versions come from git tags (`git_only`); the crate is not published to crates.io.

```sh
docker pull ghcr.io/stephanmeijer/gym-mcp:latest   # or :X.Y.Z / :X.Y
```

## License

[MPL-2.0](LICENSE)
