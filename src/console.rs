//! `cu console`: everything the page wrote to its console, by level.
//!
//! cu never sends `Runtime.enable` (a page can see that, and it is one of the
//! tells stealth mode removes), so there is no `Runtime.consoleAPICalled`
//! stream to listen to. The capture lives in the page instead: a small script
//! registered with `Page.addScriptToEvaluateOnNewDocument` wraps the console
//! methods (in `Proxy`s, so `toString()` and `name` stay native), listens for
//! uncaught errors, unhandled rejections, CSP violations and failed
//! sub-resources, and wraps `fetch` / `XMLHttpRequest` to note failures and
//! 4xx/5xx replies. Entries sit in a ring buffer (1000) under a
//! `Symbol.for("cu.console")` key; `GET /v1/console` reads and filters it.
//!
//! Limits, stated plainly: a tab is captured from the first `cu navigate`
//! or `cu console` on it, not from before; entries from before a navigation
//! that cu did not see are lost with the old document. `CU_CONSOLE=off`
//! turns the capture off for a session that must not touch the page's console.

use crate::server::{Tab, json_string};
use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

/// Levels, in the order `warn+` expands from.
pub const LEVELS: &[&str] = &[
    "debug", "log", "info", "trace", "warn", "error", "exception", "csp", "network",
];

/// `--level` as the list of levels it names. `all` and empty mean every
/// level; `verbose` is the DevTools name for `debug`; `warn+` is warnings and
/// everything worse (`error`, `exception`, `csp`, `network`).
pub fn expand_levels(spec: &str) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    for part in spec.split(',').map(|p| p.trim().to_ascii_lowercase()) {
        if part.is_empty() || part == "all" {
            return Ok(Vec::new());
        }
        let part = if part == "verbose" { "debug".to_string() } else { part };
        if let Some(base) = part.strip_suffix('+') {
            let base = if base == "verbose" { "debug" } else { base };
            let Some(at) = LEVELS.iter().position(|l| *l == base) else {
                return Err(format!("unknown level {base}; use {}", LEVELS.join(", ")));
            };
            // Severity order: debug < log < info < trace < warn < error...
            for l in &LEVELS[at..] {
                out.push((*l).to_string());
            }
        } else if LEVELS.contains(&part.as_str()) {
            out.push(part);
        } else {
            return Err(format!(
                "unknown level {part}; use {}, verbose, all or LEVEL+",
                LEVELS.join(", ")
            ));
        }
    }
    out.dedup();
    Ok(out)
}

pub const CONSOLE_JS: &str = r##"(function () {
  var K = Symbol.for("cu.console");
  if (window[K]) return;
  var buf = [], seq = 0, MAX = 1000;
  function str(a) {
    try {
      if (typeof a === "string") return a;
      if (a instanceof Error) return a.stack || String(a);
      if (typeof a === "function") return "function " + a.name;
      if (a && typeof a === "object") {
        var seen = [];
        return JSON.stringify(a, function (k, v) {
          if (typeof v === "bigint") return String(v);
          if (typeof v === "object" && v !== null) {
            if (seen.indexOf(v) > -1) return "[Circular]";
            seen.push(v);
          }
          return v;
        });
      }
      return String(a);
    } catch (e) { try { return String(a); } catch (e2) { return "[unprintable]"; } }
  }
  function add(level, text, extra) {
    text = String(text);
    if (text.length > 2000) text = text.slice(0, 2000) + "...";
    var e = { n: ++seq, t: Date.now(), level: level, text: text, page: location.href };
    if (extra) for (var k in extra) e[k] = extra[k];
    buf.push(e);
    if (buf.length > MAX) buf.shift();
  }
  var map = { log: "log", info: "info", debug: "debug", warn: "warn", error: "error", trace: "trace", assert: "error", table: "log", dir: "log", dirxml: "log" };
  Object.keys(map).forEach(function (m) {
    var orig = console[m];
    if (typeof orig !== "function") return;
    try {
      console[m] = new Proxy(orig, {
        apply: function (target, self, args) {
          try {
            var a = Array.prototype.slice.call(args);
            if (m === "assert") { if (a[0]) return Reflect.apply(target, self, args); a = ["Assertion failed:"].concat(a.slice(1)); }
            add(map[m], a.map(str).join(" "));
          } catch (e) {}
          return Reflect.apply(target, self, args);
        }
      });
    } catch (e) {}
  });
  addEventListener("error", function (ev) {
    try {
      var t = ev.target;
      if (t && t !== window && t.tagName) {
        add("network", "Failed to load " + t.tagName.toLowerCase() + " " + (t.currentSrc || t.src || t.href || ""), { kind: "resource" });
      } else {
        add("exception", (ev.message || "error") + " at " + ev.filename + ":" + ev.lineno + ":" + ev.colno + (ev.error && ev.error.stack ? "\n" + ev.error.stack : ""));
      }
    } catch (e) {}
  }, true);
  addEventListener("unhandledrejection", function (ev) {
    try { add("exception", "Unhandled promise rejection: " + str(ev.reason)); } catch (e) {}
  });
  document.addEventListener("securitypolicyviolation", function (ev) {
    try { add("csp", "Blocked by CSP (" + ev.violatedDirective + "): " + ev.blockedURI + (ev.sourceFile ? " from " + ev.sourceFile + ":" + ev.lineNumber : "")); } catch (e) {}
  }, true);
  try {
    var of = window.fetch;
    if (typeof of === "function") {
      window.fetch = new Proxy(of, {
        apply: function (target, self, args) {
          var p = Reflect.apply(target, self, args), u = "", m = "GET";
          try { u = String(args[0] && args[0].url ? args[0].url : args[0]); m = String((args[1] && args[1].method) || (args[0] && args[0].method) || "GET").toUpperCase(); } catch (e) {}
          p.then(function (r) { if (r && r.status >= 400) add("network", m + " " + u + " -> " + r.status + " " + (r.statusText || ""), { kind: "fetch", status: r.status }); },
                 function (err) { add("network", m + " " + u + " failed: " + str(err), { kind: "fetch" }); });
          return p;
        }
      });
    }
    var P = XMLHttpRequest.prototype, oo = P.open, os = P.send;
    P.open = new Proxy(oo, { apply: function (t, self, a) { try { self.__cuReq = [String(a[0]).toUpperCase(), String(a[1])]; } catch (e) {} return Reflect.apply(t, self, a); } });
    P.send = new Proxy(os, { apply: function (t, self, a) {
      try {
        var q = self.__cuReq || ["GET", ""];
        self.addEventListener("loadend", function () {
          if (self.status === 0) add("network", q[0] + " " + q[1] + " failed", { kind: "xhr" });
          else if (self.status >= 400) add("network", q[0] + " " + q[1] + " -> " + self.status + " " + (self.statusText || ""), { kind: "xhr", status: self.status });
        });
      } catch (e) {}
      return Reflect.apply(t, self, a);
    } });
  } catch (e) {}
  Object.defineProperty(window, K, { enumerable: false, value: {
    read: function (levels, grep, since, limit, clear) {
      var out = buf.filter(function (e) {
        return e.n > since && (!levels.length || levels.indexOf(e.level) > -1) && (!grep || e.text.toLowerCase().indexOf(grep.toLowerCase()) > -1);
      });
      var total = out.length;
      if (limit > 0 && out.length > limit) out = out.slice(out.length - limit);
      if (clear) { buf.length = 0; }
      return { entries: out, matched: total, last: seq, page: location.href };
    },
    lines: function (o) {
      return o.entries.map(function (e) {
        var d = new Date(e.t), p = function (n, w) { return String(n).padStart(w, "0"); };
        return "#" + e.n + " " + p(d.getHours(), 2) + ":" + p(d.getMinutes(), 2) + ":" + p(d.getSeconds(), 2) + "." + p(d.getMilliseconds(), 3) + " " + e.level.toUpperCase().padEnd(10) + e.text;
      }).join("\n");
    }
  } });
})();"##;

fn registered() -> &'static Mutex<HashSet<Tab>> {
    static SET: OnceLock<Mutex<HashSet<Tab>>> = OnceLock::new();
    SET.get_or_init(|| Mutex::new(HashSet::new()))
}

pub fn disabled() -> bool {
    std::env::var("CU_CONSOLE").is_ok_and(|v| v.eq_ignore_ascii_case("off"))
}

/// Register the capture for every document this tab loads from now on, once
/// per tab. Best effort: a navigation must never fail over console capture.
pub fn ensure(tab: &Tab) {
    if disabled() {
        return;
    }
    {
        let mut set = registered().lock().unwrap_or_else(|e| e.into_inner());
        if !set.insert(tab.clone()) {
            return;
        }
    }
    // `addScriptToEvaluateOnNewDocument` lives and dies with the DevTools
    // connection that registered it, and the pooled connections come and go.
    // So a connection of its own holds the registration open for as long as
    // the tab lives, and the entry is dropped when that connection ends.
    let tab = tab.clone();
    let params = format!(
        "{{\"source\":{},\"runImmediately\":true}}",
        json_string(CONSOLE_JS)
    );
    let (tx, rx) = std::sync::mpsc::channel::<bool>();
    let owner = tab.clone();
    std::thread::spawn(move || {
        let mut connection = match crate::server::CdpConnection::open(&owner) {
            Ok(c) => c,
            Err(_) => {
                let _ = tx.send(false);
                return;
            }
        };
        // Chromium registers the script only for a session with the Page
        // agent enabled (`Page.enable` is not `Runtime.enable`: nothing in
        // the page can see it).
        let _ = connection.call("Page.enable", "{}");
        if connection.call("Page.addScriptToEvaluateOnNewDocument", &params).is_err() {
            let _ = tx.send(false);
            return;
        }
        let _ = tx.send(true);
        loop {
            let started = std::time::Instant::now();
            let deadline = started + std::time::Duration::from_secs(300);
            let _ = connection.pump_events(deadline, &mut |_| false);
            // A read that ends at once is a closed socket, not a quiet page.
            if started.elapsed() < std::time::Duration::from_secs(2) {
                break;
            }
        }
        registered().lock().unwrap_or_else(|e| e.into_inner()).remove(&owner);
    });
    if !rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap_or(false) {
        registered().lock().unwrap_or_else(|e| e.into_inner()).remove(&tab);
    }
}

/// Read (and optionally clear) the buffer. `text` gives lines, otherwise JSON.
pub fn read(
    tab: &Tab,
    levels: &[String],
    grep: &str,
    since: u64,
    limit: usize,
    clear: bool,
    text: bool,
) -> (&'static str, &'static str, String) {
    if disabled() {
        return (
            "409 Conflict",
            "application/json",
            "{\"error\":\"console capture is off (CU_CONSOLE=off)\"}".into(),
        );
    }
    ensure(tab);
    let lv = format!(
        "[{}]",
        levels.iter().map(|l| json_string(l)).collect::<Vec<_>>().join(",")
    );
    let expr = format!(
        "(function(){{var K=Symbol.for('cu.console');var fresh=false;if(!window[K]){{{install}fresh=true;}}\
         var a=window[K];if(!a)return JSON.stringify({{error:'capture could not be installed on this page'}});\
         var o=a.read({lv},{grep},{since},{limit},{clear});o.fresh=fresh;\
         return {out};}})()",
        install = CONSOLE_JS,
        grep = json_string(grep),
        out = if text {
            "(fresh?'(capture started now; earlier output of this document was not recorded)\\n':'')+(o.entries.length?a.lines(o):'(no matching console output)')+'\\n-- '+o.entries.length+' of '+o.matched+' matching, last #'+o.last+' --'"
        } else {
            "JSON.stringify(o)"
        },
    );
    let params = format!("{{\"expression\":{},\"returnByValue\":true}}", json_string(&expr));
    match tab.command("Runtime.evaluate", &params) {
        Ok(reply) => match crate::server::evaluated_string(&reply) {
            Some(v) if text => ("200 OK", "text/plain; charset=utf-8", v),
            Some(v) => ("200 OK", "application/json", v),
            None => (
                "502 Bad Gateway",
                "application/json",
                "{\"error\":\"page did not answer the console read\"}".into(),
            ),
        },
        Err(e) => (
            "502 Bad Gateway",
            "application/json",
            format!("{{\"error\":{}}}", json_string(&e)),
        ),
    }
}
