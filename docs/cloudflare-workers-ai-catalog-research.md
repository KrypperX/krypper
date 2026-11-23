# Cloudflare Workers AI catalog source

Research checked on 2026-11-23. This note records source options and their limits; it does not add runtime catalog syncing.

## Public machine-readable catalog

The best no-credential source found is Cloudflare's JSON feed at [`https://ai-cloudflare-com.pages.dev/api/models`](https://ai-cloudflare-com.pages.dev/api/models). An anonymous GET returned HTTP 200 with a top-level `models` array and 76 records on the date checked. Cloudflare's own docs refresh script fetches this exact URL with plain `fetch()` and no authorization header, then writes `data.models` as one JSON file per model into the public `cloudflare/cloudflare-docs` repository ([sync script](https://github.com/cloudflare/cloudflare-docs/blob/production/bin/fetch-ai-models.js)).

Each record has fields including `name`, `id`, `description`, `task`, `properties`, `tags`, `schema`, `created_at`, and `deprecated`. For example, `name` is the runnable Workers AI model ID (such as `@cf/meta/llama-4-scout-17b-16e-instruct`); `task` carries a task name and description; and `properties` is a list of `{property_id, value}` entries. Depending on the model, properties include `context_window`, `function_calling`, `vision`, `reasoning`, `beta`, `require_workers_paid`, and `planned_deprecation_date`. The per-model `schema` carries request input/output schemas. These properties are optional/model-specific, so consumers should preserve unknown fields and treat missing values as unknown rather than false. See the [Cloudflare docs collection schema](https://github.com/cloudflare/cloudflare-docs/blob/production/src/content.config.ts) and an [example model record](https://github.com/cloudflare/cloudflare-docs/blob/production/src/content/workers-ai-models/llama-4-scout-17b-16e-instruct.json).

The docs repository is a useful public snapshot, not the live feed: its sync script writes the API response into `src/content/workers-ai-models/*.json`, skips records whose planned deprecation date has passed, and keeps the last good files if a fetch fails. The collection loader reads one JSON file per model. The script does not document the Pages URL as a versioned public API, so treat it as an implementation source that Cloudflare currently uses for its docs, not as a stability-guaranteed contract.

## Documented account API

Cloudflare also documents `GET /accounts/{account_id}/ai/models/search` as searching Workers AI models. It requires an account ID and API authentication; accepted permissions include Workers AI Read and Workers AI Metadata Read. The route supports `search`, `task`, `author`, `source`, pagination, `hide_experimental`, `include_deprecated`, and optional `format=openrouter`. Cloudflare documents the ordinary `result` as an array of unknown objects, so the public JSON feed and docs record schema are easier to consume when a no-account source is needed. See the [Model Search API reference](https://developers.cloudflare.com/api/resources/ai/subresources/models/methods/list/).

## Catalog entries do not prove account access

Cloudflare's model catalog is discovery metadata, not an account entitlement check. The pricing docs say some models require a Workers Paid plan or prepaid AI Gateway credits; the changelog gives examples where free-plan calls return 403. A record's `require_workers_paid` property can inform a UI, but a catalog entry alone does not prove that a particular account, plan, billing setup, agreement, or capacity state can invoke it. See [Workers AI pricing](https://developers.cloudflare.com/workers-ai/platform/pricing/) and the [Workers AI changelog](https://developers.cloudflare.com/changelog/product-group/ai/4/).

## Comparison with models.dev in this repo

[models.dev](https://models.dev/) publishes JSON endpoints for provider data, provider-agnostic model metadata, and a combined catalog, backed by provider-organized TOML source files. This checkout already vendors a pinned `snapshots/models-dev/api.json` snapshot and maps common fields such as context/output limits, modalities, tool calling, reasoning, and status in [`models-dev-catalog.md`](models-dev-catalog.md).

Use the existing models.dev snapshot for offline, cross-provider discovery and common model metadata. Use Cloudflare's feed for Workers AI-specific IDs, task labels, request schemas, context and capability properties, and deprecation or paid-plan metadata. The Cloudflare feed is narrower and more provider-authoritative; neither source is proof of account-specific inference access.

The current Krypper change keeps the pinned cross-provider snapshot as its catalog data. `cloudflare` selects the `cloudflare-workers-ai` source; `cloudflare-ai-gateway` selects both `cloudflare-ai-gateway` and `cloudflare-workers-ai` records because the unified REST endpoint routes third-party and Workers AI model IDs. Calls through AI Gateway use the documented `cf-aig-gateway-id` header. Krypper does not fetch either catalog at runtime. See Cloudflare's [AI Gateway REST API guide](https://developers.cloudflare.com/ai-gateway/usage/rest-api/) for supported paths and IDs.

## Recommendation

For the existing offline, multi-provider catalog, keep models.dev as the common snapshot. If Workers AI needs richer metadata, import the public Cloudflare JSON feed as provider-specific supplemental data and preserve its optional properties/status fields. Use the account-scoped search API only when authenticated account context is already available; still present results as catalog entries rather than guaranteed inference availability.
