//! Where a browser context says it is: country, region, town.
//!
//! A search engine decides "where" from several signals at once -- the
//! egress IP, the `Accept-Language` header, the page's locale and time zone,
//! the position `navigator.geolocation` answers with, and the query
//! parameters the engine itself documents (`gl`/`hl`/`uule` for Google,
//! `cc`/`mkt`/`setlang` for Bing, `kl` for DuckDuckGo). A [`Location`] is one
//! coherent set of those values. [`crate::server`] applies it to a named
//! browser context (locale, time zone and position are DevTools overrides
//! scoped to that context's tab; the proxy is fixed when the context is
//! created) and [`search_url`] turns it into the engine's own parameters.
//!
//! Nothing here hides anything: a context with no proxy still leaves through
//! this machine's IP, and [`coherence`] says so instead of pretending the
//! location is complete.

/// One country the built-in table knows: enough to fill in a coherent
/// locale, time zone and a fallback position (the capital) when only the
/// country is given.
#[derive(Clone, Copy, Debug)]
pub struct Country {
    pub code: &'static str,
    /// English name, which is what Google's canonical location names use.
    pub name: &'static str,
    pub locale: &'static str,
    pub timezone: &'static str,
    pub lat: f64,
    pub lon: f64,
}

const fn c(
    code: &'static str,
    name: &'static str,
    locale: &'static str,
    timezone: &'static str,
    lat: f64,
    lon: f64,
) -> Country {
    Country {
        code,
        name,
        locale,
        timezone,
        lat,
        lon,
    }
}

/// Countries with a capital position and the locale most searches there use.
pub const COUNTRIES: &[Country] = &[
    c("ES", "Spain", "es-ES", "Europe/Madrid", 40.4168, -3.7038),
    c(
        "MX",
        "Mexico",
        "es-MX",
        "America/Mexico_City",
        19.4326,
        -99.1332,
    ),
    c(
        "AR",
        "Argentina",
        "es-AR",
        "America/Argentina/Buenos_Aires",
        -34.6037,
        -58.3816,
    ),
    c(
        "CO",
        "Colombia",
        "es-CO",
        "America/Bogota",
        4.7110,
        -74.0721,
    ),
    c(
        "CL",
        "Chile",
        "es-CL",
        "America/Santiago",
        -33.4489,
        -70.6693,
    ),
    c("PE", "Peru", "es-PE", "America/Lima", -12.0464, -77.0428),
    c(
        "VE",
        "Venezuela",
        "es-VE",
        "America/Caracas",
        10.4806,
        -66.9036,
    ),
    c(
        "EC",
        "Ecuador",
        "es-EC",
        "America/Guayaquil",
        -0.1807,
        -78.4678,
    ),
    c(
        "UY",
        "Uruguay",
        "es-UY",
        "America/Montevideo",
        -34.9011,
        -56.1645,
    ),
    c(
        "PY",
        "Paraguay",
        "es-PY",
        "America/Asuncion",
        -25.2637,
        -57.5759,
    ),
    c(
        "BO",
        "Bolivia",
        "es-BO",
        "America/La_Paz",
        -16.4897,
        -68.1193,
    ),
    c(
        "CR",
        "Costa Rica",
        "es-CR",
        "America/Costa_Rica",
        9.9281,
        -84.0907,
    ),
    c("PA", "Panama", "es-PA", "America/Panama", 8.9824, -79.5199),
    c(
        "DO",
        "Dominican Republic",
        "es-DO",
        "America/Santo_Domingo",
        18.4861,
        -69.9312,
    ),
    c(
        "GT",
        "Guatemala",
        "es-GT",
        "America/Guatemala",
        14.6349,
        -90.5069,
    ),
    c(
        "US",
        "United States",
        "en-US",
        "America/New_York",
        38.9072,
        -77.0369,
    ),
    c(
        "CA",
        "Canada",
        "en-CA",
        "America/Toronto",
        45.4215,
        -75.6972,
    ),
    c(
        "GB",
        "United Kingdom",
        "en-GB",
        "Europe/London",
        51.5074,
        -0.1278,
    ),
    c("IE", "Ireland", "en-IE", "Europe/Dublin", 53.3498, -6.2603),
    c(
        "AU",
        "Australia",
        "en-AU",
        "Australia/Sydney",
        -35.2809,
        149.1300,
    ),
    c(
        "NZ",
        "New Zealand",
        "en-NZ",
        "Pacific/Auckland",
        -41.2865,
        174.7762,
    ),
    c("IN", "India", "en-IN", "Asia/Kolkata", 28.6139, 77.2090),
    c(
        "ZA",
        "South Africa",
        "en-ZA",
        "Africa/Johannesburg",
        -25.7479,
        28.2293,
    ),
    c("FR", "France", "fr-FR", "Europe/Paris", 48.8566, 2.3522),
    c("BE", "Belgium", "fr-BE", "Europe/Brussels", 50.8503, 4.3517),
    c(
        "CH",
        "Switzerland",
        "de-CH",
        "Europe/Zurich",
        46.9480,
        7.4474,
    ),
    c("DE", "Germany", "de-DE", "Europe/Berlin", 52.5200, 13.4050),
    c("AT", "Austria", "de-AT", "Europe/Vienna", 48.2082, 16.3738),
    c("IT", "Italy", "it-IT", "Europe/Rome", 41.9028, 12.4964),
    c("PT", "Portugal", "pt-PT", "Europe/Lisbon", 38.7223, -9.1393),
    c(
        "BR",
        "Brazil",
        "pt-BR",
        "America/Sao_Paulo",
        -15.7939,
        -47.8828,
    ),
    c(
        "NL",
        "Netherlands",
        "nl-NL",
        "Europe/Amsterdam",
        52.3676,
        4.9041,
    ),
    c(
        "SE",
        "Sweden",
        "sv-SE",
        "Europe/Stockholm",
        59.3293,
        18.0686,
    ),
    c("NO", "Norway", "nb-NO", "Europe/Oslo", 59.9139, 10.7522),
    c(
        "DK",
        "Denmark",
        "da-DK",
        "Europe/Copenhagen",
        55.6761,
        12.5683,
    ),
    c(
        "FI",
        "Finland",
        "fi-FI",
        "Europe/Helsinki",
        60.1699,
        24.9384,
    ),
    c("PL", "Poland", "pl-PL", "Europe/Warsaw", 52.2297, 21.0122),
    c("CZ", "Czechia", "cs-CZ", "Europe/Prague", 50.0755, 14.4378),
    c(
        "RO",
        "Romania",
        "ro-RO",
        "Europe/Bucharest",
        44.4268,
        26.1025,
    ),
    c("GR", "Greece", "el-GR", "Europe/Athens", 37.9838, 23.7275),
    c("TR", "Turkey", "tr-TR", "Europe/Istanbul", 39.9334, 32.8597),
    c("RU", "Russia", "ru-RU", "Europe/Moscow", 55.7558, 37.6173),
    c("UA", "Ukraine", "uk-UA", "Europe/Kyiv", 50.4501, 30.5234),
    c("JP", "Japan", "ja-JP", "Asia/Tokyo", 35.6762, 139.6503),
    c(
        "KR",
        "South Korea",
        "ko-KR",
        "Asia/Seoul",
        37.5665,
        126.9780,
    ),
    c("CN", "China", "zh-CN", "Asia/Shanghai", 39.9042, 116.4074),
    c(
        "MA",
        "Morocco",
        "fr-MA",
        "Africa/Casablanca",
        34.0209,
        -6.8416,
    ),
    c("AD", "Andorra", "ca-AD", "Europe/Andorra", 42.5063, 1.5218),
];

/// The country a code or English/Spanish name names (`ES`, `es`, `Spain`,
/// `España`, `UK`).
pub fn country(name: &str) -> Option<&'static Country> {
    let wanted = fold(name.trim());
    let wanted = match wanted.as_str() {
        "uk" | "england" | "reino unido" => "gb".to_string(),
        "espana" => "es".to_string(),
        "mexico" => "mx".to_string(),
        "usa" | "eeuu" | "estados unidos" => "us".to_string(),
        "francia" => "fr".to_string(),
        "alemania" => "de".to_string(),
        "italia" => "it".to_string(),
        "brasil" => "br".to_string(),
        "portugal" => "pt".to_string(),
        other => other.to_string(),
    };
    COUNTRIES
        .iter()
        .find(|c| fold(c.code) == wanted || fold(c.name) == wanted)
}

/// Lower case, with the accents of Latin letters taken off, so `Sevilla`,
/// `SEVILLA` and `Sevílla` compare equal. Used for keyword matching too.
pub fn fold(s: &str) -> String {
    s.chars()
        .flat_map(|ch| ch.to_lowercase())
        .map(|ch| match ch {
            'á' | 'à' | 'â' | 'ä' | 'ã' | 'å' => 'a',
            'é' | 'è' | 'ê' | 'ë' => 'e',
            'í' | 'ì' | 'î' | 'ï' => 'i',
            'ó' | 'ò' | 'ô' | 'ö' | 'õ' => 'o',
            'ú' | 'ù' | 'û' | 'ü' => 'u',
            'ç' => 'c',
            'ñ' => 'n',
            other => other,
        })
        .collect()
}

/// One coherent "where": the values a context is given and the search
/// parameters are derived from.
#[derive(Clone, Debug, PartialEq)]
pub struct Location {
    /// ISO 3166-1 alpha-2, upper case.
    pub country: String,
    pub country_name: String,
    pub region: Option<String>,
    pub city: Option<String>,
    pub lat: f64,
    pub lon: f64,
    /// Metres, for `navigator.geolocation`.
    pub accuracy: f64,
    pub locale: String,
    pub timezone: String,
    /// Where the position came from: `given`, `resolved` (OpenStreetMap
    /// Nominatim), or `capital` (the country's fallback).
    pub source: String,
    /// The name Google's `uule` carries: `City,Region,Country`.
    pub canonical: String,
    pub notes: Vec<String>,
}

/// What a caller asked for, before it is completed into a [`Location`].
#[derive(Clone, Debug, Default)]
pub struct Request {
    pub country: String,
    pub region: Option<String>,
    pub city: Option<String>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub locale: Option<String>,
    pub timezone: Option<String>,
    /// Overrides the `uule` name (`Seville,Andalusia,Spain`).
    pub canonical: Option<String>,
}

impl Request {
    /// A request from `"Town[,Region],Country"`: the last part is the
    /// country, the first the town, anything in between the region.
    pub fn parse(spec: &str) -> Result<Self, String> {
        let parts: Vec<&str> = spec
            .split(',')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .collect();
        let Some((country, rest)) = parts.split_last() else {
            return Err("empty location; use \"Town,Region,Country\" or a country code".into());
        };
        let mut request = Request {
            country: country.to_string(),
            ..Default::default()
        };
        match rest {
            [] => {}
            [city] => request.city = Some(city.to_string()),
            [city, region @ ..] => {
                request.city = Some(city.to_string());
                request.region = Some(region.join(", "));
            }
        }
        Ok(request)
    }

    /// A request from the fields of a JSON body (`country`, `region`, `city`,
    /// `lat`, `lon`, `locale`, `timezone`, `canonical`, or `geo` as one
    /// `"Town,Region,Country"` string).
    pub fn from_json(body: &str) -> Result<Option<Self>, String> {
        let get =
            |k: &str| crate::server::json_string_value(body, k).filter(|v| !v.trim().is_empty());
        let mut request = match get("geo") {
            Some(spec) => Self::parse(&spec)?,
            None => match get("country") {
                Some(country) => Request {
                    country,
                    ..Default::default()
                },
                None => return Ok(None),
            },
        };
        if let Some(country) = get("country") {
            request.country = country;
        }
        request.region = get("region").or(request.region);
        request.city = get("city").or(request.city);
        request.locale = get("locale");
        request.timezone = get("timezone");
        request.canonical = get("canonical");
        let number = |k: &str| {
            crate::server::json_number(body, k)
                .or_else(|| get(k).and_then(|v| v.parse::<f64>().ok()))
        };
        request.lat = number("lat");
        request.lon = number("lon");
        Ok(Some(request))
    }

    /// Whether a town or region was named but no position given: the place
    /// has to be looked up.
    pub fn needs_lookup(&self) -> bool {
        (self.city.is_some() || self.region.is_some()) && (self.lat.is_none() || self.lon.is_none())
    }

    /// The Nominatim query for this request.
    pub fn lookup_query(&self) -> String {
        let mut parts = Vec::new();
        if let Some(city) = &self.city {
            parts.push(city.clone());
        }
        if let Some(region) = &self.region {
            parts.push(region.clone());
        }
        parts.push(
            country(&self.country)
                .map(|c| c.name.to_string())
                .unwrap_or_else(|| self.country.clone()),
        );
        parts.join(", ")
    }
}

/// A place a geocoder found.
#[derive(Clone, Debug, PartialEq)]
pub struct Place {
    pub lat: f64,
    pub lon: f64,
    pub display_name: String,
    pub city: Option<String>,
    pub region: Option<String>,
    pub country_code: Option<String>,
    pub country_name: Option<String>,
}

/// The first result of a Nominatim `format=json&addressdetails=1` answer.
pub fn parse_nominatim(json: &str) -> Option<Place> {
    let first = crate::server::json_objects(json).into_iter().next()?;
    let number = |k: &str| {
        crate::server::json_string_value(&first, k)
            .and_then(|v| v.parse::<f64>().ok())
            .or_else(|| crate::server::json_number(&first, k))
    };
    let address = crate::server::json_object(&first, "address").unwrap_or("{}");
    let field = |keys: &[&str]| {
        keys.iter()
            .find_map(|k| crate::server::json_string_value(address, k))
    };
    Some(Place {
        lat: number("lat")?,
        lon: number("lon")?,
        display_name: crate::server::json_string_value(&first, "display_name").unwrap_or_default(),
        city: field(&[
            "city",
            "town",
            "village",
            "municipality",
            "hamlet",
            "suburb",
        ]),
        region: field(&["state", "province", "region", "county"]),
        country_code: field(&["country_code"]).map(|c| c.to_ascii_uppercase()),
        country_name: field(&["country"]),
    })
}

/// The Nominatim search URL for `query` (English names: they are what
/// Google's canonical location names use).
pub fn nominatim_url(query: &str) -> String {
    format!(
        "https://nominatim.openstreetmap.org/search?format=json&addressdetails=1&limit=1&accept-language=en&q={}",
        url_encode(query)
    )
}

/// Complete a request into a location. `place` is what a lookup found for
/// it, when one was made.
pub fn complete(request: &Request, place: Option<&Place>) -> Result<Location, String> {
    let found = country(&request.country).or_else(|| {
        place
            .and_then(|p| p.country_code.as_deref())
            .and_then(country)
    });
    let mut notes = Vec::new();
    let (code, name, base_locale, base_tz, cap_lat, cap_lon) = match found {
        Some(c) => (
            c.code.to_string(),
            c.name.to_string(),
            c.locale.to_string(),
            c.timezone.to_string(),
            c.lat,
            c.lon,
        ),
        None => {
            let code = request.country.trim().to_ascii_uppercase();
            if code.len() != 2 || !code.chars().all(|c| c.is_ascii_alphabetic()) {
                return Err(format!(
                    "unknown country {:?}; give an ISO code (ES, MX, US...)",
                    request.country
                ));
            }
            if request.locale.is_none() || request.timezone.is_none() {
                return Err(format!(
                    "country {code} is not in the built-in table: give --lang and --tz as well"
                ));
            }
            notes.push(format!("country {code} is not in the built-in table"));
            (code.clone(), code, String::new(), String::new(), 0.0, 0.0)
        }
    };
    let (lat, lon, source, accuracy) = match (request.lat, request.lon, place) {
        (Some(lat), Some(lon), _) => (lat, lon, "given", 100.0),
        (_, _, Some(p)) => (p.lat, p.lon, "resolved", 1000.0),
        _ if found.is_some() => {
            if request.city.is_some() || request.region.is_some() {
                notes.push(
                    "the place could not be looked up: the position is the capital's; give --lat/--lon"
                        .into(),
                );
            }
            (cap_lat, cap_lon, "capital", 20_000.0)
        }
        _ => return Err("no position: give --lat and --lon".into()),
    };
    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
        return Err(format!("position out of range: {lat},{lon}"));
    }
    let city = request
        .city
        .clone()
        .or_else(|| place.and_then(|p| p.city.clone()));
    let region = request
        .region
        .clone()
        .or_else(|| place.and_then(|p| p.region.clone()));
    let timezone = match &request.timezone {
        Some(tz) => tz.clone(),
        None => {
            let (tz, estimated) = zone_for(&code, region.as_deref(), lon, &base_tz);
            if estimated {
                notes.push(format!(
                    "time zone {tz} estimated from the position; give --tz to fix it"
                ));
            }
            tz
        }
    };
    let locale = request.locale.clone().unwrap_or(base_locale);
    // Google's canonical names are English (`Seville,Andalusia,Spain`):
    // the geocoder's names are used when there are some, the caller's
    // otherwise. Google accepts both in practice.
    let canonical = request.canonical.clone().unwrap_or_else(|| {
        let mut parts = Vec::new();
        if let Some(c) = place.and_then(|p| p.city.clone()).or_else(|| city.clone()) {
            parts.push(c);
        }
        if let Some(r) = place
            .and_then(|p| p.region.clone())
            .or_else(|| region.clone())
        {
            parts.push(r);
        }
        parts.push(name.clone());
        parts.join(",")
    });
    Ok(Location {
        country: code,
        country_name: name,
        region,
        city,
        lat,
        lon,
        accuracy,
        locale,
        timezone,
        source: source.into(),
        canonical,
        notes,
    })
}

/// The zone of a place in a country that has several, from its region or
/// longitude; `(zone, estimated)`.
fn zone_for(code: &str, region: Option<&str>, lon: f64, base: &str) -> (String, bool) {
    let region = region.map(fold).unwrap_or_default();
    let pick = |zones: &[(f64, &str)]| {
        zones
            .iter()
            .find(|(west_of, _)| lon < *west_of)
            .map(|(_, z)| z.to_string())
            .unwrap_or_else(|| base.to_string())
    };
    match code {
        "ES" if region.contains("canar") => ("Atlantic/Canary".into(), false),
        "PT" if region.contains("azor") => ("Atlantic/Azores".into(), false),
        "US" => (
            pick(&[
                (-150.0, "Pacific/Honolulu"),
                (-130.0, "America/Anchorage"),
                (-114.5, "America/Los_Angeles"),
                (-101.5, "America/Denver"),
                (-86.5, "America/Chicago"),
            ]),
            true,
        ),
        "CA" => (
            pick(&[
                (-120.0, "America/Vancouver"),
                (-110.0, "America/Edmonton"),
                (-95.0, "America/Winnipeg"),
                (-64.0, "America/Toronto"),
                (-57.0, "America/Halifax"),
            ]),
            true,
        ),
        "MX" => (
            if lon >= -89.0 {
                "America/Cancun".to_string()
            } else {
                pick(&[(-114.0, "America/Tijuana"), (-105.5, "America/Mazatlan")])
            },
            true,
        ),
        "BR" => (
            pick(&[(-67.0, "America/Rio_Branco"), (-52.0, "America/Manaus")]),
            true,
        ),
        "AU" => (
            pick(&[(129.0, "Australia/Perth"), (141.0, "Australia/Adelaide")]),
            true,
        ),
        "RU" => (pick(&[(45.0, "Europe/Moscow")]), lon >= 45.0),
        _ => (base.to_string(), false),
    }
}

/// The `uule` value Google reads a canonical location name from (the
/// "w+CAIQICI" form: a protobuf with the name's length as one base64
/// alphabet character, then the name in base64).
pub fn uule(canonical: &str) -> String {
    const KEYS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let bytes = canonical.as_bytes();
    let length = KEYS[bytes.len() % KEYS.len()] as char;
    format!("w+CAIQICI{length}{}", crate::server::base64_encode(bytes))
}

/// Percent-encode a query value.
pub fn url_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// The search engines `cu serp` knows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Engine {
    Google,
    Bing,
    DuckDuckGo,
}

impl Engine {
    pub fn parse(name: &str) -> Result<Self, String> {
        match name.to_ascii_lowercase().as_str() {
            "" | "google" => Ok(Self::Google),
            "bing" => Ok(Self::Bing),
            "duckduckgo" | "ddg" => Ok(Self::DuckDuckGo),
            other => Err(format!(
                "unknown engine {other}; use google, bing or duckduckgo"
            )),
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Google => "google",
            Self::Bing => "bing",
            Self::DuckDuckGo => "duckduckgo",
        }
    }
}

/// The result page URL of `query` on `engine`, with that engine's own
/// location and language parameters when a location is given.
pub fn search_url(engine: Engine, query: &str, location: Option<&Location>, num: u32) -> String {
    let q = url_encode(query);
    let language = |l: &Location| l.locale.split('-').next().unwrap_or("en").to_string();
    match engine {
        Engine::Google => {
            // `pws=0` turns personalised results off, so two locations
            // differ by location and not by this profile's history.
            let mut url = format!("https://www.google.com/search?q={q}&pws=0");
            if num != 10 {
                url.push_str(&format!("&num={num}"));
            }
            if let Some(l) = location {
                url.push_str(&format!(
                    "&hl={}&gl={}&uule={}",
                    language(l),
                    l.country.to_ascii_lowercase(),
                    uule(&l.canonical)
                ));
            }
            url
        }
        Engine::Bing => {
            let mut url = format!("https://www.bing.com/search?q={q}");
            if num != 10 {
                url.push_str(&format!("&count={num}"));
            }
            if let Some(l) = location {
                url.push_str(&format!(
                    "&cc={}&setlang={}&mkt={}-{}",
                    l.country,
                    language(l),
                    language(l),
                    l.country
                ));
            }
            url
        }
        Engine::DuckDuckGo => {
            let mut url = format!("https://html.duckduckgo.com/html/?q={q}");
            if let Some(l) = location {
                url.push_str(&format!(
                    "&kl={}-{}",
                    l.country.to_ascii_lowercase(),
                    language(l)
                ));
            }
            url
        }
    }
}

/// What does not add up about a location applied to a context.
pub fn coherence(location: &Location, proxy: Option<&str>) -> Vec<String> {
    let mut warnings = location.notes.clone();
    if proxy.is_none() {
        warnings.push(
            "no proxy: requests leave through this machine's IP, so an engine that locates by IP \
             (Bing, Google's local pack) still sees this machine's place; use --proxy with an exit \
             in the same country for a complete location"
                .into(),
        );
    }
    if let Some(c) = country(&location.country) {
        let lang = location.locale.split('-').next().unwrap_or("");
        let native = c.locale.split('-').next().unwrap_or("");
        if !lang.is_empty() && lang != native {
            warnings.push(format!(
                "locale {} is not {}'s usual language ({}): fine on purpose (an English search in Spain), \
                 but a site may read it as a traveller",
                location.locale, c.name, c.locale
            ));
        }
    }
    warnings
}

/// The location as JSON.
pub fn json(location: &Location) -> String {
    use crate::server::json_string;
    let opt = |v: &Option<String>| {
        v.as_deref()
            .map(json_string)
            .unwrap_or_else(|| "null".into())
    };
    format!(
        "{{\"country\":{},\"country_name\":{},\"region\":{},\"city\":{},\"lat\":{},\"lon\":{},\"accuracy\":{},\"locale\":{},\"timezone\":{},\"source\":{},\"canonical\":{},\"uule\":{}}}",
        json_string(&location.country),
        json_string(&location.country_name),
        opt(&location.region),
        opt(&location.city),
        location.lat,
        location.lon,
        location.accuracy,
        json_string(&location.locale),
        json_string(&location.timezone),
        json_string(&location.source),
        json_string(&location.canonical),
        json_string(&uule(&location.canonical)),
    )
}

/// A context name for a location: `geo-es-sevilla`.
pub fn context_name(location: &Location) -> String {
    let mut name = format!("geo-{}", location.country.to_ascii_lowercase());
    for part in [&location.city, &location.region] {
        if let Some(p) = part {
            let slug: String = fold(p)
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
                .collect();
            let slug = slug.trim_matches('-').to_string();
            if !slug.is_empty() {
                name.push('-');
                name.push_str(&slug);
                break;
            }
        }
    }
    name.chars().take(60).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uule_matches_the_known_encoding() {
        // The value checked against google.com by hand: it localised a
        // "pizzeria" search to Seville.
        assert_eq!(
            uule("Sevilla,Andalusia,Spain"),
            "w+CAIQICIXU2V2aWxsYSxBbmRhbHVzaWEsU3BhaW4="
        );
    }

    #[test]
    fn a_spec_splits_into_town_region_and_country() {
        let r = Request::parse("Sevilla, Andalucía, ES").unwrap();
        assert_eq!(r.country, "ES");
        assert_eq!(r.city.as_deref(), Some("Sevilla"));
        assert_eq!(r.region.as_deref(), Some("Andalucía"));
        assert!(r.needs_lookup());
        let r = Request::parse("mx").unwrap();
        assert!(r.city.is_none() && !r.needs_lookup());
        assert!(Request::parse(" , ").is_err());
    }

    #[test]
    fn a_country_alone_falls_back_to_the_capital() {
        let l = complete(&Request::parse("España").unwrap(), None).unwrap();
        assert_eq!(l.country, "ES");
        assert_eq!(l.locale, "es-ES");
        assert_eq!(l.timezone, "Europe/Madrid");
        assert_eq!(l.source, "capital");
        assert_eq!(l.canonical, "Spain");
    }

    #[test]
    fn a_resolved_town_keeps_the_geocoder_names() {
        let place = parse_nominatim(
            r#"[{"lat":"37.3886","lon":"-5.9823","display_name":"Seville, Andalusia, Spain","address":{"city":"Seville","state":"Andalusia","country":"Spain","country_code":"es"}}]"#,
        )
        .unwrap();
        let l = complete(&Request::parse("Sevilla,ES").unwrap(), Some(&place)).unwrap();
        assert_eq!(l.source, "resolved");
        assert_eq!(l.city.as_deref(), Some("Sevilla"));
        assert_eq!(l.canonical, "Seville,Andalusia,Spain");
        assert!((l.lat - 37.3886).abs() < 1e-6);
    }

    #[test]
    fn the_canaries_get_their_own_zone_and_the_us_an_estimated_one() {
        let mut r = Request::parse("Las Palmas,Canarias,ES").unwrap();
        r.lat = Some(28.1);
        r.lon = Some(-15.4);
        assert_eq!(complete(&r, None).unwrap().timezone, "Atlantic/Canary");
        let mut r = Request::parse("Austin,Texas,US").unwrap();
        r.lat = Some(30.27);
        r.lon = Some(-97.74);
        let l = complete(&r, None).unwrap();
        assert_eq!(l.timezone, "America/Chicago");
        assert!(l.notes.iter().any(|n| n.contains("estimated")));
    }

    #[test]
    fn search_urls_carry_each_engines_own_parameters() {
        let l = complete(&Request::parse("Bilbao,Basque Country,ES").unwrap(), None).unwrap();
        let g = search_url(Engine::Google, "pizzería barata", Some(&l), 10);
        assert!(g.starts_with("https://www.google.com/search?q=pizzer%C3%ADa+barata&pws=0"));
        assert!(g.contains("&hl=es&gl=es&uule=w+CAIQICI"));
        let b = search_url(Engine::Bing, "x", Some(&l), 20);
        assert!(b.contains("&count=20&cc=ES&setlang=es&mkt=es-ES"));
        let d = search_url(Engine::DuckDuckGo, "x", Some(&l), 10);
        assert!(d.ends_with("&kl=es-es"));
        assert_eq!(context_name(&l), "geo-es-bilbao");
    }

    #[test]
    fn json_fields_build_a_request() {
        let r = Request::from_json(
            r#"{"geo":"Bilbao,ES","lat":43.26,"lon":"-2.93","timezone":"Europe/Madrid"}"#,
        )
        .unwrap()
        .unwrap();
        assert_eq!(r.city.as_deref(), Some("Bilbao"));
        assert_eq!(r.lat, Some(43.26));
        assert_eq!(r.lon, Some(-2.93));
        assert!(!r.needs_lookup());
        assert!(Request::from_json("{}").unwrap().is_none());
    }

    #[test]
    fn missing_proxy_is_reported() {
        let l = complete(&Request::parse("ES").unwrap(), None).unwrap();
        assert!(coherence(&l, None).iter().any(|w| w.contains("no proxy")));
        assert!(
            !coherence(&l, Some("http://es.proxy:8080"))
                .iter()
                .any(|w| w.contains("no proxy"))
        );
    }
}
