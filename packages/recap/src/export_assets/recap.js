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
  const contents = document.querySelector('.contents');
  const links = [...document.querySelectorAll('.contents a')];
  const chapters = links.map(link => document.getElementById(link.hash.slice(1)));
  const panel = contents?.querySelector('details');
  if (panel && links.length > 1) {
    const label = document.createElement('label');
    label.className = 'contents-search';
    label.textContent = 'Find a chapter';
    const search = document.createElement('input');
    search.type = 'search';
    search.placeholder = 'Filter chapter titles';
    label.append(search);
    const status = document.createElement('div');
    status.className = 'contents-status';
    status.setAttribute('role', 'status');
    panel.insertBefore(label, panel.querySelector('ol'));
    panel.insertBefore(status, panel.querySelector('ol'));
    const titles = links.map(link => link.textContent.toLocaleLowerCase());
    const filter = () => {
      const query = search.value.trim().toLocaleLowerCase();
      let count = 0;
      links.forEach((link, index) => {
        const matches = titles[index].includes(query);
        link.parentElement.hidden = !matches;
        if (matches) count++;
      });
      status.textContent = query ? `${count} of ${links.length} chapters` : '';
    };
    search.addEventListener('input', filter);
    search.addEventListener('keydown', event => {
      if (event.key === 'Escape') { search.value = ''; filter(); }
    });
    // Filtering is navigation-only; it never hides substantive document content.
  }
  if (chapters.length && chapters.every(Boolean)) {
    let active = -1;
    let pending = false;
    const update = () => {
      pending = false;
      // Binary search avoids scanning all chapters on every scroll frame.
      let low = 0;
      let high = chapters.length;
      const boundary = Math.min(160, innerHeight * 0.25);
      while (low < high) {
        const middle = Math.floor((low + high) / 2);
        if (chapters[middle].getBoundingClientRect().top <= boundary) low = middle + 1;
        else high = middle;
      }
      const atEnd = scrollY > 0 && scrollY + innerHeight >= document.documentElement.scrollHeight - 2;
      const next = atEnd ? chapters.length - 1 : Math.max(0, low - 1);
      if (active === next) return;
      if (active >= 0) links[active].removeAttribute('aria-current');
      active = next;
      links[active].setAttribute('aria-current', 'location');
      // Keep the active chapter visible on desktop without moving the document
      // or stealing focus while someone is searching or browsing the contents.
      if (panel?.open && matchMedia('(min-width:761px)').matches
          && !contents.contains(document.activeElement) && !contents.matches(':hover')
          && !links[active].parentElement.hidden) {
        const item = links[active].getBoundingClientRect();
        const bounds = panel.getBoundingClientRect();
        if (item.bottom > bounds.bottom) panel.scrollTop += item.bottom - bounds.bottom + 8;
        else if (item.top < bounds.top) panel.scrollTop -= bounds.top - item.top + 8;
      }
    };
    const schedule = () => {
      if (!pending) { pending = true; requestAnimationFrame(update); }
    };
    addEventListener('scroll', schedule, { passive: true });
    addEventListener('resize', schedule);
    addEventListener('hashchange', schedule);
    addEventListener('load', schedule);
    update();
  }
})();
