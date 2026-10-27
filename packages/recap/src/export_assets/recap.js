/* Optional enhancements only. No remote requests and no untrusted HTML insertion. */
(() => {
  'use strict';
  if (typeof HTMLDialogElement !== 'undefined') {
    const dialog = document.createElement('dialog');
    dialog.setAttribute('aria-label', 'Enlarged recap image');
    const close = document.createElement('button');
    close.type = 'button';
    close.textContent = 'Close image · Esc';
    const image = document.createElement('img');
    dialog.append(close, image);
    document.body.append(dialog);
    close.addEventListener('click', () => dialog.close());
    dialog.addEventListener('click', event => { if (event.target === dialog) dialog.close(); });
    for (const original of document.querySelectorAll('figure img')) {
      original.classList.add('zoomable');
      original.tabIndex = 0;
      original.setAttribute('role', 'button');
      original.setAttribute('aria-label', `Enlarge image: ${original.alt}`);
      original.setAttribute('aria-haspopup', 'dialog');
      const open = () => { image.src = original.src; image.alt = original.alt; dialog.showModal(); };
      original.addEventListener('click', open);
      original.addEventListener('keydown', event => {
        if (event.key === 'Enter' || event.key === ' ') { event.preventDefault(); open(); }
      });
    }
  }
  if ('IntersectionObserver' in window) {
    const links = [...document.querySelectorAll('.contents a')];
    const observer = new IntersectionObserver(entries => {
      for (const entry of entries) {
        if (!entry.isIntersecting) continue;
        for (const link of links) {
          if (link.hash === `#${entry.target.id}`) link.setAttribute('aria-current', 'location');
          else link.removeAttribute('aria-current');
        }
      }
    }, { rootMargin: '-10% 0px -60% 0px', threshold: 0 });
    for (const section of document.querySelectorAll('.chapter')) observer.observe(section);
  }
})();
