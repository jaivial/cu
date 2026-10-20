//! Capturing a 2FA setup QR off the page, and rendering one back.
//!
//! Enrollment pages show a QR that encodes an `otpauth://` URI. An agent
//! used to have to read that URI out of a screenshot by eye — or ask the
//! person to. `capture_otpauth` finds square, QR-looking elements (an
//! `<img>`, a `<canvas>`, an `<svg>`; attribute text mentioning qr / otp /
//! 2fa / authenticator ranks first), clips each out of the tab and decodes
//! it with `rqrr` until one yields an `otpauth://` URI, with a full-viewport
//! pass as the fallback. The URI is the enrollment seed: callers seal it in
//! the vault; it is never logged and never returned past the capture route's
//! storage step.
//!
//! `render_svg` is the opposite trip: the temporary hand-off page shows the
//! same URI as a QR again, so the person can enroll their own authenticator
//! from the same seed the agent just captured.

use crate::server::{Tab, evaluated_string, json_number, json_objects};

/// One decoded `otpauth://` URI from the tab, or why none was found.
pub fn capture_otpauth(tab: &Tab) -> Result<String, String> {
    // Candidate elements: visible, roughly square, big enough to scan.
    // Anything whose surroundings mention QR/OTP/2FA ranks first; then by
    // area. At most six clips are decoded before the viewport fallback.
    let expression = r#"(()=>{
      const hint=/(qr|otp|2fa|totp|authenticator|verification)/i;
      const out=[];
      for(const el of document.querySelectorAll('img,canvas,svg')){
        const r=el.getBoundingClientRect();
        if(r.width<64||r.height<64) continue;
        if(r.width>innerWidth*1.2||r.height>innerHeight*1.2) continue;
        const ratio=r.width/r.height;
        if(ratio<0.7||ratio>1.43) continue;
        const text=[el.alt,el.title,el.id,el.className,el.src&&el.src.slice(0,120),
                    el.parentElement&&el.parentElement.className,
                    el.parentElement&&el.parentElement.id].filter(Boolean).join(' ');
        const score=(hint.test(text)?1e12:0)+r.width*r.height;
        out.push({x:r.left+scrollX,y:r.top+scrollY,w:r.width,h:r.height,score});
      }
      out.sort((a,b)=>b.score-a.score);
      return JSON.stringify(out.slice(0,6));
    })()"#;
    let reply = tab.command(
        "Runtime.evaluate",
        &format!(
            "{{\"expression\":{},\"returnByValue\":true}}",
            crate::server::json_string(expression)
        ),
    )?;
    let listed = evaluated_string(&reply).unwrap_or_else(|| "[]".into());
    let mut clips: Vec<(f64, f64, f64, f64)> = json_objects(&listed)
        .iter()
        .filter_map(|o| {
            Some((
                json_number(o, "x")?,
                json_number(o, "y")?,
                json_number(o, "w")?,
                json_number(o, "h")?,
            ))
        })
        .collect();
    clips.push((0.0, 0.0, 0.0, 0.0)); // (0,0,0,0): the whole viewport, last resort
    let mut saw_qr = false;
    let mut found: Vec<String> = Vec::new();
    for (x, y, w, h) in clips {
        let params = if w > 0.0 {
            // A margin around the element: quiet zone matters to decoders,
            // and a hair of surrounding page costs nothing.
            let pad = 16.0;
            format!(
                "{{\"format\":\"png\",\"clip\":{{\"x\":{},\"y\":{},\"width\":{},\"height\":{},\"scale\":2}},\"captureBeyondViewport\":true}}",
                (x - pad).max(0.0),
                (y - pad).max(0.0),
                w + 2.0 * pad,
                h + 2.0 * pad,
            )
        } else {
            "{\"format\":\"png\"}".into()
        };
        let Ok(result) = tab.command("Page.captureScreenshot", &params) else {
            continue;
        };
        let Some(data) = crate::server::json_string_value(&result, "data") else {
            continue;
        };
        let Ok(png) = crate::server::decode_base64(&data) else {
            continue;
        };
        for content in decode_png(&png) {
            if content.starts_with("otpauth://") {
                // The same QR shows in its element clip and in the viewport
                // fallback; distinct URIs are the ambiguity to refuse.
                if !found.contains(&content) {
                    found.push(content);
                }
            } else {
                saw_qr = true;
            }
        }
    }
    if found.len() > 1 {
        return Err(format!(
            "{} different setup QRs on this page; refusing to guess - narrow to the right element or tab and retry",
            found.len()
        ));
    }
    if let Some(uri) = found.pop() {
        return Ok(uri);
    }
    Err(if saw_qr {
        "a QR was found but it is not an otpauth:// setup code".into()
    } else {
        "no QR code on this page".into()
    })
}

/// Every QR string a PNG carries (normally zero or one).
fn decode_png(png: &[u8]) -> Vec<String> {
    let Ok(image) = image::load_from_memory_with_format(png, image::ImageFormat::Png) else {
        return Vec::new();
    };
    let gray = image.to_luma8();
    let mut prepared = rqrr::PreparedImage::prepare(gray);
    prepared
        .detect_grids()
        .into_iter()
        .filter_map(|grid| grid.decode().ok().map(|(_, content)| content))
        .collect()
}

/// An `otpauth://` URI as a standalone SVG QR, for the hand-off page.
pub fn render_svg(uri: &str) -> Result<String, String> {
    let code = qrcode::QrCode::new(uri.as_bytes()).map_err(|e| format!("QR render: {e}"))?;
    Ok(code
        .render::<qrcode::render::svg::Color>()
        .min_dimensions(240, 240)
        .quiet_zone(true)
        .build())
}

