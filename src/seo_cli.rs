//! `cu seo ...` and `cu serp ...`: the CLI side of [`cu::seo`].
use std::fs;
use std::io::Write;
use std::thread;
use std::time::Duration;

use cu::server::{
    json_array, json_number, json_object, json_objects, json_string, json_string_value,
    json_strings,
};

use crate::request;

/// Options shared by the SEO commands: `--flag value` pairs, positional
/// words, and the flags that can repeat (`--geo`).
struct SeoArgs {
    words: Vec<String>,
    flags: Vec<(String, String)>,
    md: bool,
}

impl SeoArgs {
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut out = SeoArgs {
            words: Vec::new(),
            flags: Vec::new(),
            md: false,
        };
        let mut i = 0;
        while i < args.len() {
            let a = &args[i];
            if a == "--md" {
                out.md = true;
            } else if let Some(name) = a.strip_prefix("--") {
                let value = args.get(i + 1).ok_or(format!("--{name} needs a value"))?;
                out.flags.push((name.to_string(), value.clone()));
                i += 1;
            } else {
                out.words.push(a.clone());
            }
            i += 1;
        }
        Ok(out)
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.flags
            .iter()
            .rev()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    fn all(&self, name: &str) -> Vec<String> {
        self.flags
            .iter()
            .filter(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
            .collect()
    }

    fn number(&self, name: &str, default: f64) -> Result<f64, String> {
        match self.get(name) {
            Some(v) => v.parse().map_err(|_| format!("--{name} takes a number")),
            None => Ok(default),
        }
    }

    /// The `POST /v1/seo/geo` body for location `spec` plus the flags.
    fn geo_body(&self, spec: &str) -> Result<String, String> {
        let mut body = format!("{{\"geo\":{}", json_string(spec));
        for (flag, key) in [
            ("lang", "locale"),
            ("tz", "timezone"),
            ("proxy", "proxy"),
            ("canonical", "canonical"),
        ] {
            if let Some(v) = self.get(flag) {
                body.push_str(&format!(",\"{key}\":{}", json_string(v)));
            }
        }
        for key in ["lat", "lon"] {
            if let Some(v) = self.get(key) {
                let n: f64 = v.parse().map_err(|_| format!("--{key} takes a number"))?;
                body.push_str(&format!(",\"{key}\":{n}"));
            }
        }
        body.push('}');
        Ok(body)
    }

    fn keywords(&self) -> Vec<String> {
        self.all("keywords")
            .iter()
            .flat_map(|k| {
                k.split(',')
                    .map(|s| s.trim().to_string())
                    .collect::<Vec<_>>()
            })
            .filter(|k| !k.is_empty())
            .collect()
    }
}

fn json_list(items: &[String]) -> String {
    format!(
        "[{}]",
        items
            .iter()
            .map(|k| json_string(k))
            .collect::<Vec<_>>()
            .join(",")
    )
}

/// The context a location spec gets by default: `geo-sevilla-es`.
fn geo_context_name(spec: &str) -> String {
    let folded = cu::geo::fold(spec);
    let mut slug = String::from("geo");
    for part in folded.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let s: String = part
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let s = s.trim_matches('-').to_string();
        if !s.is_empty() {
            slug.push('-');
            slug.push_str(&s);
        }
    }
    slug.chars().take(60).collect()
}

/// Give `context` the location `spec`; warnings go to stderr.
fn apply_geo(context: &str, spec: &str, opts: &SeoArgs) -> Result<String, String> {
    let reply = request(
        "POST",
        &format!("/v1/seo/geo?context={context}"),
        Some(&opts.geo_body(spec)?),
    )?;
    for w in json_array(&reply, "warnings")
        .map(|a| json_strings(&a))
        .unwrap_or_default()
    {
        eprintln!("cu: {context}: {w}");
    }
    Ok(reply)
}

/// The context a command runs in: `--context`, the one `--geo` names, or
/// `fallback`. Never the session's own tab, which is left where it was.
fn context_for(opts: &SeoArgs, fallback: &str) -> String {
    match (opts.get("context"), opts.get("geo")) {
        (Some(c), _) => c.to_string(),
        (None, Some(spec)) => geo_context_name(spec),
        (None, None) => fallback.to_string(),
    }
}

pub fn seo(args: &[String]) -> Result<(), String> {
    let usage =
        "usage: cu seo audit URL | crawl URL | geo CONTEXT [LOCATION] | robots URL (see `cu help`)";
    let action = args.first().ok_or(usage)?.as_str();
    let opts = SeoArgs::parse(&args[1..])?;
    let context = context_for(&opts, "seo");
    if action != "geo"
        && let Some(spec) = opts.get("geo")
    {
        apply_geo(&context, spec, &opts)?;
    }
    match action {
        "audit" => {
            let url = opts
                .words
                .first()
                .ok_or("usage: cu seo audit URL [--keywords \"k1,k2\"]")?;
            let body = format!(
                "{{\"url\":{},\"keywords\":{}}}",
                json_string(url),
                json_list(&opts.keywords())
            );
            let reply = request(
                "POST",
                &format!("/v1/seo/audit?context={context}"),
                Some(&body),
            )?;
            if opts.md {
                print!("{}", audit_md(&reply));
            } else {
                println!("{reply}");
            }
            Ok(())
        }
        "robots" => {
            let url = opts.words.first().ok_or("usage: cu seo robots URL")?;
            request(
                "POST",
                &format!("/v1/navigate?context={context}"),
                Some(&format!("{{\"url\":{}}}", json_string(url))),
            )?;
            let reply = request("GET", &format!("/v1/seo/robots?context={context}"), None)?;
            let robots = cu::seo::Robots::parse(
                &json_string_value(&reply, "text").unwrap_or_default(),
                "cu",
            );
            let path = cu::seo::split_url(url)
                .map(|(_, p)| p)
                .unwrap_or_else(|| "/".into());
            println!(
                "{{\"origin\":{},\"status\":{},\"path\":{},\"allowed\":{},\"rules\":{},\"crawl_delay\":{},\"sitemaps\":{}}}",
                json_string(&json_string_value(&reply, "origin").unwrap_or_default()),
                json_number(&reply, "status").unwrap_or(0.0),
                json_string(&path),
                robots.allowed(&path),
                robots.rules.len(),
                robots
                    .crawl_delay
                    .map(|d| d.to_string())
                    .unwrap_or_else(|| "null".into()),
                json_list(&robots.sitemaps)
            );
            Ok(())
        }
        "geo" => {
            let name = opts
                .words
                .first()
                .ok_or("usage: cu seo geo CONTEXT [\"Town,Region,Country\"] [--proxy URL]")?;
            match opts.words.get(1) {
                Some(spec) => println!("{}", apply_geo(name, spec, &opts)?),
                None => println!(
                    "{}",
                    request("GET", &format!("/v1/seo/geo?context={name}"), None)?
                ),
            }
            Ok(())
        }
        "crawl" => {
            let url = opts
                .words
                .first()
                .ok_or("usage: cu seo crawl URL [--max-pages N] [--depth N]")?;
            let options = cu::seo::CrawlOptions {
                max_pages: opts.number("max-pages", 20.0)?.clamp(1.0, 500.0) as usize,
                max_depth: opts.number("depth", 2.0)?.clamp(0.0, 10.0) as usize,
                // Never faster than one page a second, whatever is asked.
                delay: Duration::from_millis(opts.number("delay-ms", 1500.0)?.max(1000.0) as u64),
                keywords: opts.keywords(),
            };
            let mut out = match opts.get("out") {
                Some(path) => Some(fs::File::create(path).map_err(|e| format!("{path}: {e}"))?),
                None => None,
            };
            let mut call = |m: &str, p: &str, b: Option<&str>| request(m, p, b);
            let mut emit = |page: &cu::seo::CrawledPage, report: &str| {
                eprintln!(
                    "cu: [{}] {} {} -- {} issue(s)",
                    page.depth,
                    page.status,
                    page.url,
                    page.issues.len()
                );
                if let Some(f) = out.as_mut() {
                    let _ = writeln!(f, "{report}");
                }
            };
            let summary =
                cu::seo::crawl(url, &context, &options, &mut call, &mut emit, &mut |d| {
                    thread::sleep(d)
                })?;
            println!("{summary}");
            Ok(())
        }
        other => Err(format!("unknown seo command {other}; {usage}")),
    }
}

pub fn serp(args: &[String]) -> Result<(), String> {
    let compare = args.first().map(String::as_str) == Some("compare");
    let opts = SeoArgs::parse(if compare { &args[1..] } else { args })?;
    let query = opts.words.join(" ");
    if query.trim().is_empty() {
        return Err("usage: cu serp QUERY [--geo LOC] [--engine google|bing|duckduckgo] | cu serp compare QUERY --geo A --geo B".into());
    }
    let mut body = format!("{{\"query\":{}", json_string(&query));
    if let Some(e) = opts.get("engine") {
        body.push_str(&format!(",\"engine\":{}", json_string(e)));
    }
    if let Some(t) = opts.get("target") {
        body.push_str(&format!(",\"target\":{}", json_string(t)));
    }
    body.push_str(&format!(",\"num\":{}}}", opts.number("num", 10.0)?));
    let one = |context: &str| {
        request(
            "POST",
            &format!("/v1/seo/serp?context={context}"),
            Some(&body),
        )
    };
    if compare {
        let geos = opts.all("geo");
        if geos.len() < 2 {
            return Err("cu serp compare needs two or more --geo locations".into());
        }
        let mut pages = Vec::new();
        for spec in &geos {
            let context = geo_context_name(spec);
            apply_geo(&context, spec, &opts)?;
            eprintln!("cu: searching {query:?} from {spec} ...");
            pages.push((spec.clone(), one(&context)?));
        }
        let diff = cu::seo::compare(&pages);
        if opts.md {
            print!("{}", compare_md(&query, &pages, &diff));
        } else {
            println!(
                "{{\"query\":{},\"comparison\":{diff},\"pages\":[{}]}}",
                json_string(&query),
                pages
                    .iter()
                    .map(|(_, p)| p.as_str())
                    .collect::<Vec<_>>()
                    .join(",")
            );
        }
        return Ok(());
    }
    let context = context_for(&opts, "serp");
    if let Some(spec) = opts.get("geo") {
        apply_geo(&context, spec, &opts)?;
    }
    let page = one(&context)?;
    if opts.md {
        print!("{}", serp_md(&page));
    } else {
        println!("{page}");
    }
    Ok(())
}

fn serp_md(page: &str) -> String {
    let mut out = String::new();
    let location = json_object(page, "location")
        .and_then(|l| json_string_value(l, "canonical"))
        .unwrap_or_else(|| "(sin ubicación)".into());
    out.push_str(&format!(
        "## {} -- \"{}\" -- {}\n\n",
        json_string_value(page, "engine").unwrap_or_default(),
        json_string_value(page, "query").unwrap_or_default(),
        location
    ));
    if let Some(w) = json_string_value(page, "engine_location") {
        out.push_str(&format!("El buscador sitúa la búsqueda en: {w}\n\n"));
    }
    for (title, key) in [("Orgánicos", "organic"), ("Anuncios", "ads")] {
        let items = json_array(page, key)
            .map(|a| json_objects(&a))
            .unwrap_or_default();
        if items.is_empty() {
            continue;
        }
        out.push_str(&format!("### {title}\n"));
        for o in items {
            out.push_str(&format!(
                "{}. {} -- {}\n",
                json_number(&o, "position").unwrap_or(0.0),
                json_string_value(&o, "title").unwrap_or_default(),
                json_string_value(&o, "url").unwrap_or_default()
            ));
        }
        out.push('\n');
    }
    let local = json_array(page, "local")
        .map(|a| json_objects(&a))
        .unwrap_or_default();
    if !local.is_empty() {
        out.push_str("### Pack local\n");
        for o in local {
            out.push_str(&format!(
                "- {}\n",
                json_string_value(&o, "name").unwrap_or_default()
            ));
        }
        out.push('\n');
    }
    for (title, key) in [
        ("Preguntas relacionadas", "people_also_ask"),
        ("Búsquedas relacionadas", "related"),
    ] {
        let items = json_array(page, key)
            .map(|a| json_strings(&a))
            .unwrap_or_default();
        if !items.is_empty() {
            out.push_str(&format!("### {title}\n- {}\n\n", items.join("\n- ")));
        }
    }
    if let Some(t) = json_object(page, "target") {
        out.push_str(&format!(
            "Dominio objetivo {}: posición {}\n",
            json_string_value(t, "domain").unwrap_or_default(),
            json_number(t, "position")
                .map(|p| p.to_string())
                .unwrap_or_else(|| "fuera de los resultados".into())
        ));
    }
    out
}

fn compare_md(query: &str, pages: &[(String, String)], diff: &str) -> String {
    let mut out = format!("# \"{query}\" desde {} ubicaciones\n\n", pages.len());
    for (_, page) in pages {
        out.push_str(&serp_md(page));
        out.push('\n');
    }
    out.push_str(&format!(
        "## Comparación\n\n- idénticos: {}\n- URLs distintas: {}\n- solapamiento: {}\n",
        diff.contains("\"identical\":true"),
        json_number(diff, "distinct_urls").unwrap_or(0.0),
        json_number(diff, "overlap").unwrap_or(0.0)
    ));
    for o in json_array(diff, "by_location")
        .map(|a| json_objects(&a))
        .unwrap_or_default()
    {
        let only = json_array(&o, "only_here")
            .map(|a| json_strings(&a))
            .unwrap_or_default();
        out.push_str(&format!(
            "- {}: {} resultado(s) que no salen en las demás\n",
            json_string_value(&o, "location").unwrap_or_default(),
            only.len()
        ));
    }
    out
}

fn audit_md(report: &str) -> String {
    let mut out = format!(
        "# Auditoría SEO: {}\n\n- estado HTTP: {}\n- title ({}): {}\n- meta description ({}): {}\n- canonical: {}\n- palabras: {}\n- h1: {}\n",
        json_string_value(report, "url").unwrap_or_default(),
        json_number(report, "status").unwrap_or(0.0),
        json_number(report, "title_length").unwrap_or(0.0),
        json_string_value(report, "title").unwrap_or_default(),
        json_number(report, "meta_description_length").unwrap_or(0.0),
        json_string_value(report, "meta_description").unwrap_or_default(),
        json_string_value(report, "canonical").unwrap_or_else(|| "-".into()),
        json_number(report, "word_count").unwrap_or(0.0),
        json_array(report, "h1_texts")
            .map(|a| json_strings(&a).join(" | "))
            .unwrap_or_default(),
    );
    let issues = json_array(report, "issues")
        .map(|a| json_strings(&a))
        .unwrap_or_default();
    out.push_str(&format!("\n## Problemas ({})\n", issues.len()));
    for i in issues {
        out.push_str(&format!("- {i}\n"));
    }
    if let Some(k) = json_object(report, "keywords") {
        out.push_str(&format!("\n## Palabras clave\n```json\n{k}\n```\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use cu::server;

    #[test]
    fn location_specs_name_their_context() {
        assert_eq!(
            geo_context_name("Sevilla, Andalucía, ES"),
            "geo-sevilla-andalucia-es"
        );
        assert_eq!(geo_context_name("ES"), "geo-es");
        assert!(server::valid_name(&geo_context_name(
            "A Coruña (Galicia),ES"
        )));
    }

    #[test]
    fn flags_repeat_and_keywords_split() {
        let args: Vec<String> = [
            "q",
            "--geo",
            "A,ES",
            "--geo",
            "B,ES",
            "--keywords",
            "x, y",
            "--md",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let o = SeoArgs::parse(&args).unwrap();
        assert_eq!(o.words, vec!["q"]);
        assert_eq!(o.all("geo"), vec!["A,ES", "B,ES"]);
        assert_eq!(o.keywords(), vec!["x", "y"]);
        assert!(o.md);
        assert!(SeoArgs::parse(&["--geo".to_string()]).is_err());
    }
}
