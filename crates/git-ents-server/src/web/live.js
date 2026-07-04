document.querySelectorAll('[data-live-check]').forEach((container) => {
  const url = container.dataset.liveCheck;
  const poll = () => {
    fetch(url, { cache: 'no-store' })
      .then((response) => {
        if (response.headers.get('X-Check-Live') === 'done') {
          window.location.reload();
          return null;
        }
        return response.text();
      })
      .then((html) => {
        if (html === null) return;
        container.innerHTML = html;
        setTimeout(poll, 1000);
      })
      .catch(() => setTimeout(poll, 2000));
  };
  poll();
});
