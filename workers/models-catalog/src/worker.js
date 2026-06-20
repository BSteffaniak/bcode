export default {
  async fetch(request, env, ctx) {
    return handleRequest(request, env, ctx);
  },
};

const PROVIDER_REGISTRY = {
  bedrock: {
    providerId: 'bedrock',
    discover: discoverBedrock,
  },
};

async function handleRequest(request, env, ctx) {
  const url = new URL(request.url);
  try {
    if (url.pathname === '/api/v1/catalog') {
      return jsonResponse(await mergedCatalog(request, env, ctx));
    }
    if (url.pathname === '/api/v1/catalog/curated') {
      return jsonResponse(await loadCuratedCatalog(request, env));
    }
    if (url.pathname.startsWith('/api/v1/live/')) {
      const providerId = decodeURIComponent(url.pathname.slice('/api/v1/live/'.length));
      const snapshot = await getLiveSnapshot(providerId, env, ctx, { allowBlockingRefresh: true });
      if (!snapshot) return jsonResponse({ error: 'live snapshot unavailable' }, 404);
      return jsonResponse(snapshot);
    }
    if (url.pathname.startsWith('/api/internal/refresh/')) {
      if (request.method !== 'POST') return jsonResponse({ error: 'method not allowed' }, 405);
      if (!authorized(request, env)) return jsonResponse({ error: 'unauthorized' }, 401);
      const providerId = decodeURIComponent(url.pathname.slice('/api/internal/refresh/'.length));
      return jsonResponse(await refreshProvider(providerId, env, { force: true }));
    }
    if (url.pathname === '/api/internal/health') {
      if (!authorized(request, env)) return jsonResponse({ error: 'unauthorized' }, 401);
      return jsonResponse({ ok: true, dynamic_providers: Object.keys(PROVIDER_REGISTRY) });
    }
    if (url.pathname === '/' || url.pathname === '/home') {
      return htmlResponse(renderShell());
    }
    if (env.ASSETS) return env.ASSETS.fetch(request);
    return jsonResponse({ error: 'not found' }, 404);
  } catch (error) {
    console.error(error);
    return jsonResponse({ error: 'internal server error' }, 500);
  }
}

async function mergedCatalog(request, env, ctx) {
  const catalog = await loadCuratedCatalog(request, env);
  const live = {};
  for (const providerId of dynamicProviderIds(catalog, env)) {
    const snapshot = await getLiveSnapshot(providerId, env, ctx, { allowBlockingRefresh: true });
    if (snapshot) {
      live[providerId] = snapshot;
      mergeSnapshot(catalog, snapshot);
    }
  }
  catalog.live = liveSummary(live, env);
  return catalog;
}

async function loadCuratedCatalog(request, env) {
  if (env.CURATED_CATALOG_JSON) return JSON.parse(env.CURATED_CATALOG_JSON);
  if (env.ASSETS) {
    const assetUrl = new URL('/v1/catalog.json', request.url);
    const response = await env.ASSETS.fetch(new Request(assetUrl, request));
    if (response.ok) return response.json();
  }
  throw new Error('curated catalog asset /v1/catalog.json is unavailable');
}

function dynamicProviderIds(catalog, env) {
  const configured = csv(env.DYNAMIC_PROVIDERS);
  if (configured.length > 0) return configured.filter((id) => PROVIDER_REGISTRY[id]);
  return Object.entries(catalog.providers || {})
    .filter(([id, provider]) => id in PROVIDER_REGISTRY || provider.kind === 'bedrock')
    .map(([id]) => id)
    .filter((id) => PROVIDER_REGISTRY[id]);
}

async function getLiveSnapshot(providerId, env, ctx, options = {}) {
  const cached = await readSnapshot(providerId, env);
  const state = snapshotState(cached, env);
  if (state === 'fresh') return cached;

  if (state === 'soft_stale') {
    ctx.waitUntil(refreshProvider(providerId, env).catch((error) => console.error(error)));
    return cached;
  }

  if (options.allowBlockingRefresh) {
    try {
      return (await refreshProvider(providerId, env)).snapshot;
    } catch (error) {
      console.error(error);
      return cached;
    }
  }
  return cached;
}

function snapshotState(snapshot, env) {
  if (!snapshot) return 'missing';
  const generatedAt = Date.parse(snapshot.generated_at || '');
  if (!Number.isFinite(generatedAt)) return 'hard_stale';
  const ageSeconds = (Date.now() - generatedAt) / 1000;
  if (ageSeconds <= numberEnv(env.LIVE_FRESH_FOR_SECONDS, 900)) return 'fresh';
  if (ageSeconds <= numberEnv(env.LIVE_MAX_STALE_SECONDS, 21_600)) return 'soft_stale';
  return 'hard_stale';
}

async function readSnapshot(providerId, env) {
  if (!env.LIVE_SNAPSHOTS) return null;
  const object = await env.LIVE_SNAPSHOTS.get(snapshotKey(providerId));
  if (!object) return null;
  return object.json();
}

async function refreshProvider(providerId, env, options = {}) {
  const provider = PROVIDER_REGISTRY[providerId];
  if (!provider) throw new Error(`unknown dynamic provider: ${providerId}`);
  if (!env.LIVE_SNAPSHOTS) throw new Error('LIVE_SNAPSHOTS binding is required for refresh');

  await assertRefreshAllowed(providerId, env, options);
  const lock = await acquireRefreshLock(providerId, env);
  try {
    const snapshot = await provider.discover(env);
    const body = JSON.stringify(snapshot, null, 2);
    await env.LIVE_SNAPSHOTS.put(snapshotKey(providerId), body, {
      httpMetadata: { contentType: 'application/json' },
    });
    if (booleanEnv(env.LIVE_WRITE_HISTORY, false)) {
      await env.LIVE_SNAPSHOTS.put(historyKey(providerId, snapshot.generated_at), body, {
        httpMetadata: { contentType: 'application/json' },
      });
    }
    await writeRefreshStatus(providerId, env, {
      last_success_at: snapshot.generated_at,
      last_failure_at: null,
      last_error: null,
    });
    return { refreshed: true, provider_id: providerId, model_count: Object.keys(snapshot.models || {}).length, snapshot };
  } catch (error) {
    await writeRefreshStatus(providerId, env, {
      last_success_at: null,
      last_failure_at: new Date().toISOString(),
      last_error: publicError(error),
    });
    throw error;
  } finally {
    await releaseRefreshLock(providerId, env, lock);
  }
}

async function assertRefreshAllowed(providerId, env, options) {
  const status = await readJsonObject(refreshStatusKey(providerId), env);
  const failedAt = Date.parse((status && status.last_failure_at) || '');
  const cooldownSeconds = numberEnv(env.LIVE_REFRESH_FAILURE_COOLDOWN_SECONDS, 300);
  if (!options.force && Number.isFinite(failedAt) && Date.now() - failedAt < cooldownSeconds * 1000) {
    throw new Error(`refresh for ${providerId} is cooling down after a recent failure`);
  }
}

async function acquireRefreshLock(providerId, env) {
  const existing = await readJsonObject(refreshLockKey(providerId), env);
  const expiresAt = Date.parse((existing && existing.expires_at) || '');
  if (Number.isFinite(expiresAt) && expiresAt > Date.now()) {
    throw new Error(`refresh for ${providerId} is already running`);
  }
  const ttlSeconds = numberEnv(env.LIVE_REFRESH_LOCK_SECONDS, 120);
  const lock = {
    owner: crypto.randomUUID(),
    created_at: new Date().toISOString(),
    expires_at: new Date(Date.now() + ttlSeconds * 1000).toISOString(),
  };
  await putJsonObject(refreshLockKey(providerId), lock, env);
  return lock;
}

async function releaseRefreshLock(providerId, env, lock) {
  const existing = await readJsonObject(refreshLockKey(providerId), env);
  if (existing && existing.owner === lock.owner) {
    await putJsonObject(refreshLockKey(providerId), {
      owner: null,
      created_at: existing.created_at,
      expires_at: new Date(0).toISOString(),
    }, env);
  }
}

async function readJsonObject(key, env) {
  if (!env.LIVE_SNAPSHOTS) return null;
  const object = await env.LIVE_SNAPSHOTS.get(key);
  if (!object) return null;
  return object.json();
}

async function putJsonObject(key, value, env) {
  await env.LIVE_SNAPSHOTS.put(key, JSON.stringify(value, null, 2), {
    httpMetadata: { contentType: 'application/json' },
  });
}

function snapshotKey(providerId) {
  return `live/v1/${providerId}/latest.json`;
}

function historyKey(providerId, generatedAt) {
  return `live/v1/${providerId}/history/${generatedAt.replaceAll(':', '-')}.json`;
}

function refreshLockKey(providerId) {
  return `live/v1/${providerId}/refresh-lock.json`;
}

function refreshStatusKey(providerId) {
  return `live/v1/${providerId}/refresh-status.json`;
}

function mergeSnapshot(catalog, snapshot) {
  const provider = catalog.providers && catalog.providers[snapshot.provider_id];
  if (!provider) return;
  provider.models ||= {};
  for (const liveModel of Object.values(snapshot.models || {})) {
    const entry = provider.models[liveModel.model_id] || liveOnlyEntry(liveModel);
    if (!entry.display_name && liveModel.display_name) entry.display_name = liveModel.display_name;
    if (!entry.context_window && liveModel.context_window) entry.context_window = liveModel.context_window;
    if (!entry.max_output_tokens && liveModel.max_output_tokens) entry.max_output_tokens = liveModel.max_output_tokens;
    entry.capabilities = mergeCapabilities(entry.capabilities || {}, liveModel.capabilities || {});
    entry.live = {
      status: liveModel.status || null,
      regions: liveModel.regions || [],
      last_seen_at: snapshot.generated_at,
      source: 'provider_live',
    };
    provider.models[liveModel.model_id] = entry;
  }
}

function liveOnlyEntry(liveModel) {
  return {
    model_id: liveModel.model_id,
    display_name: liveModel.display_name || liveModel.model_id,
    aliases: [],
    status: 'unknown',
    bcode_support: 'unknown',
    context_window: liveModel.context_window || null,
    max_output_tokens: liveModel.max_output_tokens || null,
    family: null,
    provider_model_kind: null,
    replaced_by: null,
    notes: null,
    documentation_url: null,
    pricing: null,
    capabilities: liveModel.capabilities || {},
    reasoning: null,
    source: {},
  };
}

function mergeCapabilities(left, right) {
  const keys = [
    'text_input',
    'image_input',
    'text_output',
    'tool_use',
    'structured_outputs',
    'reasoning',
    'prompt_cache',
    'native_web_search',
  ];
  return Object.fromEntries(keys.map((key) => [key, Boolean(left[key] || right[key])]));
}

function liveSummary(snapshots, env) {
  return Object.fromEntries(Object.entries(snapshots).map(([providerId, snapshot]) => [
    providerId,
    {
      generated_at: snapshot.generated_at,
      expires_at: snapshot.expires_at || null,
      state: snapshotState(snapshot, env),
      model_count: Object.keys(snapshot.models || {}).length,
    },
  ]));
}

async function discoverBedrock(env) {
  const regions = csv(env.BEDROCK_DISCOVERY_REGIONS);
  if (regions.length === 0) throw new Error('BEDROCK_DISCOVERY_REGIONS is required');
  const snapshot = {
    schema_version: '1.0.0',
    provider_id: 'bedrock',
    generated_at: new Date().toISOString(),
    expires_at: null,
    models: {},
  };
  for (const region of regions) {
    const summaries = await listBedrockFoundationModels(region, env);
    for (const summary of summaries) {
      const modelId = summary.modelId;
      if (!modelId) continue;
      const model = snapshot.models[modelId] || liveBedrockModel(summary);
      model.regions ||= [];
      if (!model.regions.includes(region)) model.regions.push(region);
      snapshot.models[modelId] = model;
    }
  }
  return snapshot;
}

function liveBedrockModel(summary) {
  return {
    model_id: summary.modelId,
    display_name: summary.modelName || null,
    status: summary.modelLifecycle && summary.modelLifecycle.status ? summary.modelLifecycle.status : null,
    regions: [],
    capabilities: {
      text_input: includesText(summary.inputModalities),
      image_input: includesImage(summary.inputModalities),
      text_output: includesText(summary.outputModalities),
      tool_use: false,
      structured_outputs: false,
      reasoning: false,
      prompt_cache: false,
      native_web_search: false,
    },
    context_window: null,
    max_output_tokens: null,
    raw: compactBedrockSummary(summary),
  };
}

async function listBedrockFoundationModels(region, env) {
  const host = `bedrock.${region}.amazonaws.com`;
  const response = await signedAwsFetch({
    env,
    region,
    service: 'bedrock',
    method: 'GET',
    url: `https://${host}/foundation-models`,
    body: '',
  });
  if (!response.ok) throw new Error(`Bedrock ListFoundationModels ${region} failed: ${response.status} ${await response.text()}`);
  const json = await response.json();
  return json.modelSummaries || [];
}

async function signedAwsFetch({ env, region, service, method, url, body }) {
  const accessKey = env.AWS_ACCESS_KEY_ID;
  const secretKey = env.AWS_SECRET_ACCESS_KEY;
  if (!accessKey || !secretKey) throw new Error('AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY are required');

  const parsed = new URL(url);
  const now = new Date();
  const amzDate = isoBasic(now);
  const dateStamp = amzDate.slice(0, 8);
  const payloadHash = await sha256Hex(body);
  const headers = {
    host: parsed.host,
    'x-amz-content-sha256': payloadHash,
    'x-amz-date': amzDate,
  };
  if (env.AWS_SESSION_TOKEN) headers['x-amz-security-token'] = env.AWS_SESSION_TOKEN;

  const signedHeaderNames = Object.keys(headers).sort();
  const canonicalHeaders = signedHeaderNames.map((name) => `${name}:${headers[name]}\n`).join('');
  const canonicalRequest = [
    method,
    parsed.pathname || '/',
    canonicalQuery(parsed.searchParams),
    canonicalHeaders,
    signedHeaderNames.join(';'),
    payloadHash,
  ].join('\n');
  const credentialScope = `${dateStamp}/${region}/${service}/aws4_request`;
  const stringToSign = ['AWS4-HMAC-SHA256', amzDate, credentialScope, await sha256Hex(canonicalRequest)].join('\n');
  const signingKey = await awsSigningKey(secretKey, dateStamp, region, service);
  const signature = await hmacHex(signingKey, stringToSign);
  headers.authorization = `AWS4-HMAC-SHA256 Credential=${accessKey}/${credentialScope}, SignedHeaders=${signedHeaderNames.join(';')}, Signature=${signature}`;
  return fetch(url, { method, headers, body: body || undefined });
}

async function awsSigningKey(secretKey, dateStamp, region, service) {
  const kDate = await hmacBytes(utf8(`AWS4${secretKey}`), dateStamp);
  const kRegion = await hmacBytes(kDate, region);
  const kService = await hmacBytes(kRegion, service);
  return hmacBytes(kService, 'aws4_request');
}

async function sha256Hex(value) {
  const bytes = typeof value === 'string' ? utf8(value) : value;
  return hex(await crypto.subtle.digest('SHA-256', bytes));
}

async function hmacBytes(keyBytes, value) {
  const key = await crypto.subtle.importKey('raw', keyBytes, { name: 'HMAC', hash: 'SHA-256' }, false, ['sign']);
  return new Uint8Array(await crypto.subtle.sign('HMAC', key, utf8(value)));
}

async function hmacHex(keyBytes, value) {
  return hex(await hmacBytes(keyBytes, value));
}

function canonicalQuery(searchParams) {
  return [...searchParams.entries()]
    .sort(([left], [right]) => left.localeCompare(right))
    .map(([key, value]) => `${encodeURIComponent(key)}=${encodeURIComponent(value)}`)
    .join('&');
}

function isoBasic(date) {
  return date.toISOString().replace(/[:-]|\.\d{3}/g, '');
}

function compactBedrockSummary(summary) {
  return {
    modelArn: summary.modelArn,
    modelId: summary.modelId,
    modelName: summary.modelName,
    providerName: summary.providerName,
    inputModalities: summary.inputModalities || [],
    outputModalities: summary.outputModalities || [],
    responseStreamingSupported: summary.responseStreamingSupported || false,
    inferenceTypesSupported: summary.inferenceTypesSupported || [],
    modelLifecycle: summary.modelLifecycle || null,
  };
}

function includesText(values) {
  return (values || []).some((value) => String(value).toLowerCase() === 'text');
}

function includesImage(values) {
  return (values || []).some((value) => String(value).toLowerCase() === 'image');
}

function authorized(request, env) {
  if (!env.INTERNAL_REFRESH_TOKEN) return false;
  return request.headers.get('authorization') === `Bearer ${env.INTERNAL_REFRESH_TOKEN}`;
}

function csv(value) {
  return String(value || '')
    .split(',')
    .map((part) => part.trim())
    .filter(Boolean);
}

function numberEnv(value, fallback) {
  const parsed = Number(value);
  return Number.isFinite(parsed) ? parsed : fallback;
}

function booleanEnv(value, fallback) {
  if (value === undefined || value === null || value === '') return fallback;
  return ['1', 'true', 'yes', 'on'].includes(String(value).toLowerCase());
}

function publicError(error) {
  return String(error && error.message ? error.message : error).slice(0, 500);
}

function utf8(value) {
  return new TextEncoder().encode(value);
}

function hex(buffer) {
  return [...new Uint8Array(buffer)].map((byte) => byte.toString(16).padStart(2, '0')).join('');
}

function jsonResponse(value, status = 200) {
  return new Response(JSON.stringify(value, null, 2), {
    status,
    headers: {
      'content-type': 'application/json; charset=utf-8',
      'cache-control': 'public, max-age=60, stale-while-revalidate=300',
    },
  });
}

function htmlResponse(value) {
  return new Response(value, { headers: { 'content-type': 'text/html; charset=utf-8' } });
}

function renderShell() {
  return `<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>models.bmux.dev</title>
  <style>
    body{margin:0;background:#0d1117;color:#c9d1d9;font-family:ui-monospace,SFMono-Regular,Menlo,monospace}main{padding:32px;max-width:1200px;margin:auto}h1{color:#7ee787;font-size:42px;margin:0 0 8px}.muted{color:#8b949e}.card{background:#161b22;border:1px solid #30363d;border-radius:8px;padding:16px;margin:12px 0}.model{background:#0d1117;border:1px solid #30363d;border-radius:6px;padding:12px;margin:8px 0}.row{display:grid;grid-template-columns:1fr 1fr 1fr;gap:12px}.pill{color:#7ee787}.search{width:100%;box-sizing:border-box;background:#0d1117;color:#c9d1d9;border:1px solid #30363d;border-radius:8px;padding:12px;margin:20px 0}
  </style>
</head>
<body><main><h1>models.bmux.dev</h1><p class="muted">Curated baseline plus on-demand live provider discovery.</p><input id="q" class="search" placeholder="Search models"><div id="app">Loading catalog…</div></main>
<script>
let catalog;
fetch('/api/v1/catalog').then(r=>r.json()).then(data=>{catalog=data;render();}).catch(e=>{document.getElementById('app').textContent=String(e)});
document.getElementById('q').addEventListener('input',render);
function render(){if(!catalog)return;const q=document.getElementById('q').value.toLowerCase();let html='';const providers=Object.values(catalog.providers||{});const models=providers.flatMap(p=>Object.values(p.models||{}).map(m=>[p,m]));const visible=models.filter(([p,m])=>(p.provider_id+' '+m.model_id+' '+m.display_name).toLowerCase().includes(q));html+='<div class="row"><div class="card"><b>'+providers.length+'</b><br><span class="muted">providers</span></div><div class="card"><b>'+models.length+'</b><br><span class="muted">models</span></div><div class="card"><b>'+visible.filter(([,m])=>m.live).length+'</b><br><span class="muted">live seen</span></div></div>';for(const [provider,model] of visible){const live=model.live?((model.live.regions||[]).join(', ')||'live'):'curated';html+='<div class="model"><b>'+escapeHtml(model.display_name||model.model_id)+'</b><div class="muted">'+escapeHtml(provider.provider_id)+' · '+escapeHtml(model.model_id)+'</div><div>context '+(model.context_window||'—')+' · output '+(model.max_output_tokens||'—')+' · support '+escapeHtml(model.bcode_support||'unknown')+'</div><div class="pill">'+escapeHtml(live)+(model.live&&model.live.status?' · '+escapeHtml(model.live.status):'')+'</div></div>'}document.getElementById('app').innerHTML=html;}
function escapeHtml(v){return String(v).replace(/[&<>"]/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;'}[c]));}
</script></body></html>`;
}
