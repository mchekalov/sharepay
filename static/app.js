// SharePay's entire client JS surface (spec §4.5), enumerated:
//   1. `this.form.requestSubmit()` on file-input `onchange` (host upload,
//      host retry) — inline in the upload form's HTML (`templates/
//      host_upload.html`), not here, since it's a one-line attribute, not
//      a reusable function.
//   2. This file: one delegated click handler on `document.body` for
//      `.copy-btn` -> clipboard write + toast. Used by both "Copy my
//      total" (participant view) and "Copy link" (host QR screen).
//   3. HTMX itself (`/static/htmx.min.js`) — the only third-party script.
//
// Event delegation from `document.body` (not a direct listener on the
// button) is the critical detail: `.copy-btn` elements live inside HTMX-
// polled/swapped fragments (`#bill-fragment`), so a listener attached
// directly to the button would be silently lost on the next swap.
document.body.addEventListener('click', (e) => {
  const btn = e.target.closest('.copy-btn');
  if (!btn) return;

  const toast = document.getElementById('toast-region');
  const showToast = (message, duration) => {
    toast.textContent = message;
    toast.classList.add('show');
    setTimeout(() => toast.classList.remove('show'), duration);
  };

  if (typeof navigator.clipboard === 'undefined') {
    showToast("Couldn't copy — long-press to copy manually", 2500);
    return;
  }

  navigator.clipboard.writeText(btn.dataset.copyText).then(() => {
    showToast('Copied to clipboard', 2000);
  }).catch(() => {
    showToast("Couldn't copy — long-press to copy manually", 2500);
  });
});
