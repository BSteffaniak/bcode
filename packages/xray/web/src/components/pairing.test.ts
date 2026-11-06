import { describe, expect, it, vi } from 'vitest';
import { redeemPairing } from './pairing';

describe('one-time launcher pairing', () => {
  it('sends the code only in the dedicated header with redirects forbidden', async () => {
    const bootstrap = { schema_version: 1, endpoint: 'http://127.0.0.1:1234', bearer: 'test-bearer', session_id: 'session', deep_capture: false };
    const fetcher = vi.fn<typeof fetch>().mockResolvedValue(new Response(JSON.stringify(bootstrap)));
    expect(await redeemPairing('test-pairing-code', fetcher)).toEqual(bootstrap);
    expect(fetcher).toHaveBeenCalledExactlyOnceWith('/__xray/bootstrap', {
      method: 'POST', credentials: 'same-origin', cache: 'no-store', redirect: 'error',
      headers: { Accept: 'application/json', 'x-xray-pairing-code': 'test-pairing-code' },
    });
  });
  it.each(['', 'code\r\nheader', 'x'.repeat(4097)])('rejects invalid input before sending', async code => {
    const fetcher = vi.fn<typeof fetch>();
    await expect(redeemPairing(code, fetcher)).rejects.toThrow('Invalid pairing code');
    expect(fetcher).not.toHaveBeenCalled();
  });
  it('does not echo server errors or codes in failure messages', async () => {
    const fetcher = vi.fn<typeof fetch>().mockResolvedValue(new Response('sensitive error', { status: 403 }));
    await expect(redeemPairing('test-pairing-code', fetcher)).rejects.toThrow('Pairing unavailable');
  });
});
