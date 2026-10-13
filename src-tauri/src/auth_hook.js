(() => {
  if (window.__suncordAuthHook) return;
  window.__suncordAuthHook = true;
  const host = location.hostname;
  if (!(host === "discord.com" || host.endsWith(".discord.com"))) return;

  const deliver = (raw) => {
    if (window.__suncordSent || raw == null) return;
    let token = String(raw).trim();
    if (token.startsWith('"')) {
      try {
        token = JSON.parse(token);
      } catch (error) {
        return;
      }
    }
    token = String(token).replace(/^(Bot|Bearer)\s+/i, "").trim();
    if (token.length < 50) return;
    window.__suncordSent = true;
    const invoke = window.__TAURI_INTERNALS__ && window.__TAURI_INTERNALS__.invoke;
    if (invoke) {
      Promise.resolve(invoke("auth_capture_token", { token })).catch(() => {
        window.__suncordSent = false;
      });
    }
    location.assign("suncord-auth://capture?token=" + encodeURIComponent(token));
  };

  const originalFetch = window.fetch;
  window.fetch = function (input, init) {
    try {
      const headers = new Headers(
        (init && init.headers) || (input instanceof Request ? input.headers : undefined),
      );
      const auth = headers.get("authorization");
      if (auth) deliver(auth);
    } catch (error) {
      /* header inspection is best-effort */
    }
    return originalFetch.apply(this, arguments);
  };

  const originalSet = XMLHttpRequest.prototype.setRequestHeader;
  XMLHttpRequest.prototype.setRequestHeader = function (name, value) {
    if (String(name).toLowerCase() === "authorization") deliver(value);
    return originalSet.apply(this, arguments);
  };

  const poll = () => {
    if (window.__suncordRetry) {
      window.__suncordSent = false;
      window.__suncordRetry = false;
    }
    try {
      const stored = localStorage.getItem("token");
      if (stored) deliver(stored);
    } catch (error) {
      /* private mode or a not-yet-ready document */
    }
  };
  setInterval(poll, 400);
  poll();
})();
