const readline = require("readline");

const rl = readline.createInterface({
  input: process.stdin,
  output: process.stdout,
  terminal: false
});

function writeLine(str) {
  return new Promise((resolve) => {
    if (!process.stdout.write(str + "\n")) {
      process.stdout.once("drain", resolve);
    } else {
      process.nextTick(resolve);
    }
  });
}

rl.on("line", async (line) => {
  line = line.trim();
  if (!line) return;

  let req;
  try {
    req = JSON.parse(line);
  } catch (e) {
    await writeLine(JSON.stringify({ status: 400, error: "bad json", retry_after: null }));
    await writeLine(JSON.stringify({ end: true }));
    return;
  }

  const timeoutMs = (req.timeout_seconds || 120) * 1000;
  const ac = new AbortController();
  const timer = setTimeout(() => ac.abort(new Error("timeout")), timeoutMs);

  try {
    const fetchOpts = {
      method: "POST",
      headers: req.headers,
      body: typeof req.payload === "string" ? req.payload : JSON.stringify(req.payload),
      signal: ac.signal
    };
    if (req.proxy) {
      fetchOpts.proxy = req.proxy;
      fetchOpts.respectNoProxy = false;
    } else {
      fetchOpts.proxy = false;
    }

    const res = await fetch(req.url, fetchOpts);
    clearTimeout(timer);

    let retryAfter = null;
    const ra = res.headers.get("retry-after");
    if (ra) {
      const parsed = parseInt(ra, 10);
      if (!isNaN(parsed)) retryAfter = parsed;
    }

    await writeLine(JSON.stringify({
      status: res.status,
      retry_after: retryAfter
    }));

    if (res.status >= 400 || !res.body) {
      if (res.body && typeof res.body.cancel === "function") {
        try { await res.body.cancel(); } catch (_) {}
      }
      await writeLine(JSON.stringify({ end: true }));
      return;
    }

    const reader = res.body.getReader();
    try {
      while (true) {
        const { done, value } = await reader.read();
        if (done) break;
        if (value && value.length > 0) {
          const b64 = Buffer.from(value).toString("base64");
          await writeLine(JSON.stringify({ data: b64 }));
        }
      }
    } finally {
      try { reader.releaseLock(); } catch (_) {}
    }

    await writeLine(JSON.stringify({ end: true }));
  } catch (err) {
    clearTimeout(timer);
    const isTimeout = err && (err.name === "AbortError" || err.message === "timeout");
    await writeLine(JSON.stringify({
      status: isTimeout ? 504 : 502,
      error: "transport error",
      retry_after: null
    }));
    await writeLine(JSON.stringify({ end: true }));
  }
});
