# Exa search provider

Exa support is owned by the bundled `bcode.web-search` plugin. Bcode's generic tool and
runtime packages do not interpret Exa requests or responses.

## Configure authentication

The preferred integrated flow uses Bcode's generic plugin-auth CLI:

```sh
bcode auth providers          # includes exa when bcode.web-search is enabled
bcode auth login exa          # securely prompts for and stores the key
bcode auth status exa
```

This requires neither an exported key nor an `sshenv run` wrapper. The web-search plugin
registers Exa dynamically, while Bcode owns enrollment, secure storage, ownership checks,
and invocation-scoped secret delivery.

To force Exa instead of automatic provider selection, configure:

```toml
[web_search]
provider = "exa"
```

Environment-based compatibility remains available. Keep the key outside the repository:

```sh
export EXA_API_KEY="..."
export BCODE_WEB_SEARCH_PROVIDER="exa" # optional; forces Exa over earlier auto providers
```

Or reference the environment explicitly from Bcode configuration:

```toml
[web_search]
provider = "exa"

[web_search.providers.exa.api_key]
backend = "env"
name = "EXA_API_KEY"
```

Credential precedence is explicit configured reference, selected integrated auth profile,
then conventional `EXA_API_KEY` fallback. Run `web.status` to confirm Exa is selected.
Status reports availability, credential source, and owner, but never the key value.

## Searches

A basic request uses bounded highlights, which are efficient for agent workflows:

```json
{
  "query": "recent Rust async runtime developments",
  "provider": "exa",
  "max_results": 5
}
```

Generic domain and freshness fields are translated to Exa's native filters:

```json
{
  "query": "language model inference",
  "provider": "exa",
  "site": "arxiv.org",
  "freshness": "month"
}
```

For Exa, `freshness` accepts `day`, `week`, `month`, or `year`. `region` accepts a
two-letter country code. Exa does not support the generic `safe_search` input, so the
plugin rejects it rather than ignoring it.

## Provider options

`provider_options` is accepted only when Exa is selected. Unknown fields are rejected.
Supported options are:

- `search_type`: `auto`, `fast`, `instant`, `deep-lite`, `deep`, or `deep-reasoning`
- `category`: `company`, `people`, `publication`, `news`, `personal_site`, or
  `financial_report`
- `include_domains` and `exclude_domains`
- `start_published_date`, `end_published_date`, `start_crawl_date`, and
  `end_crawl_date` as ISO 8601 dates/timestamps
- `include_text` and `exclude_text` (at most one value each, matching Exa limits)
- `content`: `highlights` (default), `text`, or `summary`
- `max_characters`: text-content limit from 1 through 20,000
- `max_age_hours`: Exa content cache age (`0` requests live content; `-1` is cache-only)

Example:

```json
{
  "query": "recent work on agent memory",
  "provider": "exa",
  "max_results": 5,
  "provider_options": {
    "search_type": "fast",
    "category": "publication",
    "include_domains": ["arxiv.org"],
    "content": "highlights",
    "max_age_hours": 24
  }
}
```

`company` and `people` searches reject publication-date filters and excluded domains,
matching Exa's API restrictions. Generic `site` may be combined only with an identical
`include_domains` entry; conflicting filters fail before making a request.

Full text and especially per-result summaries can cost more than highlights. Keep result
counts low on a free plan and request those modes only when needed.

## Live smoke testing

Normal tests never call Exa. To smoke-test manually, enroll through `bcode auth login exa`
(or explicitly provide `EXA_API_KEY` for compatibility), rebuild `bcode_web_search_plugin`,
check `bcode auth status exa` and `web.status`, and make one low-result explicit Exa search.
Never put the key in command arguments, fixtures, snapshots, or logs. Authentication failures
identify the missing credential; quota failures report the limit without including provider
response secrets.
