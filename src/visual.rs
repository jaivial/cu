//! Visual references for QA: what an element *is*, where it is drawn, and
//! which code renders it.
//!
//! A snapshot names elements (`ref=e7 button "Sign in"`) and a screenshot
//! shows pixels, but neither points at the other, and neither says where in
//! the code an element comes from. This module bridges the three:
//!
//! - [`inspect`]: everything needed to find one element again -- a unique CSS
//!   selector (piercing open shadow roots with ` >>> `), test ids, the
//!   landmarks around it, its box and key styles -- and, on a development
//!   build, the component that rendered it and its source file (React, Vue,
//!   Svelte, Angular, or the attributes inspector plugins add).
//! - overlays ([`draw`] / [`clear`]): labelled boxes painted over elements for
//!   one screenshot (`annotate` = snapshot refs, `marks` = QA marks), so the
//!   image carries the same handles the snapshot does.
//! - marks ([`add_mark`], [`marks_json`], [`marks_markdown`]): QA notes pinned
//!   to elements, kept per tab, rendered as a report a developer can act on.
//!
//! Locating runs in cu's isolated world, like every helper. Reading the
//! component is the one step that needs the page's own world -- framework
//! bookkeeping (`__reactFiber$...`, `__vueParentComponent`) are JavaScript
//! properties of the page's realm, invisible from an isolated one -- so the
//! node is resolved there through `DOM.resolveNode` and read without calling
//! any page function.
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use crate::server::{
    self, CdpConnection, Tab, checkin, checkout, evaluated_string, json_number, json_object,
    json_string, json_string_value, json_value,
};

/// The frame that owns `reference` and the target that renders it: the tab
/// itself, or an out-of-process iframe's own target. `None` frame means the
/// main document.
fn frame_of(tab: &Tab, reference: Option<&str>) -> (Tab, Option<String>) {
    let Some(frame) = reference.and_then(|r| server::frame_for_ref(tab, r)) else {
        return (tab.clone(), None);
    };
    match server::target_for_frame(tab, &frame) {
        Some(target) => (target, Some(frame)),
        None => (tab.clone(), Some(frame)),
    }
}

/// Run `f` on one DevTools connection of `tab`. Remote object ids belong to
/// the session that made them, so a lookup that passes objects between
/// commands must not hop between pooled connections.
///
/// `f` answers `Err` for a broken connection and `Ok(Err)` for an answer the
/// page gave ("matches nothing"). Only the first drops the connection: cu's
/// isolated world -- and the ref registry in it -- belongs to the session
/// that created it, so dropping a healthy one would forget every ref.
fn with_conn<T>(
    tab: &Tab,
    f: impl FnOnce(&mut CdpConnection) -> Result<Result<T, String>, String>,
) -> Result<T, String> {
    let mut conn = checkout(tab)?;
    let result = f(&mut conn)?;
    checkin(tab, conn);
    result
}

/// JavaScript that finds an element by a selector [`INSPECT_FN`] produced:
/// plain CSS, or CSS segments joined by ` >>> ` to step into open shadow
/// roots.
const FIND_JS: &str = "((s)=>{let root=document,el=null;for(const part of s.split(' >>> ')){if(!root)return null;try{el=root.querySelector(part);}catch(e){return null;}root=el&&el.shadowRoot;}return el;})";

/// Everything about one element that helps find it again, read in cu's
/// isolated world with the element as `this`. Values of fields are never
/// read: a QA report must not carry what a user typed (or a password).
const INSPECT_FN: &str = r#"function(){
  const el = this;
  const clean = (s) => (s || '').replace(/\s+/g, ' ').trim();
  const q = (s) => s.replace(/\\/g, '\\\\').replace(/"/g, '\\"');
  const TEST = ['data-testid', 'data-test-id', 'data-test', 'data-cy', 'data-qa'];
  const unique = (sel, root) => { try { return root.querySelectorAll(sel).length === 1; } catch (e) { return false; } };
  const step = (e, root) => {
    const tag = e.localName;
    if (e.id && unique('#' + CSS.escape(e.id), root)) return ['#' + CSS.escape(e.id), true];
    for (const a of TEST) {
      const v = e.getAttribute(a);
      if (v) { const s = '[' + a + '="' + q(v) + '"]'; if (unique(s, root)) return [s, true]; }
    }
    const n = e.getAttribute('name');
    if (n) { const s = tag + '[name="' + q(n) + '"]'; if (unique(s, root)) return [s, true]; }
    const p = e.parentElement;
    if (!p) return [tag, false];
    const same = [...p.children].filter((c) => c.localName === tag);
    return [same.length > 1 ? tag + ':nth-of-type(' + (same.indexOf(e) + 1) + ')' : tag, false];
  };
  const within = (e) => {
    const root = e.getRootNode(), parts = [];
    for (let c = e; c && c.nodeType === 1; c = c.parentElement) {
      const [s, anchored] = step(c, root);
      parts.unshift(s);
      if (anchored || unique(parts.join(' > '), root)) break;
    }
    return parts.join(' > ');
  };
  const selector = (e) => {
    const root = e.getRootNode();
    return root instanceof ShadowRoot ? selector(root.host) + ' >>> ' + within(e) : within(e);
  };
  const up = (c) => c.parentElement || (c.getRootNode() instanceof ShadowRoot ? c.getRootNode().host : null);
  const role = el.getAttribute('role')
    || ({A: 'link', BUTTON: 'button', SELECT: 'combobox', TEXTAREA: 'textbox', IMG: 'img',
         H1: 'heading', H2: 'heading', H3: 'heading', H4: 'heading', NAV: 'navigation', FORM: 'form'})[el.tagName]
    || (el.tagName === 'INPUT'
      ? ({checkbox: 'checkbox', radio: 'radio', search: 'searchbox', number: 'spinbutton',
          button: 'button', submit: 'button', reset: 'button', file: 'button', range: 'slider'})[el.type] || 'textbox'
      : el.isContentEditable ? 'textbox' : 'generic');
  const name = (() => {
    const by = el.getAttribute('aria-labelledby');
    if (by) { const t = clean(by.split(/\s+/).map((id) => document.getElementById(id)?.textContent).join(' ')); if (t) return t; }
    if (el.getAttribute('aria-label')) return clean(el.getAttribute('aria-label'));
    if (el.labels && el.labels[0]) return clean(el.labels[0].textContent);
    if (el.alt) return clean(el.alt);
    const img = el.querySelector && el.querySelector('img[alt]');
    if (img) return clean(img.alt);
    return clean(el.textContent) || clean(el.getAttribute('placeholder')) || clean(el.getAttribute('title'));
  })().slice(0, 120);
  const landmarks = [];
  for (let c = up(el); c && c.nodeType === 1 && landmarks.length < 6; c = up(c)) {
    const tag = c.localName, r = c.getAttribute('role') || '';
    if (/^(main|nav|header|footer|aside|form|dialog|article|section)$/.test(tag)
      || /^(main|navigation|banner|contentinfo|complementary|form|dialog|alertdialog|region|search|tabpanel|menu|listbox|grid|table)$/.test(r)
      || tag.includes('-')) {
      let d = tag + (c.id ? '#' + c.id : '') + (r ? '[role=' + r + ']' : '');
      const label = clean(c.getAttribute('aria-label'));
      if (label) d += ' "' + label.slice(0, 40) + '"';
      landmarks.unshift(d);
    }
  }
  let sourceAttr = '', componentAttr = '';
  for (let c = el; c && c.nodeType === 1 && !(sourceAttr && componentAttr); c = up(c)) {
    const a = (n) => c.getAttribute(n) || '';
    if (!sourceAttr) {
      if (a('data-inspector-relative-path')) sourceAttr = [a('data-inspector-relative-path'), a('data-inspector-line'), a('data-inspector-column')].filter(Boolean).join(':');
      else if (a('data-v-inspector')) sourceAttr = a('data-v-inspector');
      else if (a('data-astro-source-file')) sourceAttr = a('data-astro-source-file') + (a('data-astro-source-loc') ? ':' + a('data-astro-source-loc') : '');
      else if (a('data-source-file')) sourceAttr = a('data-source-file') + (a('data-source-line') ? ':' + a('data-source-line') : '');
    }
    if (!componentAttr) componentAttr = a('data-component') || a('data-component-name');
  }
  const testid = TEST.map((a) => el.getAttribute(a)).find(Boolean) || '';
  const b = el.getBoundingClientRect();
  const cs = getComputedStyle(el);
  const styles = {};
  for (const k of ['display', 'position', 'color', 'background-color', 'font-family', 'font-size', 'font-weight',
                   'line-height', 'padding', 'margin', 'border', 'border-radius', 'opacity', 'z-index', 'overflow', 'cursor']) {
    let v = cs.getPropertyValue(k);
    if (k === 'font-family') v = v.split(',')[0].replace(/["']/g, '').trim();
    if (k !== 'display' && /^(|normal|auto|none|0px|static|visible|1|rgba\(0, 0, 0, 0\)|0px none.*)$/.test(v)) continue;
    styles[k] = v;
  }
  const state = {};
  for (const k of ['disabled', 'checked', 'required', 'readOnly']) if (el[k] === true) state[k.toLowerCase()] = true;
  for (const k of ['type', 'href', 'placeholder', 'aria-expanded', 'aria-selected', 'aria-checked', 'aria-pressed',
                   'aria-invalid', 'aria-current', 'aria-disabled']) {
    const v = el.getAttribute(k);
    if (v !== null && v !== '') state[k] = v.slice(0, 200);
  }
  if (document.activeElement === el) state.focused = true;
  return JSON.stringify({
    ref: window.__cu ? window.__cu.ref(el) : '',
    tag: el.localName,
    role, name,
    text: clean(el.innerText || el.textContent).slice(0, 160),
    selector: selector(el),
    testid,
    id: el.id || '',
    classes: [...el.classList].slice(0, 8).join(' '),
    landmarks: landmarks.join(' > '),
    source_attr: sourceAttr,
    component_attr: componentAttr,
    in_viewport: b.bottom > 0 && b.right > 0 && b.top < innerHeight && b.left < innerWidth,
    box: { x: Math.round(b.left + scrollX), y: Math.round(b.top + scrollY), w: Math.round(b.width), h: Math.round(b.height) },
    viewport: { vw: innerWidth, vh: innerHeight, sx: Math.round(scrollX), sy: Math.round(scrollY) },
    state, styles,
    next: window.__cu ? window.__cu.next() : 0,
  });
}"#;

/// The component that rendered an element, read in the page's own world
/// (framework bookkeeping lives there) with the element as `this`. Only
/// properties are read; no page function is called. Development builds keep
/// names and sources; a production build usually yields minified names or
/// nothing, and then the answer is an empty object.
const COMPONENT_FN: &str = r#"function(){
  const el = this;
  const short = (u) => {
    if (!u) return '';
    try { const x = new URL(u, location.href); u = x.origin === location.origin ? x.pathname : x.origin + x.pathname; } catch (e) {}
    return String(u).replace(/\?.*$/, '');
  };
  const out = (framework, chain, source, via) => JSON.stringify({
    framework, component: chain[0] || '', components: chain.slice(0, 8).join(' < '),
    source: source || '', source_via: source ? via : '',
  });
  const fiberKey = Object.keys(el).find((k) => k.startsWith('__reactFiber$') || k.startsWith('__reactInternalInstance$'));
  if (fiberKey) {
    const nameOf = (t) => !t || typeof t === 'string' ? ''
      : t.displayName || t.name || (t.render && (t.render.displayName || t.render.name)) || (t.type && nameOf(t.type)) || '';
    const fromStack = (e) => {
      const lines = String(e && e.stack || '').split('\n').slice(1);
      for (const line of lines) {
        const m = line.match(/\(?((?:https?|file|webpack[\w-]*):\/\/\S+?):(\d+):(\d+)\)?\s*$/);
        if (!m || /node_modules|react-dom|react_jsx|jsx-dev-runtime|jsx-runtime|react\.development|scheduler/.test(m[1])) continue;
        return short(m[1]) + ':' + m[2] + ':' + m[3];
      }
      return '';
    };
    const fiber = el[fiberKey];
    const chain = [];
    for (let f = fiber.return; f && chain.length < 8; f = f.return) {
      const n = nameOf(f.type);
      if (n && chain[chain.length - 1] !== n) chain.push(n);
    }
    let source = '', via = '';
    for (let f = fiber; f && !source; f = f.return) {
      if (f._debugSource) {
        const s = f._debugSource;
        source = short(s.fileName) + ':' + s.lineNumber + (s.columnNumber ? ':' + s.columnNumber : '');
        via = 'react _debugSource';
      } else if (f._debugStack) {
        source = fromStack(f._debugStack);
        via = 'react _debugStack (served file)';
      }
    }
    return out('react', chain, source, via);
  }
  for (let c = el; c; c = c.parentElement) {
    if (c.__vueParentComponent) {
      const chain = []; let file = '';
      for (let i = c.__vueParentComponent; i && chain.length < 8; i = i.parent) {
        const t = i.type || {};
        const n = t.name || t.__name || (t.__file ? t.__file.split('/').pop().replace(/\.vue$/, '') : '');
        if (n) chain.push(n);
        if (!file && t.__file) file = t.__file;
      }
      return out('vue', chain, file, 'vue __file');
    }
    if (c.__vue__) {
      const chain = []; let file = '';
      for (let v = c.__vue__; v && chain.length < 8; v = v.$parent) {
        const o = v.$options || {};
        const n = o.name || o._componentTag || (o.__file ? o.__file.split('/').pop().replace(/\.vue$/, '') : '');
        if (n) chain.push(n);
        if (!file && o.__file) file = o.__file;
      }
      return out('vue2', chain, file, 'vue __file');
    }
    const meta = c.__svelte_meta;
    if (meta && meta.loc) {
      const l = meta.loc;
      const file = l.file || '';
      return out('svelte', [file.split('/').pop().replace(/\.svelte$/, '')], file + (l.line != null ? ':' + l.line + ':' + l.column : ''), 'svelte __svelte_meta');
    }
  }
  const ng = window.ng;
  if (ng && typeof ng.getOwningComponent === 'function') {
    const chain = [];
    for (let c = ng.getOwningComponent(el); c && chain.length < 8; ) {
      chain.push(c.constructor && c.constructor.name || '');
      const host = ng.getHostElement && ng.getHostElement(c);
      c = host && host.parentElement ? ng.getOwningComponent(host.parentElement) : null;
    }
    return out('angular', chain.filter(Boolean), '', '');
  }
  return '{}';
}"#;

/// Inspect the element behind `reference` (a snapshot ref) or the first match
/// of `selector`, and return the JSON object that describes it.
///
/// A selector is looked up in the main document; the element gets a ref, so
/// the next action can use it directly.
pub fn inspect(
    tab: &Tab,
    reference: Option<&str>,
    selector: Option<&str>,
) -> Result<String, String> {
    let (target, frame) = frame_of(tab, reference);
    with_conn(&target, |conn| {
        let tree = conn.call("Page.getFrameTree", "{}")?;
        let frames = server::flatten_frame_tree(&tree);
        let Some(main) = frames.first().map(|f| f.id.clone()) else {
            return Ok(Err("page has no main frame".into()));
        };
        let frame_id = frame.clone().unwrap_or_else(|| main.clone());
        // The main document follows the helper policy; an iframe is always
        // read in its isolated world, where the snapshot left its refs.
        let world = if frame_id == main && target == *tab {
            crate::execution::helper_world()
        } else {
            crate::execution::World::Isolated
        };
        let mut cmd = |m: &str, p: &str| conn.call(m, p);
        let context = match crate::execution::context_fragment(&mut cmd, world, &frame_id) {
            Ok(context) => context,
            Err(e) => return Ok(Err(e)),
        };
        let expression = match (reference, selector) {
            (Some(r), _) => format!(
                "(()=>{{if(!window.__cu)return JSON.stringify({{error:'this page has no refs yet; take a snapshot first'}});\
                 const el=window.__cu.element({r});\
                 return el||JSON.stringify({{error:{r}+' is not on the page any more; take a new snapshot'}});}})()",
                r = json_string(r)
            ),
            (None, Some(s)) => format!(
                "window.__cuBase={};{}\n;(()=>{{const el={FIND_JS}({s});\
                 return el||JSON.stringify({{error:{s}+' matches nothing on the page'}});}})()",
                server::snapshot::base_for(tab.cdp_port, &frame_id),
                crate::actions::REGISTRY_JS,
                s = json_string(s)
            ),
            (None, None) => return Ok(Err("give ref or selector".into())),
        };
        let reply = conn.call(
            "Runtime.evaluate",
            &format!(
                "{{\"expression\":{},{context}\"silent\":true}}",
                json_string(&expression)
            ),
        )?;
        if reply.contains("\"exceptionDetails\"") {
            return Ok(Err(json_string_value(&reply, "description")
                .unwrap_or_else(|| "inspecting the element failed".into())));
        }
        let Some(object) = json_string_value(&reply, "objectId") else {
            let value = evaluated_string(&reply).unwrap_or_default();
            return Ok(Err(
                json_string_value(&value, "error").unwrap_or_else(|| "element not found".into())
            ));
        };
        let described = call_on(conn, &object, INSPECT_FN);
        let component = component_of(conn, &object).unwrap_or_default();
        release(conn, &object);
        let mut info = match described {
            Ok(info) => info,
            Err(e) => return Ok(Err(e)),
        };
        // A selector lookup may have handed out a ref: keep the daemon's
        // counter above it, so a later document never reuses the number.
        server::snapshot::saw_refs_up_to(&info);
        if frame_id != main {
            let url = frames
                .iter()
                .find(|f| f.id == frame_id)
                .map(|f| f.url.clone())
                .unwrap_or_default();
            info = splice(&info, &format!("\"frame\":{}", json_string(&url)));
        }
        if let Some(inner) = component
            .trim()
            .strip_prefix('{')
            .and_then(|c| c.strip_suffix('}'))
            && !inner.trim().is_empty()
        {
            info = splice(&info, inner);
        }
        Ok(Ok(info))
    })
}

/// `object` with `fields` (a `"k":v,...` list) added at the end.
fn splice(object: &str, fields: &str) -> String {
    match object.trim_end().strip_suffix('}') {
        Some(head) => format!("{head},{fields}}}"),
        None => object.to_string(),
    }
}

/// Call `function` with `object` as `this` and return the string it produced.
fn call_on(conn: &mut CdpConnection, object: &str, function: &str) -> Result<String, String> {
    let reply = conn.call(
        "Runtime.callFunctionOn",
        &format!(
            "{{\"objectId\":{},\"functionDeclaration\":{},\"returnByValue\":true,\"silent\":true}}",
            json_string(object),
            json_string(function)
        ),
    )?;
    if reply.contains("\"exceptionDetails\"") {
        return Err(json_string_value(&reply, "description")
            .unwrap_or_else(|| "script failed in the page".into()));
    }
    evaluated_string(&reply).ok_or_else(|| "the page returned nothing".into())
}

fn release(conn: &mut CdpConnection, object: &str) {
    let _ = conn.call(
        "Runtime.releaseObject",
        &format!("{{\"objectId\":{}}}", json_string(object)),
    );
}

/// The component JSON of the element `object` (an isolated-world handle):
/// the same DOM node, resolved in the page's own world, read by
/// [`COMPONENT_FN`].
fn component_of(conn: &mut CdpConnection, object: &str) -> Option<String> {
    let node = conn
        .call(
            "DOM.describeNode",
            &format!("{{\"objectId\":{}}}", json_string(object)),
        )
        .ok()?;
    let backend = json_value(&node, "backendNodeId")?;
    let resolved = conn
        .call(
            "DOM.resolveNode",
            &format!("{{\"backendNodeId\":{backend}}}"),
        )
        .ok()?;
    let main = json_string_value(&resolved, "objectId")?;
    let info = call_on(conn, &main, COMPONENT_FN).ok();
    release(conn, &main);
    info.filter(|i| i.starts_with('{'))
}

/// One labelled box an overlay draws.
#[derive(Clone, Debug, PartialEq)]
pub struct OverlayItem {
    pub label: String,
    pub reference: Option<String>,
    pub selector: Option<String>,
    pub color: &'static str,
}

/// Colours that stay apart on light and dark pages alike.
const PALETTE: [&str; 8] = [
    "#e6194b", "#3cb44b", "#4363d8", "#f58231", "#911eb4", "#008080", "#f032e6", "#9a6324",
];

/// Colour of a mark by severity.
fn severity_color(severity: Option<&str>) -> &'static str {
    match severity {
        Some("blocker") => "#d00000",
        Some("major") => "#e85d04",
        Some("minor") => "#c99700",
        Some("nit") => "#2a7ae2",
        _ => "#7b2ff7",
    }
}

/// Overlay items for snapshot refs, one colour per ref in turn.
pub fn ref_items(refs: &[String]) -> Vec<OverlayItem> {
    refs.iter()
        .enumerate()
        .map(|(i, r)| OverlayItem {
            label: r.clone(),
            reference: Some(r.clone()),
            selector: None,
            color: PALETTE[i % PALETTE.len()],
        })
        .collect()
}

fn items_json(items: &[OverlayItem]) -> String {
    let list: Vec<String> = items
        .iter()
        .map(|i| {
            format!(
                "{{\"label\":{},\"ref\":{},\"selector\":{},\"color\":\"{}\"}}",
                json_string(&i.label),
                i.reference
                    .as_deref()
                    .map(json_string)
                    .unwrap_or_else(|| "null".into()),
                i.selector
                    .as_deref()
                    .map(json_string)
                    .unwrap_or_else(|| "null".into()),
                i.color
            )
        })
        .collect();
    format!("[{}]", list.join(","))
}

/// Paints labelled boxes over the given elements, inside a closed shadow root
/// so the page's styles cannot reach them and they cannot reach the page.
/// Boxes are placed in document coordinates, so they stay on their element
/// whether the capture is of the viewport or of a clip beyond it. Waits (at
/// most 250 ms) for the paint, like the screenshot's own measure step.
const DRAW_JS: &str = r#"(async (items, find) => {
  if (window.__cuOverlay) window.__cuOverlay.remove();
  const host = document.createElement('cu-overlay');
  host.style.cssText = 'all:initial;position:absolute;left:0;top:0;width:0;height:0;z-index:2147483647;pointer-events:none;display:block';
  const root = host.attachShadow({ mode: 'closed' });
  const drawn = [];
  for (const it of items) {
    let el = it.ref && window.__cu ? window.__cu.element(it.ref) : null;
    if (!el && it.selector) el = find(it.selector);
    if (!el) continue;
    const r = el.getBoundingClientRect();
    if (r.width < 1 || r.height < 1) continue;
    const x = r.left + scrollX, y = r.top + scrollY;
    const box = document.createElement('div');
    box.style.cssText = `position:absolute;left:${x}px;top:${y}px;width:${r.width}px;height:${r.height}px;box-sizing:border-box;border:2px solid ${it.color};background:${it.color}1f;border-radius:3px`;
    const tag = document.createElement('div');
    tag.textContent = it.label;
    // Above the box, or below it when the box touches the top: a label
    // inside the box would hide what the image is meant to show.
    const above = r.top >= 16;
    tag.style.cssText = `position:absolute;left:${Math.max(x, 0)}px;top:${above ? y - 16 : y + r.height + 1}px;font:700 11px/14px ui-monospace,Menlo,Consolas,monospace;color:#fff;background:${it.color};padding:1px 4px;border-radius:3px;white-space:nowrap;box-shadow:0 0 0 1px #fff`;
    root.append(box, tag);
    drawn.push(it.label);
  }
  document.documentElement.appendChild(host);
  window.__cuOverlay = host;
  await new Promise((r) => { let done = false; const go = () => { if (!done) { done = true; r(); } };
    requestAnimationFrame(() => requestAnimationFrame(go)); setTimeout(go, 250); });
  return JSON.stringify({ drawn });
})"#;

const CLEAR_JS: &str = "(()=>{if(window.__cuOverlay){window.__cuOverlay.remove();window.__cuOverlay=null;}return '1';})()";

/// Where an overlay was drawn, so it can be cleared from the same place.
pub struct Drawn {
    pub labels: Vec<String>,
    places: Vec<(Tab, String)>,
}

/// Draw `items` over the page of `tab`. Each item is drawn in the frame its
/// ref lives in (iframes included, in their own document, so the box lands
/// where the frame shows the element); items without a known frame go to the
/// main document.
pub fn draw(tab: &Tab, items: &[OverlayItem]) -> Result<Drawn, String> {
    let mut groups: Vec<((Tab, Option<String>), Vec<OverlayItem>)> = Vec::new();
    for item in items {
        let key = frame_of(tab, item.reference.as_deref());
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, list)) => list.push(item.clone()),
            None => groups.push((key, vec![item.clone()])),
        }
    }
    let mut drawn = Drawn {
        labels: Vec::new(),
        places: Vec::new(),
    };
    for ((target, frame), list) in groups {
        let mut cmd = |m: &str, p: &str| target.command(m, p);
        let main = crate::execution::main_frame(&mut cmd)?;
        let frame_id = frame.unwrap_or_else(|| main.clone());
        // The registry the refs live in is in cu's isolated world (the main
        // document's follows `CU_HELPER_WORLD`): the overlay is drawn from
        // there so it can find them.
        let world = if frame_id == main && target == *tab {
            crate::execution::helper_world()
        } else {
            crate::execution::World::Isolated
        };
        let context = crate::execution::context_fragment(&mut cmd, world, &frame_id)?;
        let expression = format!("{DRAW_JS}({},{FIND_JS})", items_json(&list));
        let reply = cmd(
            "Runtime.evaluate",
            &crate::execution::evaluate_params(&expression, &context, true),
        )?;
        drawn.places.push((target.clone(), context));
        let value = evaluated_string(&reply).unwrap_or_default();
        if let Some(labels) = server::json_array(&value, "drawn") {
            drawn.labels.extend(
                labels
                    .trim_matches(|c| c == '[' || c == ']')
                    .split(',')
                    .filter_map(|l| {
                        let l = l.trim();
                        (!l.is_empty())
                            .then(|| json_string_value(&format!("{{\"l\":{l}}}"), "l"))?
                    }),
            );
        }
    }
    Ok(drawn)
}

/// Remove an overlay [`draw`] painted.
pub fn clear(drawn: &Drawn) {
    for (target, context) in &drawn.places {
        let _ = target.command(
            "Runtime.evaluate",
            &crate::execution::evaluate_params(CLEAR_JS, context, false),
        );
    }
}

/// A QA note pinned to an element.
#[derive(Clone, Debug)]
pub struct Mark {
    pub n: usize,
    pub url: String,
    pub reference: String,
    pub selector: String,
    pub severity: Option<String>,
    pub note: String,
    /// The [`inspect`] JSON at the time of the mark.
    pub info: String,
}

pub const SEVERITIES: [&str; 4] = ["blocker", "major", "minor", "nit"];

fn marks() -> &'static Mutex<HashMap<Tab, Vec<Mark>>> {
    static MARKS: OnceLock<Mutex<HashMap<Tab, Vec<Mark>>>> = OnceLock::new();
    MARKS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The page's URL, as the browser reports it.
fn page_url(tab: &Tab) -> String {
    tab.command(
        "Runtime.evaluate",
        "{\"expression\":\"location.href\",\"returnByValue\":true}",
    )
    .ok()
    .and_then(|r| evaluated_string(&r))
    .unwrap_or_default()
}

/// Inspect an element and keep a note on it. Returns the mark as JSON.
pub fn add_mark(
    tab: &Tab,
    reference: Option<&str>,
    selector: Option<&str>,
    note: &str,
    severity: Option<&str>,
) -> Result<String, String> {
    if note.trim().is_empty() {
        return Err("a mark needs a note saying what is wrong".into());
    }
    if let Some(s) = severity
        && !SEVERITIES.contains(&s)
    {
        return Err(format!(
            "severity must be one of {}, got {s}",
            SEVERITIES.join(", ")
        ));
    }
    let info = inspect(tab, reference, selector)?;
    let mut all = marks().lock().unwrap_or_else(|e| e.into_inner());
    let list = all.entry(tab.clone()).or_default();
    let mark = Mark {
        n: list.last().map_or(1, |m| m.n + 1),
        url: page_url(tab),
        reference: json_string_value(&info, "ref").unwrap_or_default(),
        selector: json_string_value(&info, "selector").unwrap_or_default(),
        severity: severity.map(str::to_string),
        note: note.trim().to_string(),
        info,
    };
    let json = mark_json(&mark);
    list.push(mark);
    Ok(json)
}

fn mark_json(mark: &Mark) -> String {
    format!(
        "{{\"n\":{},\"url\":{},\"ref\":{},\"selector\":{},\"severity\":{},\"note\":{},\"inspect\":{}}}",
        mark.n,
        json_string(&mark.url),
        json_string(&mark.reference),
        json_string(&mark.selector),
        mark.severity
            .as_deref()
            .map(json_string)
            .unwrap_or_else(|| "null".into()),
        json_string(&mark.note),
        mark.info
    )
}

pub fn marks_of(tab: &Tab) -> Vec<Mark> {
    marks()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(tab)
        .cloned()
        .unwrap_or_default()
}

pub fn clear_marks(tab: &Tab) -> usize {
    marks()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(tab)
        .map_or(0, |l| l.len())
}

pub fn marks_json(marks: &[Mark]) -> String {
    format!(
        "{{\"marks\":[{}]}}",
        marks.iter().map(mark_json).collect::<Vec<_>>().join(",")
    )
}

/// Overlay items for the marks made on `url`: numbered like the report, the
/// ref first and the selector when the ref has gone stale.
pub fn mark_items(marks: &[Mark], url: &str) -> Vec<OverlayItem> {
    marks
        .iter()
        .filter(|m| m.url == url)
        .map(|m| OverlayItem {
            label: format!("#{}", m.n),
            reference: Some(m.reference.clone()).filter(|r| !r.is_empty()),
            selector: Some(m.selector.clone()).filter(|s| !s.is_empty()),
            color: severity_color(m.severity.as_deref()),
        })
        .collect()
}

pub fn current_url(tab: &Tab) -> String {
    page_url(tab)
}

/// The marks as a Markdown report: one section per mark with what is wrong,
/// how to find the element in the browser and where it is in the code.
pub fn marks_markdown(marks: &[Mark]) -> String {
    if marks.is_empty() {
        return "# QA marks\n\nNo marks.\n".into();
    }
    let mut out = format!("# QA marks ({})\n", marks.len());
    let mut last_url = "";
    for m in marks {
        if m.url != last_url {
            out.push_str(&format!("\n## {}\n", m.url));
            last_url = &m.url;
        }
        out.push_str(&describe_mark(m));
    }
    out
}

fn describe_mark(m: &Mark) -> String {
    let info = &m.info;
    let field = |k: &str| json_string_value(info, k).filter(|v| !v.is_empty());
    let severity = m
        .severity
        .as_deref()
        .map(|s| format!("[{s}] "))
        .unwrap_or_default();
    let mut out = format!("\n### {}. {severity}{}\n\n", m.n, m.note);
    let role = field("role").unwrap_or_else(|| "element".into());
    let what = match field("name") {
        Some(name) => format!("{role} \"{name}\""),
        None => role,
    };
    out.push_str(&format!(
        "- element: {what} `<{}>`{}\n",
        field("tag").unwrap_or_default(),
        if m.reference.is_empty() {
            String::new()
        } else {
            format!(" (ref {})", m.reference)
        }
    ));
    out.push_str(&format!("- selector: `{}`\n", m.selector));
    if let Some(t) = field("testid") {
        out.push_str(&format!("- test id: `{t}`\n"));
    }
    if let Some(frame) = field("frame") {
        out.push_str(&format!("- inside frame: {frame}\n"));
    }
    if let Some(l) = field("landmarks") {
        out.push_str(&format!("- inside: {l}\n"));
    }
    let component = field("component").or_else(|| field("component_attr"));
    if let Some(c) = component {
        let framework = field("framework")
            .map(|f| format!(" ({f})"))
            .unwrap_or_default();
        let chain = field("components")
            .filter(|chain| *chain != c)
            .map(|chain| format!(" -- {chain}"))
            .unwrap_or_default();
        out.push_str(&format!("- component: {c}{framework}{chain}\n"));
    }
    if let Some(s) = field("source").or_else(|| field("source_attr")) {
        out.push_str(&format!("- source: `{s}`\n"));
    }
    if let Some(b) = json_object(info, "box") {
        let n = |k: &str| json_number(b, k).unwrap_or(0.0);
        let viewport = json_object(info, "viewport")
            .map(|v| {
                format!(
                    ", viewport {}x{}",
                    json_number(v, "vw").unwrap_or(0.0),
                    json_number(v, "vh").unwrap_or(0.0)
                )
            })
            .unwrap_or_default();
        out.push_str(&format!(
            "- box: {}x{} at ({}, {}){viewport}\n",
            n("w"),
            n("h"),
            n("x"),
            n("y")
        ));
    }
    if let Some(styles) = json_object(info, "styles").filter(|s| s.len() > 2) {
        out.push_str(&format!("- styles: `{styles}`\n"));
    }
    if let Some(state) = json_object(info, "state").filter(|s| s.len() > 2) {
        out.push_str(&format!("- state: `{state}`\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mark(n: usize, info: &str) -> Mark {
        Mark {
            n,
            url: "http://localhost:5173/login".into(),
            reference: "e7".into(),
            selector: "#login > button".into(),
            severity: Some("major".into()),
            note: "Button overlaps the footer at 375px".into(),
            info: info.into(),
        }
    }

    #[test]
    fn a_mark_reads_as_a_report_a_developer_can_act_on() {
        let info = r##"{"ref":"e7","tag":"button","role":"button","name":"Sign in","text":"Sign in","selector":"#login > button","testid":"submit","id":"","classes":"btn","landmarks":"main > form#login","source_attr":"","component_attr":"","in_viewport":true,"box":{"x":24,"y":610,"w":320,"h":44},"viewport":{"vw":375,"vh":667,"sx":0,"sy":0},"state":{"type":"submit"},"styles":{"display":"block"},"framework":"react","component":"SubmitButton","components":"SubmitButton < LoginForm < App","source":"/src/LoginForm.tsx:42:7","source_via":"react _debugSource"}"##;
        let md = marks_markdown(&[mark(1, info)]);
        assert!(md.contains("## http://localhost:5173/login"), "{md}");
        assert!(
            md.contains("### 1. [major] Button overlaps the footer at 375px"),
            "{md}"
        );
        assert!(
            md.contains("- element: button \"Sign in\" `<button>` (ref e7)"),
            "{md}"
        );
        assert!(md.contains("- selector: `#login > button`"), "{md}");
        assert!(md.contains("- test id: `submit`"), "{md}");
        assert!(md.contains("- inside: main > form#login"), "{md}");
        assert!(
            md.contains("- component: SubmitButton (react) -- SubmitButton < LoginForm < App"),
            "{md}"
        );
        assert!(md.contains("- source: `/src/LoginForm.tsx:42:7`"), "{md}");
        assert!(
            md.contains("- box: 320x44 at (24, 610), viewport 375x667"),
            "{md}"
        );
    }

    #[test]
    fn a_build_plugin_source_is_used_when_the_framework_gives_none() {
        let info = r#"{"ref":"e2","tag":"a","role":"link","name":"Docs","selector":"nav > a","source_attr":"src/Nav.vue:12:5","component_attr":"NavBar","box":{"x":0,"y":0,"w":10,"h":10},"state":{},"styles":{}}"#;
        let md = marks_markdown(&[mark(2, info)]);
        assert!(md.contains("- source: `src/Nav.vue:12:5`"), "{md}");
        assert!(md.contains("- component: NavBar\n"), "{md}");
        assert!(!md.contains("- styles"), "empty styles are left out: {md}");
    }

    #[test]
    fn no_marks_is_said_plainly() {
        assert!(marks_markdown(&[]).contains("No marks."));
    }

    #[test]
    fn mark_overlays_follow_the_report_numbers_and_the_page() {
        let mut other = mark(2, "{}");
        other.url = "http://localhost:5173/other".into();
        other.severity = None;
        let items = mark_items(&[mark(1, "{}"), other], "http://localhost:5173/login");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "#1");
        assert_eq!(items[0].reference.as_deref(), Some("e7"));
        assert_eq!(items[0].selector.as_deref(), Some("#login > button"));
        assert_eq!(items[0].color, severity_color(Some("major")));
    }

    #[test]
    fn overlay_items_are_passed_to_the_page_as_json() {
        let items = ref_items(&["e1".into(), "e2".into()]);
        assert_eq!(items[0].color, PALETTE[0]);
        assert_eq!(items[1].color, PALETTE[1]);
        assert_eq!(
            items_json(&items[..1]),
            format!(
                r#"[{{"label":"e1","ref":"e1","selector":null,"color":"{}"}}]"#,
                PALETTE[0]
            )
        );
    }

    #[test]
    fn splice_adds_fields_to_an_object() {
        assert_eq!(splice(r#"{"a":1}"#, r#""b":2"#), r#"{"a":1,"b":2}"#);
    }
}
