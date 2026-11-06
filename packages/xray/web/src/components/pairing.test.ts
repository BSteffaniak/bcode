import { describe, expect, it, vi } from 'vitest';
import { redeemPairing, takeLaunchTicket } from './pairing';
import { validateBootstrap } from '../core/transport';

const bootstrap = { schema_version: 1 as const, endpoint: 'http://127.0.0.1:1234/', bearer: 'test-bearer', session_id: 'session', deep_capture: false };
describe('one-time launcher pairing', () => {
  it('sends the code only in the dedicated header with redirects and cookies forbidden', async () => {
    const fetcher = vi.fn<typeof fetch>().mockResolvedValue(new Response(JSON.stringify(bootstrap)));
    expect(await redeemPairing('test-pairing-code', fetcher)).toEqual(bootstrap);
    expect(fetcher).toHaveBeenCalledExactlyOnceWith('/__xray/bootstrap', {
      method: 'POST', credentials: 'omit', cache: 'no-store', redirect: 'error', referrerPolicy: 'no-referrer',
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
  it('clears synchronously before any request and shares a single pending redemption across StrictMode effects', async () => {
    let release!: (response: Response) => void;
    const fetcher = vi.fn<typeof fetch>(() => new Promise(resolve => { release = resolve; }));
    const replaceState = vi.fn();
    const ticket = takeLaunchTicket({ hash: '#xray-ticket=memory%2Donly', pathname: '/inspect', search: '?view=1' }, { state: null, replaceState }, fetcher)!;
    expect(replaceState).toHaveBeenCalledExactlyOnceWith(null, '', '/inspect?view=1');
    expect(fetcher).not.toHaveBeenCalled();
    const first = ticket.redeem();
    expect(ticket.redeem()).toBe(first);
    expect(fetcher).toHaveBeenCalledTimes(1);
    expect(fetcher.mock.calls[0][1]?.headers).toMatchObject({ 'x-xray-pairing-code': 'memory-only' });
    release(new Response(JSON.stringify(bootstrap)));
    expect(await first).toEqual(bootstrap);
    expect(ticket.redeem()).toBe(first);
    expect(fetcher).toHaveBeenCalledTimes(1);
  });
  it('never retries failed redemption', async () => {
    const fetcher = vi.fn<typeof fetch>().mockResolvedValue(new Response('private', { status: 401 }));
    const ticket = takeLaunchTicket({ hash: '#xray-ticket=used', pathname: '/', search: '' }, { state: null, replaceState: vi.fn() }, fetcher)!;
    await expect(ticket.redeem()).rejects.toThrow('Pairing unavailable');
    await expect(ticket.redeem()).rejects.toThrow('Pairing unavailable');
    expect(fetcher).toHaveBeenCalledTimes(1);
  });
  it.each(['#xray-ticket=%zz', '#xray-ticket=', '#xray-ticket=a%0Ab'])('clears malformed launch fragments and fails closed: %s', async hash => {
    const fetcher = vi.fn<typeof fetch>(); const replaceState = vi.fn();
    const ticket = takeLaunchTicket({ hash, pathname: '/', search: '' }, { state: null, replaceState }, fetcher)!;
    expect(replaceState).toHaveBeenCalledTimes(1);
    await expect(ticket.redeem()).rejects.toThrow();
    expect(fetcher).not.toHaveBeenCalled();
  });
  it('fails closed when history is unavailable', async () => {
    const fetcher = vi.fn<typeof fetch>();
    const ticket = takeLaunchTicket({ hash: '#xray-ticket=secret', pathname: '/', search: '' }, { state: null, replaceState() { throw new Error('blocked'); } }, fetcher)!;
    await expect(ticket.redeem()).rejects.toThrow();
    expect(fetcher).not.toHaveBeenCalled();
  });
  it('ignores unrelated fragments without redeeming', () => {
    expect(takeLaunchTicket({ hash: '#help', pathname: '/', search: '' }, { state: null, replaceState: vi.fn() })).toBeUndefined();
  });
});

describe('gateway endpoint contract', () => {
  it.each(['/', 'https://inspector.example/'])('allows exactly the same-origin root: %s', endpoint => {
    expect(validateBootstrap({ ...bootstrap, endpoint, demo: true }, 'https://inspector.example').href).toBe('https://inspector.example/');
  });
  it.each(['//inspector.example/', '/other', './', 'https://other.example/', 'https://inspector.example:444/', 'http://inspector.example/', 'https://user@inspector.example/', 'https://inspector.example/?token=x', 'https://inspector.example/#x', 'http://localhost:1234/', 'http://127.0.0.1.evil/'])('rejects unsafe endpoint: %s', endpoint => {
    expect(() => validateBootstrap({ ...bootstrap, endpoint }, 'https://inspector.example')).toThrow();
  });
  it('does not allow relative roots without a browser origin', () => {
    expect(() => validateBootstrap({ ...bootstrap, endpoint: '/' })).toThrow();
  });
  it('retains explicit loopback support', () => {
    expect(validateBootstrap(bootstrap).href).toBe(bootstrap.endpoint);
  });
  it('rejects unknown demo representations', () => {
    expect(() => validateBootstrap({ ...bootstrap, demo: 'yes' } as never)).toThrow();
  });
});
