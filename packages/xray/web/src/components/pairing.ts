import type { InspectorBootstrap } from '../contracts';
import { validateBootstrap } from '../core/transport';

export const PAIRING_FAILURE = 'This launch link expired, was already used, or could not connect in this browser. Use a fresh CLI launch link or enter a pairing code, or open a recording. Nothing was saved; no automatic retry was attempted.';

/** Redeem an explicitly entered, one-time code without URL or persistent storage. */
export async function redeemPairing(code: string, fetcher: typeof fetch = fetch): Promise<InspectorBootstrap> {
  if (!code || code.length > 4096 || /[^\x21-\x7e]/u.test(code)) throw new Error('Invalid pairing code');
  const response = await fetcher('/__xray/bootstrap', {
    method: 'POST', credentials: 'omit', cache: 'no-store', redirect: 'error', referrerPolicy: 'no-referrer',
    headers: { Accept: 'application/json', 'x-xray-pairing-code': code },
  });
  if (!response.ok) throw new Error('Pairing unavailable');
  const bootstrap = await response.json() as InspectorBootstrap;
  validateBootstrap(bootstrap);
  return bootstrap;
}

export interface LaunchTicket { redeem(): Promise<InspectorBootstrap> }

/** Call synchronously at entry, before React or asynchronous work. The fragment is never retained in history. */
export function takeLaunchTicket(location: Pick<Location, 'hash' | 'pathname' | 'search'>, history: Pick<History, 'replaceState' | 'state'>, fetcher: typeof fetch = fetch): LaunchTicket | undefined {
  if (!location.hash.startsWith('#xray-ticket=')) return;
  let code = location.hash.slice('#xray-ticket='.length);
  let valid = true;
  try {
    history.replaceState(history.state, '', location.pathname + location.search);
    code = decodeURIComponent(code);
  } catch { code = ''; valid = false; }
  let redemption: Promise<InspectorBootstrap> | undefined;
  return { redeem() {
    if (!redemption) {
      redemption = valid ? redeemPairing(code, fetcher) : Promise.reject(new Error('Pairing unavailable'));
      code = ''; // Only the in-flight request and its result survive; StrictMode shares that result.
    }
    return redemption;
  } };
}

/** Pairing-required responses are normal when opening a protected inspector manually. */
export class PairingRequired extends Error {}

/** Probe the same-Origin local host once; StrictMode shares the result. */
export function openLocalInspector(fetcher: typeof fetch = fetch): LaunchTicket {
  let result: Promise<InspectorBootstrap> | undefined;
  return { redeem() {
    result ??= (async () => {
      const response = await fetcher('/__xray/bootstrap', {
        method: 'POST', credentials: 'omit', cache: 'no-store', redirect: 'error', referrerPolicy: 'no-referrer',
        headers: { Accept: 'application/json' },
      });
      if (response.status === 403) throw new PairingRequired();
      if (!response.ok) throw new Error('Local inspector unavailable');
      const bootstrap = await response.json() as InspectorBootstrap;
      validateBootstrap(bootstrap);
      return bootstrap;
    })();
    return result;
  } };
}
