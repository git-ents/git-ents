function copyText(text) {
  if (navigator.clipboard && navigator.clipboard.writeText) {
    return navigator.clipboard.writeText(text);
  }
  return new Promise((resolve, reject) => {
    const textarea = document.createElement('textarea');
    textarea.value = text;
    textarea.style.position = 'fixed';
    textarea.style.opacity = '0';
    document.body.appendChild(textarea);
    textarea.focus();
    textarea.select();
    try {
      const ok = document.execCommand('copy');
      document.body.removeChild(textarea);
      if (ok) {
        resolve();
      } else {
        reject(new Error('execCommand copy failed'));
      }
    } catch (err) {
      document.body.removeChild(textarea);
      reject(err);
    }
  });
}

document.querySelectorAll('[data-copy]').forEach((btn) => {
  btn.addEventListener('click', () => {
    const label = btn.textContent;
    copyText(btn.dataset.copy)
      .then(() => {
        btn.textContent = 'Copied';
        setTimeout(() => { btn.textContent = label; }, 1200);
      })
      .catch(() => {
        btn.textContent = 'Copy failed';
        setTimeout(() => { btn.textContent = label; }, 1200);
      });
  });
});
