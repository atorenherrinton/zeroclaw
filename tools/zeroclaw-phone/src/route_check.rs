//! Diagnostic matching of the existing configured phone topology. This does not
//! attest the process occupying a port or change ingress authentication/routing.

use crate::common::{self, SafeResult, check};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{net::SocketAddrV4, path::Path};

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteKind {
    Direct,
    GooglePushBridge,
}

// Extract only topology from the bridge's existing private configuration. Its
// notification credentials and other settings are neither used nor exposed.
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct BridgeRoute {
    root: String,
    #[serde(rename = "PublicURL")]
    public_url: String,
    listen: String,
    upstream: String,
}

pub fn select(
    tunnels: &Value,
    config_dir: &Path,
    public_base: &str,
    phone_port: u16,
) -> SafeResult<RouteKind> {
    check(phone_port != 0, "phone_route_port_invalid")?;
    let mut matches = tunnels
        .get("tunnels")
        .and_then(Value::as_array)
        .ok_or("tunnel_response_invalid")?
        .iter()
        .filter(|tunnel| tunnel["public_url"] == public_base);
    let tunnel = matches.next().ok_or("tunnel_route_mismatch")?;
    check(matches.next().is_none(), "tunnel_route_ambiguous")?;
    let address = tunnel["config"]["addr"]
        .as_str()
        .ok_or("tunnel_address_invalid")?;
    let phone_address = format!("http://127.0.0.1:{phone_port}");
    if address == phone_address {
        // A direct tunnel has no dependency on an optional bridge installation.
        return Ok(RouteKind::Direct);
    }

    let extensions = config_dir.join("extensions");
    let bridge_dir = extensions.join("google-push");
    for directory in [config_dir, extensions.as_path(), bridge_dir.as_path()] {
        common::existing_private_dir(directory)?;
    }
    let raw = common::private_read(&bridge_dir.join("config.json"))?;
    // Go's bridge decoder accepts case-folded aliases and takes the later
    // occurrence. Do not silently ignore an alias that could override a route
    // field. Non-ASCII names can also case-fold to ASCII in Go (for example ſ).
    let fields: Value = serde_json::from_str(&raw).map_err(|_| "bridge_route_config_invalid")?;
    let fields = fields.as_object().ok_or("bridge_route_config_invalid")?;
    check(
        fields.keys().all(|key| {
            key.is_ascii()
                && ["Root", "PublicURL", "Listen", "Upstream"]
                    .iter()
                    .all(|canonical| {
                        !key.eq_ignore_ascii_case(canonical) || key.as_str() == *canonical
                    })
        }),
        "bridge_route_config_invalid",
    )?;
    // Parse the original bytes again so exact duplicate topology fields remain
    // an error; decoding only the Value above would silently keep the last one.
    let bridge: BridgeRoute =
        serde_json::from_str(&raw).map_err(|_| "bridge_route_config_invalid")?;
    check(
        Path::new(&bridge.root) == bridge_dir && bridge.public_url == public_base,
        "bridge_route_identity_mismatch",
    )?;
    check(
        bridge.upstream == phone_address,
        "bridge_phone_upstream_mismatch",
    )?;
    let listener: SocketAddrV4 = bridge
        .listen
        .parse()
        .map_err(|_| "bridge_listener_invalid")?;
    check(
        *listener.ip() == std::net::Ipv4Addr::LOCALHOST
            && listener.port() != 0
            && listener.port() != phone_port
            && bridge.listen == listener.to_string(),
        "bridge_listener_invalid",
    )?;
    check(
        address == format!("http://{listener}"),
        "tunnel_route_mismatch",
    )?;
    Ok(RouteKind::GooglePushBridge)
}

#[cfg(test)]
mod tests;
