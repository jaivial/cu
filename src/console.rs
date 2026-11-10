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
use std::sync::atomic::Ordering;
use std::time::Duration;

/// Levels, in the order `warn+` expands from.
pub const LEVELS: &[&str] = &[
    "debug",
    "log",
    "info",
    "trace",
    "warn",
    "error",
    "exception",
    "csp",
    "network",
    "dialog",
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
        let part = if part == "verbose" {
            "debug".to_string()
        } else {
            part
        };
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
  // Every fetch and XHR, failed or not: what the page asked its API for.
  var net = [], nseq = 0, NMAX = 500;
  function req(method, url, status, ms, kind, error) {
    try {
      url = String(url);
      if (url.indexOf(location.origin + "/") === 0) url = url.slice(location.origin.length);
      if (url.length > 600) url = url.slice(0, 600) + "...";
      var e = { n: ++nseq, t: Date.now(), method: method, url: url, status: status, ms: ms, kind: kind };
      if (error) e.error = String(error).slice(0, 200);
      net.push(e);
      if (net.length > NMAX) net.shift();
    } catch (x) {}
  }
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
          var p = Reflect.apply(target, self, args), u = "", m = "GET", t0 = Date.now();
          try { u = String(args[0] && args[0].url ? args[0].url : args[0]); m = String((args[1] && args[1].method) || (args[0] && args[0].method) || "GET").toUpperCase(); } catch (e) {}
          p.then(function (r) { req(m, u, r ? r.status : 0, Date.now() - t0, "fetch"); if (r && r.status >= 400) add("network", m + " " + u + " -> " + r.status + " " + (r.statusText || ""), { kind: "fetch", status: r.status }); },
                 function (err) { req(m, u, 0, Date.now() - t0, "fetch", str(err)); add("network", m + " " + u + " failed: " + str(err), { kind: "fetch" }); });
          return p;
        }
      });
    }
    var P = XMLHttpRequest.prototype, oo = P.open, os = P.send;
    P.open = new Proxy(oo, { apply: function (t, self, a) { try { self.__cuReq = [String(a[0]).toUpperCase(), String(a[1])]; } catch (e) {} return Reflect.apply(t, self, a); } });
    P.send = new Proxy(os, { apply: function (t, self, a) {
      try {
        var q = self.__cuReq || ["GET", ""], t0 = Date.now();
        self.addEventListener("loadend", function () {
          req(q[0], q[1], self.status, Date.now() - t0, "xhr", self.status === 0 ? "failed" : "");
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
    net: function (grep, since, limit, failed) {
      var out = net.filter(function (e) {
        return e.n > since && (!failed || e.status === 0 || e.status >= 400) && (!grep || (e.method + " " + e.url).toLowerCase().indexOf(grep.toLowerCase()) > -1);
      });
      var total = out.length;
      if (limit > 0 && out.length > limit) out = out.slice(out.length - limit);
      return { requests: out, matched: total, last: nseq, page: location.href };
    },
    netlines: function (o) {
      return o.requests.map(function (e) {
        var d = new Date(e.t), p = function (n, w) { return String(n).padStart(w, "0"); };
        return "#" + e.n + " " + p(d.getHours(), 2) + ":" + p(d.getMinutes(), 2) + ":" + p(d.getSeconds(), 2) + "." + p(d.getMilliseconds(), 3) + " " + e.method.padEnd(7) + e.url + " -> " + (e.status || "failed" + (e.error && e.error !== "failed" ? " (" + e.error + ")" : "")) + "  " + e.ms + " ms";
      }).join("\n");
    },
    lines: function (o) {
      return o.entries.map(function (e) {
        var d = new Date(e.t), p = function (n, w) { return String(n).padStart(w, "0"); };
        return "#" + e.n + " " + p(d.getHours(), 2) + ":" + p(d.getMinutes(), 2) + ":" + p(d.getSeconds(), 2) + "." + p(d.getMilliseconds(), 3) + " " + e.level.toUpperCase().padEnd(10) + e.text;
      }).join("\n");
    }
  } });
})();"##;

pub fn disabled() -> bool {
    std::env::var("CU_CONSOLE").is_ok_and(|v| v.eq_ignore_ascii_case("off"))
}

/// Register the capture for every document this tab loads from now on, once
/// per keeper session. Best effort: a navigation must never fail over console
/// capture.
///
/// `addScriptToEvaluateOnNewDocument` lives and dies with the DevTools
/// session that registered it, and the pooled connections come and go. So
/// it is registered on the tab's keeper ([`crate::keeper`]), the session held
/// open for as long as the tab lives (which also watches it for dialogs);
/// a new keeper registers it again.
pub fn ensure(tab: &Tab) {
    if disabled() {
        return;
    }
    let Ok(keeper) = crate::keeper::ensure(tab) else {
        return;
    };
    if keeper.console.swap(true, Ordering::SeqCst) {
        return;
    }
    let params = format!(
        "{{\"source\":{},\"runImmediately\":true}}",
        json_string(CONSOLE_JS)
    );
    let registered = keeper
        .call(
            "Page.addScriptToEvaluateOnNewDocument",
            &params,
            Duration::from_secs(5),
        )
        .is_ok_and(|reply| !reply.contains("\"error\":{"));
    if !registered {
        keeper.console.store(false, Ordering::SeqCst);
    }
}

/// The tab's dialogs (see [`crate::dialogs`]) that the level and grep
/// filters let through: the `dialog` level of `cu console`.
fn dialog_entries(tab: &Tab, levels: &[String], grep: &str) -> Vec<crate::dialogs::Entry> {
    if !levels.is_empty() && !levels.iter().any(|l| l == "dialog") {
        return Vec::new();
    }
    let grep = grep.to_lowercase();
    crate::dialogs::entries(tab)
        .into_iter()
        .filter(|e| {
            grep.is_empty()
                || e.message.to_lowercase().contains(&grep)
                || e.kind.name().contains(&grep)
        })
        .collect()
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
    let dialogs = dialog_entries(tab, levels, grep);
    // A page stopped by a dialog cannot be read: answer from what the
    // daemon knows, the dialogs.
    if let Some(why) = crate::dialogs::blocked(tab) {
        return if text {
            let mut out = format!("(the page cannot be read: {why})\n");
            for entry in &dialogs {
                out.push_str(&entry.line());
                out.push('\n');
            }
            ("200 OK", "text/plain; charset=utf-8", out)
        } else {
            (
                "200 OK",
                "application/json",
                format!(
                    "{{\"entries\":[],\"blocked\":{},\"dialogs\":[{}]}}",
                    json_string(&why),
                    dialogs
                        .iter()
                        .map(|e| e.json())
                        .collect::<Vec<_>>()
                        .join(",")
                ),
            )
        };
    }
    ensure(tab);
    let lv = format!(
        "[{}]",
        levels
            .iter()
            .map(|l| json_string(l))
            .collect::<Vec<_>>()
            .join(",")
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
    let params = format!(
        "{{\"expression\":{},\"returnByValue\":true}}",
        json_string(&expr)
    );
    match tab.command("Runtime.evaluate", &params) {
        Ok(reply) => match crate::server::evaluated_string(&reply) {
            Some(mut v) if text => {
                for entry in &dialogs {
                    v.push('\n');
                    v.push_str(&entry.line());
                }
                ("200 OK", "text/plain; charset=utf-8", v)
            }
            Some(mut v) => {
                if !dialogs.is_empty() && v.ends_with('}') {
                    let list: Vec<String> = dialogs.iter().map(|e| e.json()).collect();
                    v.insert_str(v.len() - 1, &format!(",\"dialogs\":[{}]", list.join(",")));
                }
                ("200 OK", "application/json", v)
            }
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

/// `GET /v1/net`: the fetch and XHR requests the page made, newest last --
/// method, address (same-origin ones without the origin), status and how long
/// they took. `failed` keeps the ones that did not answer or answered 4xx/5xx.
///
/// This is the part of a network tab a QA pass reads: which endpoints a page
/// calls and what they answer. It comes from the same wrappers that note the
/// failures for `cu console --level network`, kept in a ring of 500 per
/// document, so it costs nothing until it is asked for and it starts with the
/// document (or with the first `cu console` / `cu net` on a tab cu did not
/// open). Scripts, styles and images are not listed; a failed one is in
/// `cu console --level network`.
pub fn net(
    tab: &Tab,
    grep: &str,
    since: u64,
    limit: usize,
    failed: bool,
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
    let expr = format!(
        "(function(){{var K=Symbol.for('cu.console');var fresh=false;if(!window[K]){{{install}fresh=true;}}\
         var a=window[K];if(!a||!a.net)return JSON.stringify({{error:'request capture is not installed on this document; it starts with the next one'}});\
         var o=a.net({grep},{since},{limit},{failed});o.fresh=fresh;\
         return {out};}})()",
        install = CONSOLE_JS,
        grep = json_string(grep),
        out = if text {
            "(fresh?'(capture started now; earlier requests of this document were not recorded)\\n':'')+(o.requests.length?a.netlines(o):'(no matching requests)')+'\\n-- '+o.requests.length+' of '+o.matched+' matching, last #'+o.last+' --'"
        } else {
            "JSON.stringify(o)"
        },
    );
    let params = format!(
        "{{\"expression\":{},\"returnByValue\":true}}",
        json_string(&expr)
    );
    match tab.command("Runtime.evaluate", &params) {
        Ok(reply) => match crate::server::evaluated_string(&reply) {
            Some(v) if text && !v.starts_with("{\"error\"") => {
                ("200 OK", "text/plain; charset=utf-8", v)
            }
            Some(v) => ("200 OK", "application/json", v),
            None => (
                "502 Bad Gateway",
                "application/json",
                "{\"error\":\"page did not answer the request read\"}".into(),
            ),
        },
        Err(e) => (
            "502 Bad Gateway",
            "application/json",
            format!("{{\"error\":{}}}", json_string(&e)),
        ),
    }
}

/// The most entries a reply carries; the count says how many there were.
const PROBLEMS_SHOWN: usize = 10;

/// What went wrong on `tab` since the last reply that said so: errors,
/// uncaught exceptions, CSP violations and requests that failed or answered
/// 4xx/5xx. `None` when there is nothing new (or nothing can be read), else a
/// JSON object `{"new":N,"entries":[{"level":..,"text":..}]}` with the last
/// few entries, for the reply of the action that is about to be answered.
///
/// An agent reads the console when it thinks of it (4 times in 80 commands
/// of one QA session, measured), and an error it never asked about is an
/// error the QA missed. So the reply to `navigate` and to every action
/// carries what is new, at the price of one evaluation. Warnings and plain
/// log lines stay in `cu console`: a development build writes dozens.
///
/// "Since the last reply" is by the page's clock, per tab, so it survives the
/// navigation an action causes; what a document wrote before it was replaced
/// is gone with it, as it is for `cu console`.
pub fn problems(
    tab: &Tab,
    command: &mut dyn FnMut(&str, &str) -> Result<String, String>,
) -> Option<String> {
    if disabled() || crate::dialogs::blocked(tab).is_some() {
        return None;
    }
    let since = seen()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(tab)
        .copied()
        .unwrap_or(0);
    let reply = command("Runtime.evaluate", &problems_params(since)).ok()?;
    let value = crate::server::evaluated_string(&reply)?;
    let now = crate::server::json_number(&value, "now")? as u64;
    seen()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(tab.clone(), now);
    let count = crate::server::json_number(&value, "new").unwrap_or(0.0);
    (count >= 1.0).then(|| {
        // Without the clock: `{"new":N,"entries":[...]}`.
        match value.find("\"new\":") {
            Some(at) => format!("{{{}", &value[at..]),
            None => value,
        }
    })
}

/// The evaluation behind [`problems`]: the entries of the four levels newer
/// than `since` (the page's `Date.now()`), cut to the last few and to 300
/// characters each, with the page's clock first.
fn problems_params(since: u64) -> String {
    let expr = format!(
        "(function(){{var a=window[Symbol.for('cu.console')],now=Date.now();\
         if(!a)return JSON.stringify({{now:now,new:0}});\
         var e=a.read(['error','exception','csp','network'],'',0,0,false).entries\
         .filter(function(x){{return x.t>{since};}});\
         return JSON.stringify({{now:now,new:e.length,entries:e.slice(-{PROBLEMS_SHOWN}).map(function(x){{\
         var o={{level:x.level,text:x.text.length>300?x.text.slice(0,300)+'...':x.text}};\
         if(x.status)o.status=x.status;return o;}})}});}})()"
    );
    format!(
        "{{\"expression\":{},\"returnByValue\":true}}",
        json_string(&expr)
    )
}

/// The page clock at the last [`problems`] reply of each tab.
fn seen() -> &'static std::sync::Mutex<std::collections::HashMap<Tab, u64>> {
    static SEEN: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<Tab, u64>>> =
        std::sync::OnceLock::new();
    SEEN.get_or_init(Default::default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn problems_are_the_four_levels_since_a_time() {
        let params = problems_params(1_794_000_000_123);
        // Errors, exceptions, CSP and failed requests; not warnings or logs.
        assert!(params.contains("['error','exception','csp','network']"));
        assert!(!params.contains("'warn'") && !params.contains("'log'"));
        assert!(params.contains("x.t>1794000000123"));
        // The reply starts with the page's clock, which is the next `since`.
        assert!(params.contains("now:now,new:e.length"));
        assert!(params.contains(&format!("slice(-{PROBLEMS_SHOWN})")));
    }

    #[test]
    fn the_page_script_keeps_every_request_and_still_notes_the_failures() {
        // One ring for what `cu net` lists, the old one for `cu console`.
        assert!(CONSOLE_JS.contains("function req(method, url, status, ms, kind, error)"));
        assert!(CONSOLE_JS.contains("net: function (grep, since, limit, failed)"));
        assert!(CONSOLE_JS.contains(r#"add("network", m + " " + u + " -> " + r.status"#));
        // Both wrappers report to it: fetch on either outcome, XHR on loadend.
        assert_eq!(CONSOLE_JS.matches("req(m, u, ").count(), 2);
        assert_eq!(CONSOLE_JS.matches("req(q[0], q[1], ").count(), 1);
    }
}
