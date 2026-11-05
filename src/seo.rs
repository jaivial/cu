//! SEO on top of the browser: page audits, a polite same-site crawl,
//! location-aware search result pages.
//!
//! Everything here reads pages the way a visitor's browser does -- in cu's
//! isolated world, so the page's own scripts never see it -- and nothing
//! here defeats a defence. A search engine that answers with a CAPTCHA, a
//! `/sorry/` page or a rate limit stops the run with the challenge state
//! [`crate::challenge`] reports, and a person decides what happens next.
//! The crawler obeys `robots.txt` (its `Crawl-delay` too), stays on one
//! host and waits between pages; search result pages are spaced per engine
//! ([`serp_interval`]).
//!
//! **Locations.** [`apply_geo`] gives a named browser context one coherent
//! [`crate::geo::Location`]: an optional proxy (fixed when the context is
//! created), and the locale, `Accept-Language`, time zone and
//! `navigator.geolocation` position as DevTools overrides. Those overrides
//! live as long as the DevTools session that set them, so each geo context
//! keeps a dedicated connection open for its whole life ([`GeoState`]).
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::geo::{self, Engine, Location};
use crate::server::{
    CdpConnection, Tab, evaluated_string, json_array, json_number, json_object, json_objects,
    json_string, json_string_value, json_strings,
};

// ---------------------------------------------------------------------------
// Page evaluation
// ---------------------------------------------------------------------------

/// Evaluate `expression` in cu's isolated world of the tab's main frame and
/// return the string it produced (the scripts here all `JSON.stringify`).
pub fn eval(tab: &Tab, expression: &str, await_promise: bool) -> Result<String, String> {
    let mut cmd = |m: &str, p: &str| tab.command(m, p);
    let frame = crate::execution::main_frame(&mut cmd)?;
    let context = crate::execution::isolated_context(&mut cmd, &frame)?;
    let params = crate::execution::evaluate_params(
        expression,
        &format!("\"contextId\":{context},"),
        await_promise,
    );
    let reply = tab.command("Runtime.evaluate", &params)?;
    if reply.contains("\"exceptionDetails\"") {
        let detail = json_object(&reply, "exceptionDetails")
            .and_then(|d| {
                json_string_value(d, "description").or_else(|| json_string_value(d, "text"))
            })
            .unwrap_or_else(|| "the page script failed".into());
        return Err(detail.lines().next().unwrap_or_default().to_string());
    }
    evaluated_string(&reply).ok_or_else(|| "the page returned nothing".into())
}

/// Navigate `tab` to `url` through cu's scheduler (per-origin pacing,
/// challenge classification) and answer with the navigation's JSON, or an
/// error when the browser could not go there.
fn navigate(tab: &Tab, url: &str) -> Result<String, String> {
    let (status, _, body) = crate::server::navigate_scheduled(tab, url, None);
    if !status.starts_with('2') {
        return Err(json_string_value(&body, "error").unwrap_or(body));
    }
    Ok(body)
}

/// The challenge state a navigation reported (`ready`, `unknown`,
/// `human_required`...).
fn challenge_state(navigation: &str) -> String {
    json_object(navigation, "challenge")
        .and_then(|c| json_string_value(c, "state"))
        .unwrap_or_else(|| "unknown".into())
}

/// A state no automatic step may continue through.
fn stops_the_run(state: &str) -> bool {
    matches!(
        state,
        "human_required" | "blocked" | "rate_limited" | "challenge_pending"
    )
}

// ---------------------------------------------------------------------------
// Audit
// ---------------------------------------------------------------------------

/// The in-page audit: takes the keywords, answers with the report as JSON.
/// Read-only; the one request it makes is a same-origin `HEAD` of the page
/// itself, for the `X-Robots-Tag` and status a script cannot see otherwise.
pub const AUDIT_JS: &str = r#"async function (kws) {
  const clean = (s) => (s || '').replace(/\s+/g, ' ').trim();
  const txt = (el) => el ? clean(el.textContent) : '';
  // A heading drawn with an image (a logo in the h1) reads as its alt text,
  // as a search engine reads it.
  const htxt = (el) => {
    if (!el) return '';
    const own = clean(el.textContent);
    const alts = [...el.querySelectorAll('img[alt], svg[aria-label], [aria-label]')]
      .map(i => clean(i.getAttribute('alt') || i.getAttribute('aria-label'))).filter(Boolean).join(' ');
    return clean(own + ' ' + alts);
  };
  const abs = (u) => { try { return new URL(u, location.href).href; } catch (e) { return null; } };
  const meta = (sel) => { const m = document.querySelector(sel); return m ? clean(m.getAttribute('content')) : null; };
  const fold = (s) => (s || '').toLowerCase().normalize('NFD').replace(/[\u0300-\u036f]/g, '');
  let head = {};
  try {
    const r = await fetch(location.href, { method: 'HEAD', cache: 'no-store', credentials: 'include', redirect: 'follow' });
    head = { status: r.status, x_robots_tag: r.headers.get('x-robots-tag'), content_type: r.headers.get('content-type'), last_modified: r.headers.get('last-modified') };
  } catch (e) { head = { error: String(e) }; }
  const nav = performance.getEntriesByType('navigation')[0] || {};
  const title = clean(document.title);
  const description = meta('meta[name="description" i]');
  const robots = meta('meta[name="robots" i]');
  const viewport = meta('meta[name="viewport" i]');
  const canonicalEl = document.querySelector('link[rel~="canonical" i]');
  const canonical = canonicalEl ? abs(canonicalEl.getAttribute('href')) : null;
  const hreflang = [...document.querySelectorAll('link[rel~="alternate" i][hreflang]')]
    .slice(0, 60).map(l => ({ lang: l.getAttribute('hreflang'), href: abs(l.getAttribute('href')) }));
  const counts = {}; const outline = [];
  for (const h of document.querySelectorAll('h1,h2,h3,h4,h5,h6')) {
    const t = h.tagName.toLowerCase(); counts[t] = (counts[t] || 0) + 1;
    if (outline.length < 80) outline.push(t + ': ' + htxt(h).slice(0, 140));
  }
  const h1 = [...document.querySelectorAll('h1')].map(htxt);
  const sub = [...document.querySelectorAll('h2,h3')].map(htxt).join(' \n ');
  const body = document.body ? document.body.innerText : '';
  const words = body.split(/\s+/).filter(w => /[\p{L}\p{N}]/u.test(w));
  const total = words.length;
  const imgs = [...document.images];
  const noAlt = imgs.filter(i => !i.hasAttribute('alt'));
  const alts = imgs.map(i => i.getAttribute('alt') || '').join(' \n ');
  const host = location.hostname.replace(/^www\./, '');
  const internal = []; const seen = new Set(); let external = 0, nofollow = 0; const externalHosts = {};
  for (const a of document.querySelectorAll('a[href]')) {
    const href = abs(a.getAttribute('href'));
    if (!href || !/^https?:/.test(href)) continue;
    const u = new URL(href); u.hash = '';
    if ((a.getAttribute('rel') || '').toLowerCase().includes('nofollow')) nofollow++;
    const h = u.hostname.replace(/^www\./, '');
    if (h === host) { if (!seen.has(u.href)) { seen.add(u.href); internal.push(u.href); } }
    else { external++; externalHosts[h] = (externalHosts[h] || 0) + 1; }
  }
  const jsonld = [];
  for (const s of document.querySelectorAll('script[type="application/ld+json" i]')) {
    try {
      const d = JSON.parse(s.textContent);
      const items = Array.isArray(d) ? d : (d['@graph'] || [d]);
      for (const it of items) { const t = it && it['@type']; if (t) jsonld.push(Array.isArray(t) ? t.join(',') : String(t)); }
    } catch (e) { jsonld.push('invalid JSON-LD'); }
  }
  const og = {};
  for (const m of document.querySelectorAll('meta[property^="og:" i], meta[name^="twitter:" i]'))
    og[(m.getAttribute('property') || m.getAttribute('name')).toLowerCase()] = clean(m.getAttribute('content')).slice(0, 300);
  const bodyF = fold(body);
  const path = fold(decodeURIComponent(location.pathname)).replace(/[-_/.]+/g, ' ');
  const first100 = fold(words.slice(0, 100).join(' '));
  const keywords = {};
  for (const k of kws) {
    const f = fold(k).trim(); if (!f) continue;
    const esc = f.replace(/[.*+?^${}()|[\]\\]/g, '\\$&').replace(/\s+/g, '\\s+');
    const src = '(^|[^\\p{L}\\p{N}])' + esc + '(?=$|[^\\p{L}\\p{N}])';
    const has = (s) => new RegExp(src, 'u').test(fold(s));
    const count = (bodyF.match(new RegExp(src, 'gu')) || []).length;
    const n = f.split(/\s+/).length;
    const at = bodyF.search(new RegExp(src, 'u'));
    keywords[k] = {
      count, in_body: count > 0, density: total ? +(100 * count * n / total).toFixed(2) : 0,
      first_word: at < 0 ? null : bodyF.slice(0, at).split(/\s+/).filter(Boolean).length + 1,
      in_title: has(title), in_description: has(description || ''), in_h1: has(h1.join(' \n ')),
      in_h2_h3: has(sub), in_url: path.includes(f), in_alt: has(alts), in_first_100_words: has(first100),
    };
  }
  const stop = new Set(('de la que el en y a los del se las por un para con no una su al lo como mas más pero sus le ya o este si porque esta entre cuando muy sin sobre tambien también me hasta hay donde quien desde todo nos durante todos uno les ni contra otros ese eso ante ellos e esto mi antes algunos que unos yo otro otras otra el tanto esa estos mucho quienes nada muchos cual poco ella estar estas algunas algo nosotros es son fue ser has ha tu te ti the of and to in a is that for it on with as was at by an be this are from or have not your you we our can will all more its but they their his her he she which also than into about if so no do does how what when where who why’s ' +
    'cookies cookie inicio home menu menú').split(/\s+/));
  const tally = (n) => {
    const m = {};
    const ws = words.map(w => fold(w).replace(/[^\p{L}\p{N}]+/gu, '')).filter(Boolean);
    for (let i = 0; i + n <= ws.length; i++) {
      const g = ws.slice(i, i + n);
      if (stop.has(g[0]) || stop.has(g[n - 1]) || g.some(w => w.length < 2 || /^\d+$/.test(w))) continue;
      const k = g.join(' '); m[k] = (m[k] || 0) + 1;
    }
    return Object.entries(m).filter(e => e[1] > 1).sort((a, b) => b[1] - a[1]).slice(0, 15).map(e => ({ term: e[0], count: e[1] }));
  };
  const issues = [];
  const status = head.status || nav.responseStatus || 0;
  if (status >= 400) issues.push('status ' + status);
  if (!title) issues.push('missing title');
  else if (title.length > 60) issues.push('title too long (' + title.length + ' > 60)');
  else if (title.length < 15) issues.push('title too short (' + title.length + ' < 15)');
  if (!description) issues.push('missing meta description');
  else if (description.length > 160) issues.push('meta description too long (' + description.length + ' > 160)');
  else if (description.length < 50) issues.push('meta description too short (' + description.length + ' < 50)');
  if (h1.length === 0) issues.push('no h1');
  else if (h1.every(t => !t)) issues.push('h1 has no text (nor alt text)');
  if (h1.length > 1) issues.push('multiple h1 (' + h1.length + ')');
  const index = fold((robots || '') + ',' + (head.x_robots_tag || ''));
  if (/noindex/.test(index)) issues.push('noindex');
  if (/nofollow/.test(index)) issues.push('nofollow (page level)');
  if (canonical && canonical.replace(/\/$/, '') !== location.href.split('#')[0].replace(/\/$/, '')) issues.push('canonical points elsewhere: ' + canonical);
  if (!canonical) issues.push('no canonical');
  if (!document.documentElement.lang) issues.push('missing html lang');
  if (!viewport) issues.push('missing viewport');
  if (noAlt.length) issues.push('images without alt: ' + noAlt.length);
  if (total < 300) issues.push('thin content (' + total + ' words)');
  if (!og['og:title']) issues.push('no Open Graph title');
  for (const [k, v] of Object.entries(keywords)) {
    if (!v.count && !v.in_title && !v.in_h1 && !v.in_description) issues.push('keyword "' + k + '" not on the page');
    else if (!v.count) issues.push('keyword "' + k + '" not in the visible text');
    if (v.count && !v.in_title) issues.push('keyword "' + k + '" not in the title');
    if (v.density > 4) issues.push('keyword "' + k + '" density ' + v.density + '% (stuffing?)');
  }
  return JSON.stringify({
    url: location.href, status, redirects: nav.redirectCount || 0, head,
    lang: document.documentElement.lang || null, charset: document.characterSet,
    title, title_length: title.length, meta_description: description,
    meta_description_length: description ? description.length : 0, robots, canonical, viewport, hreflang,
    headings: counts, h1_texts: h1, outline, word_count: total,
    images: { total: imgs.length, without_alt: noAlt.length, empty_alt: imgs.filter(i => i.hasAttribute('alt') && !i.getAttribute('alt').trim()).length },
    links: { internal: internal.length, external, nofollow,
      external_hosts: Object.entries(externalHosts).sort((a, b) => b[1] - a[1]).slice(0, 15).map(e => e[0]),
      internal_urls: internal.slice(0, 500) },
    structured_data: jsonld, social: og, keywords,
    top_terms: { words: tally(1), phrases: tally(2), trigrams: tally(3) },
    load_ms: nav.duration ? Math.round(nav.duration) : null, issues,
  });
}"#;

/// `POST /v1/seo/audit`: `{url?, keywords?}` -- navigate (when a URL is
/// given) and audit the page the tab is on.
pub fn audit(tab: &Tab, body: &str) -> (&'static str, &'static str, String) {
    let url = json_string_value(body, "url").unwrap_or_default();
    let keywords = json_array(body, "keywords")
        .map(|a| json_strings(&a))
        .unwrap_or_default();
    let mut navigation = None;
    if !url.is_empty() {
        match navigate(tab, &url) {
            Ok(n) => navigation = Some(n),
            Err(e) => return error("502 Bad Gateway", &e),
        }
    }
    let state = navigation.as_deref().map(challenge_state);
    if let Some(state) = state.as_deref().filter(|s| stops_the_run(s)) {
        return (
            "409 Conflict",
            "application/json",
            format!(
                "{{\"error\":{},\"challenge\":{}}}",
                json_string(&format!(
                    "the site answered with a defence page ({state}); not audited. A person can deal with it in the live viewer"
                )),
                navigation
                    .as_deref()
                    .and_then(|n| json_object(n, "challenge"))
                    .unwrap_or("null")
            ),
        );
    }
    let expression = format!("({AUDIT_JS})({})", json_list(&keywords));
    match eval(tab, &expression, true) {
        Ok(report) => {
            let report = match (state, report.strip_suffix('}')) {
                (Some(state), Some(open)) => {
                    format!("{open},\"challenge_state\":{}}}", json_string(&state))
                }
                _ => report,
            };
            ("200 OK", "application/json", report)
        }
        Err(e) => error("502 Bad Gateway", &format!("audit failed: {e}")),
    }
}

/// `GET /v1/seo/robots`: the current origin's `robots.txt`, fetched by the
/// page itself (same origin, same cookies, same proxy).
pub fn robots(tab: &Tab) -> (&'static str, &'static str, String) {
    const JS: &str = r#"(async () => {
      try {
        const r = await fetch('/robots.txt', { cache: 'no-store', credentials: 'omit' });
        const t = r.ok ? await r.text() : '';
        return JSON.stringify({ origin: location.origin, status: r.status, text: t.slice(0, 200000) });
      } catch (e) { return JSON.stringify({ origin: location.origin, status: 0, text: '', error: String(e) }); }
    })()"#;
    match eval(tab, JS, true) {
        Ok(v) => ("200 OK", "application/json", v),
        Err(e) => error("502 Bad Gateway", &e),
    }
}

fn error(status: &'static str, message: &str) -> (&'static str, &'static str, String) {
    (
        status,
        "application/json",
        format!("{{\"error\":{}}}", json_string(message)),
    )
}

fn json_list(items: &[String]) -> String {
    format!(
        "[{}]",
        items
            .iter()
            .map(|s| json_string(s))
            .collect::<Vec<_>>()
            .join(",")
    )
}

// ---------------------------------------------------------------------------
// robots.txt
// ---------------------------------------------------------------------------

/// The rules of the `robots.txt` group that applies to cu.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Robots {
    /// `(allow, pattern)` in file order.
    pub rules: Vec<(bool, String)>,
    pub crawl_delay: Option<f64>,
    pub sitemaps: Vec<String>,
}

impl Robots {
    /// The group for `agent` (its own if the file names it, `*` otherwise),
    /// per RFC 9309.
    pub fn parse(text: &str, agent: &str) -> Self {
        let agent = agent.to_ascii_lowercase();
        // Groups: the agents they name and their lines.
        let mut groups: Vec<(Vec<String>, Vec<(String, String)>)> = Vec::new();
        let mut sitemaps = Vec::new();
        let mut in_agents = false;
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let key = key.trim().to_ascii_lowercase();
            let value = value.trim().to_string();
            match key.as_str() {
                "user-agent" => {
                    if !in_agents {
                        groups.push((Vec::new(), Vec::new()));
                    }
                    in_agents = true;
                    if let Some(g) = groups.last_mut() {
                        g.0.push(value.to_ascii_lowercase());
                    }
                }
                "sitemap" => sitemaps.push(value),
                _ => {
                    in_agents = false;
                    if let Some(g) = groups.last_mut() {
                        g.1.push((key, value));
                    }
                }
            }
        }
        let own: Vec<_> = groups
            .iter()
            .filter(|g| g.0.iter().any(|a| a == &agent))
            .collect();
        let chosen: Vec<_> = if own.is_empty() {
            groups
                .iter()
                .filter(|g| g.0.iter().any(|a| a == "*"))
                .collect()
        } else {
            own
        };
        let mut robots = Robots {
            sitemaps,
            ..Default::default()
        };
        for (_, lines) in chosen {
            for (key, value) in lines {
                match key.as_str() {
                    "allow" if !value.is_empty() => robots.rules.push((true, value.clone())),
                    "disallow" if !value.is_empty() => robots.rules.push((false, value.clone())),
                    "crawl-delay" => robots.crawl_delay = value.parse().ok().or(robots.crawl_delay),
                    _ => {}
                }
            }
        }
        robots
    }

    /// Whether `path` (with its query) may be fetched: the longest matching
    /// rule wins, `Allow` on a tie.
    pub fn allowed(&self, path: &str) -> bool {
        let mut best: Option<(usize, bool)> = None;
        for (allow, pattern) in &self.rules {
            if pattern_matches(pattern, path) {
                let len = pattern.len();
                best = match best {
                    Some((l, a)) if l > len || (l == len && a) => Some((l, a)),
                    _ => Some((len, *allow)),
                };
            }
        }
        best.map(|(_, allow)| allow).unwrap_or(true)
    }
}

/// A robots.txt path pattern: a prefix, with `*` for any run and a final
/// `$` for the end.
fn pattern_matches(pattern: &str, path: &str) -> bool {
    let (pattern, anchored) = match pattern.strip_suffix('$') {
        Some(p) => (p, true),
        None => (pattern, false),
    };
    let parts: Vec<&str> = pattern.split('*').collect();
    let mut at = 0usize;
    for (i, part) in parts.iter().enumerate() {
        if i == 0 {
            if !path.starts_with(part) {
                return false;
            }
            at = part.len();
            continue;
        }
        if part.is_empty() {
            continue;
        }
        let last = i == parts.len() - 1;
        if last && anchored {
            return path.len() >= at + part.len() && path.ends_with(part);
        }
        match path[at..].find(part) {
            Some(found) => at += found + part.len(),
            None => return false,
        }
    }
    !anchored || at == path.len() || pattern.ends_with('*')
}

// ---------------------------------------------------------------------------
// Crawl
// ---------------------------------------------------------------------------

/// What a crawl is allowed to do.
#[derive(Clone, Debug)]
pub struct CrawlOptions {
    pub max_pages: usize,
    pub max_depth: usize,
    /// Pause between pages; `robots.txt`'s `Crawl-delay` wins when longer.
    pub delay: Duration,
    pub keywords: Vec<String>,
}

/// One crawled page, as the summary sees it.
#[derive(Clone, Debug, Default)]
pub struct CrawledPage {
    pub url: String,
    pub depth: usize,
    pub status: u16,
    pub title: String,
    pub description: String,
    pub issues: Vec<String>,
}

/// Split `url` into `(scheme://host, path?query)`, fragment dropped.
pub fn split_url(url: &str) -> Option<(String, String)> {
    let url = url.split('#').next()?;
    let (scheme, rest) = url.split_once("://")?;
    let (host, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => match rest.find('?') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        },
    };
    let path = if path.starts_with('?') {
        format!("/{path}")
    } else {
        path.to_string()
    };
    Some((
        format!(
            "{}://{}",
            scheme.to_ascii_lowercase(),
            host.to_ascii_lowercase()
        ),
        path,
    ))
}

/// Links a crawler never opens: files, not pages.
fn is_asset(path: &str) -> bool {
    let p = path.split('?').next().unwrap_or("").to_ascii_lowercase();
    [
        ".jpg", ".jpeg", ".png", ".gif", ".webp", ".svg", ".pdf", ".zip", ".gz", ".mp4", ".mp3",
        ".webm", ".avi", ".mov", ".css", ".js", ".json", ".xml", ".ico", ".woff", ".woff2", ".ttf",
        ".doc", ".docx", ".xls", ".xlsx", ".ppt", ".pptx", ".rar", ".exe", ".dmg",
    ]
    .iter()
    .any(|ext| p.ends_with(ext))
}

/// Crawl one site breadth-first through `request` (the CLI's daemon call:
/// method, path, body). Each page's full report goes to `emit`; the
/// summary JSON comes back.
pub fn crawl(
    start: &str,
    context: &str,
    options: &CrawlOptions,
    request: &mut dyn FnMut(&str, &str, Option<&str>) -> Result<String, String>,
    emit: &mut dyn FnMut(&CrawledPage, &str),
    sleep: &mut dyn FnMut(Duration),
) -> Result<String, String> {
    let (origin, _) = split_url(start).ok_or("give an absolute http(s) URL")?;
    let host_of = |o: &str| {
        o.split_once("://")
            .map(|(_, h)| h.trim_start_matches("www.").to_string())
            .unwrap_or_default()
    };
    let site = host_of(&origin);
    let mut queue: std::collections::VecDeque<(String, usize)> = std::collections::VecDeque::from(
        [(start.split('#').next().unwrap_or(start).to_string(), 0)],
    );
    let mut seen: std::collections::HashSet<String> =
        queue.iter().map(|(u, _)| u.clone()).collect();
    let mut robots: Option<Robots> = None;
    let mut robots_status = 0u16;
    let mut pages: Vec<CrawledPage> = Vec::new();
    let mut skipped_robots = 0usize;
    let mut stopped: Option<String> = None;
    let mut delay = options.delay;
    let keywords = json_list(&options.keywords);
    while let Some((url, depth)) = queue.pop_front() {
        if pages.len() >= options.max_pages {
            break;
        }
        let Some((_, path)) = split_url(&url) else {
            continue;
        };
        if let Some(r) = &robots
            && !r.allowed(&path)
        {
            skipped_robots += 1;
            continue;
        }
        if !pages.is_empty() {
            sleep(delay);
        }
        let body = format!("{{\"url\":{},\"keywords\":{keywords}}}", json_string(&url));
        let report = match request(
            "POST",
            &format!("/v1/seo/audit?context={context}"),
            Some(&body),
        ) {
            Ok(r) => r,
            Err(e) => {
                if e.contains("defence page") || e.contains("\"challenge\"") {
                    stopped = Some(format!(
                        "stopped at {url}: {}",
                        json_string_value(&e, "error").unwrap_or(e)
                    ));
                    break;
                }
                let page = CrawledPage {
                    url: url.clone(),
                    depth,
                    issues: vec![format!(
                        "not loaded: {}",
                        json_string_value(&e, "error").unwrap_or(e.clone())
                    )],
                    ..Default::default()
                };
                emit(
                    &page,
                    &format!(
                        "{{\"url\":{},\"error\":{}}}",
                        json_string(&url),
                        json_string(&e)
                    ),
                );
                pages.push(page);
                continue;
            }
        };
        // robots.txt is read once, from the first page the browser opened,
        // and the first page itself is checked against it too.
        if robots.is_none() {
            let reply = request("GET", &format!("/v1/seo/robots?context={context}"), None)
                .unwrap_or_default();
            robots_status = json_number(&reply, "status").unwrap_or(0.0) as u16;
            let parsed =
                Robots::parse(&json_string_value(&reply, "text").unwrap_or_default(), "cu");
            if let Some(d) = parsed.crawl_delay {
                delay = delay.max(Duration::from_secs_f64(d.min(60.0)));
            }
            let first_allowed = parsed.allowed(&path);
            robots = Some(parsed);
            if !first_allowed {
                stopped = Some(format!("robots.txt disallows {path}; nothing crawled"));
                break;
            }
        }
        let page = CrawledPage {
            url: json_string_value(&report, "url").unwrap_or_else(|| url.clone()),
            depth,
            status: json_number(&report, "status").unwrap_or(0.0) as u16,
            title: json_string_value(&report, "title").unwrap_or_default(),
            description: json_string_value(&report, "meta_description").unwrap_or_default(),
            issues: json_array(&report, "issues")
                .map(|a| json_strings(&a))
                .unwrap_or_default(),
        };
        emit(&page, &report);
        // The page a redirect ended on counts as seen too.
        seen.insert(page.url.clone());
        pages.push(page);
        if depth >= options.max_depth {
            continue;
        }
        let links = json_object(&report, "links")
            .and_then(|l| json_array(l, "internal_urls"))
            .map(|a| json_strings(&a))
            .unwrap_or_default();
        for link in links {
            let Some((link_origin, link_path)) = split_url(&link) else {
                continue;
            };
            if host_of(&link_origin) != site || is_asset(&link_path) {
                continue;
            }
            let link = format!("{link_origin}{link_path}");
            if seen.insert(link.clone()) {
                queue.push_back((link, depth + 1));
            }
        }
    }
    Ok(crawl_summary(
        &origin,
        &pages,
        robots.as_ref(),
        robots_status,
        skipped_robots,
        queue.len(),
        stopped,
    ))
}

fn crawl_summary(
    origin: &str,
    pages: &[CrawledPage],
    robots: Option<&Robots>,
    robots_status: u16,
    skipped_robots: usize,
    left: usize,
    stopped: Option<String>,
) -> String {
    let mut issue_counts: HashMap<String, usize> = HashMap::new();
    for p in pages {
        for i in &p.issues {
            // Group "title too long (72 > 60)" with its siblings.
            let key = i
                .split(" (")
                .next()
                .unwrap_or(i)
                .split(':')
                .next()
                .unwrap_or(i)
                .to_string();
            *issue_counts.entry(key).or_default() += 1;
        }
    }
    let duplicates = |field: fn(&CrawledPage) -> &str| {
        let mut by: HashMap<&str, Vec<&str>> = HashMap::new();
        for p in pages {
            let v = field(p);
            if !v.is_empty() {
                by.entry(v).or_default().push(&p.url);
            }
        }
        let mut dups: Vec<String> = by
            .into_iter()
            .filter(|(_, urls)| urls.len() > 1)
            .map(|(v, urls)| {
                format!(
                    "{{\"value\":{},\"urls\":{}}}",
                    json_string(v),
                    json_list(&urls.iter().map(|u| u.to_string()).collect::<Vec<_>>())
                )
            })
            .collect();
        dups.sort();
        format!("[{}]", dups.join(","))
    };
    let mut issues: Vec<(String, usize)> = issue_counts.into_iter().collect();
    issues.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let errors: Vec<String> = pages
        .iter()
        .filter(|p| p.status >= 400 || p.status == 0)
        .map(|p| {
            format!(
                "{{\"url\":{},\"status\":{}}}",
                json_string(&p.url),
                p.status
            )
        })
        .collect();
    format!(
        "{{\"origin\":{},\"pages\":{},\"left_in_queue\":{left},\"skipped_by_robots\":{skipped_robots},\"robots_txt\":{{\"status\":{robots_status},\"rules\":{},\"crawl_delay\":{},\"sitemaps\":{}}},\"issues\":{{{}}},\"duplicate_titles\":{},\"duplicate_descriptions\":{},\"errors\":[{}],\"stopped\":{}}}",
        json_string(origin),
        pages.len(),
        robots.map(|r| r.rules.len()).unwrap_or(0),
        robots
            .and_then(|r| r.crawl_delay)
            .map(|d| d.to_string())
            .unwrap_or_else(|| "null".into()),
        json_list(&robots.map(|r| r.sitemaps.clone()).unwrap_or_default()),
        issues
            .iter()
            .map(|(k, n)| format!("{}:{n}", json_string(k)))
            .collect::<Vec<_>>()
            .join(","),
        duplicates(|p| &p.title),
        duplicates(|p| &p.description),
        errors.join(","),
        stopped
            .map(|s| json_string(&s))
            .unwrap_or_else(|| "null".into()),
    )
}

// ---------------------------------------------------------------------------
// Locations
// ---------------------------------------------------------------------------

/// A location applied to a context, and the DevTools session that keeps its
/// overrides alive.
struct GeoState {
    location: Location,
    proxy: Option<String>,
    target: String,
    browser_context: String,
    connection: Option<CdpConnection>,
    /// The browser-level session that granted the geolocation permission:
    /// Chromium resets a permission override when the session that set it
    /// detaches, so it is kept open too.
    permission: Option<CdpConnection>,
}

fn geo_states() -> &'static Mutex<HashMap<(u16, String), GeoState>> {
    static STATES: OnceLock<Mutex<HashMap<(u16, String), GeoState>>> = OnceLock::new();
    STATES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Drop the location of a context that was closed.
pub fn forget_geo(cdp_port: u16, name: &str) {
    geo_states()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&(cdp_port, name.to_string()));
}

/// The location applied to context `name`, if any.
pub fn geo_of(cdp_port: u16, name: &str) -> Option<(Location, Option<String>)> {
    geo_states()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&(cdp_port, name.to_string()))
        .map(|s| (s.location.clone(), s.proxy.clone()))
}

/// A proxy URL the browser accepts: `scheme://host:port`, no credentials
/// (Chromium does not take them in `proxyServer`; an authenticating proxy
/// needs an allow-listed IP for now).
pub fn check_proxy(proxy: &str) -> Result<String, String> {
    let proxy = proxy.trim();
    let (scheme, rest) = proxy
        .split_once("://")
        .ok_or("a proxy is scheme://host:port (http, https, socks4, socks5)")?;
    if !matches!(scheme, "http" | "https" | "socks4" | "socks5") {
        return Err(format!("unsupported proxy scheme {scheme}"));
    }
    if rest.contains('@') {
        return Err(
            "proxy credentials in the URL are not supported: Chromium ignores them; use a proxy that \
             authorises this machine's IP"
                .into(),
        );
    }
    let host_port = rest.trim_end_matches('/');
    if host_port.is_empty() || host_port.contains('/') {
        return Err("a proxy is scheme://host:port".into());
    }
    Ok(format!("{scheme}://{host_port}"))
}

/// Nominatim answers, cached on disk so a place is asked about once.
fn cached_lookup(data: &Path, query: &str) -> Option<String> {
    let cache = std::fs::read_to_string(data.join("geo-cache.tsv")).ok()?;
    cache.lines().find_map(|l| {
        let (q, json) = l.split_once('\t')?;
        (q == query).then(|| json.to_string())
    })
}

fn store_lookup(data: &Path, query: &str, json: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(data.join("geo-cache.tsv"))
    {
        let _ = writeln!(
            f,
            "{}\t{}",
            query.replace(['\t', '\n'], " "),
            json.replace('\n', " ")
        );
    }
}

/// Look a place up with OpenStreetMap Nominatim, through the context's own
/// tab (so through its proxy), at most once a second as its usage policy
/// asks, and cached.
fn lookup(tab: &Tab, data: &Path, query: &str) -> Result<Option<geo::Place>, String> {
    if let Some(json) = cached_lookup(data, query) {
        return Ok(geo::parse_nominatim(&json));
    }
    static LAST: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();
    {
        let mut last = LAST
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(at) = *last {
            let wait = Duration::from_millis(1100).saturating_sub(at.elapsed());
            std::thread::sleep(wait);
        }
        *last = Some(Instant::now());
    }
    navigate(tab, &geo::nominatim_url(query))?;
    let text = eval(
        tab,
        "JSON.stringify(document.body ? document.body.innerText : '')",
        false,
    )?;
    let json = json_string_value(&format!("{{\"t\":{text}}}"), "t").unwrap_or_default();
    if !json.trim_start().starts_with('[') {
        return Err("the geocoder did not answer with JSON".into());
    }
    store_lookup(data, query, json.trim());
    let _ = tab.command("Page.navigate", "{\"url\":\"about:blank\"}");
    Ok(geo::parse_nominatim(&json))
}

/// `POST /v1/seo/geo?context=NAME`: give context `NAME` a location (see
/// [`geo::Request::from_json`] for the body) and an optional `proxy`.
pub fn apply_geo(
    cdp_port: u16,
    data: &Path,
    name: &str,
    body: &str,
) -> (&'static str, &'static str, String) {
    if crate::minbrowser::active() {
        return error(
            "400 Bad Request",
            "a location is held by a browser context and Min (Electron) has none; \
             start the daemon with CU_BROWSER=chromium for `cu seo geo` and `--geo`",
        );
    }
    if !crate::server::valid_name(name) {
        return error(
            "400 Bad Request",
            "give the context a name: ?context=NAME (letters, digits, - and _)",
        );
    }
    let request = match geo::Request::from_json(body) {
        Ok(Some(r)) => r,
        Ok(None) => {
            return error(
                "400 Bad Request",
                "give a location: {\"geo\":\"Town,Region,Country\"} or {\"country\":\"ES\"}",
            );
        }
        Err(e) => return error("400 Bad Request", &e),
    };
    let proxy = match json_string_value(body, "proxy").filter(|p| !p.trim().is_empty()) {
        Some(p) => match check_proxy(&p) {
            Ok(p) => Some(p),
            Err(e) => return error("400 Bad Request", &e),
        },
        None => None,
    };
    // The proxy is a property of the browser context: a different one
    // means a new context (its cookies go with the old one).
    let existing = crate::server::context_info(cdp_port, name);
    if let Some((_, _, current)) = &existing
        && *current != proxy
    {
        let _ = crate::server::close_context(cdp_port, name);
    }
    let tab = match crate::server::context_tab_with(cdp_port, name, data, proxy.as_deref()) {
        Ok(tab) => tab,
        Err(e) => return error("502 Bad Gateway", &e),
    };
    let Some((browser_context, target, _)) = crate::server::context_info(cdp_port, name) else {
        return error("502 Bad Gateway", "the context went away");
    };
    let place = if request.needs_lookup() {
        let query = request.lookup_query();
        // One retry: a context opened a moment ago sometimes drops the
        // first navigation's socket.
        let first = lookup(&tab, data, &query);
        let result = match first {
            Err(_) => {
                std::thread::sleep(Duration::from_millis(1200));
                lookup(&tab, data, &query)
            }
            ok => ok,
        };
        match result {
            Ok(place) => place,
            Err(e) => {
                eprintln!("cu: geocoding {:?} failed: {e}", request.lookup_query());
                None
            }
        }
    } else {
        None
    };
    let location = match geo::complete(&request, place.as_ref()) {
        Ok(l) => l,
        Err(e) => return error("400 Bad Request", &e),
    };
    if let Err(e) = install(
        cdp_port,
        name,
        &target,
        &browser_context,
        &location,
        proxy.clone(),
    ) {
        return error(
            "502 Bad Gateway",
            &format!("could not apply the location: {e}"),
        );
    }
    let warnings = geo::coherence(&location, proxy.as_deref());
    (
        "200 OK",
        "application/json",
        format!(
            "{{\"context\":{},\"location\":{},\"proxy\":{},\"warnings\":{}}}",
            json_string(name),
            geo::json(&location),
            proxy
                .as_deref()
                .map(json_string)
                .unwrap_or_else(|| "null".into()),
            json_list(&warnings)
        ),
    )
}

/// Set the overrides on a dedicated connection to the context's tab and
/// keep that connection.
fn install(
    cdp_port: u16,
    name: &str,
    target: &str,
    browser_context: &str,
    location: &Location,
    proxy: Option<String>,
) -> Result<(), String> {
    let mut connection = CdpConnection::target(cdp_port, target)?;
    set_overrides(&mut connection, location)?;
    let permission = grant_geolocation(cdp_port, browser_context)?;
    geo_states()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(
            (cdp_port, name.to_string()),
            GeoState {
                location: location.clone(),
                proxy,
                target: target.to_string(),
                browser_context: browser_context.to_string(),
                connection: Some(connection),
                permission: Some(permission),
            },
        );
    Ok(())
}

/// Grant geolocation in one browser context, for every origin, on a
/// browser-level session the caller keeps open (see [`GeoState`]), so a
/// page asking for the position gets the overridden one instead of a
/// prompt nobody answers.
fn grant_geolocation(cdp_port: u16, browser_context: &str) -> Result<CdpConnection, String> {
    let version = crate::server::http_get(&format!("127.0.0.1:{cdp_port}"), "/json/version")?;
    let ws = json_string_value(&version, "webSocketDebuggerUrl")
        .ok_or("Chromium did not expose a browser endpoint")?;
    let mut connection = CdpConnection::connect(&ws)?;
    connection
        .call(
            "Browser.setPermission",
            &format!(
                "{{\"permission\":{{\"name\":\"geolocation\"}},\"setting\":\"granted\",\"browserContextId\":{}}}",
                json_string(browser_context)
            ),
        )
        .or_else(|_| {
            connection.call(
                "Browser.grantPermissions",
                &format!(
                    "{{\"permissions\":[\"geolocation\"],\"browserContextId\":{}}}",
                    json_string(browser_context)
                ),
            )
        })?;
    Ok(connection)
}

fn set_overrides(connection: &mut CdpConnection, location: &Location) -> Result<(), String> {
    // A second override on the same session is refused while one is in
    // effect, so each is cleared first.
    let _ = connection.call("Emulation.setLocaleOverride", "{}");
    connection.call(
        "Emulation.setLocaleOverride",
        &format!(
            "{{\"locale\":{}}}",
            json_string(&location.locale.replace('-', "_"))
        ),
    )?;
    let _ = connection.call("Emulation.setTimezoneOverride", "{\"timezoneId\":\"\"}");
    connection.call(
        "Emulation.setTimezoneOverride",
        &format!("{{\"timezoneId\":{}}}", json_string(&location.timezone)),
    )?;
    connection.call(
        "Emulation.setGeolocationOverride",
        &format!(
            "{{\"latitude\":{},\"longitude\":{},\"accuracy\":{}}}",
            location.lat, location.lon, location.accuracy
        ),
    )?;
    // The browser's own User-Agent, with the location's languages.
    let agent = connection
        .call(
            "Runtime.evaluate",
            "{\"expression\":\"navigator.userAgent\",\"returnByValue\":true}",
        )
        .ok()
        .and_then(|r| evaluated_string(&r))
        .filter(|a| !a.is_empty())
        .ok_or("could not read the browser's User-Agent")?;
    connection.call(
        "Network.setUserAgentOverride",
        &format!(
            "{{\"userAgent\":{},\"acceptLanguage\":{},\"platform\":{}}}",
            json_string(&agent.replace("HeadlessChrome", "Chrome")),
            json_string(&accept_language(&location.locale)),
            json_string(&crate::stealth::platform())
        ),
    )?;
    Ok(())
}

/// `es-ES` -> `es-ES,es,en`; English is offered last, as most browsers
/// configured outside English-speaking countries do. No `q=` weights:
/// Chromium adds its own to the list it is given.
pub fn accept_language(locale: &str) -> String {
    let language = locale.split('-').next().unwrap_or(locale);
    let mut out = locale.to_string();
    if language != locale {
        out.push_str(&format!(",{language}"));
    }
    if language != "en" {
        out.push_str(",en");
    }
    out
}

/// Make sure the context's overrides are in force: a connection the
/// browser dropped is reopened and the overrides set again.
pub fn ensure_geo(cdp_port: u16, name: &str) -> Result<Option<Location>, String> {
    let mut states = geo_states().lock().unwrap_or_else(|e| e.into_inner());
    let Some(state) = states.get_mut(&(cdp_port, name.to_string())) else {
        return Ok(None);
    };
    let alive = state.connection.as_mut().is_some_and(|c| {
        c.call(
            "Runtime.evaluate",
            "{\"expression\":\"1\",\"returnByValue\":true}",
        )
        .is_ok()
    });
    if !alive {
        let mut connection = CdpConnection::target(cdp_port, &state.target)?;
        set_overrides(&mut connection, &state.location)?;
        state.connection = Some(connection);
    }
    let granted = state
        .permission
        .as_mut()
        .is_some_and(|c| c.call("Browser.getVersion", "{}").is_ok());
    if !granted {
        state.permission = Some(grant_geolocation(cdp_port, &state.browser_context)?);
    }
    Ok(Some(state.location.clone()))
}

/// What a page in the context sees: language, time zone, position.
pub const GEO_CHECK_JS: &str = r#"(async () => {
  const pos = await new Promise((resolve) => {
    if (!navigator.geolocation) return resolve({ error: 'no geolocation API' });
    navigator.geolocation.getCurrentPosition(
      (p) => resolve({ lat: p.coords.latitude, lon: p.coords.longitude, accuracy: p.coords.accuracy }),
      (e) => resolve({ error: e.message || String(e.code) }), { timeout: 4000 });
  });
  return JSON.stringify({
    url: location.href, language: navigator.language, languages: navigator.languages,
    timezone: Intl.DateTimeFormat().resolvedOptions().timeZone,
    intl_locale: Intl.NumberFormat().resolvedOptions().locale,
    local_time: new Date().toString(), position: pos,
  });
})()"#;

/// `GET /v1/seo/geo?context=NAME`: the location applied and what the
/// context's page reports back.
pub fn show_geo(tab: &Tab, cdp_port: u16, name: &str) -> (&'static str, &'static str, String) {
    if let Err(e) = ensure_geo(cdp_port, name) {
        return error("502 Bad Gateway", &e);
    }
    let applied = geo_of(cdp_port, name);
    let seen = match eval(tab, GEO_CHECK_JS, true) {
        Ok(v) => v,
        Err(e) => format!("{{\"error\":{}}}", json_string(&e)),
    };
    let (location, proxy) = match &applied {
        Some((l, p)) => (
            geo::json(l),
            p.as_deref()
                .map(json_string)
                .unwrap_or_else(|| "null".into()),
        ),
        None => ("null".into(), "null".into()),
    };
    (
        "200 OK",
        "application/json",
        format!(
            "{{\"context\":{},\"location\":{location},\"proxy\":{proxy},\"page\":{seen}}}",
            json_string(name)
        ),
    )
}

// ---------------------------------------------------------------------------
// Search result pages
// ---------------------------------------------------------------------------

/// The minimum time between two result pages of one engine, from this
/// daemon: a person's pace, not a scraper's. `CU_SERP_INTERVAL_MS` changes
/// it (never below one second).
pub fn serp_interval(engine: Engine) -> Duration {
    let base = match engine {
        Engine::Google => 8_000,
        Engine::Bing | Engine::DuckDuckGo => 3_000,
    };
    let ms = std::env::var("CU_SERP_INTERVAL_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(base)
        .max(1_000);
    Duration::from_millis(ms)
}

/// Wait for the engine's turn; answers how long it waited.
fn take_turn(engine: Engine) -> Duration {
    static LAST: OnceLock<Mutex<HashMap<&'static str, Instant>>> = OnceLock::new();
    let lock = LAST.get_or_init(|| Mutex::new(HashMap::new()));
    let wait = {
        let mut last = lock.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        let ready_at = last
            .get(engine.name())
            .map(|at| *at + serp_interval(engine))
            .unwrap_or(now)
            .max(now);
        // Book the slot before sleeping, so concurrent callers queue.
        last.insert(engine.name(), ready_at);
        ready_at - now
    };
    std::thread::sleep(wait);
    wait
}

/// The result page reader: `(engine, targetDomain)` in, JSON out. Organic
/// results, ads, the local pack, "people also ask" and related searches,
/// each engine with its own selectors and its own redirect links decoded.
pub const SERP_JS: &str = r#"function (engine, target) {
  const clean = (s) => (s || '').replace(/\s+/g, ' ').trim();
  const txt = (el) => el ? clean(el.innerText || el.textContent) : '';
  const decode = (href) => {
    try {
      const u = new URL(href, location.href);
      if (/(^|\.)bing\.com$/.test(u.hostname) && u.pathname.startsWith('/ck/')) {
        const p = u.searchParams.get('u');
        if (p && p.startsWith('a1')) return atob(p.slice(2).replace(/-/g, '+').replace(/_/g, '/'));
      }
      if (/duckduckgo\.com$/.test(u.hostname) && u.searchParams.get('uddg')) return u.searchParams.get('uddg');
      if (/(^|\.)google\.[a-z.]+$/.test(u.hostname) && u.pathname === '/url') return u.searchParams.get('q') || u.searchParams.get('url') || u.href;
      return u.href;
    } catch (e) { return href; }
  };
  const host = (u) => { try { return new URL(u).hostname.replace(/^www\./, ''); } catch (e) { return ''; } };
  const uniq = (list) => [...new Set(list.filter(Boolean))];
  const organic = [], ads = [], local = [], seen = new Set();
  let paa = [], related = [];
  const push = (list, title, href, snippet) => {
    const url = decode(href);
    if (!url || !/^https?:/.test(url) || seen.has(url)) return;
    seen.add(url);
    list.push({ position: list.length + 1, title: clean(title).slice(0, 200), url, domain: host(url), snippet: clean(snippet).slice(0, 320) });
  };
  if (engine === 'google') {
    // Organic links are often `/goto?url=<opaque token>` now: the target is
    // then only in the result's `<cite>` (the green URL line).
    const citeUrl = (block) => {
      // "https://domain.es › blog › post" -> https://domain.es/blog/post;
      // a breadcrumb cut with "..." keeps only the part before the cut.
      for (const c of block ? block.querySelectorAll('cite, [role="text"].x2VHCd, .qzEoUe') : []) {
        const parts = clean(c.textContent).split(/\s+[›>]\s+/);
        let base = parts[0].trim();
        if (!/^https?:\/\//.test(base)) {
          if (!/^[a-z0-9.-]+\.[a-z]{2,}(\/|$)/i.test(base)) continue;
          base = 'https://' + base;
        }
        const rest = [];
        for (const p of parts.slice(1)) {
          if (/(\.\.\.|…)/.test(p) || /\s/.test(p.trim())) break;
          rest.push(encodeURIComponent(p.trim()).replace(/%2F/g, '/'));
        }
        return base.replace(/\/$/, '') + (rest.length ? '/' + rest.join('/') : '/');
      }
      return null;
    };
    for (const h3 of document.querySelectorAll('#search a h3, #rso a h3, #botstuff a h3')) {
      const a = h3.closest('a');
      if (!a) continue;
      const block = a.closest('.MjjYud, .g, [data-hveid], [data-sokoban-container]') || a.parentElement;
      let href = decode(a.href);
      if (/(^|\.)google\.[a-z.]+$/.test(host(href))) {
        const real = citeUrl(a.closest('.yuRUbf, .kb0PBd') || block) || citeUrl(block);
        if (!real) continue;
        href = real;
      }
      const s = block && block.querySelector('.VwiC3b, [data-sncf], [style*="line-clamp"], .ITZIwc');
      push(organic, txt(h3), href, s ? txt(s) : '');
    }
    for (const ad of document.querySelectorAll('#tads [data-text-ad], #tadsb [data-text-ad], #bottomads [data-text-ad]')) {
      const a = ad.querySelector('a[href]'); const h = ad.querySelector('[role="heading"]');
      if (!a) continue;
      // An ad's link is a Google click URL: its shown address is the target.
      let href = (a.getAttribute('data-pcu') || '').split(',')[0] || decode(a.href);
      if (/(^|\.)google\.[a-z.]+$/.test(host(href)) || /googleadservices|doubleclick/.test(href)) href = citeUrl(ad) || href;
      push(ads, txt(h), href, txt(ad.querySelector('.MUxGbd, .yDYNvb')));
    }
    const localNames = [...document.querySelectorAll('.rllt__details [role="heading"], .VkpGBb [role="heading"], [data-cid] .OSrXXb, .dbg0pd, .VqeGe [role="heading"].yVCOtc, .VqeGe div.yVCOtc[role="heading"]')].map(txt);
    for (const name of uniq(localNames).slice(0, 20)) local.push({ position: local.length + 1, name });
    const asked = clean(new URLSearchParams(location.search).get('q') || '').toLowerCase();
    paa = uniq([...document.querySelectorAll('.related-question-pair [data-q], [jsname="yEVEwb"] [data-q], div[data-q]')]
      .map(e => clean(e.getAttribute('data-q')))).filter(q => q.toLowerCase() !== asked);
    if (!related.length) related = uniq([...document.querySelectorAll('a[href^="/search?"] .dg6jd, a[href^="/search?"] .mtv5bd, .y6Uyqe a, .ngTNl a')].map(txt));
    related = uniq(related.concat([...document.querySelectorAll('#bres a, #botstuff a[href*="/search?"]')].map(txt)))
      .filter(t => t.length > 1 && t.length < 120 && !/^(Siguiente|Next|Anterior|Previous|\d+)$/i.test(t));
  } else if (engine === 'bing') {
    for (const li of document.querySelectorAll('#b_results > li.b_algo')) {
      const a = li.querySelector('h2 a[href]');
      if (a) push(organic, txt(a), a.href, txt(li.querySelector('.b_caption p, .b_lineclamp2, .b_lineclamp3, .b_algoSlug, .b_paractl')));
    }
    for (const li of document.querySelectorAll('#b_results li.b_ad li, #b_results .b_adSlug')) {
      const box = li.closest('li') || li; const a = box.querySelector('h2 a[href], a[href]');
      if (a) push(ads, txt(box.querySelector('h2')) || txt(a), a.href, txt(box.querySelector('p')));
    }
    const localNames = [...document.querySelectorAll('.b_localTitle, .lc_content h2, [data-tag="localListing"] h2, .b_scard .lc_content .b_factrow:first-child, .LocalListings .tit, .localsCard h2')].map(txt);
    for (const name of uniq(localNames).slice(0, 20)) local.push({ position: local.length + 1, name });
    paa = uniq([...document.querySelectorAll('.df_qntext, [data-tag="RelatedQnA.Item"] .b_1linetrunc')].map(txt));
    related = uniq([...document.querySelectorAll('.b_rs a, #brsv3 a, .b_vList .b_suggestionText')].map(txt)).filter(t => t.length < 120);
  } else {
    for (const r of document.querySelectorAll('.result')) {
      const a = r.querySelector('a.result__a'); if (!a) continue;
      push(r.classList.contains('result--ad') ? ads : organic, txt(a), a.getAttribute('href'), txt(r.querySelector('.result__snippet')));
    }
  }
  const body = document.body ? document.body.innerText : '';
  const consent = !!document.querySelector('form[action*="consent." i], #CXQnmb, #bnp_container, #bnp_cookie_banner')
    || /Antes de ir a Google|Before you continue to Google/.test(body.slice(0, 3000));
  const stats = txt(document.querySelector('#result-stats, .sb_count'));
  const where = txt(document.querySelector('#swml, .VYM29, #ftrB #b_footerItems .b_footerLocation, .b_footerLocation, #est_loc'));
  let found = null;
  if (target) {
    const t = target.toLowerCase().replace(/^www\./, '');
    const hit = organic.find(r => r.domain === t || r.domain.endsWith('.' + t));
    found = { domain: t, position: hit ? hit.position : null, url: hit ? hit.url : null,
      in_local: local.some(l => l.name.toLowerCase().includes(t.split('.')[0])) };
  }
  return JSON.stringify({ title: document.title, consent_overlay: consent, result_stats: stats || null, engine_location: where || null,
    organic, ads, local, people_also_ask: paa.slice(0, 12), related: related.slice(0, 16), target: found });
}"#;

/// `POST /v1/seo/serp`: `{query, engine?, num?, target?}` on the tab's
/// context, with the context's location (see [`apply_geo`]).
pub fn serp(tab: &Tab, cdp_port: u16, body: &str) -> (&'static str, &'static str, String) {
    let query = json_string_value(body, "query").unwrap_or_default();
    if query.trim().is_empty() {
        return error("400 Bad Request", "query is required");
    }
    let engine = match Engine::parse(&json_string_value(body, "engine").unwrap_or_default()) {
        Ok(e) => e,
        Err(e) => return error("400 Bad Request", &e),
    };
    let num = json_number(body, "num").unwrap_or(10.0).clamp(1.0, 100.0) as u32;
    let target = json_string_value(body, "target").unwrap_or_default();
    let context = crate::server::context_of(tab);
    let location = match context.as_deref() {
        Some(name) => match ensure_geo(cdp_port, name) {
            Ok(l) => l,
            Err(e) => return error("502 Bad Gateway", &e),
        },
        None => None,
    };
    let url = geo::search_url(engine, &query, location.as_ref(), num);
    let waited = take_turn(engine);
    let navigation = match navigate(tab, &url) {
        Ok(n) => n,
        Err(e) => return error("502 Bad Gateway", &e),
    };
    let challenge = json_object(&navigation, "challenge")
        .unwrap_or("null")
        .to_string();
    let header = format!(
        "\"engine\":{},\"query\":{},\"url\":{},\"context\":{},\"location\":{},\"throttled_ms\":{}",
        json_string(engine.name()),
        json_string(&query),
        json_string(&url),
        context
            .as_deref()
            .map(json_string)
            .unwrap_or_else(|| "null".into()),
        location
            .as_ref()
            .map(geo::json)
            .unwrap_or_else(|| "null".into()),
        waited.as_millis()
    );
    // A Google `/sorry/` page, a CAPTCHA or a block: stop, say so, and
    // leave it to a person. Nothing here retries around it.
    let state = challenge_state(&navigation);
    if stops_the_run(&state) {
        return (
            "409 Conflict",
            "application/json",
            format!(
                "{{{header},\"error\":{},\"challenge\":{challenge}}}",
                json_string(&format!(
                    "{} answered with a defence page ({state}); stopped. Open the tab in the live viewer if a person wants to continue, and slow down",
                    engine.name()
                ))
            ),
        );
    }
    // A result page fills in after the load event on some engines: read it
    // until organic results show up, for a few seconds at most.
    let expression = format!(
        "({SERP_JS})({}, {})",
        json_string(engine.name()),
        json_string(&target)
    );
    let started = Instant::now();
    let mut page = String::new();
    loop {
        match eval(tab, &expression, false) {
            Ok(v) => page = v,
            Err(e) if started.elapsed() > Duration::from_secs(6) => {
                return error(
                    "502 Bad Gateway",
                    &format!("could not read the result page: {e}"),
                );
            }
            Err(_) => {}
        }
        let has_results = json_array(&page, "organic").is_some_and(|a| a.len() > 2);
        if has_results || started.elapsed() > Duration::from_secs(6) {
            break;
        }
        std::thread::sleep(Duration::from_millis(400));
    }
    // After the wait: the page may have turned into a challenge (Google
    // redirects to `/sorry/` from script).
    if let Ok(probe) = crate::challenge::probe_on(tab, |m, p| tab.command(m, p)) {
        let verdict = crate::challenge::classify(&probe);
        if verdict.state.is_defence() {
            return (
                "409 Conflict",
                "application/json",
                format!(
                    "{{{header},\"error\":{},\"signals\":{}}}",
                    json_string(&format!(
                        "{} answered with a defence page; stopped. A person can look at it in the live viewer",
                        engine.name()
                    )),
                    json_list(&verdict.signals)
                ),
            );
        }
    }
    let inner = page.trim().strip_prefix('{').unwrap_or("}").to_string();
    (
        "200 OK",
        "application/json",
        format!("{{{header},\"challenge\":{challenge},{inner}"),
    )
}

/// Compare result pages for the same query (as `cu serp compare` prints
/// them): which domains each location has, the ones they share, and how
/// shared ones moved.
pub fn compare(results: &[(String, String)]) -> String {
    let lists: Vec<(String, Vec<String>, Vec<String>)> = results
        .iter()
        .map(|(label, page)| {
            let organic = json_array(page, "organic").unwrap_or_default();
            let urls = json_objects(&organic)
                .iter()
                .filter_map(|o| json_string_value(o, "url"))
                .collect::<Vec<_>>();
            let local = json_array(page, "local")
                .map(|a| {
                    json_objects(&a)
                        .iter()
                        .filter_map(|o| json_string_value(o, "name"))
                        .collect()
                })
                .unwrap_or_default();
            (label.clone(), urls, local)
        })
        .collect();
    let all: Vec<&String> = lists.iter().flat_map(|(_, u, _)| u.iter()).collect();
    let common: Vec<String> = {
        let mut c: Vec<String> = all
            .iter()
            .filter(|u| lists.iter().all(|(_, urls, _)| urls.contains(u)))
            .map(|u| u.to_string())
            .collect();
        c.dedup();
        c.sort();
        c.dedup();
        c
    };
    let per = lists
        .iter()
        .map(|(label, urls, local)| {
            let only: Vec<String> = urls
                .iter()
                .filter(|u| lists.iter().filter(|(l, _, _)| l != label).all(|(_, o, _)| !o.contains(u)))
                .cloned()
                .collect();
            let positions: Vec<String> = common
                .iter()
                .map(|u| {
                    format!(
                        "{}:{}",
                        json_string(u),
                        urls.iter().position(|x| x == u).map(|p| p + 1).unwrap_or(0)
                    )
                })
                .collect();
            format!(
                "{{\"location\":{},\"organic\":{},\"only_here\":{},\"positions_of_common\":{{{}}},\"local\":{}}}",
                json_string(label),
                json_list(urls),
                json_list(&only),
                positions.join(","),
                json_list(local)
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let identical = lists
        .windows(2)
        .all(|w| w[0].1 == w[1].1 && w[0].2 == w[1].2);
    let total: std::collections::HashSet<&String> = all.iter().copied().collect();
    format!(
        "{{\"identical\":{identical},\"distinct_urls\":{},\"common\":{},\"overlap\":{:.2},\"by_location\":[{per}]}}",
        total.len(),
        json_list(&common),
        if total.is_empty() {
            0.0
        } else {
            common.len() as f64 / total.len() as f64
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn robots_groups_and_longest_match() {
        let text = "User-agent: Googlebot\nDisallow: /\n\nUser-agent: *\nDisallow: /admin\nDisallow: /*.pdf$\nAllow: /admin/public\nCrawl-delay: 2\nSitemap: https://x.test/sitemap.xml\n";
        let r = Robots::parse(text, "cu");
        assert!(r.allowed("/"));
        assert!(!r.allowed("/admin/users"));
        assert!(r.allowed("/admin/public/page"));
        assert!(!r.allowed("/files/a.pdf"));
        assert!(r.allowed("/files/a.pdf?x=1"));
        assert_eq!(r.crawl_delay, Some(2.0));
        assert_eq!(r.sitemaps, vec!["https://x.test/sitemap.xml".to_string()]);
        let own = Robots::parse(
            "User-agent: cu\nDisallow: /private\nUser-agent: *\nDisallow: /",
            "cu",
        );
        assert!(own.allowed("/blog"));
        assert!(!own.allowed("/private/x"));
        assert!(Robots::parse("", "cu").allowed("/anything"));
    }

    #[test]
    fn urls_split_into_origin_and_path() {
        assert_eq!(
            split_url("HTTPS://Example.test/a/b?x=1#top"),
            Some(("https://example.test".into(), "/a/b?x=1".into()))
        );
        assert_eq!(
            split_url("https://example.test"),
            Some(("https://example.test".into(), "/".into()))
        );
        assert!(is_asset("/img/logo.PNG"));
        assert!(!is_asset("/blog/post"));
    }

    #[test]
    fn proxies_are_checked() {
        assert_eq!(
            check_proxy("http://10.0.0.1:3128/").unwrap(),
            "http://10.0.0.1:3128"
        );
        assert!(check_proxy("socks5://h:1080").is_ok());
        assert!(check_proxy("http://user:pw@h:1").is_err());
        assert!(check_proxy("ftp://h:1").is_err());
        assert!(check_proxy("h:1").is_err());
    }

    #[test]
    fn accept_language_offers_the_language_then_english() {
        assert_eq!(accept_language("es-ES"), "es-ES,es,en");
        assert_eq!(accept_language("en-GB"), "en-GB,en");
    }

    #[test]
    fn a_crawl_follows_internal_links_obeys_robots_and_finds_duplicates() {
        let pages: HashMap<&str, (&str, &str)> = HashMap::from([
            (
                "https://s.test/",
                (
                    "Home",
                    "[\"https://s.test/a\",\"https://s.test/private/x\",\"https://s.test/logo.png\",\"https://s.test/b#frag\"]",
                ),
            ),
            (
                "https://s.test/a",
                ("Same", "[\"https://s.test/\",\"https://s.test/b\"]"),
            ),
            ("https://s.test/b", ("Same", "[\"https://s.test/c\"]")),
            ("https://s.test/c", ("Deep", "[]")),
        ]);
        let mut requests = Vec::new();
        let mut request = |method: &str,
                           path: &str,
                           body: Option<&str>|
         -> Result<String, String> {
            requests.push(format!("{method} {path}"));
            if path.starts_with("/v1/seo/robots") {
                return Ok(
                    "{\"status\":200,\"text\":\"User-agent: *\\nDisallow: /private\\n\"}".into(),
                );
            }
            let url = json_string_value(body.unwrap_or(""), "url").unwrap();
            let (title, links) = pages.get(url.as_str()).ok_or("404")?;
            Ok(format!(
                "{{\"url\":{},\"status\":200,\"title\":{},\"meta_description\":\"\",\"issues\":[\"missing meta description\"],\"links\":{{\"internal_urls\":{links}}}}}",
                json_string(&url),
                json_string(title)
            ))
        };
        let mut emitted = Vec::new();
        let mut slept = 0;
        let options = CrawlOptions {
            max_pages: 10,
            max_depth: 1,
            delay: Duration::from_millis(1),
            keywords: vec![],
        };
        let summary = crawl(
            "https://s.test/",
            "seo",
            &options,
            &mut request,
            &mut |p, _| emitted.push(p.url.clone()),
            &mut |_| slept += 1,
        )
        .unwrap();
        // Depth 1: home, a and b (the fragment dropped); c is depth 2; the
        // private page is never requested; the PNG is not a page.
        assert_eq!(
            emitted,
            vec!["https://s.test/", "https://s.test/a", "https://s.test/b"]
        );
        assert_eq!(slept, 2);
        assert!(!requests.iter().any(|r| r.contains("private")));
        assert!(summary.contains("\"pages\":3"), "{summary}");
        assert!(summary.contains("\"skipped_by_robots\":1"), "{summary}");
        assert!(
            summary.contains("\"missing meta description\":3"),
            "{summary}"
        );
        assert!(summary.contains("\"value\":\"Same\""), "{summary}");
    }

    #[test]
    fn compare_reports_shared_and_unique_results() {
        let a =
            r#"{"organic":[{"url":"https://x/1"},{"url":"https://x/2"}],"local":[{"name":"A"}]}"#;
        let b =
            r#"{"organic":[{"url":"https://x/2"},{"url":"https://x/3"}],"local":[{"name":"B"}]}"#;
        let out = compare(&[("Sevilla".into(), a.into()), ("Bilbao".into(), b.into())]);
        assert!(out.contains("\"identical\":false"), "{out}");
        assert!(out.contains("\"common\":[\"https://x/2\"]"), "{out}");
        assert!(out.contains("\"only_here\":[\"https://x/1\"]"), "{out}");
        assert!(
            out.contains("\"positions_of_common\":{\"https://x/2\":2}"),
            "{out}"
        );
        assert!(out.contains("\"distinct_urls\":3"), "{out}");
    }
}
