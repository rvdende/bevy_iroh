// Loads a wasm-bindgen bundle with a progress bar over the canvas: download, compile, start.
//
// `boot({ canvas, size })` fetches `app_bg.wasm` next to this file while showing how much has
// arrived, compiles it, then hands the compiled module to the bundle's `init` so nothing is
// fetched twice. `size` is the wasm's byte size when the page knows it (the site build writes
// it in), which is what makes the bar honest behind a gzipping server: the stream yields
// uncompressed bytes while `Content-Length` is the compressed count. Without it the header is
// used when the response is not encoded, and the bar is indeterminate otherwise.
//
// Shared by every demo page; nothing in it knows which example it is loading.

const STYLE = `
.wasm-loader { position: absolute; inset: 0; display: flex; align-items: center; justify-content: center;
  background: #15171a; color: #e6e6ea; font: 15px/1.5 system-ui, sans-serif; z-index: 10;
  transition: opacity .4s; }
.wasm-loader.done { opacity: 0; pointer-events: none; }
.wasm-loader .box { width: min(26rem, 80%); }
.wasm-loader .title { color: #6fd38a; font-weight: 600; margin-bottom: .6rem; }
.wasm-loader .bar { height: 8px; background: #2c3036; border-radius: 4px; overflow: hidden; }
.wasm-loader .fill { height: 100%; width: 0; background: #6fd38a; transition: width .15s; }
.wasm-loader .fill.busy { width: 30%; animation: wasm-loader-slide 1.2s ease-in-out infinite; }
@keyframes wasm-loader-slide { 0% { margin-left: 0 } 50% { margin-left: 70% } 100% { margin-left: 0 } }
.wasm-loader .status { margin-top: .5rem; color: #9a9ea6; font-size: .9em; min-height: 1.4em; }
.wasm-loader .hint { margin-top: .9rem; color: #6b6f77; font-size: .8em; }
.wasm-loader .error { color: #f28b82; white-space: pre-wrap; }
`;

function megabytes(n) {
  return (n / 1e6).toFixed(1) + " MB";
}

/// The overlay: a title, a bar, a status line. Lives inside the canvas's parent.
function overlay(canvas, title) {
  if (!document.getElementById("wasm-loader-style")) {
    const style = document.createElement("style");
    style.id = "wasm-loader-style";
    style.textContent = STYLE;
    document.head.appendChild(style);
  }
  const parent = canvas.parentElement;
  if (getComputedStyle(parent).position === "static") parent.style.position = "relative";
  const root = document.createElement("div");
  root.className = "wasm-loader";
  root.innerHTML = `<div class="box"><div class="title"></div><div class="bar"><div class="fill"></div></div>
    <div class="status"></div><div class="hint">The browser keeps it after the first load.</div></div>`;
  root.querySelector(".title").textContent = title;
  parent.appendChild(root);
  const fill = root.querySelector(".fill");
  const status = root.querySelector(".status");
  return {
    progress(received, total) {
      if (total > 0) {
        fill.classList.remove("busy");
        fill.style.width = Math.min(100, (100 * received) / total).toFixed(1) + "%";
        status.textContent = `Downloading ${megabytes(received)} of ${megabytes(total)}`;
      } else {
        fill.classList.add("busy");
        status.textContent = `Downloading ${megabytes(received)}`;
      }
    },
    phase(text) {
      fill.classList.add("busy");
      status.textContent = text;
    },
    done() {
      root.classList.add("done");
      setTimeout(() => root.remove(), 500);
    },
    fail(error) {
      fill.classList.remove("busy");
      fill.style.width = "0";
      status.innerHTML = `<span class="error"></span>`;
      status.querySelector(".error").textContent = "Could not load: " + (error && error.message ? error.message : error);
    },
  };
}

/// Fetches `url` reporting progress, and returns its bytes.
async function download(url, size, ui) {
  const response = await fetch(url);
  if (!response.ok) throw new Error(`${url}: HTTP ${response.status}`);
  let total = Number(size) > 0 ? Number(size) : 0;
  if (!total && !response.headers.get("content-encoding")) {
    total = Number(response.headers.get("content-length")) || 0;
  }
  const chunks = [];
  let received = 0;
  ui.progress(0, total);
  const reader = response.body.getReader();
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    chunks.push(value);
    received += value.length;
    ui.progress(received, total);
  }
  const bytes = new Uint8Array(received);
  let at = 0;
  for (const chunk of chunks) {
    bytes.set(chunk, at);
    at += chunk.length;
  }
  return bytes;
}

/// Resolves once the app has sized the canvas, which is when Bevy has made its window; or
/// after `limit` ms, so a quiet start still uncovers the page.
function started(canvas, limit) {
  return new Promise((resolve) => {
    const deadline = performance.now() + limit;
    const tick = () => {
      if ((canvas.width > 0 && canvas.height > 0) || performance.now() > deadline) resolve();
      else requestAnimationFrame(tick);
    };
    tick();
  });
}

export async function boot({ canvas, size, title = document.title, glue = "./app.js", wasm = "./app_bg.wasm" } = {}) {
  canvas = canvas || document.getElementById("bevy");
  const ui = overlay(canvas, title);
  try {
    const bytes = await download(new URL(wasm, import.meta.url), size, ui);
    ui.phase("Compiling " + megabytes(bytes.length));
    const module = await WebAssembly.compile(bytes);
    ui.phase("Starting");
    const { default: init } = await import(new URL(glue, import.meta.url));
    await init({ module_or_path: module }).catch((e) => {
      // winit uses an exception for control flow once the event loop is running.
      if (!String(e).includes("Using exceptions for control flow")) throw e;
    });
    await started(canvas, 15000);
    ui.done();
  } catch (error) {
    console.error(error);
    ui.fail(error);
  }
}
