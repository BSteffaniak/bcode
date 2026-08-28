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
