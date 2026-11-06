import { describe, expect, it } from 'vitest';
import { renderToStaticMarkup } from 'react-dom/server';
import App from '../App';

describe('offline inspector shell', () => {
  it('renders useful navigation and one-way controls without bootstrap or storage', () => {
    const html = renderToStaticMarkup(<App />);
    expect(html).toContain('Connect live');
    expect(html).toContain('type="password"');
    expect(html).toContain('autoComplete="off"');
    expect(html).toContain('CLI pairing code');
    expect(html).toContain('cleared on submission');
    expect(html).toContain('Open folder');
    expect(html).toContain('Activities');
    expect(html).toContain('Issues');
    expect(html).toContain('ingest');
    expect(html).toContain('No deep capture is enabled here');
    expect(html).toContain('No execution authority');
    expect(html).not.toContain('Cancel execution');
  });
});
