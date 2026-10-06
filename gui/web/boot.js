// kage client boot. Draws the connection form, then loads the wasm
// bundle once the form is submitted: the form hands the server URL and
// the token to the wasm entry through window.__kageConnect, so the
// token never rides any URL. No dependencies, no inline script, no
// inline styles: everything here is allowed by the strict CSP the
// server sends on the page.

// The page styles, including the form. A constructed stylesheet keeps
// `style-src 'self'` intact; element styles are the fallback for
// browsers without constructed stylesheets.
const STYLES = `
  html, body { margin: 0; padding: 0; height: 100%; background: #0f0e13;
               color: #cdc9d6; font: 14px/1.5 monospace; overflow: hidden; }
  canvas { outline: none; }
  main { height: 100%; display: flex; align-items: center; justify-content: center; }
  form { display: flex; flex-direction: column; gap: 12px; width: min(360px, 90vw); }
  h1 { font-size: 18px; margin: 0 0 4px; color: #f2a65a; }
  label { display: flex; flex-direction: column; gap: 4px; }
  input { background: #17151d; color: #cdc9d6; border: 1px solid #46444c;
          border-radius: 4px; padding: 8px; font: inherit; }
  input:focus { outline: 1px solid #f2a65a; }
  button { background: #f2a65a; color: #0f0e13; border: 0; border-radius: 4px;
           padding: 8px; font: inherit; font-weight: 600; cursor: pointer; }
  p { color: #9895a0; margin: 0; }
  p.error { color: #f2727f; }
`;

function applyStyles() {
  if (typeof CSSStyleSheet === "function" &&
      typeof new CSSStyleSheet().replaceSync === "function") {
    const sheet = new CSSStyleSheet();
    sheet.replaceSync(STYLES);
    document.adoptedStyleSheets = [sheet];
    return;
  }
  for (const element of [document.documentElement, document.body]) {
    element.style.margin = "0";
    element.style.padding = "0";
    element.style.height = "100%";
    element.style.background = "#0f0e13";
    element.style.color = "#cdc9d6";
    element.style.font = "14px/1.5 monospace";
    element.style.overflow = "hidden";
  }
}

// The endpoint the form starts from: the ?ws= developer override, else
// /acp on the page's own origin with the scheme mapped to ws or wss.
function defaultEndpoint() {
  const override = new URLSearchParams(window.location.search).get("ws");
  if (override) {
    return override;
  }
  const scheme = window.location.protocol === "https:" ? "wss:" : "ws:";
  return scheme + "//" + window.location.host + "/acp";
}

// Whether the server refuses `token`. A browser does not say why a
// WebSocket handshake failed, so a refused token would only show as a
// client that never connects. When the endpoint is this page's own
// server, a HEAD on it answers 401 for a refused token before the app
// boots; any other endpoint, or a failed check, is left to the dial.
async function tokenRefused(endpoint, token) {
  let url;
  try {
    url = new URL(endpoint);
  } catch {
    return false;
  }
  if (url.host !== window.location.host) {
    return false;
  }
  url.protocol = window.location.protocol;
  try {
    const reply = await fetch(url, {
      method: "HEAD",
      headers: { Authorization: "Bearer " + token },
      cache: "no-store",
    });
    return reply.status === 401;
  } catch {
    return false;
  }
}

// Loads the wasm bundle. The generated glue fetches the module from
// its own directory, a same-origin request under the served CSP.
async function boot() {
  try {
    const glue = await import("./kage_desktop.js");
    await glue.default();
  } catch (error) {
    // The wasm read clears the holder on success; on failure nothing
    // else would, so drop the token here too.
    window.__kageConnect = null;
    const stage = document.createElement("main");
    stage.append(failure(error));
    document.body.replaceChildren(stage);
  }
}

function failure(error) {
  const pre = document.createElement("pre");
  pre.textContent = "kage client failed to load: " + error;
  pre.style.color = "#f2727f";
  pre.style.padding = "2em";
  pre.style.whiteSpace = "pre-wrap";
  return pre;
}

// The connection form. Submitting stores the values for the wasm
// entry, drops the form and boots the app.
function connectionForm() {
  const form = document.createElement("form");

  const title = document.createElement("h1");
  title.textContent = "kage client";
  form.append(title);

  const hint = document.createElement("p");
  hint.textContent = "Connect to a kage serve endpoint.";
  form.append(hint);

  const serverLabel = document.createElement("label");
  serverLabel.textContent = "Server URL";
  const server = document.createElement("input");
  server.name = "server";
  server.value = defaultEndpoint();
  server.spellcheck = false;
  server.autocomplete = "url";
  serverLabel.append(server);
  form.append(serverLabel);

  const tokenLabel = document.createElement("label");
  tokenLabel.textContent = "Token";
  const token = document.createElement("input");
  token.name = "token";
  token.type = "password";
  token.autocomplete = "off";
  // Phone keyboards capitalize and correct by default, which mangles
  // a pasted or typed hex token.
  token.autocapitalize = "off";
  token.spellcheck = false;
  token.setAttribute("autocorrect", "off");
  token.required = true;
  token.autofocus = true;
  tokenLabel.append(token);
  form.append(tokenLabel);

  const connect = document.createElement("button");
  connect.type = "submit";
  connect.textContent = "Connect";
  form.append(connect);

  const error = document.createElement("p");
  error.className = "error";
  error.hidden = true;
  form.append(error);

  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    const endpoint = server.value.trim();
    const secret = token.value.trim();
    if (!secret) {
      return;
    }
    connect.disabled = true;
    const refused = await tokenRefused(endpoint, secret);
    connect.disabled = false;
    if (refused) {
      error.textContent =
        "The server refused this token. Copy it from the kage serve output.";
      error.hidden = false;
      token.select();
      return;
    }
    window.__kageConnect = { server: endpoint, token: secret };
    document.body.replaceChildren();
    boot();
  });
  return form;
}

function main() {
  applyStyles();
  const stage = document.createElement("main");
  stage.append(connectionForm());
  document.body.replaceChildren(stage);
}

main();
