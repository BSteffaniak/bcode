import { openLocalInspector, takeLaunchTicket, type LaunchTicket } from './components/pairing';
import './styles.css';

// Remove the launch capability before loading React, mounting, or scheduling asynchronous work.
const launchTicket = takeLaunchTicket(window.location, window.history) ?? openLocalInspector();
void Promise.all([import('react-dom/client'), import('react'), import('./App')]).then(([{ createRoot }, { StrictMode, createElement }, { default: App }]) => {
  const root = document.getElementById('root');
  if (!root) throw new Error('Inspector root element is missing');
  createRoot(root).render(createElement(StrictMode, null, createElement<{ launchTicket?: LaunchTicket }>(App, { launchTicket })));
});
