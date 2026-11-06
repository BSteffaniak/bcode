import type { InspectorBootstrap } from '../contracts';

/** Redeem an explicitly entered, one-time code without URL or persistent storage. */
export async function redeemPairing(code: string, fetcher: typeof fetch = fetch): Promise<InspectorBootstrap> {
  if (!code || code.length > 4096 || /[^\x21-\x7e]/u.test(code)) throw new Error('Invalid pairing code');
  const response = await fetcher('/__xray/bootstrap', {
    method: 'POST', credentials: 'same-origin', cache: 'no-store', redirect: 'error',
    headers: { Accept: 'application/json', 'x-xray-pairing-code': code },
  });
  if (!response.ok) throw new Error('Pairing unavailable');
  return await response.json() as InspectorBootstrap;
}
