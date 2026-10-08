import assert from 'node:assert/strict';
import test from 'node:test';

import { applyBedrockPricing, loadBedrockPricingSeed, mergeSnapshot } from './src/worker.js';

const pricing = {
  currency: 'USD',
  unit: 'per_million_tokens',
  input_micros: 3_000_000,
  output_micros: 15_000_000,
  rules: [],
};

test('loads a versioned normalized Bedrock pricing seed', async () => {
  const env = {
    ASSETS: {
      async fetch(request) {
        assert.equal(new URL(request.url).pathname, '/v1/live/bedrock.json');
        return Response.json({
          schema_version: '1.0.0',
          provider_id: 'bedrock',
          models: {
            claude: { model_id: 'anthropic.claude-test', pricing },
            new_model: { model_id: 'vendor.new-model', pricing: null },
          },
        });
      },
    },
  };

  const seed = await loadBedrockPricingSeed(env);
  assert.deepEqual(seed.get('anthropic.claude-test'), pricing);
  assert.equal(seed.has('vendor.new-model'), false);
});

test('rejects an empty pricing seed', async () => {
  const env = {
    ASSETS: {
      async fetch() {
        return Response.json({ schema_version: '1.0.0', provider_id: 'bedrock', models: {} });
      },
    },
  };

  await assert.rejects(loadBedrockPricingSeed(env), /contains no priced models/);
});

test('copies pricing by exact model id and leaves new models unpriced', () => {
  const seed = new Map([['anthropic.claude-test', pricing]]);
  const known = { model_id: 'anthropic.claude-test', pricing: null };
  const newlyIntroduced = { model_id: 'vendor.new-model', pricing: null };

  applyBedrockPricing(known, seed);
  applyBedrockPricing(newlyIntroduced, seed);

  assert.deepEqual(known.pricing, pricing);
  assert.equal(newlyIntroduced.pricing, null);
});

test('xai live overlays supplement curated inventory without inventing future pricing', () => {
  const baseline = {
    model_id: 'grok-4.7',
    pricing,
    deployments: [{ provider: 'xai', auth_mode: 'api_key', api_surface: 'responses_api', integration: 'bcode' }],
  };
  const catalog = { providers: { xai: {
    default_model_id: 'grok-4.7',
    fallback_model_ids: ['grok-4.7', 'grok-4.3'],
    models: { 'grok-4.7': baseline, 'grok-4.3': { model_id: 'grok-4.3' } },
  } } };
  mergeSnapshot(catalog, {
    provider_id: 'xai', generated_at: '2026-10-08T00:00:00Z',
    models: {
      'grok-4.7': { model_id: 'grok-4.7', pricing: null, capabilities: {} },
      'grok-future-discovered': { model_id: 'grok-future-discovered', capabilities: {} },
    },
  });
  assert.deepEqual(catalog.providers.xai.models['grok-4.7'].pricing, pricing);
  assert.equal(catalog.providers.xai.models['grok-4.7'].deployments.length, 1);
  assert.equal(catalog.providers.xai.models['grok-future-discovered'].pricing, null);
  assert.equal(catalog.providers.xai.models['grok-future-discovered'].context_window, null);
  assert.equal(catalog.providers.xai.models['grok-future-discovered'].bcode_support, 'unknown');
  assert.deepEqual(catalog.providers.xai.fallback_model_ids, ['grok-4.7', 'grok-4.3']);
  assert.equal(catalog.providers.xai.default_model_id, 'grok-4.7');
});

test('merged catalog carries live pricing', () => {
  const catalog = { providers: { bedrock: { models: {} } } };
  mergeSnapshot(catalog, {
    provider_id: 'bedrock',
    generated_at: '2026-01-01T00:00:00Z',
    models: {
      claude: {
        model_id: 'anthropic.claude-test',
        display_name: 'Claude Test',
        pricing,
        capabilities: {},
      },
    },
  });

  assert.deepEqual(catalog.providers.bedrock.models['anthropic.claude-test'].pricing, pricing);
});
