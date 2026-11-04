# 0037 — Bedrock, Vertex, Gemini and Azure OpenAI providers; embeddings

Status: accepted (2026-11-04)

**Context.** The gateway spoke the OpenAI wire format and Anthropic's. That covers most self-hosted servers (vLLM, Ollama, SGLang) and most API vendors, but not the two clouds whose auth isn't a bearer key, nor Azure's deployment URLs. And agents doing retrieval had no way to get embeddings through the same budget and record as their chat calls.

**Decision.** Four more `ProviderConnection.spec.provider` values and one more gateway op.

| provider | spec | the secret's value | wire |
|---|---|---|---|
| `bedrock` | `region` | `KEY_ID:SECRET[:SESSION_TOKEN]` | Converse API, SigV4-signed (`clankr_gateway::aws`, forty lines, proven by the AWS test-suite vector) |
| `vertex` | `project`, `region` | a service-account JSON key | Gemini body under the project; the key is exchanged for an OAuth access token (RS256 JWT bearer grant), cached per connection until it nears expiry |
| `gemini` | — | API key | `generateContent` with `x-goog-api-key` |
| `azure-openai` | `endpoint` (the resource) | API key | OpenAI body at `/openai/deployments/{model}/…`, `api-key` header; the model is the deployment name |

- **Embeddings.** `POST /v1/namespaces/{ns}/models/{claim}/embed {inputs}` → `{embeddings, usage, provider, model}`; agent op `embed`; SDK `ctx.models.embed(claim, inputs)` in Python, TypeScript and Go. Same claim, allowlist, reservation (estimated input tokens, settled to the provider's count) and `model_calls` row (`stop_reason = 'embeddings'`) as `generate`. OpenAI-compatible, Azure, Gemini, Vertex and Bedrock Titan serve it; Anthropic and replay refuse with `UnsupportedCapability`; mock returns deterministic 8-dimensional vectors.
- **Credentials** still never leave the control plane. For Vertex, neither the key nor the minted token does.

**Consequences.**
- One more place to keep in step with vendors' wire formats; the tests pin each shape against a fake endpoint.
- Not yet: Bedrock Cohere embeddings (a different body), Vertex's partner models under other publishers, streaming, tool-use passthrough, and fallback chains between connections.
