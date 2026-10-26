//! maps — geocoding, POI search, road distance/directions and timezone lookups
//! over plain HTTP (OpenStreetMap Nominatim, OSRM, TimeAPI.io).
//!
//! Port of the `maps` productivity skill's `maps_client.py` operations as a
//! single action-dispatched tool: `search` (Nominatim geocode), `reverse`,
//! `distance` and `directions` (OSRM route with the driving/walking/cycling
//! profile map), and `timezone` (TimeAPI.io coordinate lookup). Endpoint URLs,
//! parameter names and the OSRM profile mapping are the skill's, verbatim.
//!
//! The HTTP seam is the injected `HttpGet` trait (tests stub it; no live
//! network). Every action returns a JSON object; unknown actions and invalid
//! coordinates are `{"error": ...}`.

use serde_json::{json, Value};

use crate::util::tool_error;

/// `maps_client.py` constants.
pub const NOMINATIM_SEARCH: &str = "https://nominatim.openstreetmap.org/search";
pub const NOMINATIM_REVERSE: &str = "https://nominatim.openstreetmap.org/reverse";
pub const OSRM_BASE: &str = "https://router.project-osrm.org/route/v1";
pub const TIMEAPI_BASE: &str = "https://timeapi.io/api/timezone/coordinate";
pub const USER_AGENT: &str = "HermesAgent/1.0 (contact: hermes@agent.ai)";

/// `OSRM_PROFILES`: the agent-facing mode → OSRM profile.
pub const OSRM_PROFILES: &[(&str, &str)] = &[
    ("driving", "driving"),
    ("walking", "foot"),
    ("cycling", "bike"),
];

/// One authenticated-less GET returning a JSON body.
pub trait HttpGet: Send + Sync {
    fn get(&self, url: &str) -> Result<String, String>;
}

/// The production getter: the shared blocking client with the skill's UA.
pub struct ReqwestGetter;

impl HttpGet for ReqwestGetter {
    fn get(&self, url: &str) -> Result<String, String> {
        let client = crate::web_common::http_client();
        let resp = client
            .get(url)
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .map_err(|e| format!("maps request failed: {e}"))?;
        let status = resp.status();
        let body = resp.text().map_err(|e| format!("maps read failed: {e}"))?;
        if !status.is_success() {
            return Err(format!("maps request returned HTTP {}", status.as_u16()));
        }
        Ok(body)
    }
}

fn osrm_profile(mode: &str) -> Option<&'static str> {
    OSRM_PROFILES
        .iter()
        .find(|(name, _)| *name == mode.to_lowercase().as_str())
        .map(|(_, profile)| *profile)
}

fn valid_coord(lat: f64, lon: f64) -> bool {
    (-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon)
}

/// Nominatim search rows → compact `{name, lat, lon, type, display_name}`.
pub fn parse_search_results(body: &str) -> Vec<Value> {
    let Ok(rows) = serde_json::from_str::<Value>(body) else {
        return Vec::new();
    };
    rows.as_array()
        .map(|arr| {
            arr.iter()
                .map(|r| {
                    json!({
                        "name": r.get("name").cloned().unwrap_or(Value::Null),
                        "lat": r.get("lat").cloned().unwrap_or(Value::Null),
                        "lon": r.get("lon").cloned().unwrap_or(Value::Null),
                        "type": r.get("type").cloned().unwrap_or(Value::Null),
                        "display_name": r.get("display_name").cloned().unwrap_or(Value::Null),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// OSRM route body → `(duration_s, distance_m)`.
pub fn parse_route(body: &str) -> Result<(f64, f64), String> {
    let parsed: Value =
        serde_json::from_str(body).map_err(|e| format!("OSRM returned invalid JSON: {e}"))?;
    if parsed.get("code").and_then(Value::as_str) != Some("Ok") {
        let message = parsed
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                parsed
                    .get("code")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_else(|| "unknown error".to_string());
        return Err(format!("OSRM routing failed: {message}"));
    }
    let route = parsed
        .get("routes")
        .and_then(Value::as_array)
        .and_then(|r| r.first())
        .ok_or_else(|| "OSRM routing failed: no routes".to_string())?;
    let duration = route.get("duration").and_then(Value::as_f64).unwrap_or(0.0);
    let distance = route.get("distance").and_then(Value::as_f64).unwrap_or(0.0);
    Ok((duration, distance))
}

/// `_format_duration`: hours + minutes + seconds, human units.
pub fn format_duration(seconds: f64) -> String {
    let total = seconds.round() as i64;
    let (h, rem) = (total / 3600, total % 3600);
    let (m, s) = (rem / 60, rem % 60);
    if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m {s}s")
    } else {
        format!("{s}s")
    }
}

/// `_format_distance`: km with one decimal above 1km, metres below.
pub fn format_distance(metres: f64) -> String {
    if metres >= 1000.0 {
        format!("{:.1} km", metres / 1000.0)
    } else {
        format!("{metres:.0} m")
    }
}

/// Geocode a free-text query, taking the first hit (Python `geocode_single`).
fn geocode_single(getter: &dyn HttpGet, query: &str) -> Result<(f64, f64, String), String> {
    let url = format!(
        "{NOMINATIM_SEARCH}?q={}&format=json&limit=5&addressdetails=1",
        urlencode(query)
    );
    let body = getter.get(&url)?;
    let rows = parse_search_results(&body);
    let first = rows
        .first()
        .ok_or_else(|| format!("No results for {query:?}."))?;
    let lat = first["lat"]
        .as_str()
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0);
    let lon = first["lon"]
        .as_str()
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0);
    Ok((
        lat,
        lon,
        first["display_name"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
    ))
}

/// Minimal form encoding for a single query value.
fn urlencode(value: &str) -> String {
    let mut out = String::new();
    for b in value.bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Execute `maps` — the registry entry point.
pub fn execute(args: &Value) -> String {
    let getter = ReqwestGetter;
    execute_with(&getter, args)
}

/// `execute` against an injected getter (tests, embedding hosts).
pub fn execute_with(getter: &dyn HttpGet, args: &Value) -> String {
    let action = args
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_lowercase();
    match action.as_str() {
        "search" | "geocode" => search(getter, args),
        "reverse" => reverse(getter, args),
        "distance" => route(getter, args, false),
        "directions" => route(getter, args, true),
        "timezone" => timezone(getter, args),
        other => tool_error(format!(
            "Unknown maps action: {other}. Use: search, reverse, distance, directions, timezone"
        )),
    }
}

fn search(getter: &dyn HttpGet, args: &Value) -> String {
    let Some(query) = args
        .get("query")
        .and_then(Value::as_str)
        .filter(|q| !q.trim().is_empty())
    else {
        return tool_error("query is required for action='search'");
    };
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(5)
        .clamp(1, 50);
    let url = format!(
        "{NOMINATIM_SEARCH}?q={}&format=json&limit={limit}&addressdetails=1",
        urlencode(query)
    );
    match getter.get(&url) {
        Ok(body) => {
            let rows = parse_search_results(&body);
            json!({ "success": true, "query": query, "results": rows, "count": rows.len() })
                .to_string()
        }
        Err(e) => tool_error(e),
    }
}

fn reverse(getter: &dyn HttpGet, args: &Value) -> String {
    let (Some(lat), Some(lon)) = (
        args.get("lat").and_then(Value::as_f64),
        args.get("lon").and_then(Value::as_f64),
    ) else {
        return tool_error("lat and lon (numbers) are required for action='reverse'");
    };
    if !valid_coord(lat, lon) {
        return tool_error("lat must be within [-90, 90] and lon within [-180, 180]");
    }
    let url = format!("{NOMINATIM_REVERSE}?lat={lat}&lon={lon}&format=json&addressdetails=1");
    match getter.get(&url) {
        Ok(body) => {
            let parsed: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            json!({
                "success": true,
                "lat": parsed.get("lat").cloned().unwrap_or(Value::Null),
                "lon": parsed.get("lon").cloned().unwrap_or(Value::Null),
                "display_name": parsed.get("display_name").cloned().unwrap_or(Value::Null),
            })
            .to_string()
        }
        Err(e) => tool_error(e),
    }
}

fn route(getter: &dyn HttpGet, args: &Value, steps: bool) -> String {
    let Some(mode) = args
        .get("mode")
        .and_then(Value::as_str)
        .map(|m| m.to_lowercase())
    else {
        return tool_error("mode is required (driving, walking, cycling)");
    };
    let Some(profile) = osrm_profile(&mode) else {
        return tool_error(format!(
            "Invalid mode '{}'. Choose from: {}",
            mode,
            OSRM_PROFILES
                .iter()
                .map(|(n, _)| *n)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    };
    let (Some(from), Some(to)) = (args.get("from"), args.get("to")) else {
        return tool_error("from and to (free-text places) are required");
    };
    for (label, place) in [("from", from), ("to", to)] {
        if place.as_str().unwrap_or("").trim().is_empty() {
            return tool_error(format!("{label} cannot be empty"));
        }
    }
    let (o_lat, o_lon, o_name) = match geocode_single(getter, from.as_str().unwrap()) {
        Ok(v) => v,
        Err(e) => return tool_error(e),
    };
    let (d_lat, d_lon, d_name) = match geocode_single(getter, to.as_str().unwrap()) {
        Ok(v) => v,
        Err(e) => return tool_error(e),
    };
    let url = format!(
        "{OSRM_BASE}/{profile}/{o_lon},{o_lat};{d_lon},{d_lat}?overview=false&steps={}",
        if steps { "true" } else { "false" }
    );
    match getter.get(&url) {
        Ok(body) => match parse_route(&body) {
            Ok((duration, distance)) => json!({
                "success": true,
                "from": o_name,
                "to": d_name,
                "mode": mode,
                "duration_s": duration,
                "distance_m": distance,
                "duration": format_duration(duration),
                "distance": format_distance(distance),
                "steps": if steps {
                    json!(steps_list(&body))
                } else {
                    Value::Null
                },
            })
            .to_string(),
            Err(e) => tool_error(e),
        },
        Err(e) => tool_error(e),
    }
}

/// OSRM `steps=true` turn-by-turn list: `{name, instruction, distance_m}`.
fn steps_list(body: &str) -> Vec<Value> {
    let Ok(parsed) = serde_json::from_str::<Value>(body) else {
        return Vec::new();
    };
    parsed["routes"][0]["legs"][0]["steps"]
        .as_array()
        .map(|steps| {
            steps
                .iter()
                .map(|s| {
                    json!({
                        "name": s.get("name").cloned().unwrap_or(Value::Null),
                        "instruction": s.get("maneuver").and_then(|m| m.get("instruction")).cloned().unwrap_or(Value::Null),
                        "distance_m": s.get("distance").cloned().unwrap_or(Value::Null),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn timezone(getter: &dyn HttpGet, args: &Value) -> String {
    let (Some(lat), Some(lon)) = (
        args.get("lat").and_then(Value::as_f64),
        args.get("lon").and_then(Value::as_f64),
    ) else {
        return tool_error("lat and lon (numbers) are required for action='timezone'");
    };
    if !valid_coord(lat, lon) {
        return tool_error("Latitude must be between -90 and 90.");
    }
    let url = format!("{TIMEAPI_BASE}?latitude={lat}&longitude={lon}");
    match getter.get(&url) {
        Ok(body) => {
            let parsed: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            json!({
                "success": true,
                "lat": lat,
                "lon": lon,
                "timezone": parsed.get("timeZone").cloned().unwrap_or(Value::Null),
                "current_local_time": parsed.get("currentLocalTime").cloned().unwrap_or(Value::Null),
                "utc_offset": parsed
                    .get("currentUtcOffset")
                    .and_then(|o| o.get("hours"))
                    .cloned()
                    .unwrap_or(Value::Null),
            })
            .to_string()
        }
        Err(e) => tool_error(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    /// Scripted getter: pops URLs, answers fixtures, records requests.
    struct StubGetter {
        responses: Mutex<Vec<Result<String, String>>>,
        requests: Mutex<Vec<String>>,
    }

    impl StubGetter {
        fn new(responses: Vec<Result<String, String>>) -> Self {
            Self {
                responses: Mutex::new(responses),
                requests: Mutex::new(Vec::new()),
            }
        }
    }

    impl HttpGet for StubGetter {
        fn get(&self, url: &str) -> Result<String, String> {
            self.requests.lock().unwrap().push(url.to_string());
            self.responses
                .lock()
                .unwrap()
                .pop()
                .unwrap_or(Err("no fixture".into()))
        }
    }

    fn v(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    const NOMINATIM_ROW: &str = r#"[{"lat":"41.3828","lon":"2.1769","name":"Barcelona","display_name":"Barcelona, Spain","type":"city"}]"#;

    const OSRM_ROUTE: &str = r#"{"code":"Ok","routes":[{"duration":5043.2,"distance":81234.5,"legs":[{"steps":[{"name":"Carrer A","distance":120.0,"maneuver":{"instruction":"Turn right"}},{"name":"","distance":3000.0,"maneuver":{"instruction":"Continue"}}]}]}]}"#;

    #[test]
    fn search_uses_nominatim_url_and_returns_rows() {
        let stub = StubGetter::new(vec![Ok(NOMINATIM_ROW.into())]);
        let out = v(&execute_with(
            &stub,
            &json!({"action": "search", "query": "barcelona", "limit": 2}),
        ));
        assert_eq!(out["success"], true);
        assert_eq!(out["count"], 1);
        assert_eq!(out["results"][0]["name"], "Barcelona");
        let url = &stub.requests.lock().unwrap()[0];
        assert!(url.starts_with(NOMINATIM_SEARCH), "{url}");
        assert!(url.contains("limit=2"));
        assert!(url.contains("q=barcelona"));
    }

    #[test]
    fn search_requires_query() {
        let out = v(&execute_with(
            &StubGetter::new(vec![]),
            &json!({"action": "search"}),
        ));
        assert!(out["error"].as_str().unwrap().contains("query is required"));
    }

    #[test]
    fn reverse_builds_url_and_returns_display_name() {
        let stub = StubGetter::new(vec![Ok(
            r#"{"lat":"41.38","lon":"2.17","display_name":"Sagrada Familia"}"#.into(),
        )]);
        let out = v(&execute_with(
            &stub,
            &json!({"action": "reverse", "lat": 41.38, "lon": 2.17}),
        ));
        assert_eq!(out["display_name"], "Sagrada Familia");
        assert!(stub.requests.lock().unwrap()[0].starts_with(NOMINATIM_REVERSE));
    }

    #[test]
    fn reverse_validates_ranges() {
        let out = v(&execute_with(
            &StubGetter::new(vec![]),
            &json!({"action": "reverse", "lat": 95.0, "lon": 0.0}),
        ));
        assert!(out["error"].as_str().unwrap().contains("within [-90, 90]"));
        let out = v(&execute_with(
            &StubGetter::new(vec![]),
            &json!({"action": "reverse"}),
        ));
        assert!(out["error"].as_str().unwrap().contains("lat and lon"));
    }

    #[test]
    fn distance_geocodes_both_ends_and_parses_osrm() {
        let stub = StubGetter::new(vec![
            Ok(OSRM_ROUTE.into()),
            Ok(NOMINATIM_ROW.into()),
            Ok(r#"[{"lat":"48.8566","lon":"2.3522","name":"Paris","display_name":"Paris, France"}]"#.into()),
        ]);
        let out = v(&execute_with(
            &stub,
            &json!({"action": "distance", "from": "Barcelona", "to": "Paris", "mode": "driving"}),
        ));
        assert_eq!(out["success"], true);
        assert_eq!(out["mode"], "driving");
        assert_eq!(out["duration_s"], 5043.2);
        assert_eq!(out["distance_m"], 81234.5);
        assert_eq!(out["duration"], "1h 24m");
        assert_eq!(out["distance"], "81.2 km");
        let reqs = stub.requests.lock().unwrap();
        assert!(reqs[2].contains(OSRM_BASE));
        assert!(
            reqs[2].contains("/driving/2.3522,48.8566;2.1769,41.3828"),
            "{}",
            reqs[2]
        );
        assert!(reqs[2].ends_with("?overview=false&steps=false"));
    }

    #[test]
    fn directions_include_steps() {
        let stub = StubGetter::new(vec![
            Ok(OSRM_ROUTE.into()),
            Ok(NOMINATIM_ROW.into()),
            Ok(NOMINATIM_ROW.into()),
        ]);
        let out = v(&execute_with(
            &stub,
            &json!({"action": "directions", "from": "Barcelona", "to": "Barcelona", "mode": "walking"}),
        ));
        assert_eq!(out["success"], true);
        let steps = out["steps"].as_array().unwrap();
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0]["instruction"], "Turn right");
        let url = stub.requests.lock().unwrap().last().unwrap().clone();
        assert!(url.contains("/foot/"), "{url}");
        assert!(url.ends_with("steps=true"));
    }

    #[test]
    fn invalid_mode_is_a_clear_error() {
        let out = v(&execute_with(
            &StubGetter::new(vec![]),
            &json!({"action": "distance", "from": "a", "to": "b", "mode": "swimming"}),
        ));
        assert!(out["error"]
            .as_str()
            .unwrap()
            .contains("Invalid mode 'swimming'"));
    }

    #[test]
    fn osrm_failure_code_surfaces_the_message() {
        let stub = StubGetter::new(vec![
            Ok(r#"{"code":"NoRoute","message":"No route found"}"#.into()),
            Ok(NOMINATIM_ROW.into()),
            Ok(NOMINATIM_ROW.into()),
        ]);
        let out = v(&execute_with(
            &stub,
            &json!({"action": "distance", "from": "x", "to": "y", "mode": "cycling"}),
        ));
        assert_eq!(out["error"], "OSRM routing failed: No route found");
    }

    #[test]
    fn timezone_parses_timeapi_fields() {
        let stub = StubGetter::new(vec![Ok(r#"{"timeZone":"Europe/Madrid","currentLocalTime":"2026-10-26T12:00:00","currentUtcOffset":{"hours":2,"minutes":0}}"#.into())]);
        let out = v(&execute_with(
            &stub,
            &json!({"action": "timezone", "lat": 41.38, "lon": 2.17}),
        ));
        assert_eq!(out["timezone"], "Europe/Madrid");
        assert_eq!(out["utc_offset"], 2);
        assert!(stub.requests.lock().unwrap()[0].starts_with(TIMEAPI_BASE));
    }

    #[test]
    fn unknown_action_and_formatting_helpers() {
        let out = v(&execute_with(
            &StubGetter::new(vec![]),
            &json!({"action": "teleport"}),
        ));
        assert!(out["error"]
            .as_str()
            .unwrap()
            .contains("Unknown maps action"));
        assert_eq!(format_duration(4523.0), "1h 15m");
        assert_eq!(format_duration(125.0), "2m 5s");
        assert_eq!(format_duration(7.0), "7s");
        assert_eq!(format_distance(81234.5), "81.2 km");
        assert_eq!(format_distance(850.0), "850 m");
    }
}
