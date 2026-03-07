//! Yandex Maps integration: geocoding and static map image generation.

use tracing::info;

/// Geocode an address using Yandex Geocoder API.
/// Returns (display_name, longitude, latitude) or an error.
pub async fn geocode(address: &str, api_key: &str) -> Result<(String, f64, f64), String> {
    let url = format!(
        "https://geocode-maps.yandex.ru/1.x/?format=json&apikey={}&geocode={}",
        api_key,
        urlencoding::encode(address)
    );

    let resp = reqwest::get(&url)
        .await
        .map_err(|e| format!("Yandex geocode request failed: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("Yandex geocode HTTP {}", resp.status()));
    }

    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("Parse geocode response: {e}"))?;

    let feature = json
        .pointer("/response/GeoObjectCollection/featureMember/0/GeoObject")
        .ok_or("No results found for this address")?;

    let name = feature
        .pointer("/metaDataProperty/GeocoderMetaData/text")
        .and_then(|v| v.as_str())
        .unwrap_or(address)
        .to_string();

    let pos = feature
        .pointer("/Point/pos")
        .and_then(|v| v.as_str())
        .ok_or("No position in geocode result")?;

    let mut parts = pos.split_whitespace();
    let lon: f64 = parts.next().and_then(|s| s.parse().ok()).ok_or("Invalid longitude")?;
    let lat: f64 = parts.next().and_then(|s| s.parse().ok()).ok_or("Invalid latitude")?;

    info!("📍 Geocoded '{}' → {}, {} (lon, lat)", address, lon, lat);
    Ok((name, lon, lat))
}

/// Fetch a static map image from Yandex Static Maps API.
/// Returns PNG/JPEG bytes.
pub async fn static_map(lon: f64, lat: f64, api_key: &str, zoom: u8) -> Result<Vec<u8>, String> {
    let url = format!(
        "https://static-maps.yandex.ru/v1?apikey={}&ll={},{}&z={}&size=600,400&pt={},{},pm2rdm",
        api_key, lon, lat, zoom, lon, lat
    );

    let resp = reqwest::get(&url)
        .await
        .map_err(|e| format!("Yandex static map request failed: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("Yandex static map HTTP {}", resp.status()));
    }

    let bytes = resp.bytes().await.map_err(|e| format!("Read map image: {e}"))?;
    Ok(bytes.to_vec())
}
