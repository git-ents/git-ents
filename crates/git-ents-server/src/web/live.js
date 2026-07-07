document.querySelectorAll('[data-live-check]').forEach((container) => {
  const url = container.dataset.liveCheck;
  const poll = () => {
    fetch(url, { cache: 'no-store' })
      .then((response) => {
        const state = response.headers.get('X-Check-Live');
        return response.text().then((html) => ({ state, html }));
      })
      .then(({ state, html }) => {
        if (state === 'done') {
          window.location.reload();
          return;
        }
        if (state === 'stale') {
          container.insertAdjacentHTML(
            'beforeend',
            '<p class="shell-note">No live output available right now; this check is still marked in progress.</p>'
          );
          return;
        }
        container.innerHTML = html;
        setTimeout(poll, 1000);
      })
      .catch(() => setTimeout(poll, 2000));
  };
  poll();
});
